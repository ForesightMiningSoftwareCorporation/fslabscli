use std::collections::{HashMap, HashSet};

use anyhow::Context;
use http_body_util::BodyExt;
use http_body_util::Empty;
use hyper::body::Bytes;
use hyper::{Method, Request, Uri};
use hyper_rustls::{ConfigBuilderExt, HttpsConnector};
use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::env;
use std::{
    fs,
    path::{Path, PathBuf},
};
use toml_edit::{DocumentMut, Table, table, value};
use walkdir::WalkDir;

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct CargoRegistry {
    pub index: Option<String>,
    pub crate_url: Option<String>,
    pub token: Option<String>,
    pub user_agent: Option<String>,
}

impl CargoRegistry {
    /// Merge another CargoRegistry into this one.
    fn merge(&mut self, other: &CargoRegistry) {
        if self.index.is_none() {
            if let Some(index) = &other.index {
                self.index = Some(index.clone());
            }
        }
        if self.crate_url.is_none() {
            if let Some(crate_url) = &other.crate_url {
                self.crate_url = Some(crate_url.clone());
            }
        }
        if self.token.is_none() {
            if let Some(token) = &other.token {
                self.token = Some(token.clone());
            }
        }
        if self.user_agent.is_none() {
            if let Some(user_agent) = &other.user_agent {
                self.user_agent = Some(user_agent.clone());
            }
        }
    }
    pub fn new(
        name: String,
        index: Option<String>,
        crate_url: Option<String>,
        token: Option<String>,
        user_agent: Option<String>,
    ) -> Self {
        let mut config = Self {
            index,
            crate_url,
            token,
            user_agent,
        };
        config.merge(&CargoRegistry::new_from_env(name.clone()));
        config.merge(&CargoRegistry::new_from_config(name.clone()));
        config
    }

    pub fn new_from_env(name: String) -> Self {
        let env_name = name.to_uppercase().replace("-", "_").replace(".", "_");
        let index = env::var(format!("CARGO_REGISTRIES_{}_INDEX", env_name)).ok();
        let crate_url = env::var(format!("CARGO_REGISTRIES_{}_CRATE_URL", env_name)).ok();
        let token = env::var(format!("CARGO_REGISTRIES_{}_TOKEN", env_name)).ok();
        let user_agent = match name.as_str() {
            "crates.io" => None,
            _ => env::var(format!("CARGO_REGISTRIES_{}_USER_AGENT", env_name)).ok(),
        };

        Self {
            index,
            crate_url,
            token,
            user_agent,
        }
    }

    pub fn new_from_config(name: String) -> Self {
        let mut config = Self {
            index: None,
            crate_url: None,
            token: None,
            user_agent: None,
        };
        let mut config_files = vec![];
        if let Ok(mut current_dir) = env::current_dir() {
            // Search parent directories until we reach the root
            loop {
                let config_path = current_dir.join(".config/config.toml");
                if config_path.exists() {
                    config_files.push(config_path);
                }

                // If we're at the root, stop
                if !current_dir.pop() {
                    break;
                }
            }
            if let Ok(cargo_home) = env::var("CARGO_HOME") {
                let config_path = PathBuf::from(cargo_home).join("config.toml");
                if config_path.exists() {
                    config_files.push(config_path);
                }
            }
            config_files.reverse();
            for config_file in config_files {
                if let Ok(config_str) = fs::read_to_string(config_file) {
                    if let Ok(cargo_config) = toml::de::from_str::<Cargo>(&config_str) {
                        if let Some(registry_config) = cargo_config.registries.get(&name) {
                            config.merge(registry_config);
                        }
                    }
                }
            }
        }
        config
    }
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
struct CargoPackageVersion {
    #[serde(alias = "vers", alias = "num")]
    pub version: String,
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
struct CargoPackage {
    name: String,
    versions: Vec<CargoPackageVersion>,
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
struct CargoSinglePackage {
    name: String,
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
struct CargoSearchResult {
    crates: Option<Vec<CargoPackage>>,
    #[serde(rename = "crate")]
    single_crate: Option<CargoSinglePackage>,
    versions: Option<Vec<CargoPackageVersion>>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Cargo {
    #[serde(rename = "registry", default)]
    registries: HashMap<String, CargoRegistry>,
    #[serde(skip)]
    client: Option<HyperClient<HttpsConnector<HttpConnector>, Empty<Bytes>>>,
}

impl Cargo {
    pub fn new(registries: &HashSet<String>) -> anyhow::Result<Self> {
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(
                rustls::ClientConfig::builder()
                    .with_native_roots()?
                    .with_no_client_auth(),
            )
            .https_or_http()
            .enable_http1()
            .build();

        Ok(Self {
            client: Some(HyperClient::builder(TokioExecutor::new()).build(https)),
            registries: registries
                .iter()
                .map(|k| {
                    (
                        k.clone(),
                        CargoRegistry::new(k.clone(), None, None, None, None),
                    )
                })
                .collect(),
        })
    }

    pub async fn check_crate_exists(
        &self,
        registry_name: String,
        name: String,
        version: String,
    ) -> anyhow::Result<bool> {
        let registry = self
            .registries
            .get(&registry_name)
            .ok_or_else(|| anyhow::anyhow!("unknown registry"))?;

        // We need an url
        if let Some(crate_url) = &registry.crate_url {
            let url: Uri = format!("{}{}", crate_url, name).parse()?;

            let user_agent = registry
                .user_agent
                .clone()
                .unwrap_or_else(|| "fslabsci".to_string());

            let Some(token) = registry.token.clone() else {
                return Err(anyhow::anyhow!(
                    "looking registry information without setting token: {}",
                    &registry_name
                ));
            };

            let req = Request::builder()
                .method(Method::GET)
                .uri(url.clone())
                .header("Content-Type", "application/json")
                .header("Accept", "application/json")
                .header("Authorization", token)
                .header("User-Agent", user_agent.clone())
                .body(Empty::default())?;

            let res = self
                .client
                .clone()
                .context("unitialized http client")?
                .request(req)
                .await
                .with_context(|| "Could not fetch from the crates registry")?;

            if res.status().as_u16() == 404 {
                // New crates
                return Ok(true);
            }
            if res.status().as_u16() >= 400 {
                anyhow::bail!(
                    "Something went wrong while getting cargo {} api data",
                    registry_name
                );
            }

            let body = res
                .into_body()
                .collect()
                .await
                .with_context(|| "Could not get body from the npm registry")?
                .to_bytes();

            let body_str = String::from_utf8_lossy(&body);
            let package: Option<CargoPackage> =
                match serde_json::from_str::<CargoSearchResult>(body_str.as_ref()) {
                    Ok(search_result) => match (search_result.crates, search_result.single_crate) {
                        (Some(crates), None) => crates.into_iter().find(|c| c.name == name),
                        (_, Some(single_crate)) => {
                            if let Some(versions) = search_result.versions {
                                Some(CargoPackage {
                                    name: single_crate.name,
                                    versions,
                                })
                            } else {
                                None
                            }
                        }
                        _ => None,
                    },
                    Err(e) => {
                        println!("Got error: {}", e);
                        None
                    }
                };

            if let Some(package) = package {
                for package_version in package.versions {
                    if package_version.version == version {
                        return Ok(true);
                    }
                }
            }
        } else {
            return Err(anyhow::anyhow!(
                "no api url setup for registry: {}",
                registry_name,
            ));
        }
        Ok(false)
    }

    pub fn get_registry(&self, name: &str) -> Option<&CargoRegistry> {
        self.registries.get(name)
    }
}

fn replace_registry_in_cargo_toml(path: &Path, target_registry_name: String) -> anyhow::Result<()> {
    // Read the content of the Cargo.toml file
    let content = fs::read_to_string(path)?;

    // Define your regex to find what you want to replace
    let re = Regex::new(r##"registry += +"(.*?)""##)?;
    let modified_content = re
        .replace_all(&content, format!("registry = \"{}\"", target_registry_name))
        .to_string();

    // Write the modified content back to the file
    fs::write(path, modified_content)?;

    Ok(())
}

pub fn patch_crate_for_registry(
    root_directory: &Path,
    working_directory: &Path,
    target_registry_name: String,
) -> anyhow::Result<()> {
    let cargo_toml_path = working_directory.join("Cargo.toml");
    let cargo_lock_path = working_directory.join("Cargo.lock");
    // Read the Cargo.toml file
    let toml_str = fs::read_to_string(&cargo_toml_path)?;
    let mut doc: DocumentMut = toml_str.parse()?;
    let mut publish_registries = toml_edit::Array::new();
    publish_registries.push(target_registry_name.clone());
    let mut empty_table = table();
    let package_table: &mut Table = doc
        .get_mut("package")
        .unwrap_or(&mut empty_table)
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("Could not get table from package "))?;
    package_table.insert("publish", value(publish_registries));

    fs::write(&cargo_toml_path, doc.to_string())?;

    // 2. Find and replace all the registry value with the provided `registry_name`
    for entry in WalkDir::new(root_directory).into_iter() {
        let entry = entry?;
        if entry.path().ends_with("Cargo.toml") {
            // Perform replacement for each Cargo.toml file found
            replace_registry_in_cargo_toml(entry.path(), target_registry_name.clone())?;
        }
    }

    // 3. If Cargo.lock exists, delete it
    if Path::new(&cargo_lock_path).exists() {
        fs::remove_file(cargo_lock_path)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[test]
    fn test_publish_key_replaced_if_present() {
        let tmp = assert_fs::TempDir::new()
            .unwrap()
            .into_persistent()
            .to_path_buf();

        // Prepare mock Cargo.toml
        let cargo_toml_path = tmp.join("Cargo.toml");

        // Create mock Cargo.toml with no publish key and no registry
        let toml_content = r#"[package]
name = "my-package"
version = "0.1.0"
publish = ["main_registry"]"#;
        fs::write(&cargo_toml_path, toml_content).unwrap();

        // Run the patch_crate_for_registry function with a registry name and main_registry
        assert!(
            patch_crate_for_registry(&tmp, &tmp, "my_registry".to_string())
                .map_err(|e| {
                    println!("Error: {:#?}", e);
                    e
                })
                .is_ok()
        );

        // Read the updated Cargo.toml and check if `publish` was correctly updated
        let updated_toml = fs::read_to_string(cargo_toml_path).unwrap();
        assert!(updated_toml.contains("publish = [\"my_registry\"]"));
    }

    #[test]
    fn test_find_and_replace_registry_in_dependencies() {
        let tmp = assert_fs::TempDir::new()
            .unwrap()
            .into_persistent()
            .to_path_buf();

        // Prepare mock Cargo.toml
        let cargo_toml_path = tmp.join("Cargo.toml");

        // Create mock Cargo.toml with main_registry
        let toml_content = r#"[package]
name = "my-package"
version = "0.1.0"
dependencies = { some_crate = { registry = "main_registry" } }"#;
        fs::write(&cargo_toml_path, toml_content).unwrap();

        // Run the patch_crate_for_registry function with a registry name and main_registry
        assert!(patch_crate_for_registry(&tmp, &tmp, "my_registry".to_string()).is_ok());

        // Read the updated Cargo.toml and check if `main_registry` was replaced
        let updated_toml = fs::read_to_string(cargo_toml_path).unwrap();
        assert!(updated_toml.contains("registry = \"my_registry\""));
    }

    #[test]
    fn test_publish_key_added_if_missing() {
        let tmp = assert_fs::TempDir::new()
            .unwrap()
            .into_persistent()
            .to_path_buf();

        // Prepare mock Cargo.toml
        let cargo_toml_path = tmp.join("Cargo.toml");

        // Create mock Cargo.toml with no publish key and no registry
        let toml_content = r#"[package]
name = "my-package"
version = "0.1.0""#;
        fs::write(&cargo_toml_path, toml_content).unwrap();

        // Run the patch_crate_for_registry function with a registry name and main_registry
        assert!(patch_crate_for_registry(&tmp, &tmp, "my_registry".to_string()).is_ok());

        // Read the updated Cargo.toml and check if `publish` was correctly updated
        let updated_toml = fs::read_to_string(cargo_toml_path).unwrap();
        assert!(updated_toml.contains("publish = [\"my_registry\"]"));
    }

    #[test]
    fn test_existing_cargo_lock() {
        let tmp = assert_fs::TempDir::new()
            .unwrap()
            .into_persistent()
            .to_path_buf();

        // Prepare mock Cargo.toml
        let cargo_toml_path = tmp.join("Cargo.toml");
        let cargo_lock_path = tmp.join("Cargo.lock");

        // Create mock Cargo.toml
        let toml_content = r#"[package]
name = "my-package"
version = "0.1.0""#;
        fs::write(&cargo_toml_path, toml_content).unwrap();

        // Create a mock Cargo.lock file
        fs::write(&cargo_lock_path, "lock data").unwrap();

        // Run the patch_crate_for_registry function and check for success
        assert!(patch_crate_for_registry(&tmp, &tmp, "my_registry".to_string()).is_ok());

        // Ensure Cargo.lock is deleted
        assert!(!cargo_lock_path.exists());
    }
}
//     use wiremock::matchers::{header, method, path};
//     use wiremock::{Mock, MockServer, ResponseTemplate};

//     use crate::crate_graph;

//     use super::*;

//     const EXISTING_PACKAGE_DATA: &str = "{\"org\":{\"id\":\"0184cce5-d7f7-d027-dc92-03ecd4bdfd44\",\"name\":\"Foresight Mining Software Corporation\",\"slug\":\"foresight-mining-software-corporation\"},\"n_crates\":1,\"n_crate_versions\":6,\"total_downloads\":25,\"crates\":[{\"id\":\"018c8382-17f4-11e6-dba9-fadb50dd1f74\",\"name\":\"hub_app\",\"total_downloads\":25,\"versions\":[{\"id\":\"018d8de9-8e73-c788-03f7-02926da47171\",\"vers\":\"0.2.0\",\"user_id\":\"0184cce5-d802-0d87-da96-33779594d8cc\",\"published\":\"2024-02-09T12:48:30.322924Z\",\"published_unix\":1707482910,\"meta\":{\"description\":null,\"categories\":[],\"keywords\":[],\"repository\":null,\"deps\":[],\"readme\":\"# `hub_app`\\n\\nShared library for applications that are launched by the Hub Launcher. Provides input data for these\\napplications so they know where to store/access data, as well as what project file should be opened\\nwhen the application starts.\"},\"raw_publish_meta\":{\"deps\":[],\"name\":\"hub_app\",\"vers\":\"0.4.1\",\"links\":null,\"badges\":{},\"readme\":\"# `hub_app`\\n\\nShared library for applications that are launched by the Hub Launcher. Provides input data for these\\napplications so they know where to store/access data, as well as what project file should be opened\\nwhen the application starts.\",\"authors\":[],\"license\":null,\"features\":{\"beta\":[],\"prod\":[],\"alpha\":[\"beta\"],\"default\":[\"embedded_assets\"],\"nightly\":[\"alpha\",\"beta\"],\"devtools\":[],\"run_init_logic\":[],\"embedded_assets\":[\"bevy_embedded_assets\"]},\"homepage\":null,\"keywords\":[],\"categories\":[],\"repository\":null,\"description\":null,\"readme_file\":\"README.md\",\"license_file\":null,\"rust_version\":null,\"documentation\":null},\"docs_url\":\"/foresight-mining-software-corporation/hub_app/0.4.1/docs\",\"n_downloads\":1,\"yanked\":null,\"is_yanked\":false}],\"latest_version\":\"0.2.0\"}]}\n";

//     #[allow(clippy::too_many_arguments)]
//     async fn cargo_test(
//         package_name: String,
//         package_version: String,
//         registry_user_agent: Option<String>,
//         expected_result: bool,
//         expected_error: bool,
//         mock_user_agent: Option<String>,
//         mock_status: Option<u16>,
//         mock_body: Option<String>,
//     ) {
//         let crate_graph = CrateGraph::new(".", None).unwrap();
//         let mut cargo = Cargo::new(&crate_graph).expect("Could not create cargo instance");

//         let mut registry = "default".to_string();

//         if let (Some(user_agent), Some(mock_status), Some(mock_body)) =
//             (mock_user_agent, mock_status, mock_body)
//         {
//             let mock_server = MockServer::start().await;
//             let prefix = "krates/by-name/".to_string();
//             Mock::given(method("GET"))
//                 .and(path(format!("{}{}", prefix, package_name)))
//                 .and(header("User-Agent", user_agent.clone()))
//                 .respond_with(
//                     ResponseTemplate::new(mock_status).set_body_raw(mock_body, "application/json"),
//                 )
//                 .mount(&mock_server)
//                 .await;
//             let mock_server_uri = mock_server.uri();
//             registry = "private".to_string();
//             cargo
//                 .add_registry(
//                     registry.clone(),
//                     format!("{}/{}", mock_server_uri, prefix),
//                     registry_user_agent,
//                 )
//                 .expect("could not add private registry");
//         }

//         let result = cargo
//             .check_crate_exists(registry, package_name, package_version)
//             .await;
//         match result {
//             Ok(exists) => {
//                 assert!(!expected_error);
//                 assert_eq!(expected_result, exists);
//             }
//             Err(_) => {
//                 assert!(expected_error);
//             }
//         }
//     }

//     #[tokio::test]
//     async fn cargo_existing_crate_and_version() {
//         cargo_test(
//             "rand".to_string(),
//             "0.8.4".to_string(),
//             None,
//             true,
//             false,
//             None,
//             None,
//             None,
//         )
//         .await;
//     }

//     #[tokio::test]
//     async fn cargo_existing_crate_and_not_version() {
//         cargo_test(
//             "rand".to_string(),
//             "99.99.99".to_string(),
//             None,
//             false,
//             false,
//             None,
//             None,
//             None,
//         )
//         .await;
//     }

//     #[tokio::test]
//     async fn cargo_existing_crate_and_version_private_reg() {
//         cargo_test(
//             "hub_app".to_string(),
//             "0.2.0".to_string(),
//             Some("my_registry my_token".to_string()),
//             true,
//             false,
//             Some("my_registry my_token".to_string()),
//             Some(200),
//             Some(EXISTING_PACKAGE_DATA.to_string()),
//         )
//         .await;
//     }

//     #[tokio::test]
//     async fn cargo_existing_crate_and_not_version_private_reg() {
//         cargo_test(
//             "hub_app".to_string(),
//             "99.99.99".to_string(),
//             Some("my_registry my_token".to_string()),
//             false,
//             false,
//             Some("my_registry my_token".to_string()),
//             Some(200),
//             Some(EXISTING_PACKAGE_DATA.to_string()),
//         )
//         .await;
//     }
//     //
//     // #[tokio::test]
//     // async fn npm_package_existing_package_custom_registry_npmrc() {
//     //     npm_test("@TestScope/test".to_string(), "0.2.0".to_string(), None, true, true, false, Some(NPM_EXISTING_SCOPE_PACKAGE_DATA.to_string()), Some("my_token".to_string()), Some(200)).await;
//     // }
//     //
//     // #[tokio::test]
//     // async fn npm_package_non_existing_package_custom_registry_npmrc() {
//     //     npm_test("@TestScope/test".to_string(), "99.99.99".to_string(), None, true, false, false, Some(NPM_EXISTING_SCOPE_PACKAGE_DATA.to_string()), Some("my_token".to_string()), Some(200)).await;
//     // }
// }

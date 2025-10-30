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
    pub name: String,
    pub index: Option<String>,
    pub crate_url: Option<String>,
    pub token: Option<String>,
    pub user_agent: Option<String>,
}

impl CargoRegistry {
    /// Merge another CargoRegistry into this one.
    fn merge(&mut self, other: &CargoRegistry) {
        if self.index.is_none()
            && let Some(index) = &other.index
        {
            self.index = Some(index.clone());
        }
        if self.crate_url.is_none()
            && let Some(crate_url) = &other.crate_url
        {
            self.crate_url = Some(crate_url.clone());
        }
        if self.token.is_none()
            && let Some(token) = &other.token
        {
            self.token = Some(token.clone());
        }
        if self.user_agent.is_none()
            && let Some(user_agent) = &other.user_agent
        {
            self.user_agent = Some(user_agent.clone());
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
            name: name.clone(),
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
        let index = env::var(format!("CARGO_REGISTRIES_{env_name}_INDEX")).ok();
        let crate_url = env::var(format!("CARGO_REGISTRIES_{env_name}_CRATE_URL")).ok();
        let token = env::var(format!("CARGO_REGISTRIES_{env_name}_TOKEN")).ok();
        let user_agent = match name.as_str() {
            "crates.io" => None,
            _ => env::var(format!("CARGO_REGISTRIES_{env_name}_USER_AGENT")).ok(),
        };

        Self {
            name,
            index,
            crate_url,
            token,
            user_agent,
        }
    }

    pub fn new_from_config(name: String) -> Self {
        let mut config = Self {
            name: name.to_string(),
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
                if let Ok(config_str) = fs::read_to_string(config_file)
                    && let Ok(cargo_config) = toml::de::from_str::<Cargo>(&config_str)
                    && let Some(registry_config) = cargo_config.registries.get(&name)
                {
                    config.merge(registry_config);
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

pub trait CrateChecker {
    async fn check_crate_exists(
        &self,
        registry_name: String,
        name: String,
        version: String,
    ) -> anyhow::Result<bool>;
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

    pub fn get_registry(&self, name: &str) -> Option<&CargoRegistry> {
        self.registries.get(name)
    }
}

impl CrateChecker for Cargo {
    async fn check_crate_exists(
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
            let url: Uri = format!("{crate_url}{name}").parse()?;

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
                return Ok(false);
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
                        println!("Got error: {e}");
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
}

fn replace_registry_in_cargo_toml(
    path: &Path,
    original_registry: &CargoRegistry,
    target_registry: &CargoRegistry,
) -> anyhow::Result<()> {
    let content = fs::read_to_string(path)?;

    let pattern = format!(
        r#"registry += +"{}""#,
        regex::escape(&original_registry.name)
    );
    let re = Regex::new(&pattern)?;
    let modified_content = re
        .replace_all(&content, format!("registry = \"{}\"", target_registry.name))
        .to_string();

    fs::write(path, modified_content)?;

    Ok(())
}

fn replace_registry_in_cargo_lock(
    path: &Path,
    original_registry: &CargoRegistry,
    target_registry: &CargoRegistry,
) -> anyhow::Result<()> {
    let Some(original_index) = original_registry.index.clone() else {
        // Cannot do anything
        return Ok(());
    };
    let Some(target_index) = target_registry.index.clone() else {
        // Cannot do anything
        return Ok(());
    };

    let content = fs::read_to_string(path)?;

    let pattern = format!(r#"source = "registry\+{}""#, regex::escape(&original_index));
    let re = Regex::new(&pattern)?;
    let modified_content = re
        .replace_all(&content, format!("source = \"registry+{}\"", target_index))
        .to_string();

    fs::write(path, modified_content)?;

    Ok(())
}

pub fn patch_crate_for_registry(
    root_directory: &Path,
    working_directory: &Path,
    original_registry: &CargoRegistry,
    target_registry: &CargoRegistry,
) -> anyhow::Result<()> {
    let cargo_toml_path = working_directory.join("Cargo.toml");
    // Read the Cargo.toml file
    let toml_str = fs::read_to_string(&cargo_toml_path)?;
    let mut doc: DocumentMut = toml_str.parse()?;
    let mut publish_registries = toml_edit::Array::new();
    publish_registries.push(target_registry.name.clone());
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
            replace_registry_in_cargo_toml(entry.path(), original_registry, target_registry)?;
        }
        if entry.path().ends_with("Cargo.lock") {
            // Perform replacement for each Cargo.lock file found
            replace_registry_in_cargo_lock(entry.path(), original_registry, target_registry)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[test]
    fn test_publish_key_replaced_if_present() {
        let original_registry =
            CargoRegistry::new("main_registry".to_string(), None, None, None, None);
        let target_registry = CargoRegistry::new("my_registry".to_string(), None, None, None, None);
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
            patch_crate_for_registry(&tmp, &tmp, &original_registry, &target_registry)
                .map_err(|e| {
                    println!("Error: {e:#?}");
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
        let original_registry =
            CargoRegistry::new("main_registry".to_string(), None, None, None, None);
        let target_registry = CargoRegistry::new("my_registry".to_string(), None, None, None, None);
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
        assert!(patch_crate_for_registry(&tmp, &tmp, &original_registry, &target_registry).is_ok(),);

        // Read the updated Cargo.toml and check if `main_registry` was replaced
        let updated_toml = fs::read_to_string(cargo_toml_path).unwrap();
        assert!(updated_toml.contains("registry = \"my_registry\""));
    }

    #[test]
    fn test_publish_key_added_if_missing() {
        let original_registry =
            CargoRegistry::new("main_registry".to_string(), None, None, None, None);
        let target_registry = CargoRegistry::new("my_registry".to_string(), None, None, None, None);
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
        assert!(patch_crate_for_registry(&tmp, &tmp, &original_registry, &target_registry).is_ok());

        // Read the updated Cargo.toml and check if `publish` was correctly updated
        let updated_toml = fs::read_to_string(cargo_toml_path).unwrap();
        assert!(updated_toml.contains("publish = [\"my_registry\"]"));
    }

    #[test]
    fn test_patching_cargo_lock() {
        let original_registry = CargoRegistry::new(
            "main_registry".to_string(),
            Some("ssh://git@ssh.example.com/main_registry/crate-index.git".to_string()),
            None,
            None,
            None,
        );
        let target_registry = CargoRegistry::new(
            "my_registry".to_string(),
            Some("ssh://git@ssh.example.org/my_registry/crate-index.git".to_string()),
            None,
            None,
            None,
        );
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
version = "0.1.0"
publish = ["main_registry"]

[dependencies]
dep = { version = "24.0.0", registry = "main_registry" }"#;
        let lock_content = r#"# This file is automatically @generated by Cargo.
# It is not intended for manual editing.
version = 4

[[package]]
name = "dep"
version = "24.0.0"
source = "registry+ssh://git@ssh.example.com/main_registry/crate-index.git"
checksum = "70c43100e9892333afde82c4883a487d82e094e41860490b1ce7eac359183611"
dependencies = []

[[package]]
name = "my-package"
version = "0.1.0"
dependencies = [
 "dep",
]"#;

        fs::write(&cargo_toml_path, toml_content).unwrap();
        fs::write(&cargo_lock_path, lock_content).unwrap();

        // Run the patch_crate_for_registry function and check for success
        assert!(patch_crate_for_registry(&tmp, &tmp, &original_registry, &target_registry).is_ok());
        let wanted_replaced_toml_content = r#"[package]
name = "my-package"
version = "0.1.0"
publish = ["my_registry"]

[dependencies]
dep = { version = "24.0.0", registry = "my_registry" }
"#;
        let wanted_replaced_lock_content = r#"# This file is automatically @generated by Cargo.
# It is not intended for manual editing.
version = 4

[[package]]
name = "dep"
version = "24.0.0"
source = "registry+ssh://git@ssh.example.org/my_registry/crate-index.git"
checksum = "70c43100e9892333afde82c4883a487d82e094e41860490b1ce7eac359183611"
dependencies = []

[[package]]
name = "my-package"
version = "0.1.0"
dependencies = [
 "dep",
]"#;

        let replaced_toml_content = fs::read_to_string(cargo_toml_path).unwrap();
        let replaced_lock_content = fs::read_to_string(cargo_lock_path).unwrap();
        assert_eq!(replaced_toml_content, wanted_replaced_toml_content);
        assert_eq!(replaced_lock_content, wanted_replaced_lock_content);
    }
}

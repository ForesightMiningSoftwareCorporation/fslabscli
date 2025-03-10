use std::collections::{HashMap, HashSet};

use crate::crate_graph::CrateGraph;
use anyhow::Context;
use http_body_util::BodyExt;
use http_body_util::Empty;
use hyper::body::Bytes;
use hyper::{Method, Request, Uri};
use hyper_rustls::{ConfigBuilderExt, HttpsConnector};
use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::PathBuf;

const CARGO_DEFAULT_CRATE_URL: &str = "https://crates.io/api/v1/crates/";

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
        let env_name = name.to_uppercase().replace("-", "_");
        let index = match name.as_str() {
            "crates.io" => None,
            _ => env::var(format!("CARGO_REGISTRIES_{}_INDEX", env_name)).ok(),
        };
        let crate_url = match name.as_str() {
            "crates.io" => Some(CARGO_DEFAULT_CRATE_URL.to_string()),
            _ => env::var(format!("CARGO_REGISTRIES_{}_CRATE_URL", env_name)).ok(),
        };
        let token = match name.as_str() {
            "crates.io" => env::var("CARGO_REGISTRY_TOKEN").ok(),
            _ => env::var(format!("CARGO_REGISTRIES_{}_TOKEN", env_name)).ok(),
        };
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
    pub fn new(crates: &CrateGraph) -> anyhow::Result<Self> {
        let mut registries: HashSet<String> = HashSet::new();
        for workspace in crates.workspaces() {
            for package in workspace.metadata.workspace_packages() {
                match &package.publish {
                    Some(r) => {
                        for registry in r {
                            registries.insert(registry.clone());
                        }
                    }
                    None => {
                        // Custome case of publishing to crates.io
                        registries.insert("crates.io".to_string());
                    }
                };
            }
        }

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
                .into_iter()
                .map(|k| (k.clone(), CargoRegistry::new(k, None, None, None, None)))
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

            let req = Request::builder()
                .method(Method::GET)
                .uri(url.clone())
                .header("Content-Type", "application/json")
                .header("Accept", "application/json")
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

#[cfg(test)]
mod tests {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const EXISTING_PACKAGE_DATA: &str = "{\"org\":{\"id\":\"0184cce5-d7f7-d027-dc92-03ecd4bdfd44\",\"name\":\"Foresight Mining Software Corporation\",\"slug\":\"foresight-mining-software-corporation\"},\"n_crates\":1,\"n_crate_versions\":6,\"total_downloads\":25,\"crates\":[{\"id\":\"018c8382-17f4-11e6-dba9-fadb50dd1f74\",\"name\":\"hub_app\",\"total_downloads\":25,\"versions\":[{\"id\":\"018d8de9-8e73-c788-03f7-02926da47171\",\"vers\":\"0.2.0\",\"user_id\":\"0184cce5-d802-0d87-da96-33779594d8cc\",\"published\":\"2024-02-09T12:48:30.322924Z\",\"published_unix\":1707482910,\"meta\":{\"description\":null,\"categories\":[],\"keywords\":[],\"repository\":null,\"deps\":[],\"readme\":\"# `hub_app`\\n\\nShared library for applications that are launched by the Hub Launcher. Provides input data for these\\napplications so they know where to store/access data, as well as what project file should be opened\\nwhen the application starts.\"},\"raw_publish_meta\":{\"deps\":[],\"name\":\"hub_app\",\"vers\":\"0.4.1\",\"links\":null,\"badges\":{},\"readme\":\"# `hub_app`\\n\\nShared library for applications that are launched by the Hub Launcher. Provides input data for these\\napplications so they know where to store/access data, as well as what project file should be opened\\nwhen the application starts.\",\"authors\":[],\"license\":null,\"features\":{\"beta\":[],\"prod\":[],\"alpha\":[\"beta\"],\"default\":[\"embedded_assets\"],\"nightly\":[\"alpha\",\"beta\"],\"devtools\":[],\"run_init_logic\":[],\"embedded_assets\":[\"bevy_embedded_assets\"]},\"homepage\":null,\"keywords\":[],\"categories\":[],\"repository\":null,\"description\":null,\"readme_file\":\"README.md\",\"license_file\":null,\"rust_version\":null,\"documentation\":null},\"docs_url\":\"/foresight-mining-software-corporation/hub_app/0.4.1/docs\",\"n_downloads\":1,\"yanked\":null,\"is_yanked\":false}],\"latest_version\":\"0.2.0\"}]}\n";

    #[allow(clippy::too_many_arguments)]
    async fn cargo_test(
        package_name: String,
        package_version: String,
        registry_user_agent: Option<String>,
        expected_result: bool,
        expected_error: bool,
        mock_user_agent: Option<String>,
        mock_status: Option<u16>,
        mock_body: Option<String>,
    ) {
        let mut cargo = Cargo::new(None).expect("Could not create cargo instance");

        let mut registry = "default".to_string();

        if let (Some(user_agent), Some(mock_status), Some(mock_body)) =
            (mock_user_agent, mock_status, mock_body)
        {
            let mock_server = MockServer::start().await;
            let prefix = "krates/by-name/".to_string();
            Mock::given(method("GET"))
                .and(path(format!("{}{}", prefix, package_name)))
                .and(header("User-Agent", user_agent.clone()))
                .respond_with(
                    ResponseTemplate::new(mock_status).set_body_raw(mock_body, "application/json"),
                )
                .mount(&mock_server)
                .await;
            let mock_server_uri = mock_server.uri();
            registry = "private".to_string();
            cargo
                .add_registry(
                    registry.clone(),
                    format!("{}/{}", mock_server_uri, prefix),
                    registry_user_agent,
                )
                .expect("could not add private registry");
        }

        let result = cargo
            .check_crate_exists(registry, package_name, package_version)
            .await;
        match result {
            Ok(exists) => {
                assert!(!expected_error);
                assert_eq!(expected_result, exists);
            }
            Err(_) => {
                assert!(expected_error);
            }
        }
    }

    #[tokio::test]
    async fn cargo_existing_crate_and_version() {
        cargo_test(
            "rand".to_string(),
            "0.8.4".to_string(),
            None,
            true,
            false,
            None,
            None,
            None,
        )
        .await;
    }

    #[tokio::test]
    async fn cargo_existing_crate_and_not_version() {
        cargo_test(
            "rand".to_string(),
            "99.99.99".to_string(),
            None,
            false,
            false,
            None,
            None,
            None,
        )
        .await;
    }

    #[tokio::test]
    async fn cargo_existing_crate_and_version_private_reg() {
        cargo_test(
            "hub_app".to_string(),
            "0.2.0".to_string(),
            Some("my_registry my_token".to_string()),
            true,
            false,
            Some("my_registry my_token".to_string()),
            Some(200),
            Some(EXISTING_PACKAGE_DATA.to_string()),
        )
        .await;
    }

    #[tokio::test]
    async fn cargo_existing_crate_and_not_version_private_reg() {
        cargo_test(
            "hub_app".to_string(),
            "99.99.99".to_string(),
            Some("my_registry my_token".to_string()),
            false,
            false,
            Some("my_registry my_token".to_string()),
            Some(200),
            Some(EXISTING_PACKAGE_DATA.to_string()),
        )
        .await;
    }
    //
    // #[tokio::test]
    // async fn npm_package_existing_package_custom_registry_npmrc() {
    //     npm_test("@TestScope/test".to_string(), "0.2.0".to_string(), None, true, true, false, Some(NPM_EXISTING_SCOPE_PACKAGE_DATA.to_string()), Some("my_token".to_string()), Some(200)).await;
    // }
    //
    // #[tokio::test]
    // async fn npm_package_non_existing_package_custom_registry_npmrc() {
    //     npm_test("@TestScope/test".to_string(), "99.99.99".to_string(), None, true, false, false, Some(NPM_EXISTING_SCOPE_PACKAGE_DATA.to_string()), Some("my_token".to_string()), Some(200)).await;
    // }
}

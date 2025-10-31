use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};

use anyhow::Context;
use git2::build::RepoBuilder;
use git2::{Cred, FetchOptions, RemoteCallbacks};
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
use temp_dir::TempDir;
use toml_edit::{DocumentMut, Table, table, value};
use walkdir::WalkDir;

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct CargoRegistry {
    pub name: String,
    pub index: Option<String>,
    pub private_key: Option<PathBuf>,
    pub crate_url: Option<String>,
    pub token: Option<String>,
    pub user_agent: Option<String>,
    pub local_index_path: Option<PathBuf>,
}

impl CargoRegistry {
    /// Merge another CargoRegistry into this one.
    fn merge(&mut self, other: &CargoRegistry) {
        if self.index.is_none()
            && let Some(index) = &other.index
        {
            self.index = Some(index.clone());
        }
        if self.private_key.is_none()
            && let Some(private_key) = &other.private_key
        {
            self.private_key = Some(private_key.clone());
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
        private_key: Option<PathBuf>,
        crate_url: Option<String>,
        token: Option<String>,
        user_agent: Option<String>,
        fetch_index: bool,
    ) -> anyhow::Result<Self> {
        let mut config = Self {
            name: name.clone(),
            index,
            private_key,
            crate_url,
            token,
            user_agent,
            local_index_path: None,
        };
        config.merge(&CargoRegistry::new_from_env(name.clone()));
        config.merge(&CargoRegistry::new_from_config(name.clone()));
        if fetch_index {
            config.fetch_index()?;
        }
        Ok(config)
    }

    pub fn new_from_env(name: String) -> Self {
        let env_name = name.to_uppercase().replace("-", "_").replace(".", "_");
        let index = env::var(format!("CARGO_REGISTRIES_{env_name}_INDEX")).ok();
        let private_key = env::var(format!("CARGO_REGISTRIES_{env_name}_PRIVATE_KEY"))
            .ok()
            .map(PathBuf::from);
        let crate_url = env::var(format!("CARGO_REGISTRIES_{env_name}_CRATE_URL")).ok();
        let token = env::var(format!("CARGO_REGISTRIES_{env_name}_TOKEN")).ok();
        let user_agent = match name.as_str() {
            "crates.io" => None,
            _ => env::var(format!("CARGO_REGISTRIES_{env_name}_USER_AGENT")).ok(),
        };

        Self {
            name,
            index,
            private_key,
            crate_url,
            token,
            user_agent,
            local_index_path: None,
        }
    }

    pub fn new_from_config(name: String) -> Self {
        let mut config = Self {
            name: name.to_string(),
            index: None,
            private_key: None,
            crate_url: None,
            token: None,
            user_agent: None,
            local_index_path: None,
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

    /// fetch_index will fetch the remote index of the registry and store it in a temp directory
    pub fn fetch_index(&mut self) -> anyhow::Result<()> {
        let index = self
            .index
            .clone()
            .context("Cannot fetch inexistent index")?;
        let tmp = TempDir::new()?.dont_delete_on_drop();
        let path = tmp.path();

        let mut builder = RepoBuilder::new();
        let mut callbacks = RemoteCallbacks::new();
        let mut fetch_options = FetchOptions::new();

        let private_key = self.private_key.clone();

        callbacks.credentials(move |_, u, _| match private_key.as_ref() {
            Some(key) => Cred::ssh_key(u.unwrap_or("git"), None, key.as_path(), None),
            None => Cred::ssh_key_from_agent(u.unwrap_or("git")),
        });
        fetch_options.remote_callbacks(callbacks);
        builder.fetch_options(fetch_options);
        builder.clone(&index, path).map_err(|e| {
            println!("Couldn't not fetch reg: {}", e);
            e
        })?;
        self.local_index_path = Some(path.to_path_buf());
        Ok(())
    }

    fn get_crate_checksum(&self, package_name: &str, version: &str) -> anyhow::Result<String> {
        let Some(local_index_path) = self.local_index_path.clone() else {
            anyhow::bail!("Cannot get checksum of unfetched registry");
        };

        let package_dir = get_package_file_dir(package_name)?;
        let package_file_path = local_index_path.join(package_dir).join(package_name);

        let package_file = File::open(&package_file_path)?;
        let reader = BufReader::new(package_file);

        for (line_num, line) in reader.lines().enumerate() {
            let line = line.with_context(|| {
                format!(
                    "Failed to read line {} from {:?}",
                    line_num + 1,
                    package_file_path
                )
            })?;

            let pkg_version: IndexPackageVersion =
                serde_json::from_str(&line).with_context(|| {
                    format!(
                        "Failed to parse JSON at line {} in {:?}",
                        line_num + 1,
                        package_file_path
                    )
                })?;

            if pkg_version.version == version {
                if pkg_version.yanked {
                    anyhow::bail!(
                        "Version {} yanked for crate {} in registry {}",
                        version,
                        package_name,
                        self.name
                    );
                }
                return pkg_version.checksum.ok_or_else(|| {
                    anyhow::anyhow!("No checksum for {}@{}", package_name, version)
                });
            }
        }

        anyhow::bail!(
            "Version {} not found for crate {} in registry {}",
            version,
            package_name,
            self.name
        )
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
    pub fn new(registries: &HashSet<String>, fetch_indexes: bool) -> anyhow::Result<Self> {
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
                .filter_map(|k| {
                    CargoRegistry::new(k.clone(), None, None, None, None, None, fetch_indexes)
                        .ok()
                        .map(|r| (k.clone(), r))
                })
                .collect(),
        })
    }

    pub fn get_registry(&self, name: &str) -> Option<&CargoRegistry> {
        self.registries.get(name)
    }

    #[cfg(test)]
    pub fn add_registry(&mut self, registry: CargoRegistry) {
        self.registries.insert(registry.name.clone(), registry);
    }
}

pub fn get_package_file_dir(package_name: &str) -> anyhow::Result<String> {
    if package_name.is_empty() {
        return Err(anyhow::anyhow!("Empty package name"));
    }

    let len = package_name.len();
    match len {
        1 | 2 => Ok(len.to_string()),
        3 => Ok(format!("3/{}", &package_name[0..1])),
        _ => Ok(format!("{}/{}", &package_name[0..2], &package_name[2..4])),
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct IndexPackageVersion {
    pub name: String,
    #[serde(rename = "vers")]
    pub version: String,
    pub yanked: bool,
    #[serde(rename = "cksum")]
    pub checksum: Option<String>,
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

fn parse_quoted_value(line: &str) -> Option<String> {
    line.split_once(" = ").and_then(|(_, value)| {
        let value = value.trim();
        if value.starts_with('"') && value.ends_with('"') {
            Some(value[1..value.len() - 1].to_string())
        } else {
            None
        }
    })
}

fn replace_registry_in_cargo_lock(
    path: &Path,
    original_registry: &CargoRegistry,
    target_registry: &CargoRegistry,
) -> anyhow::Result<()> {
    let original_index = original_registry
        .index
        .as_ref()
        .context(format!("Registry {} has no index", original_registry.name))?;
    let target_index = target_registry
        .index
        .as_ref()
        .context(format!("Registry {} has no index", target_registry.name))?;

    if original_registry.local_index_path.is_none() {
        anyhow::bail!("Registry {} index not fetched", original_registry.name);
    }
    if original_registry.local_index_path.is_none() {
        anyhow::bail!("Registry {} index not fetched", target_registry.name);
    }

    // Loop over each line, because serializing /deserializing would mess comment and stuff
    let file = File::open(path)?;
    let reader = BufReader::new(file);

    let mut output = String::new();
    let mut in_target_package = false;
    let mut current_name = String::new();
    let mut current_version = String::new();

    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim_start();

        // Detect start of new package
        if trimmed.starts_with("[[package]]") {
            in_target_package = false;
            current_name.clear();
            current_version.clear();
            output.push_str(&line);
            output.push('\n');
            continue;
        }

        // Parse name
        if trimmed.starts_with("name = ") {
            if let Some(name) = parse_quoted_value(trimmed) {
                current_name = name;
            }
            output.push_str(&line);
            output.push('\n');
            continue;
        }

        // Parse version
        if trimmed.starts_with("version = ") {
            if let Some(version) = parse_quoted_value(trimmed) {
                current_version = version;
            }
            output.push_str(&line);
            output.push('\n');
            continue;
        }

        // Parse and potentially update source
        if trimmed.starts_with("source = ") {
            if let Some(source) = parse_quoted_value(trimmed)
                && source.starts_with(&format!("registry+{}", original_index))
            {
                in_target_package = true;

                let indent = &line[..line.len() - trimmed.len()];
                output.push_str(&format!(
                    "{}source = \"registry+{}\"\n",
                    indent, target_index
                ));
                continue;
            }

            output.push_str(&line);
            output.push('\n');
            continue;
        }

        // Update checksum if we're in a target package
        if in_target_package && trimmed.starts_with("checksum = ") {
            let Ok(updated_checksum) =
                target_registry.get_crate_checksum(&current_name, &current_version)
            else {
                continue;
            };

            let indent = &line[..line.len() - trimmed.len()];
            output.push_str(&format!("{}checksum = \"{}\"\n", indent, updated_checksum));
            continue;
        }

        output.push_str(&line);
        output.push('\n');
    }

    fs::write(path, output)?;
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
    use crate::utils::test::create_rust_index;

    use super::*;
    use std::fs;
    #[test]
    fn test_publish_key_replaced_if_present() {
        let original_registry = CargoRegistry::new(
            "main_registry".to_string(),
            None,
            None,
            None,
            None,
            None,
            false,
        )
        .unwrap();
        let target_registry = CargoRegistry::new(
            "my_registry".to_string(),
            None,
            None,
            None,
            None,
            None,
            false,
        )
        .unwrap();
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
        let original_registry = CargoRegistry::new(
            "main_registry".to_string(),
            None,
            None,
            None,
            None,
            None,
            false,
        )
        .unwrap();
        let target_registry = CargoRegistry::new(
            "my_registry".to_string(),
            None,
            None,
            None,
            None,
            None,
            false,
        )
        .unwrap();

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
        let original_registry = CargoRegistry::new(
            "main_registry".to_string(),
            None,
            None,
            None,
            None,
            None,
            false,
        )
        .unwrap();
        let target_registry = CargoRegistry::new(
            "my_registry".to_string(),
            None,
            None,
            None,
            None,
            None,
            false,
        )
        .unwrap();
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
        let original_checksum = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let target_checksum = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

        let original_index = create_rust_index(original_checksum);
        let target_index = create_rust_index(target_checksum);

        let original_registry = CargoRegistry::new(
            "main_registry".to_string(),
            Some(original_index.to_string_lossy().to_string()),
            None,
            None,
            None,
            None,
            true,
        )
        .unwrap();
        let target_registry = CargoRegistry::new(
            "my_registry".to_string(),
            Some(target_index.to_string_lossy().to_string()),
            None,
            None,
            None,
            None,
            true,
        )
        .unwrap();

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
crate-test = { version = "0.2.2", registry = "main_registry" }"#;
        let lock_content = format!(
            r#"# This file is automatically @generated by Cargo.
# It is not intended for manual editing.
version = 4

[[package]]
name = "crate-test"
version = "0.2.2"
source = "registry+{}"
checksum = "{}"
dependencies = []

[[package]]
name = "my-package"
version = "0.1.0"
dependencies = [
 "dep",
]"#,
            original_index.display(),
            original_checksum
        );

        fs::write(&cargo_toml_path, toml_content).unwrap();
        fs::write(&cargo_lock_path, &lock_content).unwrap();

        // Run the patch_crate_for_registry function and check for success
        assert!(patch_crate_for_registry(&tmp, &tmp, &original_registry, &target_registry).is_ok());
        let wanted_replaced_toml_content = r#"[package]
name = "my-package"
version = "0.1.0"
publish = ["my_registry"]

[dependencies]
crate-test = { version = "0.2.2", registry = "my_registry" }
"#;
        let wanted_replaced_lock_content = format!(
            r#"# This file is automatically @generated by Cargo.
# It is not intended for manual editing.
version = 4

[[package]]
name = "crate-test"
version = "0.2.2"
source = "registry+{}"
checksum = "{}"
dependencies = []

[[package]]
name = "my-package"
version = "0.1.0"
dependencies = [
 "dep",
]
"#,
            target_index.display(),
            target_checksum
        );

        let replaced_toml_content = fs::read_to_string(cargo_toml_path).unwrap();
        let replaced_lock_content = fs::read_to_string(cargo_lock_path).unwrap();
        assert_eq!(replaced_toml_content, wanted_replaced_toml_content);
        assert_eq!(replaced_lock_content, wanted_replaced_lock_content);
    }

    #[tokio::test]
    async fn test_should_publish_if_inexisting_package() {
        let mut cargo = Cargo::new(&HashSet::new(), false).unwrap();
        let crates_io = CargoRegistry::new(
            "crates.io".to_string(),
            None,
            None,
            Some("https://crates.io/api/v1/crates/".to_string()),
            Some("some".to_string()),
            Some("fslabscli".to_string()),
            false,
        )
        .unwrap();
        cargo.add_registry(crates_io);

        let exists = cargo
            .check_crate_exists(
                "crates.io".to_string(),
                "bachibouzouk".to_string(),
                "1.0.0".to_string(),
            )
            .await
            .unwrap();

        assert!(!exists);
    }

    #[tokio::test]
    async fn test_should_not_publish_if_existing_package_version() {
        let mut cargo = Cargo::new(&HashSet::new(), false).unwrap();
        let crates_io = CargoRegistry::new(
            "crates.io".to_string(),
            None,
            None,
            Some("https://crates.io/api/v1/crates/".to_string()),
            Some("some".to_string()),
            Some("fslabscli".to_string()),
            false,
        )
        .unwrap();
        cargo.add_registry(crates_io);

        let exists = cargo
            .check_crate_exists(
                "crates.io".to_string(),
                "rand".to_string(),
                "0.8.0".to_string(),
            )
            .await
            .unwrap();

        assert!(exists);
    }

    #[tokio::test]
    async fn test_should_publish_if_existing_package_new_version() {
        let mut cargo = Cargo::new(&HashSet::new(), false).unwrap();
        let crates_io = CargoRegistry::new(
            "crates.io".to_string(),
            None,
            None,
            Some("https://crates.io/api/v1/crates/".to_string()),
            Some("some".to_string()),
            Some("fslabscli".to_string()),
            false,
        )
        .unwrap();
        cargo.add_registry(crates_io);

        let exists = cargo
            .check_crate_exists(
                "crates.io".to_string(),
                "rand".to_string(),
                "100.8.0".to_string(),
            )
            .await
            .unwrap();

        assert!(!exists);
    }

    #[tokio::test]
    async fn test_checksum_unfetch_reg() {
        let reg_index_path =
            create_rust_index("b274d286f7a6aad5a7d5b5407e9db0098c94911fb3563bf2e32854a611edfb63");
        let reg = CargoRegistry::new(
            "my_registry".to_string(),
            Some(reg_index_path.to_string_lossy().to_string()),
            None,
            None,
            None,
            None,
            false,
        )
        .unwrap();

        let checksum = reg.get_crate_checksum("crate-test", "0.2.2");
        let error = checksum.unwrap_err();
        assert_eq!(
            format!("{}", error),
            "Cannot get checksum of unfetched registry"
        );
    }

    #[tokio::test]
    async fn test_checksum_success_local() {
        let reg_index_path =
            create_rust_index("b274d286f7a6aad5a7d5b5407e9db0098c94911fb3563bf2e32854a611edfb63");
        let reg = CargoRegistry::new(
            "my_registry".to_string(),
            Some(reg_index_path.to_string_lossy().to_string()),
            None,
            None,
            None,
            None,
            true,
        )
        .unwrap();

        let checksum = reg.get_crate_checksum("crate-test", "0.2.2").unwrap();
        assert_eq!(
            checksum,
            "b274d286f7a6aad5a7d5b5407e9db0098c94911fb3563bf2e32854a611edfb63".to_string()
        )
    }

    #[tokio::test]
    async fn test_checksum_success_http_git() {
        let reg = CargoRegistry::new(
            "my_registry".to_string(),
            Some(
                "https://github.com/ForesightMiningSoftwareCorporation/fake-cargo-registry.git"
                    .to_string(),
            ),
            None,
            None,
            None,
            None,
            true,
        )
        .unwrap();

        let checksum = reg.get_crate_checksum("test-crate-3", "0.1.1").unwrap();
        assert_eq!(
            checksum,
            "b2e46d3c153c6cf8fa31efcfa96d6256e650321d087da1537faf21528b894f67".to_string()
        )
    }

    #[tokio::test]
    async fn test_checksum_unknown_package() {
        let reg_index_path =
            create_rust_index("b274d286f7a6aad5a7d5b5407e9db0098c94911fb3563bf2e32854a611edfb63");
        let reg = CargoRegistry::new(
            "my_registry".to_string(),
            Some(reg_index_path.to_string_lossy().to_string()),
            None,
            None,
            None,
            None,
            true,
        )
        .unwrap();

        let checksum = reg.get_crate_checksum("crate-test-bis", "0.2.2");

        let error = checksum.unwrap_err();
        assert_eq!(
            format!("{}", error),
            "No such file or directory (os error 2)"
        );
    }

    #[tokio::test]
    async fn test_checksum_unknown_version() {
        let reg_index_path =
            create_rust_index("b274d286f7a6aad5a7d5b5407e9db0098c94911fb3563bf2e32854a611edfb63");
        let reg = CargoRegistry::new(
            "my_registry".to_string(),
            Some(reg_index_path.to_string_lossy().to_string()),
            None,
            None,
            None,
            None,
            true,
        )
        .unwrap();

        let checksum = reg.get_crate_checksum("crate-test", "0.8.0");

        let error = checksum.unwrap_err();
        assert_eq!(
            format!("{}", error),
            "Version 0.8.0 not found for crate crate-test in registry my_registry"
        );
    }

    #[tokio::test]
    async fn test_checksum_yanked_version() {
        let reg_index_path =
            create_rust_index("b274d286f7a6aad5a7d5b5407e9db0098c94911fb3563bf2e32854a611edfb63");
        let reg = CargoRegistry::new(
            "my_registry".to_string(),
            Some(reg_index_path.to_string_lossy().to_string()),
            None,
            None,
            None,
            None,
            true,
        )
        .unwrap();

        let checksum = reg.get_crate_checksum("crate-test", "0.2.3");

        let error = checksum.unwrap_err();
        assert_eq!(
            format!("{}", error),
            "Version 0.2.3 yanked for crate crate-test in registry my_registry"
        );
    }

    #[test]
    fn test_get_package_file_dir_empty_package_name() {
        let r = get_package_file_dir("");
        let error = r.unwrap_err();
        assert_eq!(format!("{}", error), "Empty package name");
    }

    #[test]
    fn test_get_package_file_dir_single_char() {
        assert_eq!(get_package_file_dir("a").unwrap(), "1");
    }

    #[test]
    fn test_get_package_file_dir_two_chars() {
        assert_eq!(get_package_file_dir("ab").unwrap(), "2");
    }

    #[test]
    fn test_get_package_file_dir_three_chars() {
        assert_eq!(get_package_file_dir("abc").unwrap(), "3/a");
    }

    #[test]
    fn test_get_package_file_dir_four_chars() {
        assert_eq!(get_package_file_dir("abcd").unwrap(), "ab/cd");
    }

    #[test]
    fn test_get_package_file_dir_long_package_name() {
        assert_eq!(get_package_file_dir("tensorflow").unwrap(), "te/ns");
    }
}

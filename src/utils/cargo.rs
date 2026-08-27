use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};

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
use temp_dir::TempDir;
use tokio::runtime::Handle;
use toml_edit::{DocumentMut, Table, table, value};
use walkdir::WalkDir;

/// Cargo's own name for the public registry. It must be spelled exactly this
/// way: it is what `cargo publish --registry` accepts and what `package.publish`
/// is matched against. "crates.io" is not a legal registry name to cargo
/// ("invalid character `.` in registry name"), so it can never be used here.
pub const CRATES_IO: &str = "crates-io";

/// crates.io's sparse index. Cargo knows this natively and no `[registries]`
/// entry declares it, so nothing in the env or config chain can supply it.
/// Defaulted rather than required so consumers do not have to configure the
/// public registry to check whether a version is already on it.
const CRATES_IO_INDEX: &str = "sparse+https://index.crates.io/";

/// Index to fall back on when nothing in the env or config chain supplies one.
/// Kept separate from `CargoRegistry::new` so it is testable without depending
/// on the ambient `CARGO_REGISTRIES_*` vars, which CI does set.
fn default_index(registry_name: &str) -> Option<&'static str> {
    (registry_name == CRATES_IO).then_some(CRATES_IO_INDEX)
}

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
    pub fn is_sparse(&self) -> bool {
        self.index
            .as_ref()
            .is_some_and(|idx| idx.starts_with("sparse+"))
    }

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
        // Last, so an explicit argument, env var or config entry still wins.
        if config.index.is_none() {
            config.index = default_index(&config.name).map(str::to_string);
        }
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
            CRATES_IO => None,
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

        if self.is_sparse() {
            return Ok(());
        }

        let tmp = TempDir::new()?.dont_delete_on_drop();
        let path = tmp.path();

        let mut cmd = std::process::Command::new("git");
        cmd.arg("clone").arg("--depth=1");

        if let Some(key) = &self.private_key {
            if !key.is_file() {
                anyhow::bail!("SSH key path does not exist or is not a file: {:?}", key);
            }
            let ssh_command = format!(
                "ssh -i '{}' -o IdentitiesOnly=yes -o StrictHostKeyChecking=no",
                key.display().to_string().replace("'", "'\\''")
            );
            cmd.env("GIT_SSH_COMMAND", ssh_command);
        }

        cmd.arg(&index).arg(path);

        let output = cmd.output().map_err(|e| {
            println!("Couldn't not fetch reg: {}", e);
            e
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("git clone failed: {}", stderr);
        }
        self.local_index_path = Some(path.to_path_buf());
        Ok(())
    }

    fn get_crate_checksum(
        &self,
        package_name: &str,
        version: &str,
        http_client: Option<&HyperClient<HttpsConnector<HttpConnector>, Empty<Bytes>>>,
    ) -> anyhow::Result<String> {
        if self.is_sparse() {
            let index = self
                .index
                .as_ref()
                .context("Cannot get checksum without index")?;
            let base_url = index
                .strip_prefix("sparse+")
                .context("Invalid sparse index URL")?;

            // Catches URL construction footgun that leads to strange errors.
            let base_url = if base_url.ends_with('/') {
                base_url
            } else {
                &format!("{}/", base_url)
            };

            let package_dir = get_package_file_dir(package_name)?;
            let url: Uri = format!("{}{}/{}", base_url, package_dir, package_name).parse()?;

            let client = http_client.context("HTTP client required for sparse registry")?;

            let mut req_builder = Request::builder().method(Method::GET).uri(url.clone());

            if let Some(token) = &self.token {
                req_builder = req_builder.header("Authorization", token);
            }

            if let Some(user_agent) = &self.user_agent {
                req_builder = req_builder.header("User-Agent", user_agent);
            }

            let req = req_builder.body(Empty::default())?;

            let body_str = tokio::task::block_in_place(|| {
                Handle::current().block_on(async {
                    let res = client.request(req).await.with_context(|| {
                        format!("Could not fetch from sparse registry: {}", url)
                    })?;

                    if res.status().as_u16() >= 400 {
                        anyhow::bail!(
                            "Failed to fetch crate index for {}: HTTP {}",
                            package_name,
                            res.status()
                        );
                    }

                    let body = res
                        .into_body()
                        .collect()
                        .await
                        .context("Could not get body from sparse registry")?
                        .to_bytes();

                    Ok::<String, anyhow::Error>(String::from_utf8_lossy(&body).to_string())
                })
            })?;

            for line in body_str.lines() {
                let pkg_version: IndexPackageVersion = serde_json::from_str(line)
                    .with_context(|| format!("Failed to parse JSON line for {}", package_name))?;

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
        } else {
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
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Cargo {
    #[serde(rename = "registry", default)]
    registries: HashMap<String, CargoRegistry>,
    #[serde(skip)]
    client: Option<HyperClient<HttpsConnector<HttpConnector>, Empty<Bytes>>>,
}

impl Default for Cargo {
    fn default() -> Self {
        let client = (|| {
            // Ensure rustls has a crypto provider installed (use ring)
            let _ = rustls::crypto::ring::default_provider().install_default();

            let tls_config = rustls::ClientConfig::builder()
                .with_native_roots()
                .ok()? // returns None if building fails
                .with_no_client_auth();

            let https = hyper_rustls::HttpsConnectorBuilder::new()
                .with_tls_config(tls_config)
                .https_or_http()
                .enable_http1()
                .build();

            Some(HyperClient::builder(TokioExecutor::new()).build(https))
        })();
        Self {
            registries: Default::default(),
            client,
        }
    }
}
pub trait CrateChecker {
    async fn check_crate_exists(
        &self,
        registry_name: String,
        name: String,
        version: String,
    ) -> anyhow::Result<bool>;

    async fn check_crate_name_exists(
        &self,
        registry_name: String,
        name: String,
    ) -> anyhow::Result<bool>;

    fn add_registry(&mut self, registry_name: String, fetch_indexes: bool) -> anyhow::Result<()>;
}

impl Cargo {
    pub fn new(registries: &HashSet<String>, fetch_indexes: bool) -> anyhow::Result<Self> {
        Ok(Self {
            registries: registries
                .iter()
                .filter_map(|k| {
                    CargoRegistry::new(k.clone(), None, None, None, None, None, fetch_indexes)
                        .ok()
                        .map(|r| (k.clone(), r))
                })
                .collect(),
            ..Default::default()
        })
    }

    pub fn get_registry(&self, name: &str) -> Option<&CargoRegistry> {
        self.registries.get(name)
    }

    pub fn add_registry(&mut self, registry: CargoRegistry) {
        self.registries.insert(registry.name.clone(), registry);
    }

    pub fn http_client(&self) -> Option<&HyperClient<HttpsConnector<HttpConnector>, Empty<Bytes>>> {
        self.client.as_ref()
    }

    async fn sparse_index_body(
        &self,
        registry_name: &str,
        name: &str,
    ) -> anyhow::Result<Option<String>> {
        let registry = self
            .registries
            .get(registry_name)
            .ok_or_else(|| anyhow::anyhow!("unknown registry {registry_name}"))?;
        let index = registry
            .index
            .as_ref()
            .context("Cannot check crate existence without index")?;
        let base_url = index
            .strip_prefix("sparse+")
            .context("Crate existence checks require a sparse registry index")?;
        let base_url = if base_url.ends_with('/') {
            base_url.to_string()
        } else {
            format!("{base_url}/")
        };
        let package_dir = get_package_file_dir(name)?;
        let url: Uri = format!("{base_url}{package_dir}/{name}").parse()?;
        let client = self
            .client
            .as_ref()
            .context("HTTP client required for sparse registry")?;

        let mut last_error = None;
        for attempt in 1..=3u32 {
            let mut req_builder = Request::builder().method(Method::GET).uri(url.clone());
            if let Some(token) = &registry.token {
                req_builder = req_builder.header("Authorization", token);
            }
            if let Some(user_agent) = &registry.user_agent {
                req_builder = req_builder.header("User-Agent", user_agent);
            }
            let req = req_builder.body(Empty::default())?;

            match client.request(req).await {
                Ok(res) => {
                    if res.status().as_u16() == 404 {
                        return Ok(None);
                    }
                    if res.status().as_u16() == 429 || res.status().is_server_error() {
                        last_error = Some(format!("HTTP {}", res.status()));
                        if attempt < 3 {
                            tracing::warn!(
                                crate_name = %name,
                                attempt,
                                status = %res.status(),
                                "sparse registry request failed, retrying"
                            );
                            tokio::time::sleep(std::time::Duration::from_secs(attempt as u64))
                                .await;
                            continue;
                        }
                        break;
                    }
                    if res.status().as_u16() >= 400 {
                        anyhow::bail!(
                            "Failed to fetch crate index for {name}: HTTP {}",
                            res.status()
                        );
                    }
                    let body = res
                        .into_body()
                        .collect()
                        .await
                        .context("Could not get body from sparse registry")?
                        .to_bytes();
                    return Ok(Some(String::from_utf8_lossy(&body).to_string()));
                }
                Err(error) => {
                    last_error = Some(error.to_string());
                    if attempt < 3 {
                        tracing::warn!(
                            crate_name = %name,
                            attempt,
                            error = %last_error.as_deref().unwrap_or_default(),
                            "sparse registry request failed, retrying"
                        );
                        tokio::time::sleep(std::time::Duration::from_secs(attempt as u64)).await;
                    }
                }
            }
        }

        Err(anyhow::anyhow!(
            "Could not fetch from sparse registry after 3 attempts: {url}"
        ))
        .with_context(|| format!("last error: {}", last_error.unwrap_or_default()))
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

fn index_has_package_name(body: &str, expected_name: &str) -> anyhow::Result<bool> {
    let mut found = false;
    for line in body.lines() {
        let package: IndexPackageVersion = serde_json::from_str(line)
            .with_context(|| format!("Failed to parse JSON line for {expected_name}"))?;
        if package.name != expected_name {
            anyhow::bail!(
                "Registry index for {expected_name} returned package {}",
                package.name
            );
        }
        found = true;
    }
    Ok(found)
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

        if registry.is_sparse() {
            let Some(body) = self.sparse_index_body(&registry_name, &name).await? else {
                return Ok(false);
            };
            for line in body.lines() {
                let package: IndexPackageVersion = serde_json::from_str(line)
                    .with_context(|| format!("Failed to parse JSON line for {name}"))?;
                if package.version == version && !package.yanked {
                    return Ok(true);
                }
            }
            return Ok(false);
        }

        Ok(false)
    }

    async fn check_crate_name_exists(
        &self,
        registry_name: String,
        name: String,
    ) -> anyhow::Result<bool> {
        let registry = self
            .registries
            .get(&registry_name)
            .ok_or_else(|| anyhow::anyhow!("unknown registry {registry_name}"))?;
        if registry.is_sparse() {
            let Some(body) = self.sparse_index_body(&registry_name, &name).await? else {
                return Ok(false);
            };
            return index_has_package_name(&body, &name);
        }

        let local_index_path = registry
            .local_index_path
            .as_ref()
            .context("Cannot check crate existence in an unfetched registry")?;
        let package_path = local_index_path
            .join(get_package_file_dir(&name)?)
            .join(&name);
        let file = match File::open(package_path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        let body = BufReader::new(file)
            .lines()
            .collect::<Result<Vec<_>, _>>()?
            .join("\n");
        index_has_package_name(&body, &name)
    }

    fn add_registry(&mut self, registry_name: String, fetch_indexes: bool) -> anyhow::Result<()> {
        let registry = CargoRegistry::new(
            registry_name.clone(),
            None,
            None,
            None,
            None,
            None,
            fetch_indexes,
        )
        .context(format!(
            "Could not create a cargo registry from {}",
            registry_name
        ))?;
        self.add_registry(registry);
        Ok(())
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

fn source_matches_index(source: &str, index: &str) -> bool {
    if index.starts_with("sparse+") {
        source == index
    } else {
        source.starts_with(&format!("registry+{}", index))
    }
}

fn format_source_line(index: &str) -> String {
    if index.starts_with("sparse+") {
        index.to_string()
    } else {
        format!("registry+{}", index)
    }
}

fn replace_registry_in_cargo_lock(
    path: &Path,
    original_registry: &CargoRegistry,
    target_registry: &CargoRegistry,
    http_client: Option<&HyperClient<HttpsConnector<HttpConnector>, Empty<Bytes>>>,
) -> anyhow::Result<()> {
    let original_index = original_registry
        .index
        .as_ref()
        .context(format!("Registry {} has no index", original_registry.name))?;
    let target_index = target_registry
        .index
        .as_ref()
        .context(format!("Registry {} has no index", target_registry.name))?;

    if !original_registry.is_sparse() && original_registry.local_index_path.is_none() {
        anyhow::bail!("Registry {} index not fetched", original_registry.name);
    }
    if !target_registry.is_sparse() && target_registry.local_index_path.is_none() {
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
                && source_matches_index(&source, original_index)
            {
                in_target_package = true;

                let indent = &line[..line.len() - trimmed.len()];
                output.push_str(&format!(
                    "{}source = \"{}\"\n",
                    indent,
                    format_source_line(target_index)
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
                target_registry.get_crate_checksum(&current_name, &current_version, http_client)
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

/// Selectively replace registry references in Cargo.lock only for workspace member packages.
fn replace_registry_in_cargo_lock_selective(
    path: &Path,
    original_registry: &CargoRegistry,
    target_registry: &CargoRegistry,
    http_client: Option<&HyperClient<HttpsConnector<HttpConnector>, Empty<Bytes>>>,
    member_names: &std::collections::HashSet<String>,
) -> anyhow::Result<()> {
    let original_index = original_registry
        .index
        .as_ref()
        .context(format!("Registry {} has no index", original_registry.name))?;
    let target_index = target_registry
        .index
        .as_ref()
        .context(format!("Registry {} has no index", target_registry.name))?;

    if !original_registry.is_sparse() && original_registry.local_index_path.is_none() {
        anyhow::bail!("Registry {} index not fetched", original_registry.name);
    }
    if !target_registry.is_sparse() && target_registry.local_index_path.is_none() {
        anyhow::bail!("Registry {} index not fetched", target_registry.name);
    }

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

        // Parse and potentially update source — only if package is a workspace member
        if trimmed.starts_with("source = ") {
            if let Some(source) = parse_quoted_value(trimmed)
                && source_matches_index(&source, original_index)
                && member_names.contains(&current_name)
            {
                in_target_package = true;

                let indent = &line[..line.len() - trimmed.len()];
                output.push_str(&format!(
                    "{}source = \"{}\"\n",
                    indent,
                    format_source_line(target_index)
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
                target_registry.get_crate_checksum(&current_name, &current_version, http_client)
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

/// Reads `[package].name` from each member's Cargo.toml to build a set of workspace member names.
fn collect_member_package_names(
    member_paths: &[PathBuf],
) -> anyhow::Result<std::collections::HashSet<String>> {
    let mut names = std::collections::HashSet::new();
    for dir in member_paths {
        let cargo_toml = dir.join("Cargo.toml");
        if cargo_toml.exists() {
            let content = fs::read_to_string(&cargo_toml)?;
            let doc: DocumentMut = content
                .parse()
                .with_context(|| format!("Failed to parse {}", cargo_toml.display()))?;
            if let Some(pkg) = doc.get("package").and_then(|p| p.as_table())
                && let Some(name) = pkg.get("name").and_then(|n| n.as_str())
            {
                names.insert(name.to_string());
            }
        }
    }
    Ok(names)
}

/// Selectively patches registry refs in [workspace.dependencies] only for the given package names.
fn replace_registry_in_workspace_deps(
    path: &Path,
    original_registry: &CargoRegistry,
    target_registry: &CargoRegistry,
    member_names: &std::collections::HashSet<String>,
) -> anyhow::Result<()> {
    let content = fs::read_to_string(path)?;
    let mut doc: DocumentMut = content
        .parse()
        .with_context(|| format!("Failed to parse {}", path.display()))?;

    if let Some(ws) = doc.get_mut("workspace").and_then(|w| w.as_table_mut())
        && let Some(deps) = ws.get_mut("dependencies").and_then(|d| d.as_table_mut())
    {
        for (key, item) in deps.iter_mut() {
            if !member_names.contains(key.get()) {
                continue;
            }
            if let Some(tbl) = item.as_table_like_mut()
                && let Some(reg) = tbl.get("registry").and_then(|r| r.as_str())
                && reg == original_registry.name
            {
                tbl.insert("registry", toml_edit::value(&target_registry.name));
            }
        }
    }

    fs::write(path, doc.to_string())?;
    Ok(())
}

pub fn patch_crate_for_registry(
    root_directory: &Path,
    working_directory: &Path,
    original_registry: &CargoRegistry,
    target_registry: &CargoRegistry,
    http_client: Option<&HyperClient<HttpsConnector<HttpConnector>, Empty<Bytes>>>,
    member_paths: &[PathBuf],
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
    // Scope patching to workspace member dirs only. Empty slice = fallback to root (backward compat).
    let dirs_to_scan: Vec<PathBuf> = if member_paths.is_empty() {
        vec![root_directory.to_path_buf()]
    } else {
        member_paths.to_vec()
    };

    let mut patched_files = std::collections::HashSet::new();

    for dir in &dirs_to_scan {
        for entry in WalkDir::new(dir).into_iter() {
            let entry = entry?;
            let path = entry.path().to_path_buf();
            if path.ends_with("Cargo.toml") {
                // Skip root Cargo.toml in member-paths mode — handled below with selective patching
                if !member_paths.is_empty() && path == root_directory.join("Cargo.toml") {
                    continue;
                }
                if patched_files.insert(path.clone()) {
                    replace_registry_in_cargo_toml(&path, original_registry, target_registry)?;
                }
            }
            if path.ends_with("Cargo.lock") {
                // Skip root Cargo.lock in member-paths mode — handled below with selective patching
                if !member_paths.is_empty() && path == root_directory.join("Cargo.lock") {
                    continue;
                }
                if patched_files.insert(path.clone()) {
                    replace_registry_in_cargo_lock(
                        &path,
                        original_registry,
                        target_registry,
                        http_client,
                    )?;
                }
            }
        }
    }

    // Compute member names once for both root Cargo.toml and Cargo.lock selective patching
    let member_names = if !member_paths.is_empty() {
        collect_member_package_names(member_paths)?
    } else {
        std::collections::HashSet::new()
    };

    // Selectively patch workspace-root Cargo.toml — only member deps, not external ones
    let root_toml = root_directory.join("Cargo.toml");
    if root_toml.exists() && patched_files.insert(root_toml.clone()) && !member_names.is_empty() {
        replace_registry_in_workspace_deps(
            &root_toml,
            original_registry,
            target_registry,
            &member_names,
        )?;
    }

    // Selectively patch workspace-root Cargo.lock — only member packages, not external deps
    let root_lock = root_directory.join("Cargo.lock");
    if root_lock.exists() && patched_files.insert(root_lock.clone()) {
        if !member_names.is_empty() {
            replace_registry_in_cargo_lock_selective(
                &root_lock,
                original_registry,
                target_registry,
                http_client,
                &member_names,
            )?;
        } else {
            replace_registry_in_cargo_lock(
                &root_lock,
                original_registry,
                target_registry,
                http_client,
            )?;
        }
    }

    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::utils::test::create_rust_index;
    use mockall::mock;

    mock! {
        #[derive(Default)]
        pub Cargo {}
        impl CrateChecker for Cargo {
            async fn check_crate_exists(
                &self,
                _registry_name: String,
                _name: String,
                _version: String,
            ) -> anyhow::Result<bool>;

            async fn check_crate_name_exists(
                &self,
                _registry_name: String,
                _name: String,
            ) -> anyhow::Result<bool>;

            fn add_registry(
                &mut self,
                _registry_name: String,
                _fetch_indexes: bool,
            ) -> anyhow::Result<()>;
        }
    }

    use super::*;
    use std::fs;

    #[test]
    fn crate_name_exists_when_all_versions_are_yanked() {
        let body = r#"{"name":"example","vers":"1.0.0","yanked":true,"cksum":"abc"}"#;
        assert!(index_has_package_name(body, "example").unwrap());
    }

    #[test]
    fn empty_index_means_crate_name_is_missing() {
        assert!(!index_has_package_name("", "example").unwrap());
    }

    #[test]
    fn mismatched_index_package_name_fails_closed() {
        let body = r#"{"name":"other","vers":"1.0.0","yanked":false,"cksum":"abc"}"#;
        let error = index_has_package_name(body, "example").unwrap_err();
        assert_eq!(
            error.to_string(),
            "Registry index for example returned package other"
        );
    }

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
            patch_crate_for_registry(&tmp, &tmp, &original_registry, &target_registry, None, &[])
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
        assert!(
            patch_crate_for_registry(&tmp, &tmp, &original_registry, &target_registry, None, &[])
                .is_ok(),
        );

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
        assert!(
            patch_crate_for_registry(&tmp, &tmp, &original_registry, &target_registry, None, &[])
                .is_ok()
        );

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
        assert!(
            patch_crate_for_registry(&tmp, &tmp, &original_registry, &target_registry, None, &[])
                .is_ok()
        );
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
            Some("sparse+https://index.crates.io/".to_string()),
            None,
            None,
            None,
            None,
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
            Some("sparse+https://index.crates.io/".to_string()),
            None,
            None,
            None,
            None,
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

        let checksum = reg.get_crate_checksum("crate-test", "0.2.2", None);
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

        let checksum = reg.get_crate_checksum("crate-test", "0.2.2", None).unwrap();
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

        let checksum = reg
            .get_crate_checksum("test-crate-3", "0.1.1", None)
            .unwrap();
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

        let checksum = reg.get_crate_checksum("crate-test-bis", "0.2.2", None);

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

        let checksum = reg.get_crate_checksum("crate-test", "0.8.0", None);

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

        let checksum = reg.get_crate_checksum("crate-test", "0.2.3", None);

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

    #[test]
    fn test_is_sparse_with_sparse_index() {
        let registry = CargoRegistry {
            name: "test-registry".to_string(),
            index: Some("sparse+https://registry.example.com/index/".to_string()),
            ..Default::default()
        };

        assert!(registry.is_sparse());
    }

    /// Regression: crates-io has no `[registries]` entry to read an index from,
    /// so with nothing in the env chain it resolved with `index: None`, and
    /// `check_crate_exists` had no index to query.
    ///
    /// Asserted on `default_index` rather than on a constructed registry: `new`
    /// merges `CARGO_REGISTRIES_CRATES_IO_INDEX` first, and CI sets it, so the
    /// same assertion through `new` passes locally and fails there.
    #[test]
    fn test_crates_io_defaults_to_its_sparse_index() {
        assert_eq!(default_index(CRATES_IO), Some(CRATES_IO_INDEX));

        let registry = CargoRegistry {
            name: CRATES_IO.to_string(),
            index: Some(CRATES_IO_INDEX.to_string()),
            ..Default::default()
        };
        assert!(
            registry.is_sparse(),
            "existence checks take the sparse path, so the default must be a sparse URL"
        );
    }

    #[test]
    fn test_no_default_index_for_other_registries() {
        assert_eq!(default_index("fsl"), None);
    }

    /// An explicitly passed index is never overwritten: `merge` only fills
    /// fields that are still None, so this holds whatever the env supplies.
    #[test]
    fn test_explicit_index_beats_the_crates_io_default() {
        let registry = CargoRegistry::new(
            CRATES_IO.to_string(),
            Some("sparse+https://mirror.example/".to_string()),
            None,
            None,
            None,
            None,
            false,
        )
        .unwrap();

        assert_eq!(
            registry.index.as_deref(),
            Some("sparse+https://mirror.example/")
        );
    }

    /// Cargo rejects a registry name containing `.`, so this constant is what
    /// makes `cargo publish --registry` and `package.publish` matching work.
    #[test]
    fn test_crates_io_name_is_a_legal_cargo_registry_name() {
        assert!(
            !CRATES_IO.contains('.'),
            "cargo: invalid character `.` in registry name"
        );
    }

    #[test]
    fn test_is_sparse_with_git_index() {
        let registry = CargoRegistry {
            name: "test-registry".to_string(),
            index: Some("https://github.com/org/index.git".to_string()),
            ..Default::default()
        };

        assert!(!registry.is_sparse());
    }

    #[test]
    fn test_is_sparse_with_no_index() {
        let registry = CargoRegistry {
            name: "test-registry".to_string(),
            index: None,
            ..Default::default()
        };

        assert!(!registry.is_sparse());
    }

    #[test]
    fn test_source_matches_git_index() {
        assert!(source_matches_index(
            "registry+https://github.com/org/index.git",
            "https://github.com/org/index.git"
        ));
    }

    #[test]
    fn test_source_matches_sparse_index() {
        assert!(source_matches_index(
            "sparse+https://registry.example.com/index/",
            "sparse+https://registry.example.com/index/"
        ));
    }

    #[test]
    fn test_source_does_not_match_wrong_index() {
        assert!(!source_matches_index(
            "registry+https://other.com/index",
            "https://github.com/org/index.git"
        ));
    }

    #[test]
    fn test_format_source_line_git() {
        assert_eq!(
            format_source_line("https://github.com/org/index.git"),
            "registry+https://github.com/org/index.git"
        );
    }

    #[test]
    fn test_format_source_line_sparse() {
        assert_eq!(
            format_source_line("sparse+https://registry.example.com/index/"),
            "sparse+https://registry.example.com/index/"
        );
    }

    #[test]
    fn test_fetch_index_noop_for_sparse() {
        let mut registry = CargoRegistry {
            name: "test-registry".to_string(),
            index: Some("sparse+https://registry.example.com/index/".to_string()),
            ..Default::default()
        };

        let result = registry.fetch_index();

        assert!(result.is_ok());
        assert!(registry.local_index_path.is_none());
    }

    #[test]
    fn test_patching_cargo_lock_sparse_source() {
        let target_checksum = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

        let target_index = create_rust_index(target_checksum);

        let original_registry = CargoRegistry {
            name: "old_registry".to_string(),
            index: Some("sparse+https://old-registry.example.com/index/".to_string()),
            ..Default::default()
        };
        let target_registry = CargoRegistry::new(
            "new_registry".to_string(),
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

        let cargo_toml_path = tmp.join("Cargo.toml");
        let cargo_lock_path = tmp.join("Cargo.lock");

        let toml_content = r#"[package]
name = "my-package"
version = "0.1.0"
publish = ["old_registry"]

[dependencies]
crate-test = { version = "0.2.2", registry = "old_registry" }"#;
        let lock_content = r#"# This file is automatically @generated by Cargo.
# It is not intended for manual editing.
version = 4

[[package]]
name = "crate-test"
version = "0.2.2"
source = "sparse+https://old-registry.example.com/index/"
checksum = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
dependencies = []

[[package]]
name = "my-package"
version = "0.1.0"
dependencies = [
 "dep",
]
"#;

        fs::write(&cargo_toml_path, toml_content).unwrap();
        fs::write(&cargo_lock_path, lock_content).unwrap();

        assert!(
            patch_crate_for_registry(&tmp, &tmp, &original_registry, &target_registry, None, &[])
                .is_ok()
        );

        let replaced_lock_content = fs::read_to_string(cargo_lock_path).unwrap();
        assert!(replaced_lock_content.contains(&format!("registry+{}", target_index.display())));
        assert!(replaced_lock_content.contains(target_checksum));
        assert!(!replaced_lock_content.contains("sparse+https://old-registry.example.com/index/"));
        assert!(
            !replaced_lock_content
                .contains("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
    }

    #[test]
    fn test_patch_scopes_to_member_paths() {
        let tmp = assert_fs::TempDir::new()
            .unwrap()
            .into_persistent()
            .to_path_buf();

        let original_registry = CargoRegistry {
            name: "main_registry".to_string(),
            index: Some("https://main.example.com".to_string()),
            ..Default::default()
        };
        let target_registry = CargoRegistry {
            name: "target_registry".to_string(),
            index: Some("https://target.example.com".to_string()),
            ..Default::default()
        };

        // Root Cargo.toml (working_directory target)
        let root_toml = tmp.join("Cargo.toml");
        fs::write(
            &root_toml,
            r#"[package]
name = "root"
version = "0.1.0"
"#,
        )
        .unwrap();

        // Member directory
        let member_dir = tmp.join("crates").join("member1");
        fs::create_dir_all(&member_dir).unwrap();
        let member_toml = member_dir.join("Cargo.toml");
        fs::write(
            &member_toml,
            r#"[package]
name = "member1"
version = "0.1.0"

[dependencies]
some_dep = { version = "1.0", registry = "main_registry" }
"#,
        )
        .unwrap();

        // Vendor directory (should NOT be patched)
        let vendor_dir = tmp.join("vendor").join("baseline");
        fs::create_dir_all(&vendor_dir).unwrap();
        let vendor_toml = vendor_dir.join("Cargo.toml");
        fs::write(
            &vendor_toml,
            r#"[package]
name = "vendored_crate"
version = "0.1.0"

[dependencies]
old_dep = { version = "0.5", registry = "main_registry" }
"#,
        )
        .unwrap();

        let member_paths = vec![member_dir.clone()];

        // Patch scoped to member_paths only
        patch_crate_for_registry(
            &tmp,
            root_toml.parent().unwrap(),
            &original_registry,
            &target_registry,
            None,
            &member_paths,
        )
        .unwrap();

        // Member should be patched
        let member_content = fs::read_to_string(&member_toml).unwrap();
        assert!(
            member_content.contains("target_registry"),
            "Member Cargo.toml should have target_registry, got:\n{member_content}"
        );
        assert!(
            !member_content.contains("main_registry"),
            "Member Cargo.toml should not contain main_registry, got:\n{member_content}"
        );

        // Vendor should NOT be patched
        let vendor_content = fs::read_to_string(&vendor_toml).unwrap();
        assert!(
            vendor_content.contains("main_registry"),
            "Vendor Cargo.toml should still have main_registry, got:\n{vendor_content}"
        );
        assert!(
            !vendor_content.contains("target_registry"),
            "Vendor Cargo.toml should not have target_registry, got:\n{vendor_content}"
        );
    }

    #[test]
    fn test_selective_workspace_deps_patching() {
        let tmp = assert_fs::TempDir::new()
            .unwrap()
            .into_persistent()
            .to_path_buf();

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

        // Create member_a directory
        let member_a_dir = tmp.join("member_a");
        fs::create_dir_all(&member_a_dir).unwrap();

        // Create member_a Cargo.toml
        let member_a_toml = member_a_dir.join("Cargo.toml");
        fs::write(
            &member_a_toml,
            r#"[package]
name = "member_a"
version = "0.1.0"

[dependencies]
some_dep = { version = "1.0", registry = "main_registry" }
"#,
        )
        .unwrap();

        // Create root Cargo.toml with workspace.dependencies
        let root_toml = tmp.join("Cargo.toml");
        fs::write(
            &root_toml,
            r#"[workspace]
members = ["member_a"]

[workspace.dependencies]
member_a = { version = "0.1.0", registry = "main_registry" }
external_dep = { version = "2.0", registry = "main_registry" }
"#,
        )
        .unwrap();

        let member_paths = vec![member_a_dir.clone()];

        // Call patch_crate_for_registry
        patch_crate_for_registry(
            &tmp,
            &member_a_dir,
            &original_registry,
            &target_registry,
            None,
            &member_paths,
        )
        .unwrap();

        // Read and verify root Cargo.toml
        let root_content = fs::read_to_string(&root_toml).unwrap();

        // member_a should be patched (it's a workspace member)
        assert!(
            root_content.contains(r#"member_a = { version = "0.1.0", registry = "my_registry" }"#),
            "Root Cargo.toml should have member_a patched to my_registry, got:\n{root_content}"
        );

        // external_dep should NOT be patched (it's not a workspace member)
        assert!(
            root_content
                .contains(r#"external_dep = { version = "2.0", registry = "main_registry" }"#),
            "Root Cargo.toml should keep external_dep with main_registry, got:\n{root_content}"
        );

        // Read and verify member_a Cargo.toml
        let member_content = fs::read_to_string(&member_a_toml).unwrap();

        // some_dep should be patched (blanket-patched in member)
        assert!(
            member_content.contains(r#"some_dep = { version = "1.0", registry = "my_registry" }"#),
            "Member Cargo.toml should have some_dep patched to my_registry, got:\n{member_content}"
        );
    }
}

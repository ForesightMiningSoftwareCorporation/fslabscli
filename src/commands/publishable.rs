use std::{env, fs};
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::fs::{File, read_dir};
use std::io::{BufRead, BufReader, Lines};
use std::path::{Path, PathBuf};

use anyhow::Context;
use cargo_metadata::{MetadataCommand, Package};
use clap::Parser;
use docker_credential::{CredentialRetrievalError, DockerCredential};
use http_body_util::{BodyExt, Empty};
use hyper::{Method, Request, Uri};
use hyper::body::Bytes;
use hyper_rustls::ConfigBuilderExt;
use hyper_rustls::HttpsConnector;
use hyper_util::{client::legacy::Client as HyperClient, rt::TokioExecutor};
use hyper_util::client::legacy::connect::HttpConnector;
use oci_distribution::{Client, Reference};
use oci_distribution::client::{ClientConfig, ClientProtocol};
use oci_distribution::errors::{OciDistributionError, OciErrorCode};
use oci_distribution::secrets::RegistryAuth;
use serde::{Deserialize, Serialize};
use serde_json::from_value;

const NPM_DEFAULT_API_URL: &str = "https://registry.npmjs.org/";

#[derive(Debug, Parser)]
#[command(about = "Check directory for crates that need to be published.")]
pub struct Options {
    #[arg(long)]
    docker_registry: Option<String>,
    #[arg(long)]
    docker_registry_username: Option<String>,
    #[arg(long)]
    docker_registry_password: Option<String>,
    #[arg(long)]
    npm_registry_url: Option<String>,
    #[arg(long)]
    npm_registry_token: Option<String>,
    #[arg(long)]
    npm_registry_npmrc_path: Option<String>,
    #[arg(long, default_value_t = true)]
    hide_unpublishable: bool,
}

#[derive(Serialize)]
pub struct Result {
    pub workspace: String,
    pub package: String,
    pub version: String,
    pub path: PathBuf,
    pub publish: PackageMetadataFslabsCiPublish,
}

fn default_false() -> bool { false }

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct PackageMetadataFslabsCiPublishNpmNapi {
    pub publish: bool,
    pub scope: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct PackageMetadataFslabsCiPublishDocker {
    pub publish: bool,
    pub repository: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct PackageMetadataFslabsCiPublish {
    #[serde(default = "PackageMetadataFslabsCiPublishDocker::default")]
    pub docker: PackageMetadataFslabsCiPublishDocker,
    #[serde(default = "default_false")]
    pub private_registry: bool,
    #[serde(default = "default_false")]
    pub public_registry: bool,
    #[serde(default = "PackageMetadataFslabsCiPublishNpmNapi::default")]
    pub npm_napi: PackageMetadataFslabsCiPublishNpmNapi,
    #[serde(default = "default_false")]
    pub binary: bool,
}

#[derive(Deserialize, Default)]
struct PackageMetadataFslabsCi {
    pub publish: PackageMetadataFslabsCiPublish,
}

#[derive(Deserialize, Default)]
struct PackageMetadata {
    pub fslabs: PackageMetadataFslabsCi,
}

impl Result {
    pub fn new(workspace: String, package: Package) -> anyhow::Result<Self> {
        let path = package.manifest_path.canonicalize()?.parent().unwrap().to_path_buf();
        let metadata: PackageMetadata = from_value(package.metadata).unwrap_or_else(|_| PackageMetadata::default());
        Ok(Self {
            workspace,
            package: package.name,
            version: package.version.to_string(),
            path,
            publish: metadata.fslabs.publish,
        })
    }

    pub async fn check_publishable(mut self, options: &Options, npm: &Npm) -> anyhow::Result<Self> {
        if self.publish.docker.publish {
            log::debug!("Docker: checking if version {} of {} already exists", self.version, self.package);
            let docker_registry = match options.docker_registry.clone() {
                Some(r) => r,
                None => match self.publish.docker.repository.clone() {
                    Some(r) => r,
                    None => anyhow::bail!("Tried to check docker image without setting the registry"),
                }
            };
            let image: Reference = format!("{}/{}:{}", docker_registry, self.package.clone(), self.version.clone()).parse()?;
            self.publish.docker.publish = !check_docker_image_exists(
                &image,
                options.docker_registry_username.clone(),
                options.docker_registry_password.clone(),
                None,
            ).await?;
        }
        if self.publish.npm_napi.publish {
            let npm_package_prefix = match self.publish.npm_napi.scope.clone() {
                Some(s) => format!("@{}/", s),
                None => "".to_string(),
            };
            let package_name = format!("{}{}", npm_package_prefix, self.package.clone());
            log::debug!("NPM: checking if version {} of {} already exists", self.version, package_name);
            self.publish.npm_napi.publish = !npm.check_npm_package_exists(package_name.clone(), self.version.clone()).await?;
        }
        Ok(self)
    }
}

impl Display for Result {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f,
               "{} -- {} -- {}: docker: {}, private registry: {}, public registry: {}, npm_napi: {}, binary: {}",
               self.workspace, self.package, self.version,
               self.publish.docker.publish,
               self.publish.private_registry,
               self.publish.public_registry,
               self.publish.npm_napi.publish,
               self.publish.binary)
    }
}

#[derive(Serialize)]
pub struct Results(Vec<Result>);

impl Display for Results {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        for v in &self.0 {
            writeln!(f, "{}", v)?;
        }
        Ok(())
    }
}

pub async fn publishable(options: Options, working_directory: PathBuf) -> anyhow::Result<Results> {
    log::info!("Check directory for crates that need publishing");
    let path = match working_directory.is_absolute() {
        true => working_directory.clone(),
        false => working_directory.canonicalize().with_context(|| format!("Failed to get absolute path from {:?}", working_directory))?,
    };

    let npm = Npm::new(options.npm_registry_url.clone(), options.npm_registry_token.clone(), options.npm_registry_npmrc_path.clone(), true)?;
    let mut results = vec![];
    log::debug!("Base directory: {:?}", path);
    // 1. Find all workspaces to investigate
    let roots = get_cargo_roots(path).with_context(|| format!("Failed to get roots from {:?}", working_directory))?;
    // 2. For each workspace, find if one of the subcrates needs publishing
    for root in roots {
        log::debug!("Checking publishing for: {:?}", root);
        if let Some(workspace_name) = root.file_name() {
            let workspace_metadata = MetadataCommand::new()
                .current_dir(root.clone())
                .no_deps()
                .exec()
                .unwrap();
            for package in workspace_metadata.packages {
                let result = Result::new(workspace_name.to_string_lossy().to_string(), package)?.check_publishable(&options, &npm).await?;
                let should_add = match options.hide_unpublishable {
                    true => vec![result.publish.docker.publish, result.publish.binary, result.publish.npm_napi.publish, result.publish.private_registry, result.publish.public_registry].into_iter().any(|x| x),
                    false => false,
                };
                if should_add {
                    results.push(result);
                }
            }
        }
    }
    Ok(Results(results))
}

fn get_cargo_roots(root: PathBuf) -> anyhow::Result<Vec<PathBuf>> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if Path::exists(root.join("Cargo.toml").as_path()) {
        roots.push(root);
        return Ok(roots);
    }
    for entry in read_dir(root)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            let mut sub_roots = get_cargo_roots(entry.path())?;
            roots.append(&mut sub_roots);
        }
    }
    roots.sort();
    Ok(roots)
}

async fn check_docker_image_exists(
    image: &Reference,
    docker_registry_username: Option<String>,
    docker_registry_password: Option<String>,
    docker_registry_protcol: Option<ClientProtocol>,
) -> anyhow::Result<bool> {
    let protocol = docker_registry_protcol.unwrap_or(ClientProtocol::Https);
    let mut docker_client = Client::new(ClientConfig {
        protocol: protocol.clone(),
        ..Default::default()
    });
    let auth = match (docker_registry_username, docker_registry_password) {
        (Some(username), Some(password)) => {
            RegistryAuth::Basic(username, password)
        }
        _ => {
            let server = image
                .resolve_registry()
                .strip_suffix('/')
                .unwrap_or_else(|| image.resolve_registry());
            println!("Got server: {}", server);
            match docker_credential::get_credential(server) {
                Err(CredentialRetrievalError::ConfigNotFound) => RegistryAuth::Anonymous,
                Err(CredentialRetrievalError::NoCredentialConfigured) => RegistryAuth::Anonymous,
                Err(_) => {
                    RegistryAuth::Anonymous
                }
                Ok(DockerCredential::UsernamePassword(username, password)) => {
                    RegistryAuth::Basic(username, password)
                }
                Ok(DockerCredential::IdentityToken(_)) => {
                    log::warn!("Cannot use contents of docker config, identity token not supported. Using anonymous auth");
                    RegistryAuth::Anonymous
                }
            }
        }
    };
    println!("Checking image: {}, with auth: {:?} and protocol: {:?}", image, auth, protocol);
    match docker_client.fetch_manifest_digest(
        image,
        &auth,
    ).await {
        Ok(_) => Ok(true),
        Err(e) => {
            match e {
                OciDistributionError::RegistryError { envelope, .. } => {
                    for error in envelope.errors {
                        if error.code == OciErrorCode::ManifestUnknown {
                            return Ok(false);
                        }
                    }
                    anyhow::bail!("unknowned registry error")
                }
                OciDistributionError::AuthenticationFailure(e) => {
                    anyhow::bail!("failed to authenticate to the docker registry: {}", e)
                }
                _ => { anyhow::bail!("could not access docker registry: {}", e) }
            }
        }
    }
}

#[derive(Debug)]
struct NpmRegistry {
    auth_token: Option<String>,
    url: String,
}

#[derive(Debug)]
struct NpmRCConfig {
    registries: HashMap<String, NpmRegistry>,
    scopes: HashMap<String, String>,
}

fn read_lines(filename: String) -> anyhow::Result<Lines<BufReader<File>>> {
    let file = File::open(filename)?;
    Ok(BufReader::new(file).lines())
}

impl NpmRCConfig {
    pub fn new(url: Option<String>, token: Option<String>, npmrc_path: Option<String>, tls: bool) -> Self {
        let mut registries = HashMap::new();
        let registry_url = url.unwrap_or_else(|| NPM_DEFAULT_API_URL.to_string());
        registries.insert("default".to_string(), NpmRegistry {
            url: registry_url,
            auth_token: token,
        });
        let mut config = Self {
            registries,
            scopes: HashMap::new(),
        };

        let npmrc_path = if let Some(p) = npmrc_path {
            p
        } else {
            let home_dir = env::var("HOME").unwrap_or_else(|_| "~".to_string());
            format!("{}/.npmrc", home_dir)
        };

        if fs::metadata(npmrc_path.clone()).is_err() {
            return config;
        }

        if let Ok(lines) = read_lines(npmrc_path.clone()) {
            for line in lines.flatten() {
                // Registry
                let token_value: Vec<&str> = line.split(":_authToken=").collect();
                if token_value.len() == 2 {
                    let registry_name = token_value[0].to_string().split_off(2);
                    let protocol = match tls {
                        true => "https",
                        false => "http"
                    };
                    config.registries.insert(registry_name, NpmRegistry {
                        auth_token: Some(token_value[1].to_string()),
                        url: format!("{}:{}", protocol, token_value[0]),
                    });
                    continue;
                }

                let registry_value: Vec<&str> = line.split(":registry=https://").collect();
                if registry_value.len() == 2 {
                    config.scopes.insert(registry_value[0].to_string(), registry_value[1].to_string());
                }
            }
        }
        config
    }
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
struct NpmPackageVersion {
    pub name: String,
    pub version: String,
    pub deprecated: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
struct NpmPackage {
    versions: HashMap<String, NpmPackageVersion>,
}

pub struct Npm {
    rc_config: NpmRCConfig,
    client: HyperClient<HttpsConnector<HttpConnector>, Empty<Bytes>>,
}

impl Npm {
    pub fn new(registry_url: Option<String>, registry_token: Option<String>, npmrc_path: Option<String>, tls: bool) -> anyhow::Result<Self> {
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(rustls::ClientConfig::builder().with_native_roots()?.with_no_client_auth())
            .https_or_http()
            .enable_http1()
            .build();

        Ok(Self {
            rc_config: NpmRCConfig::new(registry_url, registry_token, npmrc_path, tls),
            client: HyperClient::builder(TokioExecutor::new()).build(https),
        })
    }

    pub async fn check_npm_package_exists(&self, package: String, version: String) -> anyhow::Result<bool> {
        // Infer registry if scoped
        let registry: Option<&NpmRegistry> = if package.starts_with('@') {
            if let Some((scope, _)) = package.clone().split_once('/') {
                if let Some(scoped_registry) = self.rc_config.scopes.get(scope) {
                    self.rc_config.registries.get(scoped_registry)
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        // If no registry, should use default
        let registry = match registry {
            Some(s) => s,
            None => {
                if let Some(default_reg) = self.rc_config.registries.get("default") {
                    default_reg
                } else {
                    anyhow::bail!("Could not infer npm default registry")
                }
            }
        };

        let url: Uri = format!("{}{}", registry.url, package).parse()?;

        let mut req_builder = Request::builder()
            .method(Method::GET)
            .uri(url);

        if let Some(token) = &registry.auth_token {
            req_builder = req_builder.header("Authorization", format!("Bearer {}", token));
        }

        let req = req_builder.body(Empty::default())?;
        let res = self.client.request(req).await.with_context(|| "Could not fetch from the npm registry")?;

        if res.status().as_u16() >= 400 {
            anyhow::bail!("Something went wrong while getting npm api data");
        }

        let body = res
            .into_body()
            .collect()
            .await
            .with_context(|| "Could not get body from the npm registry")?
            .to_bytes();


        let package: NpmPackage = serde_json::from_str(String::from_utf8_lossy(&body).as_ref())?;
        for (_, package_version) in package.versions {
            if package_version.version == version {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::fs::{create_dir_all, File};
    use std::io::Write;

    use assert_fs::TempDir;
    use indoc::formatdoc;
    use serial_test::serial;
    use testcontainers::{clients, Container, GenericImage};
    use testcontainers::core::WaitFor;
    use wiremock::{matchers::{method, path}, Mock, MockServer, ResponseTemplate};
    use wiremock::matchers::bearer_token;

    use super::*;

    const DOCKER_HTPASSWD: &str = "testuser:$2y$05$8/q2bfRcX74EuxGf0qOcSuhWDQJXrgWiy6Fi73/JM2tKC66qSrLve";
    const DOCKER_HTPASSWD_USERNAME: &str = "testuser";
    const DOCKER_HTPASSWD_PASSWORD: &str = "testpassword";
    const DOCKER_HTPASSWD_AUTH: &str = "dGVzdHVzZXI6dGVzdHBhc3N3b3Jk";

    const NPM_EXISTING_PACKAGE_DATA: &str = "{\"_id\":\"axios\",\"_rev\":\"779-b37ceeb27a03858a89a0226f7c554aaf\",\"name\":\"axios\",\"description\":\"Promise based HTTP client for the browser and node.js\",\"dist-tags\":{\"latest\":\"0.1.0\",\"next\":\"0.2.0\"},\"versions\":{\"0.1.0\":{\"name\":\"axios\",\"version\":\"0.1.0\",\"description\":\"Promise based XHR library\",\"main\":\"index.js\",\"scripts\":{\"test\":\"grunt test\",\"start\":\"node ./sandbox/index.js\"},\"repository\":{\"type\":\"git\",\"url\":\"https://github.com/mzabriskie/axios.git\"},\"keywords\":[\"xhr\",\"http\",\"ajax\",\"promise\"],\"author\":{\"name\":\"Matt Zabriskie\"},\"license\":\"MIT\",\"bugs\":{\"url\":\"https://github.com/mzabriskie/axios/issues\"},\"homepage\":\"https://github.com/mzabriskie/axios\",\"dependencies\":{\"es6-promise\":\"^1.0.0\"},\"devDependencies\":{\"grunt\":\"^0.4.5\",\"grunt-contrib-clean\":\"^0.6.0\",\"grunt-contrib-watch\":\"^0.6.1\",\"webpack\":\"^1.3.3-beta2\",\"webpack-dev-server\":\"^1.4.10\",\"grunt-webpack\":\"^1.0.8\",\"load-grunt-tasks\":\"^0.6.0\",\"karma\":\"^0.12.21\",\"karma-jasmine\":\"^0.1.5\",\"grunt-karma\":\"^0.8.3\",\"karma-phantomjs-launcher\":\"^0.1.4\",\"karma-jasmine-ajax\":\"^0.1.4\",\"grunt-update-json\":\"^0.1.3\",\"grunt-contrib-nodeunit\":\"^0.4.1\",\"grunt-banner\":\"^0.2.3\"},\"_id\":\"axios@0.1.0\",\"dist\":{\"shasum\":\"854e14f2999c2ef7fab058654fd995dd183688f2\",\"tarball\":\"https://registry.npmjs.org/axios/-/axios-0.1.0.tgz\",\"integrity\":\"sha512-hRPotWTy88LEsJ31RWEs2fmU7mV2YJs3Cw7Tk5XkKGtnT5NKOyIvPU+6qTWfwQFusxzChe8ozjay8r56wfpX8w==\",\"signatures\":[{\"keyid\":\"SHA256:jl3bwswu80PjjokCgh0o2w5c2U4LhQAE57gj9cz1kzA\",\"sig\":\"MEYCIQC/cOvHsV7UqLAet6WE89O4Ga3AUHgkqqoP0riLs6sgTAIhAIrePavu3Uw0T3vLyYMlfEI9bqENYjPzH5jGK8vYQVJK\"}]},\"_from\":\"./\",\"_npmVersion\":\"1.4.3\",\"_npmUser\":{\"name\":\"mzabriskie\",\"email\":\"mzabriskie@gmail.com\"},\"maintainers\":[{\"name\":\"mzabriskie\",\"email\":\"mzabriskie@gmail.com\"}],\"directories\":{},\"deprecated\":\"Critical security vulnerability fixed in v0.21.1. For more information, see https://github.com/axios/axios/pull/3410\"},\"0.2.0\":{\"name\":\"axios\",\"version\":\"0.2.0\",\"description\":\"Promise based HTTP client for the browser and node.js\",\"main\":\"index.js\",\"scripts\":{\"test\":\"grunt test\",\"start\":\"node ./sandbox/server.js\"},\"repository\":{\"type\":\"git\",\"url\":\"https://github.com/mzabriskie/axios.git\"},\"keywords\":[\"xhr\",\"http\",\"ajax\",\"promise\",\"node\"],\"author\":{\"name\":\"Matt Zabriskie\"},\"license\":\"MIT\",\"bugs\":{\"url\":\"https://github.com/mzabriskie/axios/issues\"},\"homepage\":\"https://github.com/mzabriskie/axios\",\"dependencies\":{\"es6-promise\":\"^1.0.0\"},\"devDependencies\":{\"grunt\":\"^0.4.5\",\"grunt-contrib-clean\":\"^0.6.0\",\"grunt-contrib-watch\":\"^0.6.1\",\"webpack\":\"^1.3.3-beta2\",\"webpack-dev-server\":\"^1.4.10\",\"grunt-webpack\":\"^1.0.8\",\"load-grunt-tasks\":\"^0.6.0\",\"karma\":\"^0.12.21\",\"karma-jasmine\":\"^0.1.5\",\"grunt-karma\":\"^0.8.3\",\"karma-phantomjs-launcher\":\"^0.1.4\",\"karma-jasmine-ajax\":\"^0.1.4\",\"grunt-update-json\":\"^0.1.3\",\"grunt-contrib-nodeunit\":\"^0.4.1\",\"grunt-banner\":\"^0.2.3\"},\"_id\":\"axios@0.2.0\",\"dist\":{\"shasum\":\"315cd618142078fd22f2cea35380caad19e32069\",\"tarball\":\"https://registry.npmjs.org/axios/-/axios-0.2.0.tgz\",\"integrity\":\"sha512-ZQb2IDQfop5Asx8PlKvccsSVPD8yFCwYZpXrJCyU+MqL4XgJVjMHkCTNQV/pmB0Wv7l74LUJizSM/SiPz6r9uw==\",\"signatures\":[{\"keyid\":\"SHA256:jl3bwswu80PjjokCgh0o2w5c2U4LhQAE57gj9cz1kzA\",\"sig\":\"MEQCIAkrijLTtL7uiw0fQf5GL/y7bJ+3J8Z0zrrzNLC5fTXlAiBd4Nr/EJ2nWfBGWv/9OkrAONoboG5C8t8plIt5LVeGQA==\"}]},\"_from\":\"./\",\"_npmVersion\":\"1.4.3\",\"_npmUser\":{\"name\":\"mzabriskie\",\"email\":\"mzabriskie@gmail.com\"},\"maintainers\":[{\"name\":\"mzabriskie\",\"email\":\"mzabriskie@gmail.com\"}],\"directories\":{},\"deprecated\":\"Critical security vulnerability fixed in v0.21.1. For more information, see https://github.com/axios/axios/pull/3410\"}},\"readme\":\"axios\",\"maintainers\":[],\"time\":{\"modified\":\"2022-12-29T06:38:42.456Z\",\"created\":\"2014-08-29T23:08:36.810Z\",\"0.1.0\":\"2014-08-29T23:08:36.810Z\",\"0.2.0\":\"2014-09-12T20:06:33.167Z\"},\"homepage\":\"https://axios-http.com\",\"keywords\":[],\"repository\":{\"type\":\"git\",\"url\":\"git+https://github.com/axios/axios.git\"},\"author\":{\"name\":\"Matt Zabriskie\"},\"bugs\":{\"url\":\"https://github.com/axios/axios/issues\"},\"license\":\"MIT\",\"readmeFilename\":\"README.md\",\"users\":{},\"contributors\":[]}\n";
    const NPM_EXISTING_SCOPE_PACKAGE_DATA: &str = "{\"_id\":\"@TestScope/test\",\"_rev\":\"779-b37ceeb27a03858a89a0226f7c554aaf\",\"name\":\"@TestScope/test\",\"description\":\"Promise based HTTP client for the browser and node.js\",\"dist-tags\":{\"latest\":\"0.1.0\",\"next\":\"0.2.0\"},\"versions\":{\"0.1.0\":{\"name\":\"@TestScope/test\",\"version\":\"0.1.0\",\"description\":\"Promise based XHR library\",\"main\":\"index.js\",\"scripts\":{\"test\":\"grunt test\",\"start\":\"node ./sandbox/index.js\"},\"repository\":{\"type\":\"git\",\"url\":\"https://github.com/mzabriskie/@TestScope/test.git\"},\"keywords\":[\"xhr\",\"http\",\"ajax\",\"promise\"],\"author\":{\"name\":\"Matt Zabriskie\"},\"license\":\"MIT\",\"bugs\":{\"url\":\"https://github.com/mzabriskie/@TestScope/test/issues\"},\"homepage\":\"https://github.com/mzabriskie/@TestScope/test\",\"dependencies\":{\"es6-promise\":\"^1.0.0\"},\"devDependencies\":{\"grunt\":\"^0.4.5\",\"grunt-contrib-clean\":\"^0.6.0\",\"grunt-contrib-watch\":\"^0.6.1\",\"webpack\":\"^1.3.3-beta2\",\"webpack-dev-server\":\"^1.4.10\",\"grunt-webpack\":\"^1.0.8\",\"load-grunt-tasks\":\"^0.6.0\",\"karma\":\"^0.12.21\",\"karma-jasmine\":\"^0.1.5\",\"grunt-karma\":\"^0.8.3\",\"karma-phantomjs-launcher\":\"^0.1.4\",\"karma-jasmine-ajax\":\"^0.1.4\",\"grunt-update-json\":\"^0.1.3\",\"grunt-contrib-nodeunit\":\"^0.4.1\",\"grunt-banner\":\"^0.2.3\"},\"_id\":\"@TestScope/test@0.1.0\",\"dist\":{\"shasum\":\"854e14f2999c2ef7fab058654fd995dd183688f2\",\"tarball\":\"https://registry.npmjs.org/@TestScope/test/-/@TestScope/test-0.1.0.tgz\",\"integrity\":\"sha512-hRPotWTy88LEsJ31RWEs2fmU7mV2YJs3Cw7Tk5XkKGtnT5NKOyIvPU+6qTWfwQFusxzChe8ozjay8r56wfpX8w==\",\"signatures\":[{\"keyid\":\"SHA256:jl3bwswu80PjjokCgh0o2w5c2U4LhQAE57gj9cz1kzA\",\"sig\":\"MEYCIQC/cOvHsV7UqLAet6WE89O4Ga3AUHgkqqoP0riLs6sgTAIhAIrePavu3Uw0T3vLyYMlfEI9bqENYjPzH5jGK8vYQVJK\"}]},\"_from\":\"./\",\"_npmVersion\":\"1.4.3\",\"_npmUser\":{\"name\":\"mzabriskie\",\"email\":\"mzabriskie@gmail.com\"},\"maintainers\":[{\"name\":\"mzabriskie\",\"email\":\"mzabriskie@gmail.com\"}],\"directories\":{},\"deprecated\":\"Critical security vulnerability fixed in v0.21.1. For more information, see https://github.com/@TestScope/test/@TestScope/test/pull/3410\"},\"0.2.0\":{\"name\":\"@TestScope/test\",\"version\":\"0.2.0\",\"description\":\"Promise based HTTP client for the browser and node.js\",\"main\":\"index.js\",\"scripts\":{\"test\":\"grunt test\",\"start\":\"node ./sandbox/server.js\"},\"repository\":{\"type\":\"git\",\"url\":\"https://github.com/mzabriskie/@TestScope/test.git\"},\"keywords\":[\"xhr\",\"http\",\"ajax\",\"promise\",\"node\"],\"author\":{\"name\":\"Matt Zabriskie\"},\"license\":\"MIT\",\"bugs\":{\"url\":\"https://github.com/mzabriskie/@TestScope/test/issues\"},\"homepage\":\"https://github.com/mzabriskie/@TestScope/test\",\"dependencies\":{\"es6-promise\":\"^1.0.0\"},\"devDependencies\":{\"grunt\":\"^0.4.5\",\"grunt-contrib-clean\":\"^0.6.0\",\"grunt-contrib-watch\":\"^0.6.1\",\"webpack\":\"^1.3.3-beta2\",\"webpack-dev-server\":\"^1.4.10\",\"grunt-webpack\":\"^1.0.8\",\"load-grunt-tasks\":\"^0.6.0\",\"karma\":\"^0.12.21\",\"karma-jasmine\":\"^0.1.5\",\"grunt-karma\":\"^0.8.3\",\"karma-phantomjs-launcher\":\"^0.1.4\",\"karma-jasmine-ajax\":\"^0.1.4\",\"grunt-update-json\":\"^0.1.3\",\"grunt-contrib-nodeunit\":\"^0.4.1\",\"grunt-banner\":\"^0.2.3\"},\"_id\":\"@TestScope/test@0.2.0\",\"dist\":{\"shasum\":\"315cd618142078fd22f2cea35380caad19e32069\",\"tarball\":\"https://registry.npmjs.org/@TestScope/test/-/@TestScope/test-0.2.0.tgz\",\"integrity\":\"sha512-ZQb2IDQfop5Asx8PlKvccsSVPD8yFCwYZpXrJCyU+MqL4XgJVjMHkCTNQV/pmB0Wv7l74LUJizSM/SiPz6r9uw==\",\"signatures\":[{\"keyid\":\"SHA256:jl3bwswu80PjjokCgh0o2w5c2U4LhQAE57gj9cz1kzA\",\"sig\":\"MEQCIAkrijLTtL7uiw0fQf5GL/y7bJ+3J8Z0zrrzNLC5fTXlAiBd4Nr/EJ2nWfBGWv/9OkrAONoboG5C8t8plIt5LVeGQA==\"}]},\"_from\":\"./\",\"_npmVersion\":\"1.4.3\",\"_npmUser\":{\"name\":\"mzabriskie\",\"email\":\"mzabriskie@gmail.com\"},\"maintainers\":[{\"name\":\"mzabriskie\",\"email\":\"mzabriskie@gmail.com\"}],\"directories\":{},\"deprecated\":\"Critical security vulnerability fixed in v0.21.1. For more information, see https://github.com/@TestScope/test/@TestScope/test/pull/3410\"}},\"readme\":\"@TestScope/test\",\"maintainers\":[],\"time\":{\"modified\":\"2022-12-29T06:38:42.456Z\",\"created\":\"2014-08-29T23:08:36.810Z\",\"0.1.0\":\"2014-08-29T23:08:36.810Z\",\"0.2.0\":\"2014-09-12T20:06:33.167Z\"},\"homepage\":\"https://@TestScope/test-http.com\",\"keywords\":[],\"repository\":{\"type\":\"git\",\"url\":\"git+https://github.com/@TestScope/test/@TestScope/test.git\"},\"author\":{\"name\":\"Matt Zabriskie\"},\"bugs\":{\"url\":\"https://github.com/@TestScope/test/@TestScope/test/issues\"},\"license\":\"MIT\",\"readmeFilename\":\"README.md\",\"users\":{},\"contributors\":[]}\n";

    #[test]
    fn test_get_cargo_roots_simple_crate() {
        // Create fake directory structure
        let dir = TempDir::new().expect("Could not create temp dir");
        let path = dir.path();
        let file_path = dir.path().join("Cargo.toml");
        fs::File::create(file_path).expect("Could not create root Cargo.toml");
        let roots = get_cargo_roots(path.to_path_buf()).expect("Could not get roots");
        let expected_results = vec![
            path
        ];
        assert_eq!(roots, expected_results);
    }

    #[test]
    fn test_get_cargo_roots_simple_workspace() {
        // Create fake directory structure
        let dir = TempDir::new().expect("Could not create temp dir");
        let path = dir.path();
        fs::File::create(dir.path().join("Cargo.toml")).expect("Could not create root Cargo.toml");
        create_dir_all(dir.path().join("crates/subcrate_a")).expect("Could not create subdir");
        fs::File::create(dir.path().join("crates/subcrate_a/Cargo.toml")).expect("Could not create root Cargo.toml");
        create_dir_all(dir.path().join("crates/subcrate_b")).expect("Could not create subdir");
        fs::File::create(dir.path().join("crates/subcrate_b/Cargo.toml")).expect("Could not create root Cargo.toml");
        let roots = get_cargo_roots(path.to_path_buf()).expect("Could not get roots");
        let expected_results = vec![
            path
        ];
        assert_eq!(roots, expected_results);
    }

    #[test]
    fn test_get_cargo_roots_complex_monorepo() {
        // Create fake directory structure
        // dir
        //  - subdir_a/Cargo.toml
        //  - subdir_b/Cargo_toml
        //  - subdir_b/crates/subcrate_a/Cargo.toml
        //  - subdir_b/crates/subcrate_b/Cargo.toml
        //  - subdir_c
        //  - subdir_d/subdir_a/Cargo.toml
        //  - subdir_d/subdir_b/Cargo.tom
        //  - subdir_d/subdir_b/crates/subcrate_a/Cargo.toml
        //  - subdir_d/subdir_b/crates/subcrate_b/Cargo.toml
        let dir = TempDir::new().expect("Could not create temp dir");
        create_dir_all(dir.path().join("subdir_a")).expect("Could not create subdir");
        create_dir_all(dir.path().join("subdir_b/crates/subcrate_a")).expect("Could not create subdir");
        create_dir_all(dir.path().join("subdir_b/crates/subcrate_b")).expect("Could not create subdir");
        create_dir_all(dir.path().join("subdir_c")).expect("Could not create subdir");
        create_dir_all(dir.path().join("subdir_d/subdir_a")).expect("Could not create subdir");
        create_dir_all(dir.path().join("subdir_d/subdir_b/crates/subcrate_a")).expect("Could not create subdir");
        create_dir_all(dir.path().join("subdir_d/subdir_b/crates/subcrate_b")).expect("Could not create subdir");
        fs::File::create(dir.path().join("subdir_a/Cargo.toml")).expect("Could not create root Cargo.toml");
        fs::File::create(dir.path().join("subdir_b/Cargo.toml")).expect("Could not create root Cargo.toml");
        fs::File::create(dir.path().join("subdir_b/crates/subcrate_a/Cargo.toml")).expect("Could not create root Cargo.toml");
        fs::File::create(dir.path().join("subdir_b/crates/subcrate_b/Cargo.toml")).expect("Could not create root Cargo.toml");
        fs::File::create(dir.path().join("subdir_d/subdir_a/Cargo.toml")).expect("Could not create root Cargo.toml");
        fs::File::create(dir.path().join("subdir_d/subdir_b/Cargo.toml")).expect("Could not create root Cargo.toml");
        fs::File::create(dir.path().join("subdir_d/subdir_b/crates/subcrate_a/Cargo.toml")).expect("Could not create root Cargo.toml");
        fs::File::create(dir.path().join("subdir_d/subdir_b/crates/subcrate_b/Cargo.toml")).expect("Could not create root Cargo.toml");

        let path = dir.path();
        let roots = get_cargo_roots(path.to_path_buf()).expect("Could not get roots");
        let expected_results = vec![
            path.join("subdir_a").to_path_buf(),
            path.join("subdir_b").to_path_buf(),
            path.join("subdir_d/subdir_a").to_path_buf(),
            path.join("subdir_d/subdir_b").to_path_buf(),
        ];
        assert_eq!(roots, expected_results);
    }

    async fn docker_test(docker_image: String, auth_file: bool, docker_username: Option<String>, docker_password: Option<String>, mock: bool, expected_result: bool, expected_error: bool) {
        let registry_tmp_dir = TempDir::new().expect("cannot create tmp directory");
        let docker_tmp_dir = TempDir::new().expect("cannot create tmp directory");
        let mut image: Reference = docker_image.parse().unwrap();
        let mut protocol: Option<ClientProtocol> = None;
        let mut username = docker_username.clone();
        let mut password = docker_password.clone();
        // Main scope
        let docker = clients::Cli::default();
        let registry_container: Container<GenericImage>;
        let init_docker_config = env::var("DOCKER_CONFIG");
        if mock {
            let htpasswd_path = registry_tmp_dir.path().join("htpasswd");
            let mut f = File::create(htpasswd_path).expect("Could not create htpasswd file");
            f.write_all(DOCKER_HTPASSWD.as_bytes()).expect("Could not write htpasswd file");
            let registry_image = GenericImage::new("docker.io/library/registry", "2")
                .with_env_var("REGISTRY_AUTH", "htpasswd")
                .with_env_var("REGISTRY_AUTH_HTPASSWD_REALM", "Registry Realm")
                .with_env_var("REGISTRY_AUTH_HTPASSWD_PATH", "/auth/htpasswd")
                .with_env_var("REGISTRY_PROXY_REMOTEURL", "https://registry-1.docker.io")
                .with_exposed_port(5000)
                .with_volume(registry_tmp_dir.path().to_str().expect("cannot convert auth_dir to string"), "/auth")
                .with_wait_for(WaitFor::message_on_stderr("listening on "));

            registry_container = docker.run(registry_image);

            let port = registry_container.get_host_port_ipv4(5000);
            protocol = Some(ClientProtocol::HttpsExcept(vec![format!("127.0.0.1:{}", port)]));
            image = format!("127.0.0.1:{}/{}", port, docker_image).parse().unwrap();
            if auth_file {
                let config_path = docker_tmp_dir.path().join("config.json");
                let mut f = File::create(config_path.clone()).expect("Could not create docker config file");
                let docker_config = formatdoc!(r#"
                {{
                    "auths": {{
                        "{registry}": {{
                            "auth": "{auth}"
                        }}
                    }}
                }}"#,
                    registry = format!("127.0.0.1:{}", port),
                    auth = DOCKER_HTPASSWD_AUTH
                );
                f.write_all(docker_config.as_bytes()).expect("Could not write to docker config file");
                env::set_var("DOCKER_CONFIG", docker_tmp_dir.path());
                username = None;
                password = None;
            }
        }
        let result = check_docker_image_exists(
            &image,
            username,
            password,
            protocol,
        ).await;
        match init_docker_config {
            Ok(c) => env::set_var("DOCKER_CONFIG", c),
            Err(_) => env::set_var("DOCKER_CONFIG", ""),
        }
        match result {
            Ok(exists) => {
                assert!(!expected_error);
                assert_eq!(expected_result, exists);
            }
            Err(e) => {
                println!("Got error: {}", e);
                assert!(expected_error);
            }
        }
    }

    #[tokio::test]
    #[serial(docker)]
    async fn docker_existing_image() {
        docker_test("alpine:latest".to_string(), false, None, None, false, true, false).await;
    }

    #[tokio::test]
    #[serial(docker)]
    async fn docker_existing_image_non_existing_version() {
        docker_test("alpine:NONEXISTENTTAG".to_string(), false, None, None, false, false, false).await;
    }

    #[tokio::test]
    #[serial(docker)]
    async fn docker_private_reg_existing_image() {
        docker_test("library/alpine:latest".to_string(), false, Some(DOCKER_HTPASSWD_USERNAME.to_string()), Some(DOCKER_HTPASSWD_PASSWORD.to_string()), true, true, false).await;
    }

    #[tokio::test]
    #[serial(docker)]
    async fn docker_private_reg_existing_image_non_existing_version() {
        docker_test("library/alpine:NONEXISTANT".to_string(), false, Some(DOCKER_HTPASSWD_USERNAME.to_string()), Some(DOCKER_HTPASSWD_PASSWORD.to_string()), true, false, false).await;
    }


    #[tokio::test]
    #[serial(docker)]
    async fn docker_private_reg_auth_file_existing_image() {
        docker_test("library/alpine:latest".to_string(), true, Some(DOCKER_HTPASSWD_USERNAME.to_string()), Some(DOCKER_HTPASSWD_PASSWORD.to_string()), true, true, false).await;
    }

    #[tokio::test]
    #[serial(docker)]
    async fn docker_private_reg_auth_file_existing_image_non_existing_version() {
        docker_test("library/alpine:NONEXISTANT".to_string(), true, Some(DOCKER_HTPASSWD_USERNAME.to_string()), Some(DOCKER_HTPASSWD_PASSWORD.to_string()), true, false, false).await;
    }

    async fn npm_test(package_name: String, package_version: String, registry_token: Option<String>, npmrc: bool, expected_result: bool, expected_error: bool, mocked_body: Option<String>, mocked_token: Option<String>, mocked_status: Option<u16>) {
        let tmp_dir = TempDir::new().expect("cannot create tmp directory");
        let mut registry_url: Option<String> = None;

        let mut npmrc_path: Option<String> = None;
        if let (Some(body), Some(token), Some(status)) = (mocked_body, mocked_token, mocked_status) {
            let mock_server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path(format!("/{}", package_name)))
                .and(bearer_token(token.clone()))
                .respond_with(ResponseTemplate::new(status).set_body_raw(body, "application/json"))
                .mount(&mock_server)
                .await;
            let mock_server_uri = mock_server.uri();

            if npmrc {
                let path = tmp_dir.path().join(".npmrc");
                let mut f = File::create(path.clone()).expect("Could not create npmrc file");
                let npm_mock_server_uri = mock_server_uri.replace("http://", "");
                let npmrc = format!("//{}/:_authToken={}\n@TestScope:registry=https://{}/", npm_mock_server_uri, token, npm_mock_server_uri);
                f.write_all(npmrc.as_bytes()).expect("Could not write htpasswd file");
                npmrc_path = Some(path.to_string_lossy().to_string());
            } else {
                registry_url = Some(format!("{}/", mock_server_uri));
            }
        }


        let npm = Npm::new(registry_url, registry_token, npmrc_path, false).expect("Could not get npm client");
        let result = npm.check_npm_package_exists(package_name, package_version).await;
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
    async fn npm_package_existing_package() {
        npm_test("axios".to_string(), "1.0.0".to_string(), None, false, true, false, None::<String>, None::<String>, None::<u16>).await;
    }

    #[tokio::test]
    async fn npm_package_on_existing_package() {
        npm_test("axios".to_string(), "99.99.99".to_string(), None, false, false, false, None::<String>, None::<String>, None::<u16>).await;
    }

    #[tokio::test]
    async fn npm_package_existing_package_custom_registry_token() {
        npm_test("axios".to_string(), "0.2.0".to_string(), Some("my_token".to_string()), false, true, false, Some(NPM_EXISTING_PACKAGE_DATA.to_string()), Some("my_token".to_string()), Some(200)).await;
    }

    #[tokio::test]
    async fn npm_package_non_existing_package_version_custom_registry_token() {
        npm_test("axios".to_string(), "99.99.99".to_string(), Some("my_token".to_string()), false, false, false, Some(NPM_EXISTING_PACKAGE_DATA.to_string()), Some("my_token".to_string()), Some(200)).await;
    }

    #[tokio::test]
    async fn npm_package_existing_package_custom_registry_npmrc() {
        npm_test("@TestScope/test".to_string(), "0.2.0".to_string(), None, true, true, false, Some(NPM_EXISTING_SCOPE_PACKAGE_DATA.to_string()), Some("my_token".to_string()), Some(200)).await;
    }

    #[tokio::test]
    async fn npm_package_non_existing_package_custom_registry_npmrc() {
        npm_test("@TestScope/test".to_string(), "99.99.99".to_string(), None, true, false, false, Some(NPM_EXISTING_SCOPE_PACKAGE_DATA.to_string()), Some("my_token".to_string()), Some(200)).await;
    }
}
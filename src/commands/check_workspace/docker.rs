use oci_distribution::client::{ClientConfig, ClientProtocol};
use oci_distribution::errors::{OciDistributionError, OciErrorCode};
use oci_distribution::secrets::RegistryAuth;
use oci_distribution::{Client as DockerClient, Reference};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use anyhow::Context;
use base64::prelude::*;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Method, Request};
use hyper_rustls::{ConfigBuilderExt, HttpsConnector};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::rt::TokioExecutor;
use url::Url;

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
struct DockerTokenResp {
    access_token: String,
}

#[derive(Deserialize)]
struct DockerAuthConfig {
    auth: Option<String>,
    identitytoken: Option<String>,
}

#[derive(Deserialize)]
struct DockerConfig {
    auths: Option<HashMap<String, DockerAuthConfig>>,
}

pub struct Docker {
    hyper_client: HyperClient<HttpsConnector<HttpConnector>, Full<Bytes>>,
    docker_client: DockerClient,
    registries_auths: HashMap<String, DockerCredential>,
}

#[derive(Debug, PartialEq)]
enum DockerCredential {
    IdentityToken(String, String),
    UsernamePassword(String, String),
}

impl Docker {
    pub fn new(config_path: Option<String>) -> anyhow::Result<Self> {
        let mut registries_auths = HashMap::new();
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(
                rustls::ClientConfig::builder()
                    .with_native_roots()?
                    .with_no_client_auth(),
            )
            .https_or_http()
            .enable_http1()
            .build();
        let config_path = match config_path {
            Some(c) => Some(c),
            None => {
                let home_config =
                    || env::var_os("HOME").map(|home| Path::new(&home).join(".docker"));
                env::var_os("DOCKER_CONFIG")
                    .map(|dir| Path::new(&dir).to_path_buf())
                    .or_else(home_config)
                    .map(|dir| dir.join("config.json").to_string_lossy().to_string())
            }
        };

        if let Some(p) = config_path {
            if let Ok(file) = File::open(p) {
                let reader = BufReader::new(file);
                if let Ok(config) = serde_json::from_reader::<BufReader<File>, DockerConfig>(reader)
                {
                    if let Some(registries_auth) = config.auths {
                        for (k, v) in registries_auth {
                            let mut username: Option<String> = None;
                            let mut password: Option<String> = None;
                            if let Some(encoded_auth) = v.auth {
                                if let Ok(decoded) = BASE64_STANDARD.decode(encoded_auth) {
                                    if let Ok(auth) = std::str::from_utf8(&decoded) {
                                        let parts: Vec<&str> = auth.splitn(2, ':').collect();
                                        if let Some(u) = parts.first() {
                                            username = Some(String::from(*u));
                                        }
                                        if let Some(p) = parts.get(1) {
                                            password = Some(String::from(*p));
                                        }
                                    }
                                }
                            }
                            if let (Some(username), Some(identity_token)) =
                                (username.clone(), v.identitytoken)
                            {
                                registries_auths.insert(
                                    k.to_string(),
                                    DockerCredential::IdentityToken(
                                        username,
                                        identity_token.clone(),
                                    ),
                                );
                            } else if let (Some(username), Some(password)) = (username, password) {
                                registries_auths.insert(
                                    k.to_string(),
                                    DockerCredential::UsernamePassword(
                                        username.clone(),
                                        password.clone(),
                                    ),
                                );
                            }
                        }
                    }
                }
            }
        }

        Ok(Self {
            registries_auths,
            hyper_client: HyperClient::builder(TokioExecutor::new()).build(https),
            docker_client: DockerClient::new(ClientConfig {
                protocol: ClientProtocol::Https,
                ..Default::default()
            }),
        })
    }

    pub fn add_registry_auth(&mut self, name: String, username: String, password: String) {
        self.registries_auths
            .insert(name, DockerCredential::UsernamePassword(username, password));
    }

    pub async fn check_image_exists(
        &mut self,
        registry_name: String,
        name: String,
        version: String,
    ) -> anyhow::Result<bool> {
        log::debug!(
            "Docker: checking if version {} of {} already exists",
            version,
            name
        );

        let image: Reference =
            format!("{}/{}:{}", registry_name, name.clone(), version.clone()).parse()?;

        let auth = match self.registries_auths.get(&registry_name) {
            None => RegistryAuth::Anonymous,
            Some(docker_credential) => match docker_credential {
                DockerCredential::UsernamePassword(username, password) => {
                    RegistryAuth::Basic(username.clone(), password.clone())
                }
                DockerCredential::IdentityToken(username, token) => {
                    // With the Token, we should get a password and also find the username
                    let oauth2_token_url =
                        Url::parse(format!("https://{}/oauth2/token", registry_name).as_str())
                            .unwrap();
                    let req_builder = Request::builder()
                        .method(Method::POST)
                        .uri(oauth2_token_url.to_string())
                        .header("Content-Type", "application/x-www-form-urlencoded");
                    let mut enc = ::url::form_urlencoded::Serializer::new("".to_owned());

                    enc.append_pair("grant_type", "refresh_token");
                    enc.append_pair("service", registry_name.as_str());
                    enc.append_pair("scope", format!("repository:{}:pull", name).as_str());
                    enc.append_pair("refresh_token", token.as_str());
                    let full_body = Full::new(Bytes::from(enc.finish()));
                    //let full_body = Full::new(Bytes::from(format!("grant_type=refresh_token&service={}&scope=repository:{}:pull&refresh_token={}", registry_name, name, token)));

                    let req = req_builder.body(full_body)?;
                    let res = self
                        .hyper_client
                        .request(req)
                        .await
                        .with_context(|| "Could not fetch from the npm registry")?;

                    if res.status().as_u16() >= 400 {
                        anyhow::bail!("Something went wrong while getting docker registry token");
                    }

                    let body = res
                        .into_body()
                        .collect()
                        .await
                        .with_context(|| "Could not get body from the npm registry")?
                        .to_bytes();

                    let docker_token: DockerTokenResp =
                        serde_json::from_str(String::from_utf8_lossy(&body).as_ref())?;
                    RegistryAuth::Basic(username.clone(), docker_token.access_token)
                }
            },
        };
        println!("Got auth: {:?}", auth);

        match self
            .docker_client
            .fetch_manifest_digest(&image, &auth)
            .await
        {
            Ok(_) => Ok(true),
            Err(e) => match e {
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
                _ => {
                    anyhow::bail!("could not access docker registry: {}", e)
                }
            },
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct PackageMetadataFslabsCiPublishDocker {
    pub publish: bool,
    pub repository: Option<String>,
    pub error: Option<String>,
}

impl PackageMetadataFslabsCiPublishDocker {
    pub async fn check(
        &mut self,
        package: String,
        version: String,
        docker: &mut Docker,
    ) -> anyhow::Result<()> {
        if !self.publish {
            return Ok(());
        }
        let docker_registry = match self.repository.clone() {
            Some(r) => r,
            None => anyhow::bail!("Tried to check docker image without setting the registry"),
        };
        self.publish = !docker
            .check_image_exists(docker_registry, package, version)
            .await?;
        Ok(())
    }
}

// mod tests {
//     use std::env;
//     use std::fs::File;
//     use std::io::Write;
//
//     use assert_fs::TempDir;
//     use indoc::formatdoc;
//     use serial_test::serial;
//     use testcontainers::core::WaitFor;
//     use testcontainers::{clients, Container, GenericImage};
//
//     use crate::commands::check_workspace::docker::PackageMetadataFslabsCiPublishDocker;
//
//     use super::*;
//
//     const DOCKER_HTPASSWD: &str =
//         "testuser:$2y$05$8/q2bfRcX74EuxGf0qOcSuhWDQJXrgWiy6Fi73/JM2tKC66qSrLve";
//     const DOCKER_HTPASSWD_USERNAME: &str = "testuser";
//     const DOCKER_HTPASSWD_PASSWORD: &str = "testpassword";
//     const DOCKER_HTPASSWD_AUTH: &str = "dGVzdHVzZXI6dGVzdHBhc3N3b3Jk";
//
//     async fn docker_test(
//         docker_image: String,
//         docker_tag: String,
//         auth_file: bool,
//         docker_username: Option<String>,
//         docker_password: Option<String>,
//         mock: bool,
//         expected_result: bool,
//         expected_error: bool,
//     ) {
//         let registry_tmp_dir = TempDir::new().expect("cannot create tmp directory");
//         let docker_tmp_dir = TempDir::new().expect("cannot create tmp directory");
//         let mut registry: Option<String> = None;
//         let mut protocol: Option<ClientProtocol> = None;
//         let mut username = docker_username.clone();
//         let mut password = docker_password.clone();
//         // Main scope
//         let docker = clients::Cli::default();
//         let registry_container: Container<GenericImage>;
//         let init_docker_config = env::var("DOCKER_CONFIG");
//         if mock {
//             let htpasswd_path = registry_tmp_dir.path().join("htpasswd");
//             let mut f = File::create(htpasswd_path).expect("Could not create htpasswd file");
//             f.write_all(DOCKER_HTPASSWD.as_bytes())
//                 .expect("Could not write htpasswd file");
//             let registry_image = GenericImage::new("docker.io/library/registry", "2")
//                 .with_env_var("REGISTRY_AUTH", "htpasswd")
//                 .with_env_var("REGISTRY_AUTH_HTPASSWD_REALM", "Registry Realm")
//                 .with_env_var("REGISTRY_AUTH_HTPASSWD_PATH", "/auth/htpasswd")
//                 .with_env_var("REGISTRY_PROXY_REMOTEURL", "https://registry-1.docker.io")
//                 .with_exposed_port(5000)
//                 .with_volume(
//                     registry_tmp_dir
//                         .path()
//                         .to_str()
//                         .expect("cannot convert auth_dir to string"),
//                     "/auth",
//                 )
//                 .with_wait_for(WaitFor::message_on_stderr("listening on "));
//
//             registry_container = docker.run(registry_image);
//
//             let port = registry_container.get_host_port_ipv4(5000);
//             protocol = Some(ClientProtocol::HttpsExcept(vec![format!(
//                 "127.0.0.1:{}",
//                 port
//             )]));
//             registry = Some(format!("127.0.0.1:{}", port));
//             if auth_file {
//                 let config_path = docker_tmp_dir.path().join("config.json");
//                 let mut f =
//                     File::create(config_path.clone()).expect("Could not create docker config file");
//                 let docker_config = formatdoc!(
//                     r#"
//                 {{
//                     "auths": {{
//                         "{registry}": {{
//                             "auth": "{auth}"
//                         }}
//                     }}
//                 }}"#,
//                     registry = format!("127.0.0.1:{}", port),
//                     auth = DOCKER_HTPASSWD_AUTH
//                 );
//                 f.write_all(docker_config.as_bytes())
//                     .expect("Could not write to docker config file");
//                 env::set_var("DOCKER_CONFIG", docker_tmp_dir.path());
//                 username = None;
//                 password = None;
//             }
//         }
//         let mut publish = PackageMetadataFslabsCiPublishDocker {
//             publish: true,
//             ..Default::default()
//         };
//         let error = publish
//             .check(
//                 docker_image,
//                 docker_tag,
//                 registry,
//                 username,
//                 password,
//                 protocol,
//             )
//             .await;
//         match init_docker_config {
//             Ok(c) => env::set_var("DOCKER_CONFIG", c),
//             Err(_) => env::set_var("DOCKER_CONFIG", ""),
//         }
//         assert_eq!(error.is_err(), expected_error);
//         assert_eq!(publish.publish, expected_result);
//     }
//
//     #[tokio::test]
//     #[serial(docker)]
//     async fn docker_existing_image() {
//         docker_test(
//             "alpine".to_string(),
//             "latest".to_string(),
//             false,
//             None,
//             None,
//             false,
//             true,
//             false,
//         )
//             .await;
//     }
//
//     #[tokio::test]
//     #[serial(docker)]
//     async fn docker_existing_image_non_existing_version() {
//         docker_test(
//             "alpine".to_string(),
//             "NONEXISTENTTAG".to_string(),
//             false,
//             None,
//             None,
//             false,
//             false,
//             false,
//         )
//             .await;
//     }
//
//     #[tokio::test]
//     #[serial(docker)]
//     async fn docker_private_reg_existing_image() {
//         docker_test(
//             "library/alpine".to_string(),
//             "latest".to_string(),
//             false,
//             Some(DOCKER_HTPASSWD_USERNAME.to_string()),
//             Some(DOCKER_HTPASSWD_PASSWORD.to_string()),
//             true,
//             true,
//             false,
//         )
//             .await;
//     }
//
//     #[tokio::test]
//     #[serial(docker)]
//     async fn docker_private_reg_existing_image_non_existing_version() {
//         docker_test(
//             "library/alpine".to_string(),
//             "NONEXISTANT".to_string(),
//             false,
//             Some(DOCKER_HTPASSWD_USERNAME.to_string()),
//             Some(DOCKER_HTPASSWD_PASSWORD.to_string()),
//             true,
//             false,
//             false,
//         )
//             .await;
//     }
//
//     #[tokio::test]
//     #[serial(docker)]
//     async fn docker_private_reg_auth_file_existing_image() {
//         docker_test(
//             "library/alpine".to_string(),
//             "latest".to_string(),
//             true,
//             Some(DOCKER_HTPASSWD_USERNAME.to_string()),
//             Some(DOCKER_HTPASSWD_PASSWORD.to_string()),
//             true,
//             true,
//             false,
//         )
//             .await;
//     }
//
//     #[tokio::test]
//     #[serial(docker)]
//     async fn docker_private_reg_auth_file_existing_image_non_existing_version() {
//         docker_test(
//             "library/alpine".to_string(),
//             "NONEXISTANT".to_string(),
//             true,
//             Some(DOCKER_HTPASSWD_USERNAME.to_string()),
//             Some(DOCKER_HTPASSWD_PASSWORD.to_string()),
//             true,
//             false,
//             false,
//         )
//             .await;
//     }
// }

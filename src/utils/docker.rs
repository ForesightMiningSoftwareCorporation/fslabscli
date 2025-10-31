use std::collections::HashMap;
use std::env;
use std::fs::File;
use std::path::Path;

use anyhow::Context;
use base64::Engine;
use base64::prelude::BASE64_STANDARD;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Method, Request};
use hyper_rustls::{ConfigBuilderExt, HttpsConnector};
use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use oci_distribution::client::{Client as DockerClient, ClientConfig, ClientProtocol};
use oci_distribution::errors::OciErrorCode;
use oci_distribution::{Reference, errors::OciDistributionError, secrets::RegistryAuth};
use serde::{Deserialize, Serialize};
use std::io::BufReader;

#[cfg_attr(test, mockall::automock)]
pub trait OciClient {
    async fn fetch_manifest_digest(
        &self,
        image: &Reference,
        auth: &RegistryAuth,
    ) -> Result<String, OciDistributionError>;
}

#[cfg_attr(test, mockall::automock)]
pub trait HttpClient {
    async fn post_form(&self, url: String, body: String) -> Result<Vec<u8>, anyhow::Error>;
}

pub struct RealOciClient {
    client: DockerClient,
}

pub struct RealHttpClient {
    client: HyperClient<HttpsConnector<HttpConnector>, Full<Bytes>>,
}

impl RealOciClient {
    pub fn new() -> Self {
        Self {
            client: DockerClient::new(ClientConfig {
                protocol: ClientProtocol::Https,
                ..Default::default()
            }),
        }
    }
}

impl RealHttpClient {
    pub fn new() -> anyhow::Result<Self> {
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
            client: HyperClient::builder(TokioExecutor::new()).build(https),
        })
    }
}

impl OciClient for RealOciClient {
    async fn fetch_manifest_digest(
        &self,
        image: &Reference,
        auth: &RegistryAuth,
    ) -> Result<String, OciDistributionError> {
        self.client.fetch_manifest_digest(image, auth).await
    }
}

impl HttpClient for RealHttpClient {
    async fn post_form(&self, url: String, body: String) -> Result<Vec<u8>, anyhow::Error> {
        let req = Request::builder()
            .method(Method::POST)
            .uri(url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(Full::new(Bytes::from(body)))?;

        let res = self.client.request(req).await?;
        let status = res.status();

        let body_bytes = res.into_body().collect().await?.to_bytes().to_vec();

        if status.as_u16() >= 400 {
            anyhow::bail!(
                "OAuth2 token exchange failed with status {}: {}",
                status,
                String::from_utf8_lossy(&body_bytes)
            );
        }

        Ok(body_bytes)
    }
}

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

#[derive(Debug, PartialEq)]
enum DockerCredential {
    IdentityToken(String, String),
    UsernamePassword(String, String),
}

pub struct Docker<O: OciClient, H: HttpClient> {
    oci_client: O,
    http_client: H,
    registries_auths: HashMap<String, DockerCredential>,
}

impl Docker<RealOciClient, RealHttpClient> {
    /// Create a new Docker client with production dependencies
    pub fn new(config_path: Option<String>) -> anyhow::Result<Self> {
        Self::new_with_clients(config_path, RealOciClient::new(), RealHttpClient::new()?)
    }
}

impl<O: OciClient, H: HttpClient> Docker<O, H> {
    /// Create a Docker client with custom dependencies (for testing)
    fn new_with_clients(
        config_path: Option<String>,
        oci_client: O,
        http_client: H,
    ) -> anyhow::Result<Self> {
        let registries_auths = Self::load_credentials(config_path)?;

        Ok(Self {
            oci_client,
            http_client,
            registries_auths,
        })
    }

    /// Load Docker credentials from config file
    fn load_credentials(
        config_path: Option<String>,
    ) -> anyhow::Result<HashMap<String, DockerCredential>> {
        let mut registries_auths = HashMap::new();

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

        tracing::debug!("Loading Docker credentials from: {:?}", config_path);

        if let Some(p) = config_path
            && let Ok(file) = File::open(&p)
        {
            let reader = BufReader::new(file);
            if let Ok(config) = serde_json::from_reader::<BufReader<File>, DockerConfig>(reader)
                && let Some(registries_auth) = config.auths
            {
                tracing::debug!("Found {} registry configurations", registries_auth.len());

                for (k, v) in registries_auth {
                    let credential = Self::parse_credential(&k, v)?;
                    if let Some(cred) = credential {
                        tracing::debug!("Loaded credential for registry: {}", k);
                        registries_auths.insert(k, cred);
                    }
                }
            }
        } else {
            tracing::debug!("No Docker config file found or failed to open");
        }

        Ok(registries_auths)
    }

    /// Parse a single credential from Docker config
    fn parse_credential(
        registry: &str,
        config: DockerAuthConfig,
    ) -> anyhow::Result<Option<DockerCredential>> {
        let mut username: Option<String> = None;
        let mut password: Option<String> = None;

        // Decode base64 auth field if present
        if let Some(encoded_auth) = config.auth
            && let Ok(decoded) = BASE64_STANDARD.decode(&encoded_auth)
            && let Ok(auth) = std::str::from_utf8(&decoded)
        {
            let parts: Vec<&str> = auth.splitn(2, ':').collect();
            if let Some(u) = parts.first() {
                username = Some(String::from(*u));
            }
            if let Some(p) = parts.get(1) {
                password = Some(String::from(*p));
            }
        }

        // Prefer identity token if available
        if let (Some(username), Some(identity_token)) = (username.clone(), config.identitytoken) {
            tracing::debug!("Registry {} using identity token auth", registry);
            return Ok(Some(DockerCredential::IdentityToken(
                username,
                identity_token,
            )));
        }

        // Fall back to username/password
        if let (Some(username), Some(password)) = (username, password) {
            tracing::debug!("Registry {} using username/password auth", registry);
            return Ok(Some(DockerCredential::UsernamePassword(username, password)));
        }

        tracing::debug!("Registry {} has no valid credentials", registry);
        Ok(None)
    }

    /// Add or update registry authentication
    pub fn add_registry_auth(&mut self, name: String, username: String, password: String) {
        tracing::debug!("Adding registry auth for: {}", name);
        self.registries_auths
            .insert(name, DockerCredential::UsernamePassword(username, password));
    }

    /// Check if an image exists in a registry
    pub async fn check_image_exists(
        &self,
        registry_name: String,
        name: String,
        version: String,
    ) -> anyhow::Result<bool> {
        tracing::info!(
            registry = %registry_name,
            image = %name,
            tag = %version,
            "Checking if image exists"
        );

        // Construct image reference
        let image_ref = format!("{}/{}:{}", registry_name, name, version);
        let image: Reference = image_ref
            .parse()
            .with_context(|| format!("Failed to parse image reference: {}", image_ref))?;

        tracing::debug!("Parsed image reference: {:?}", image);

        // Resolve authentication
        let auth = self.resolve_auth(image.registry(), &name).await?;
        tracing::debug!("Resolved auth type: {}", auth_type_name(&auth));

        // Fetch manifest
        match self.oci_client.fetch_manifest_digest(&image, &auth).await {
            Ok(digest) => {
                tracing::info!(
                    registry = %registry_name,
                    image = %name,
                    tag = %version,
                    digest = %digest,
                    "Image exists"
                );
                Ok(true)
            }
            Err(e) => {
                tracing::debug!("Manifest fetch error: {:?}", e);
                self.handle_fetch_error(e, &registry_name, &name, &version)
            }
        }
    }

    /// Resolve authentication for a registry
    async fn resolve_auth(
        &self,
        registry_name: &str,
        image_name: &str,
    ) -> anyhow::Result<RegistryAuth> {
        match self.registries_auths.get(registry_name) {
            None => {
                tracing::debug!(
                    "No credentials found for {}, using anonymous auth",
                    registry_name
                );
                Ok(RegistryAuth::Anonymous)
            }
            Some(DockerCredential::UsernamePassword(username, password)) => {
                tracing::debug!("Using basic auth for {}", registry_name);
                Ok(RegistryAuth::Basic(username.clone(), password.clone()))
            }
            Some(DockerCredential::IdentityToken(username, refresh_token)) => {
                tracing::debug!("Exchanging identity token for access token");
                let access_token = self
                    .exchange_identity_token(registry_name, image_name, refresh_token)
                    .await?;
                Ok(RegistryAuth::Basic(username.clone(), access_token))
            }
        }
    }
    /// Exchange identity token for access token via OAuth2
    async fn exchange_identity_token(
        &self,
        registry_name: &str,
        image_name: &str,
        refresh_token: &str,
    ) -> anyhow::Result<String> {
        let oauth2_token_url = format!("https://{}/oauth2/token", registry_name);
        tracing::debug!("OAuth2 token exchange URL: {}", oauth2_token_url);

        let mut enc = url::form_urlencoded::Serializer::new(String::new());
        enc.append_pair("grant_type", "refresh_token");
        enc.append_pair("service", registry_name);
        enc.append_pair("scope", &format!("repository:{}:pull", image_name));
        enc.append_pair("refresh_token", refresh_token);

        let form_body = enc.finish();
        tracing::debug!("OAuth2 request scope: repository:{}:pull", image_name);

        let response_bytes = self
            .http_client
            .post_form(oauth2_token_url, form_body)
            .await
            .context("Failed to exchange identity token")?;

        let token_response: DockerTokenResp = serde_json::from_slice(&response_bytes)
            .context("Failed to parse OAuth2 token response")?;

        tracing::debug!("Successfully exchanged identity token for access token");
        Ok(token_response.access_token)
    }

    /// Handle errors from manifest fetch
    fn handle_fetch_error(
        &self,
        error: OciDistributionError,
        registry_name: &str,
        image_name: &str,
        version: &str,
    ) -> anyhow::Result<bool> {
        match error {
            OciDistributionError::RegistryError { envelope, .. } => {
                tracing::debug!(
                    "Registry returned error envelope with {} errors",
                    envelope.errors.len()
                );

                // Check if ANY error is ManifestUnknown
                let has_manifest_unknown = envelope.errors.iter().any(|e| {
                    tracing::debug!("Registry error code: {:?}, message: {}", e.code, e.message);
                    e.code == OciErrorCode::ManifestUnknown
                });

                if has_manifest_unknown {
                    tracing::info!(
                        registry = %registry_name,
                        image = %image_name,
                        tag = %version,
                        "Image does not exist (ManifestUnknown)"
                    );
                    return Ok(false);
                }

                // Log all error codes for debugging
                let error_codes: Vec<String> = envelope
                    .errors
                    .iter()
                    .map(|e| format!("{:?}: {}", e.code, e.message))
                    .collect();

                tracing::error!(
                    registry = %registry_name,
                    image = %image_name,
                    tag = %version,
                    errors = ?error_codes,
                    "Registry returned non-manifest errors"
                );

                anyhow::bail!(
                    "Registry error for {}/{}:{}: {}",
                    registry_name,
                    image_name,
                    version,
                    error_codes.join("; ")
                )
            }
            OciDistributionError::AuthenticationFailure(msg) => {
                tracing::error!(
                    registry = %registry_name,
                    image = %image_name,
                    tag = %version,
                    error = %msg,
                    "Authentication failed"
                );
                anyhow::bail!(
                    "Failed to authenticate to registry {}: {}",
                    registry_name,
                    msg
                )
            }
            other => {
                tracing::error!(
                    registry = %registry_name,
                    image = %image_name,
                    tag = %version,
                    error = ?other,
                    "Unexpected error accessing registry"
                );
                anyhow::bail!(
                    "Could not access registry {} for image {}: {}",
                    registry_name,
                    image_name,
                    other
                )
            }
        }
    }
}

// Helper function for logging
fn auth_type_name(auth: &RegistryAuth) -> &'static str {
    match auth {
        RegistryAuth::Anonymous => "Anonymous",
        RegistryAuth::Basic(_, _) => "Basic",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockall::predicate::*;

    // Test helper to create Docker with mocked dependencies
    fn create_test_docker(
        credentials: HashMap<String, DockerCredential>,
        mock_oci: MockOciClient,
        mock_http: MockHttpClient,
    ) -> anyhow::Result<Docker<MockOciClient, MockHttpClient>> {
        let mut docker = Docker::new_with_clients(None, mock_oci, mock_http)?;
        docker.registries_auths = credentials;
        Ok(docker)
    }

    mod authentication {
        use super::*;

        #[tokio::test]
        async fn test_anonymous_auth_when_no_credentials() {
            let mut mock_oci = MockOciClient::new();
            mock_oci
                .expect_fetch_manifest_digest()
                .with(
                    always(),
                    function(|auth: &RegistryAuth| matches!(auth, RegistryAuth::Anonymous)),
                )
                .times(1)
                .returning(|_, _| Ok("sha256:abc123".to_string()));

            let mock_http = MockHttpClient::new();

            let docker = create_test_docker(HashMap::new(), mock_oci, mock_http).unwrap();

            let result = docker
                .check_image_exists(
                    "registry.example.com".to_string(),
                    "myapp".to_string(),
                    "v1.0.0".to_string(),
                )
                .await;

            assert!(result.is_ok());
            assert!(result.unwrap());
        }

        #[tokio::test]
        async fn test_basic_auth_with_username_password() {
            let mut credentials = HashMap::new();
            credentials.insert(
                "registry.example.com".to_string(),
                DockerCredential::UsernamePassword("user".to_string(), "pass".to_string()),
            );

            let mut mock_oci = MockOciClient::new();
            mock_oci
                .expect_fetch_manifest_digest()
                .with(
                    always(),
                    function(|auth: &RegistryAuth|
                       matches!(auth, RegistryAuth::Basic(u, p) if u == "user" && p == "pass")
                    ),
                )
                .times(1)
                .returning(|_, _| Ok("sha256:def456".to_string()));

            let mock_http = MockHttpClient::new();
            let docker = create_test_docker(credentials, mock_oci, mock_http).unwrap();

            let result = docker
                .check_image_exists(
                    "registry.example.com".to_string(),
                    "myapp".to_string(),
                    "v1.0.0".to_string(),
                )
                .await;

            assert!(result.is_ok());
            assert!(result.unwrap());
        }

        #[tokio::test]
        async fn test_identity_token_triggers_oauth2_exchange() {
            let mut credentials = HashMap::new();
            credentials.insert(
                "registry.example.com".to_string(),
                DockerCredential::IdentityToken(
                    "user".to_string(),
                    "refresh_token_123".to_string(),
                ),
            );

            let mut mock_http = MockHttpClient::new();
            mock_http
                .expect_post_form()
                .withf(|url, body| {
                    url.contains("registry.example.com/oauth2/token")
                        && body.contains("refresh_token=refresh_token_123")
                        && body.contains("grant_type=refresh_token")
                        && body.contains("scope=repository")
                })
                .times(1)
                .returning(|_, _| {
                    let response = DockerTokenResp {
                        access_token: "access_token_456".to_string(),
                    };
                    Ok(serde_json::to_vec(&response).unwrap())
                });

            let mut mock_oci = MockOciClient::new();
            mock_oci
                .expect_fetch_manifest_digest()
                .with(
                    always(),
                    function(|auth: &RegistryAuth|
                       matches!(auth, RegistryAuth::Basic(u, p) if u == "user" && p == "access_token_456")
                    ),
                )
                .times(1)
                .returning(|_, _| Ok("sha256:ghi789".to_string()));

            let docker = create_test_docker(credentials, mock_oci, mock_http).unwrap();

            let result = docker
                .check_image_exists(
                    "registry.example.com".to_string(),
                    "myapp".to_string(),
                    "v1.0.0".to_string(),
                )
                .await;

            assert!(result.is_ok());
            assert!(result.unwrap());
        }

        #[tokio::test]
        async fn test_oauth2_exchange_failure_propagates_error() {
            let mut credentials = HashMap::new();
            credentials.insert(
                "registry.example.com".to_string(),
                DockerCredential::IdentityToken("user".to_string(), "invalid_token".to_string()),
            );

            let mut mock_http = MockHttpClient::new();
            mock_http.expect_post_form().times(1).returning(|_, _| {
                Err(anyhow::anyhow!(
                    "OAuth2 token exchange failed with status 401"
                ))
            });

            let mock_oci = MockOciClient::new();
            let docker = create_test_docker(credentials, mock_oci, mock_http).unwrap();

            let result = docker
                .check_image_exists(
                    "registry.example.com".to_string(),
                    "myapp".to_string(),
                    "v1.0.0".to_string(),
                )
                .await;

            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("identity token"));
        }
        #[tokio::test]
        async fn test_oauth2_invalid_json_response() {
            let mut credentials = HashMap::new();
            credentials.insert(
                "registry.example.com".to_string(),
                DockerCredential::IdentityToken("user".to_string(), "token".to_string()),
            );

            let mut mock_http = MockHttpClient::new();
            mock_http
                .expect_post_form()
                .times(1)
                .returning(|_, _| Ok(b"not json".to_vec()));

            let mock_oci = MockOciClient::new();
            let docker = create_test_docker(credentials, mock_oci, mock_http).unwrap();

            let result = docker
                .check_image_exists(
                    "registry.example.com".to_string(),
                    "myapp".to_string(),
                    "v1.0.0".to_string(),
                )
                .await;

            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("parse"));
        }
    }

    mod error_handling {
        use super::*;
        use oci_distribution::errors::{OciEnvelope, OciError};

        #[tokio::test]
        async fn test_manifest_unknown_returns_false() {
            let mut mock_oci = MockOciClient::new();
            mock_oci
                .expect_fetch_manifest_digest()
                .times(1)
                .returning(|_, _| {
                    Err(OciDistributionError::RegistryError {
                        envelope: OciEnvelope {
                            errors: vec![OciError {
                                code: OciErrorCode::ManifestUnknown,
                                message: "manifest unknown".to_string(),
                                detail: serde_json::Value::Null,
                            }],
                        },
                        url: "http://example.com".to_string(),
                    })
                });

            let mock_http = MockHttpClient::new();
            let docker = create_test_docker(HashMap::new(), mock_oci, mock_http).unwrap();

            let result = docker
                .check_image_exists(
                    "registry.example.com".to_string(),
                    "myapp".to_string(),
                    "v1.0.0".to_string(),
                )
                .await;

            assert!(result.is_ok());
            assert!(!result.unwrap());
        }

        #[tokio::test]
        async fn test_manifest_unknown_among_multiple_errors() {
            let mut mock_oci = MockOciClient::new();
            mock_oci
                .expect_fetch_manifest_digest()
                .times(1)
                .returning(|_, _| {
                    Err(OciDistributionError::RegistryError {
                        envelope: OciEnvelope {
                            errors: vec![
                                OciError {
                                    code: OciErrorCode::Unauthorized,
                                    message: "auth required".to_string(),
                                    detail: serde_json::Value::Null,
                                },
                                OciError {
                                    code: OciErrorCode::ManifestUnknown,
                                    message: "manifest not found".to_string(),
                                    detail: serde_json::Value::Null,
                                },
                            ],
                        },
                        url: "http://example.com".to_string(),
                    })
                });

            let mock_http = MockHttpClient::new();
            let docker = create_test_docker(HashMap::new(), mock_oci, mock_http).unwrap();

            let result = docker
                .check_image_exists(
                    "registry.example.com".to_string(),
                    "myapp".to_string(),
                    "v1.0.0".to_string(),
                )
                .await;

            // Even with other errors, if ManifestUnknown is present, it means image doesn't exist
            assert!(result.is_ok());
            assert!(!result.unwrap());
        }

        #[tokio::test]
        async fn test_unauthorized_error_returns_error() {
            let mut mock_oci = MockOciClient::new();
            mock_oci
                .expect_fetch_manifest_digest()
                .times(1)
                .returning(|_, _| {
                    Err(OciDistributionError::RegistryError {
                        envelope: OciEnvelope {
                            errors: vec![OciError {
                                code: OciErrorCode::Unauthorized,
                                message: "authentication required".to_string(),
                                detail: serde_json::Value::Null,
                            }],
                        },
                        url: "http://example.com".to_string(),
                    })
                });

            let mock_http = MockHttpClient::new();
            let docker = create_test_docker(HashMap::new(), mock_oci, mock_http).unwrap();

            let result = docker
                .check_image_exists(
                    "registry.example.com".to_string(),
                    "myapp".to_string(),
                    "v1.0.0".to_string(),
                )
                .await;

            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("Registry error"));
        }

        #[tokio::test]
        async fn test_authentication_failure_returns_error() {
            let mut mock_oci = MockOciClient::new();
            mock_oci
                .expect_fetch_manifest_digest()
                .times(1)
                .returning(|_, _| {
                    Err(OciDistributionError::AuthenticationFailure(
                        "invalid credentials".to_string(),
                    ))
                });

            let mock_http = MockHttpClient::new();
            let docker = create_test_docker(HashMap::new(), mock_oci, mock_http).unwrap();

            let result = docker
                .check_image_exists(
                    "registry.example.com".to_string(),
                    "myapp".to_string(),
                    "v1.0.0".to_string(),
                )
                .await;

            assert!(result.is_err());
            let err_msg = result.unwrap_err().to_string();
            assert!(err_msg.contains("authenticate"));
            assert!(err_msg.contains("invalid credentials"));
        }
        #[tokio::test]
        async fn test_generic_oci_error_returns_error() {
            let mut mock_oci = MockOciClient::new();
            mock_oci
                .expect_fetch_manifest_digest()
                .times(1)
                .returning(|_, _| {
                    Err(OciDistributionError::GenericError(Some(
                        "network timeout".to_string(),
                    )))
                });

            let mock_http = MockHttpClient::new();
            let docker = create_test_docker(HashMap::new(), mock_oci, mock_http).unwrap();

            let result = docker
                .check_image_exists(
                    "registry.example.com".to_string(),
                    "myapp".to_string(),
                    "v1.0.0".to_string(),
                )
                .await;

            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("Could not access"));
        }
    }

    mod image_reference {
        use super::*;

        #[tokio::test]
        async fn test_valid_image_reference_construction() {
            let mut mock_oci = MockOciClient::new();
            mock_oci
                .expect_fetch_manifest_digest()
                .withf(|image, _| image.to_string() == "registry.example.com/myapp:v1.0.0")
                .times(1)
                .returning(|_, _| Ok("sha256:abc".to_string()));

            let mock_http = MockHttpClient::new();
            let docker = create_test_docker(HashMap::new(), mock_oci, mock_http).unwrap();

            let result = docker
                .check_image_exists(
                    "registry.example.com".to_string(),
                    "myapp".to_string(),
                    "v1.0.0".to_string(),
                )
                .await;

            assert!(result.is_ok());
        }

        #[tokio::test]
        async fn test_image_reference_with_nested_path() {
            let mut mock_oci = MockOciClient::new();
            mock_oci
                .expect_fetch_manifest_digest()
                .withf(|image, _| {
                    image.to_string() == "registry.example.com/org/project/myapp:latest"
                })
                .times(1)
                .returning(|_, _| Ok("sha256:def".to_string()));

            let mock_http = MockHttpClient::new();
            let docker = create_test_docker(HashMap::new(), mock_oci, mock_http).unwrap();

            let result = docker
                .check_image_exists(
                    "registry.example.com".to_string(),
                    "org/project/myapp".to_string(),
                    "latest".to_string(),
                )
                .await;

            assert!(result.is_ok());
        }
    }
    mod registry_management {
        use super::*;

        #[tokio::test]
        async fn test_add_registry_auth_overrides_existing() {
            let mut credentials = HashMap::new();
            credentials.insert(
                "registry.example.com".to_string(),
                DockerCredential::UsernamePassword("old_user".to_string(), "old_pass".to_string()),
            );

            let mut mock_oci = MockOciClient::new();
            mock_oci
                .expect_fetch_manifest_digest()
                .with(
                    always(),
                    function(|auth: &RegistryAuth| {
                        matches!(auth, RegistryAuth::Basic(u, p) if u == "new_user" && p == "new_pass")
                    }),
                )
                .times(1)
                .returning(|_, _| Ok("sha256:xyz".to_string()));

            let mock_http = MockHttpClient::new();
            let mut docker = create_test_docker(credentials, mock_oci, mock_http).unwrap();

            docker.add_registry_auth(
                "registry.example.com".to_string(),
                "new_user".to_string(),
                "new_pass".to_string(),
            );

            let result = docker
                .check_image_exists(
                    "registry.example.com".to_string(),
                    "myapp".to_string(),
                    "v1.0.0".to_string(),
                )
                .await;

            assert!(result.is_ok());
        }

        #[tokio::test]
        async fn test_add_registry_auth_for_new_registry() {
            let mock_http = MockHttpClient::new();
            let mut mock_oci = MockOciClient::new();
            mock_oci
                .expect_fetch_manifest_digest()
                .with(
                    always(),
                    function(|auth: &RegistryAuth| {
                        matches!(auth, RegistryAuth::Basic(u, p) if u == "user" && p == "pass")
                    }),
                )
                .times(1)
                .returning(|_, _| Ok("sha256:123".to_string()));

            let mut docker = create_test_docker(HashMap::new(), mock_oci, mock_http).unwrap();

            docker.add_registry_auth(
                "new-registry.com".to_string(),
                "user".to_string(),
                "pass".to_string(),
            );

            let result = docker
                .check_image_exists(
                    "new-registry.com".to_string(),
                    "myapp".to_string(),
                    "v1.0.0".to_string(),
                )
                .await;

            assert!(result.is_ok());
        }
    }
    mod credential_parsing {
        use super::*;

        #[test]
        fn test_parse_basic_auth_credential() {
            let config = DockerAuthConfig {
                auth: Some(BASE64_STANDARD.encode("username:password")),
                identitytoken: None,
            };

            let result =
                Docker::<RealOciClient, RealHttpClient>::parse_credential("test.registry", config)
                    .unwrap();

            assert_eq!(
                result,
                Some(DockerCredential::UsernamePassword(
                    "username".to_string(),
                    "password".to_string()
                ))
            );
        }

        #[test]
        fn test_parse_identity_token_credential() {
            let config = DockerAuthConfig {
                auth: Some(BASE64_STANDARD.encode("username:")),
                identitytoken: Some("token123".to_string()),
            };

            let result =
                Docker::<RealOciClient, RealHttpClient>::parse_credential("test.registry", config)
                    .unwrap();

            assert_eq!(
                result,
                Some(DockerCredential::IdentityToken(
                    "username".to_string(),
                    "token123".to_string()
                ))
            );
        }

        #[test]
        fn test_parse_auth_with_colon_in_password() {
            let config = DockerAuthConfig {
                auth: Some(BASE64_STANDARD.encode("user:pass:with:colons")),
                identitytoken: None,
            };

            let result =
                Docker::<RealOciClient, RealHttpClient>::parse_credential("test.registry", config)
                    .unwrap();

            assert_eq!(
                result,
                Some(DockerCredential::UsernamePassword(
                    "user".to_string(),
                    "pass:with:colons".to_string()
                ))
            );
        }

        #[test]
        fn test_parse_empty_auth_returns_none() {
            let config = DockerAuthConfig {
                auth: None,
                identitytoken: None,
            };

            let result =
                Docker::<RealOciClient, RealHttpClient>::parse_credential("test.registry", config)
                    .unwrap();

            assert_eq!(result, None);
        }

        #[test]
        fn test_parse_invalid_base64_returns_none() {
            let config = DockerAuthConfig {
                auth: Some("not-valid-base64!!!".to_string()),
                identitytoken: None,
            };

            let result =
                Docker::<RealOciClient, RealHttpClient>::parse_credential("test.registry", config)
                    .unwrap();

            assert_eq!(result, None);
        }
    }
}

use docker_credential::{CredentialRetrievalError, DockerCredential};
use oci_distribution::{Client, Reference};
use oci_distribution::client::{ClientConfig, ClientProtocol};
use oci_distribution::errors::{OciDistributionError, OciErrorCode};
use oci_distribution::secrets::RegistryAuth;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct PackageMetadataFslabsCiPublishDocker {
    pub publish: bool,
    pub repository: Option<String>,
}

impl PackageMetadataFslabsCiPublishDocker {
    pub async fn check(
        &mut self,
        name: String, version: String,
        docker_registry: Option<String>,
        docker_registry_username: Option<String>,
        docker_registry_password: Option<String>,
        docker_registry_protocol: Option<ClientProtocol>,
    ) -> anyhow::Result<()> {
        if !self.publish {
            return Ok(());
        }
        log::debug!("Docker: checking if version {} of {} already exists", version, name);
        let docker_registry = match docker_registry.clone() {
            Some(r) => r,
            None => match self.repository.clone() {
                Some(r) => r,
                None => anyhow::bail!("Tried to check docker image without setting the registry"),
            }
        };
        let image: Reference = format!("{}/{}:{}", docker_registry, name.clone(), version.clone()).parse()?;
        let protocol = docker_registry_protocol.unwrap_or(ClientProtocol::Https);
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
        match docker_client.fetch_manifest_digest(
            &image,
            &auth,
        ).await {
            Ok(_) => {
                self.publish = false;
                Ok(())
            }
            Err(e) => {
                match e {
                    OciDistributionError::RegistryError { envelope, .. } => {
                        for error in envelope.errors {
                            if error.code == OciErrorCode::ManifestUnknown {
                                self.publish = true;
                                return Ok(());
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
}


#[cfg(test)]
mod tests {
    use std::env;
    use std::fs::File;
    use std::io::Write;

    use assert_fs::TempDir;
    use indoc::formatdoc;
    use serial_test::serial;
    use testcontainers::{clients, Container, GenericImage};
    use testcontainers::core::WaitFor;

    use crate::commands::publishable::docker::PackageMetadataFslabsCiPublishDocker;

    use super::*;

    const DOCKER_HTPASSWD: &str = "testuser:$2y$05$8/q2bfRcX74EuxGf0qOcSuhWDQJXrgWiy6Fi73/JM2tKC66qSrLve";
    const DOCKER_HTPASSWD_USERNAME: &str = "testuser";
    const DOCKER_HTPASSWD_PASSWORD: &str = "testpassword";
    const DOCKER_HTPASSWD_AUTH: &str = "dGVzdHVzZXI6dGVzdHBhc3N3b3Jk";

    async fn docker_test(docker_image: String, docker_tag: String, auth_file: bool, docker_username: Option<String>, docker_password: Option<String>, mock: bool, expected_result: bool, expected_error: bool) {
        let registry_tmp_dir = TempDir::new().expect("cannot create tmp directory");
        let docker_tmp_dir = TempDir::new().expect("cannot create tmp directory");
        let mut registry: Option<String> = None;
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
            registry = Some(format!("127.0.0.1:{}", port));
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
        let mut publish = PackageMetadataFslabsCiPublishDocker {
            publish: true,
            ..Default::default()
        };
        let error = publish.check(
            docker_image,
            docker_tag,
            registry,
            username,
            password,
            protocol,
        ).await;
        match init_docker_config {
            Ok(c) => env::set_var("DOCKER_CONFIG", c),
            Err(_) => env::set_var("DOCKER_CONFIG", ""),
        }
        assert_eq!(error.is_err(), expected_error);
        assert_eq!(publish.publish, expected_result);
    }

    #[tokio::test]
    #[serial(docker)]
    async fn docker_existing_image() {
        docker_test("alpine".to_string(), "latest".to_string(), false, None, None, false, true, false).await;
    }

    #[tokio::test]
    #[serial(docker)]
    async fn docker_existing_image_non_existing_version() {
        docker_test("alpine".to_string(), "NONEXISTENTTAG".to_string(), false, None, None, false, false, false).await;
    }

    #[tokio::test]
    #[serial(docker)]
    async fn docker_private_reg_existing_image() {
        docker_test("library/alpine".to_string(), "latest".to_string(), false, Some(DOCKER_HTPASSWD_USERNAME.to_string()), Some(DOCKER_HTPASSWD_PASSWORD.to_string()), true, true, false).await;
    }

    #[tokio::test]
    #[serial(docker)]
    async fn docker_private_reg_existing_image_non_existing_version() {
        docker_test("library/alpine".to_string(), "NONEXISTANT".to_string(), false, Some(DOCKER_HTPASSWD_USERNAME.to_string()), Some(DOCKER_HTPASSWD_PASSWORD.to_string()), true, false, false).await;
    }


    #[tokio::test]
    #[serial(docker)]
    async fn docker_private_reg_auth_file_existing_image() {
        docker_test("library/alpine".to_string(), "latest".to_string(), true, Some(DOCKER_HTPASSWD_USERNAME.to_string()), Some(DOCKER_HTPASSWD_PASSWORD.to_string()), true, true, false).await;
    }

    #[tokio::test]
    #[serial(docker)]
    async fn docker_private_reg_auth_file_existing_image_non_existing_version() {
        docker_test("library/alpine".to_string(), "NONEXISTANT".to_string(), true, Some(DOCKER_HTPASSWD_USERNAME.to_string()), Some(DOCKER_HTPASSWD_PASSWORD.to_string()), true, false, false).await;
    }
}
use crate::utils::docker::{Docker, RealHttpClient, RealOciClient};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct PackageMetadataFslabsCiPublishDocker {
    pub publish: bool,
    pub repository: Option<String>,
    pub context: Option<String>,
    pub dockerfile: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

impl PackageMetadataFslabsCiPublishDocker {
    pub async fn check(
        &mut self,
        package: String,
        version: String,
        docker: &mut Docker<RealOciClient, RealHttpClient>,
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

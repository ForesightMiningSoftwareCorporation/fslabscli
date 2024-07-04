use chrono::prelude::*;
use indexmap::IndexMap;
use object_store::{
    azure::{MicrosoftAzure, MicrosoftAzureBuilder},
    path::Path,
    ObjectStore,
};
use serde::{Deserialize, Serialize};

use super::ResultDependency;

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
#[serde(rename_all = "snake_case")]
pub struct PackageMetadataFslabsCiPublishBinary {
    #[serde(default)]
    pub publish: bool,
    #[serde(default)]
    pub sign: bool,
    pub name: String,
    #[serde(default)]
    pub launcher: PackageMetadataFslabsCiPublishBinaryLauncher,
    #[serde(default)]
    pub installer: PackageMetadataFslabsCiPublishBinaryInstaller,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub targets: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "snake_case")]
pub struct PackageMetadataFslabsCiPublishBinaryLauncher {
    #[serde(default = "default_launcher_path")]
    pub path: String,
}

impl Default for PackageMetadataFslabsCiPublishBinaryLauncher {
    fn default() -> Self {
        Self {
            path: default_launcher_path(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "snake_case")]
pub struct PackageMetadataFslabsCiPublishBinaryInstaller {
    #[serde(default = "default_installer_path")]
    pub path: String,
    pub publish: bool,
    pub nightly: PackageMetadataFslabsCiPublishBinaryInstallerReleaseChannel,
    pub alpha: PackageMetadataFslabsCiPublishBinaryInstallerReleaseChannel,
    pub beta: PackageMetadataFslabsCiPublishBinaryInstallerReleaseChannel,
    pub prod: PackageMetadataFslabsCiPublishBinaryInstallerReleaseChannel,
    #[serde(default)]
    pub sub_apps: IndexMap<String, ResultDependency>,
}

impl Default for PackageMetadataFslabsCiPublishBinaryInstaller {
    fn default() -> Self {
        Self {
            path: default_installer_path(),
            publish: false,
            nightly: Default::default(),
            alpha: Default::default(),
            beta: Default::default(),
            prod: Default::default(),
            sub_apps: Default::default(),
        }
    }
}

fn default_launcher_path() -> String {
    "launcher".to_string()
}

fn default_installer_path() -> String {
    "installer".to_string()
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
#[serde(rename_all = "snake_case")]
pub struct PackageMetadataFslabsCiPublishBinaryInstallerReleaseChannel {
    pub upgrade_code: Option<String>,
    pub guid_prefix: Option<String>,
}

impl PackageMetadataFslabsCiPublishBinary {
    pub async fn check(
        &mut self,
        name: String,
        version: String,
        store: &Option<BinaryStore>,
        release_channel: String,
        toolchain: String,
    ) -> anyhow::Result<()> {
        if !self.publish {
            return Ok(());
        }
        let Some(object_store) = store else {
            return Ok(());
        };
        // Nightly version should be current date

        let rc_version = match release_channel.as_ref() {
            "nightly" => {
                if name.ends_with("_launcher") {
                    version.clone()
                } else {
                    Utc::now().date_naive().to_string()
                }
            }
            _ => version.clone(),
        };
        log::debug!(
            "BINARY: checking if version {} of {} already exists {:?}",
            rc_version,
            name,
            self
        );
        let mut publish = false;
        for target in self.targets.clone() {
            let extension = match target.contains("windows") {
                true => ".exe",
                false => "",
            };
            let blob_path = Path::from(format!(
                "{}/{}/{}-{}-{}-v{}{}",
                name, release_channel, name, target, toolchain, rc_version, extension
            ));
            match object_store.get_client().head(&blob_path).await {
                Ok(_) => {}
                Err(_) => {
                    publish = true;
                }
            };
        }
        self.publish = publish;
        Ok(())
    }
}

pub struct BinaryStore {
    pub client: MicrosoftAzure,
}

impl BinaryStore {
    pub fn new(
        storage_account: Option<String>,
        container_name: Option<String>,
        access_key: Option<String>,
    ) -> anyhow::Result<Option<Self>> {
        match (storage_account, container_name, access_key) {
            (Some(storage_account), Some(container_name), Some(access_key)) => Ok(Some(Self {
                client: MicrosoftAzureBuilder::new()
                    .with_account(storage_account)
                    .with_access_key(access_key)
                    .with_container_name(container_name)
                    .build()?,
            })),
            _ => Ok(None),
        }
    }

    pub fn get_client(&self) -> &MicrosoftAzure {
        &self.client
    }
}

#[cfg(test)]
mod tests {}

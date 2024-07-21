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
    pub fallback_name: Option<String>,
    #[serde(default)]
    pub rc_version: Option<String>,
    #[serde(default)]
    pub launcher: PackageMetadataFslabsCiPublishBinaryLauncher,
    #[serde(default)]
    pub installer: PackageMetadataFslabsCiPublishBinaryInstaller,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub targets: Vec<String>,
    #[serde(default)]
    pub blob_dir: Option<String>,
    #[serde(default)]
    pub blob_name: Option<String>,
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
    #[serde(default)]
    pub sub_apps_download_script: Option<String>,
    #[serde(default)]
    pub launcher_blob_dir: Option<String>,
    #[serde(default)]
    pub launcher_blob_name: Option<String>,
    #[serde(default)]
    pub installer_blob_dir: Option<String>,
    #[serde(default)]
    pub installer_blob_name: Option<String>,
    #[serde(default)]
    pub installer_blob_signed_name: Option<String>,
    #[serde(default)]
    pub upgrade_code: Option<String>,
    #[serde(default)]
    pub guid_prefix: Option<String>,
    #[serde(default)]
    pub sas_expiry: Option<String>,
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
            sub_apps_download_script: Default::default(),
            launcher_blob_dir: None,
            launcher_blob_name: None,
            installer_blob_dir: None,
            installer_blob_name: None,
            installer_blob_signed_name: None,
            upgrade_code: None,
            guid_prefix: None,
            sas_expiry: None,
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
    pub async fn check(&mut self, store: &Option<BinaryStore>) -> anyhow::Result<()> {
        if !self.publish {
            return Ok(());
        }
        let Some(object_store) = store else {
            return Ok(());
        };
        let mut publish = false;
        if let (Some(blob_dir), Some(blob_name)) = (&self.blob_dir, &self.blob_name) {
            let blob_path = format!("{}/{}", blob_dir, blob_name);
            match object_store.get_client().head(&Path::from(blob_path)).await {
                Ok(_) => {}
                Err(_) => {
                    publish = true;
                }
            };
        }
        let mut publish_installer = false;
        if let (Some(blob_dir), Some(blob_name)) = (&self.installer.installer_blob_dir, &self.installer.installer_blob_signed_name) {
            let blob_path = format!("{}/{}", blob_dir, blob_name);
            match object_store.get_client().head(&Path::from(blob_path)).await {
                Ok(_) => {}
                Err(_) => {
                    publish_installer = true;
                }
            };
        }
        self.publish = publish;
        self.installer.publish = publish_installer;

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

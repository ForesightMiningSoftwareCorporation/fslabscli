use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::utils::cargo::Cargo;

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
#[serde(rename_all = "snake_case")]
pub struct PackageMetadataFslabsCiPublishCargo {
    #[serde(default)]
    pub publish: bool,
    #[serde(alias = "alternate_registries")]
    pub registries: Option<HashSet<String>>,
    #[serde(default)]
    pub registries_publish: HashMap<String, bool>,
    #[serde(default)]
    pub allow_public: bool,
    pub error: Option<String>,
}

impl PackageMetadataFslabsCiPublishCargo {
    pub async fn check(
        &mut self,
        name: String,
        version: String,
        cargo: &Cargo,
    ) -> anyhow::Result<()> {
        tracing::debug!("Got following registries: {:?}", self.registries);
        let registries = self.registries.clone().unwrap_or_default();
        let mut overall_publish = false;
        for registry_name in registries {
            tracing::debug!(
                "CARGO: checking if version {} of {} already exists for registry {}",
                version,
                name,
                registry_name
            );

            let publish = match cargo
                .check_crate_exists(registry_name.clone(), name.clone(), version.clone())
                .await
            {
                Ok(crate_exists) => !crate_exists,
                Err(e) => {
                    tracing::error!("Could not check if crates already exists: {}", e);
                    false
                }
            };
            self.registries_publish
                .insert(registry_name.clone(), publish);
            overall_publish |= publish;
        }
        self.publish = overall_publish;
        Ok(())
    }
}

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::utils::cargo::Cargo;

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
#[serde(rename_all = "snake_case")]
pub struct PackageMetadataFslabsCiPublishCargo {
    #[serde(default)]
    pub publish: bool,
    pub registries: Option<Vec<String>>,
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
        let registries = match &self.registries {
            Some(r) => r.clone(),
            None => {
                // Should be public registry, double check this is wanted
                if self.allow_public {
                    vec!["crates.io".to_string()]
                } else {
                    tracing::debug!(
                        "Tried to publish {} to public registry without setting `fslabs_ci.publish.cargo.allow_public`",
                        name
                    );
                    vec![]
                }
            }
        };
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

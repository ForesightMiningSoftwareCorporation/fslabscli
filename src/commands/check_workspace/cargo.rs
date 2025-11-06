use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::utils::cargo::CrateChecker;

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
#[serde(rename_all = "snake_case")]
pub struct PackageMetadataFslabsCiPublishCargo {
    #[serde(skip)]
    pub publish: bool,
    #[serde(default, rename = "publish")]
    actual_publish: Option<bool>,
    #[serde(alias = "alternate_registries")]
    pub registries: Option<HashSet<String>>,
    #[serde(default)]
    pub registries_publish: HashMap<String, bool>,
    #[serde(default)]
    pub allow_public: bool,
    pub error: Option<String>,
}

impl PackageMetadataFslabsCiPublishCargo {
    pub async fn check<C: CrateChecker>(
        &mut self,
        name: String,
        version: String,
        cargo: &C,
        force: bool,
    ) -> anyhow::Result<()> {
        tracing::debug!("Got following registries: {:?}", self.registries);
        self.publish = self.actual_publish.unwrap_or(force);
        if !self.publish {
            // This package does not want to be published
            return Ok(());
        }
        if version.ends_with("dev") {
            self.publish = false;
            return Ok(());
        }
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::utils::cargo::tests::TestCargo;

    #[tokio::test]
    async fn test_standard_publish_is_respected_when_publish_inexisting_crate() {
        let toml = r#"
        publish = true
        alternate_registries = ["test_registry"]
        "#;
        let cargo = TestCargo::default();

        let mut cargo_publish: PackageMetadataFslabsCiPublishCargo = toml::from_str(toml).unwrap();
        cargo_publish
            .check("test".to_string(), "1.0.0".to_string(), &cargo, false)
            .await
            .unwrap();

        assert!(cargo_publish.publish);
    }

    #[tokio::test]
    async fn test_standard_publish_is_respected_when_not_publish_inexisting_crate() {
        let toml = r#"
        publish = false
        alternate_registries = ["test_registry"]
        "#;
        let cargo = TestCargo::default();

        let mut cargo_publish: PackageMetadataFslabsCiPublishCargo = toml::from_str(toml).unwrap();
        cargo_publish
            .check("test".to_string(), "1.0.0".to_string(), &cargo, false)
            .await
            .unwrap();

        assert!(!cargo_publish.publish);
    }

    #[tokio::test]
    async fn test_publish_default_to_not_inexisting_crate() {
        let toml = r#"
        alternate_registries = ["test_registry"]
        "#;
        let cargo = TestCargo::default();

        let mut cargo_publish: PackageMetadataFslabsCiPublishCargo = toml::from_str(toml).unwrap();
        cargo_publish
            .check("test".to_string(), "1.0.0".to_string(), &cargo, false)
            .await
            .unwrap();

        assert!(!cargo_publish.publish);
    }

    #[tokio::test]
    async fn test_publish_default_to_not_except_if_force_inexisting_crate() {
        let toml = r#"
        alternate_registries = ["test_registry"]
        "#;
        let cargo = TestCargo::default();

        let mut cargo_publish: PackageMetadataFslabsCiPublishCargo = toml::from_str(toml).unwrap();
        cargo_publish
            .check("test".to_string(), "1.0.0".to_string(), &cargo, true)
            .await
            .unwrap();

        assert!(cargo_publish.publish);
    }

    #[tokio::test]
    async fn test_publish_default_to_not_except_if_force_but_respect_package_settings_inexisting_crate()
     {
        let toml = r#"
        publish = false
        alternate_registries = ["test_registry"]
        "#;
        let cargo = TestCargo::default();

        let mut cargo_publish: PackageMetadataFslabsCiPublishCargo = toml::from_str(toml).unwrap();
        cargo_publish
            .check("test".to_string(), "1.0.0".to_string(), &cargo, true)
            .await
            .unwrap();

        assert!(!cargo_publish.publish);
    }
    #[tokio::test]
    async fn test_not_publish_if_standard_publish_but_existing_crate() {
        let toml = r#"
        publish = true
        "#;
        let cargo = TestCargo { exists: true };

        let mut cargo_publish: PackageMetadataFslabsCiPublishCargo = toml::from_str(toml).unwrap();
        cargo_publish
            .check("test".to_string(), "1.0.0".to_string(), &cargo, false)
            .await
            .unwrap();

        assert!(!cargo_publish.publish);
    }

    #[tokio::test]
    async fn test_not_publish_if_force_publish_but_existing_crate() {
        let toml = r#"
        publish = true
        alternate_registries = ["test_registry"]
        "#;
        let cargo = TestCargo { exists: true };

        let mut cargo_publish: PackageMetadataFslabsCiPublishCargo = toml::from_str(toml).unwrap();
        cargo_publish
            .check("test".to_string(), "1.0.0".to_string(), &cargo, true)
            .await
            .unwrap();

        assert!(!cargo_publish.publish);
    }
}

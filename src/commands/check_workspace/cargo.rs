use std::collections::{HashMap, HashSet};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::utils::cargo::CrateChecker;

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
#[serde(rename_all = "snake_case")]
pub struct PackageMetadataFslabsCiPublishCargo {
    #[serde(skip)]
    pub publish: bool,
    #[serde(default, rename = "publish")]
    pub(crate) actual_publish: Option<bool>,
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
        force_publish: bool,
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

            let publish = if force_publish {
                tracing::warn!(
                    "CARGO: force_publish enabled - bypassing existence check for {} v{} on registry {}",
                    name,
                    version,
                    registry_name
                );
                true
            } else {
                let crate_exists = cargo
                    .check_crate_exists(registry_name.clone(), name.clone(), version.clone())
                    .await
                    .with_context(|| {
                        format!(
                            "Could not check whether {name} v{version} exists in Cargo registry {registry_name}"
                        )
                    })?;
                !crate_exists
            };
            self.registries_publish
                .insert(registry_name.clone(), publish);
            overall_publish |= publish;
        }
        self.publish = overall_publish;
        Ok(())
    }

    pub fn filter_target_registry(&mut self, target_registry: &str) {
        for (name, publish) in self.registries_publish.iter_mut() {
            if name != target_registry {
                *publish = false;
            }
        }
        self.publish = self.registries_publish.values().any(|&v| v);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::utils::cargo::tests::MockCargo;

    #[tokio::test]
    async fn test_standard_publish_is_respected_when_publish_inexisting_crate() {
        let toml = r#"
        publish = true
        alternate_registries = ["test_registry"]
        "#;
        let mut cargo = MockCargo::new();

        cargo
            .expect_check_crate_exists()
            .returning(|_, _, _| Ok(false));

        let mut cargo_publish: PackageMetadataFslabsCiPublishCargo = toml::from_str(toml).unwrap();
        cargo_publish
            .check(
                "test".to_string(),
                "1.0.0".to_string(),
                &cargo,
                false,
                false,
            )
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
        let mut cargo = MockCargo::new();

        cargo
            .expect_check_crate_exists()
            .returning(|_, _, _| Ok(false));

        let mut cargo_publish: PackageMetadataFslabsCiPublishCargo = toml::from_str(toml).unwrap();
        cargo_publish
            .check(
                "test".to_string(),
                "1.0.0".to_string(),
                &cargo,
                false,
                false,
            )
            .await
            .unwrap();

        assert!(!cargo_publish.publish);
    }

    #[tokio::test]
    async fn test_publish_default_to_not_inexisting_crate() {
        let toml = r#"
        alternate_registries = ["test_registry"]
        "#;
        let mut cargo = MockCargo::new();

        cargo
            .expect_check_crate_exists()
            .returning(|_, _, _| Ok(false));

        let mut cargo_publish: PackageMetadataFslabsCiPublishCargo = toml::from_str(toml).unwrap();
        cargo_publish
            .check(
                "test".to_string(),
                "1.0.0".to_string(),
                &cargo,
                false,
                false,
            )
            .await
            .unwrap();

        assert!(!cargo_publish.publish);
    }

    #[tokio::test]
    async fn test_publish_default_to_not_except_if_force_inexisting_crate() {
        let toml = r#"
        alternate_registries = ["test_registry"]
        "#;
        let mut cargo = MockCargo::new();

        cargo
            .expect_check_crate_exists()
            .returning(|_, _, _| Ok(false));

        let mut cargo_publish: PackageMetadataFslabsCiPublishCargo = toml::from_str(toml).unwrap();
        cargo_publish
            .check("test".to_string(), "1.0.0".to_string(), &cargo, true, false)
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
        let mut cargo = MockCargo::new();

        cargo
            .expect_check_crate_exists()
            .returning(|_, _, _| Ok(false));

        let mut cargo_publish: PackageMetadataFslabsCiPublishCargo = toml::from_str(toml).unwrap();
        cargo_publish
            .check("test".to_string(), "1.0.0".to_string(), &cargo, true, false)
            .await
            .unwrap();

        assert!(!cargo_publish.publish);
    }
    #[tokio::test]
    async fn test_not_publish_if_standard_publish_but_existing_crate() {
        let toml = r#"
        publish = true
        "#;
        let mut cargo = MockCargo::new();

        cargo
            .expect_check_crate_exists()
            .returning(|_, _, _| Ok(true));

        let mut cargo_publish: PackageMetadataFslabsCiPublishCargo = toml::from_str(toml).unwrap();
        cargo_publish
            .check(
                "test".to_string(),
                "1.0.0".to_string(),
                &cargo,
                false,
                false,
            )
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
        let mut cargo = MockCargo::new();

        cargo
            .expect_check_crate_exists()
            .returning(|_, _, _| Ok(true));

        let mut cargo_publish: PackageMetadataFslabsCiPublishCargo = toml::from_str(toml).unwrap();
        cargo_publish
            .check("test".to_string(), "1.0.0".to_string(), &cargo, true, false)
            .await
            .unwrap();

        assert!(!cargo_publish.publish);
    }

    #[tokio::test]
    async fn test_force_publish_bypasses_crate_exists_check() {
        let toml = r#"
        publish = true
        alternate_registries = ["test_registry"]
        "#;
        let cargo = MockCargo::new();

        // No expect_check_crate_exists: it should NOT be called when force_publish=true

        let mut cargo_publish: PackageMetadataFslabsCiPublishCargo = toml::from_str(toml).unwrap();
        cargo_publish
            .check("test".to_string(), "1.0.0".to_string(), &cargo, false, true)
            .await
            .unwrap();

        assert!(cargo_publish.publish);
        assert!(cargo_publish.registries_publish["test_registry"]);
    }

    #[tokio::test]
    async fn test_registry_check_error_fails_publishability_check() {
        let toml = r#"
        publish = true
        alternate_registries = ["test_registry"]
        "#;
        let mut cargo = MockCargo::new();
        cargo
            .expect_check_crate_exists()
            .returning(|_, _, _| Err(anyhow::anyhow!("registry unavailable")));

        let mut cargo_publish: PackageMetadataFslabsCiPublishCargo = toml::from_str(toml).unwrap();
        let error = cargo_publish
            .check(
                "test".to_string(),
                "1.0.0".to_string(),
                &cargo,
                false,
                false,
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains(
            "Could not check whether test v1.0.0 exists in Cargo registry test_registry"
        ));
        assert!(cargo_publish.registries_publish.is_empty());
    }

    #[test]
    fn test_filter_target_registry_keeps_only_target() {
        // Arrange
        let mut cargo_publish = PackageMetadataFslabsCiPublishCargo {
            publish: true,
            registries_publish: HashMap::from([
                ("registry_a".to_string(), true),
                ("registry_b".to_string(), true),
            ]),
            ..Default::default()
        };

        // Act
        cargo_publish.filter_target_registry("registry_a");

        // Assert
        assert!(cargo_publish.registries_publish["registry_a"]);
        assert!(!cargo_publish.registries_publish["registry_b"]);
        assert!(cargo_publish.publish);
    }

    #[test]
    fn test_filter_target_registry_no_match_disables_publish() {
        // Arrange
        let mut cargo_publish = PackageMetadataFslabsCiPublishCargo {
            publish: true,
            registries_publish: HashMap::from([("registry_a".to_string(), true)]),
            ..Default::default()
        };

        // Act
        cargo_publish.filter_target_registry("nonexistent");

        // Assert
        assert!(!cargo_publish.registries_publish["registry_a"]);
        assert!(!cargo_publish.publish);
    }

    #[test]
    fn test_filter_target_registry_preserves_already_false() {
        // Arrange
        let mut cargo_publish = PackageMetadataFslabsCiPublishCargo {
            publish: true,
            registries_publish: HashMap::from([
                ("registry_a".to_string(), false),
                ("registry_b".to_string(), true),
            ]),
            ..Default::default()
        };

        // Act
        cargo_publish.filter_target_registry("registry_a");

        // Assert
        assert!(!cargo_publish.registries_publish["registry_a"]);
        assert!(!cargo_publish.registries_publish["registry_b"]);
        assert!(!cargo_publish.publish);
    }
}

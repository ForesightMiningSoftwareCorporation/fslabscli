use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt::{Display, Formatter};
use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, ValueEnum};
use serde::Serialize;

use crate::PrettyPrintable;
use crate::cargo_publish_policy::{
    MarkedCargoPackage, inspect_cargo_publish_policy, missing_marked_package_names,
};
use crate::cli_args::DiffOptions;
use crate::utils::cargo::Cargo;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
enum PolicyCheck {
    #[default]
    All,
    Approval,
    Registry,
}

impl PolicyCheck {
    fn approval(self) -> bool {
        matches!(self, Self::All | Self::Approval)
    }

    fn registry(self) -> bool {
        matches!(self, Self::All | Self::Registry)
    }
}

#[derive(Debug, Parser, Default)]
#[command(about = "Enforce Cargo publication approval and registry state")]
pub struct Options {
    #[clap(flatten)]
    diff: DiffOptions,
    /// Label which approves newly metadata-marked Cargo packages.
    #[arg(long, default_value = "add-crate")]
    approval_label: String,
    /// Current pull request labels.
    #[arg(long, env = "PULL_REQUEST_LABELS", value_delimiter = ',')]
    pull_request_labels: Vec<String>,
    /// Registry where every package marked on the pull request base must exist.
    #[arg(long, env, default_value = "fsl")]
    cargo_target_registry: String,
    /// Run the approval check, registry check, or both.
    #[arg(long, value_enum, default_value_t)]
    check: PolicyCheck,
}

#[derive(Debug, Serialize)]
pub struct Result {
    pub approval_checked: bool,
    pub registry_checked: bool,
    pub approval_label: String,
    pub approval_label_present: bool,
    pub newly_marked: Vec<String>,
    pub base_packages_missing_from_registry: Vec<String>,
}

impl Display for Result {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut messages = Vec::new();
        if self.approval_checked {
            if self.newly_marked.is_empty() {
                messages.push("No newly marked Cargo packages".to_string());
            } else {
                messages.push(format!(
                    "Newly marked Cargo packages: {}",
                    self.newly_marked.join(", ")
                ));
            }
        }
        if self.registry_checked {
            if self.base_packages_missing_from_registry.is_empty() {
                messages.push("All base-marked Cargo packages exist in the registry".to_string());
            } else {
                messages.push(format!(
                    "Base-marked Cargo packages missing from the registry: {}",
                    self.base_packages_missing_from_registry.join(", ")
                ));
            }
        }
        write!(f, "{}", messages.join("\n"))
    }
}

impl PrettyPrintable for Result {
    fn pretty_print(&self) -> String {
        self.to_string()
    }
}

fn validate_policy(result: &Result, check: PolicyCheck) -> anyhow::Result<()> {
    if check.registry() && !result.base_packages_missing_from_registry.is_empty() {
        anyhow::bail!(
            "Cargo packages marked for publication on the pull request base are missing from the registry: {}. Publish them before merging other pull requests.",
            result.base_packages_missing_from_registry.join(", ")
        );
    }
    if check.approval() && !result.newly_marked.is_empty() && !result.approval_label_present {
        anyhow::bail!(
            "The {} label is required for newly marked Cargo packages: {}",
            result.approval_label,
            result.newly_marked.join(", ")
        );
    }
    Ok(())
}

fn persistent_base_marked(
    base_marked: &BTreeMap<String, MarkedCargoPackage>,
    resulting_marked: &BTreeMap<String, MarkedCargoPackage>,
) -> BTreeMap<String, MarkedCargoPackage> {
    base_marked
        .iter()
        .filter(|(name, _)| resulting_marked.contains_key(*name))
        .map(|(name, package)| (name.clone(), package.clone()))
        .collect()
}

pub async fn check_cargo_publish_policy(
    options: Box<Options>,
    repo_root: PathBuf,
) -> anyhow::Result<Result> {
    if options.diff.base_sha.is_some() != options.diff.head_sha.is_some() {
        anyhow::bail!("PULL_BASE_SHA and PULL_PULL_SHA must be provided together");
    }
    let repo = gix::open(&repo_root)
        .with_context(|| format!("Failed to open git repository at {}", repo_root.display()))?;
    let (base_commit, head_commit) = options.diff.strategy().git_commits(&repo)?;
    let snapshot = inspect_cargo_publish_policy(&repo, base_commit, head_commit)?;

    let persistent_base_marked =
        persistent_base_marked(&snapshot.base_marked, &snapshot.head_marked);

    let missing = if options.check.registry() {
        let registries = HashSet::from([options.cargo_target_registry.clone()]);
        let cargo = Cargo::new(&registries, true)?;
        missing_marked_package_names(
            &cargo,
            &options.cargo_target_registry,
            &persistent_base_marked,
        )
        .await?
    } else {
        BTreeSet::new()
    };
    let labels = options
        .pull_request_labels
        .iter()
        .map(|label| label.trim())
        .filter(|label| !label.is_empty())
        .collect::<BTreeSet<_>>();
    let result = Result {
        approval_checked: options.check.approval(),
        registry_checked: options.check.registry(),
        approval_label: options.approval_label.clone(),
        approval_label_present: labels.contains(options.approval_label.as_str()),
        newly_marked: snapshot.newly_marked.into_iter().collect(),
        base_packages_missing_from_registry: missing.into_iter().collect(),
    };
    validate_policy(&result, options.check)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(newly_marked: &[&str], missing: &[&str], approved: bool) -> Result {
        Result {
            approval_checked: true,
            registry_checked: true,
            approval_label: "add-crate".to_string(),
            approval_label_present: approved,
            newly_marked: newly_marked.iter().map(|name| name.to_string()).collect(),
            base_packages_missing_from_registry: missing
                .iter()
                .map(|name| name.to_string())
                .collect(),
        }
    }

    #[test]
    fn requires_label_for_newly_marked_packages() {
        let error =
            validate_policy(&result(&["new_crate"], &[], false), PolicyCheck::All).unwrap_err();
        assert_eq!(
            error.to_string(),
            "The add-crate label is required for newly marked Cargo packages: new_crate"
        );
    }

    #[test]
    fn accepts_label_for_newly_marked_packages() {
        validate_policy(&result(&["new_crate"], &[], true), PolicyCheck::All).unwrap();
    }

    #[test]
    fn missing_base_package_fails_even_with_label() {
        let error =
            validate_policy(&result(&[], &["unpublished"], true), PolicyCheck::All).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Cargo packages marked for publication on the pull request base are missing from the registry: unpublished. Publish them before merging other pull requests."
        );
    }

    #[test]
    fn removing_or_unmarking_missing_base_package_allows_recovery() {
        let package = MarkedCargoPackage {
            name: "unpublished".to_string(),
            manifest_path: "unpublished/Cargo.toml".to_string(),
        };
        let base = BTreeMap::from([("unpublished".to_string(), package)]);
        let head = BTreeMap::new();

        assert!(persistent_base_marked(&base, &head).is_empty());
    }

    #[test]
    fn approval_only_does_not_require_registry_state() {
        validate_policy(
            &result(&["new_crate"], &["unpublished"], true),
            PolicyCheck::Approval,
        )
        .unwrap();
    }

    #[test]
    fn approval_only_output_does_not_claim_registry_was_checked() {
        let mut result = result(&[], &[], true);
        result.registry_checked = false;
        assert_eq!(result.to_string(), "No newly marked Cargo packages");
    }

    #[test]
    fn registry_only_does_not_require_approval_label() {
        validate_policy(&result(&["new_crate"], &[], false), PolicyCheck::Registry).unwrap();
    }

    #[tokio::test]
    async fn explicit_base_and_head_must_be_provided_together() {
        let options = Options {
            diff: DiffOptions {
                base_sha: Some("base".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };

        let error = check_cargo_publish_policy(Box::new(options), PathBuf::new())
            .await
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "PULL_BASE_SHA and PULL_PULL_SHA must be provided together"
        );
    }
}

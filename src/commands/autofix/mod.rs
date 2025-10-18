use crate::{PackageRelatedOptions, PrettyPrintable, crate_graph::CrateGraph};
use clap::Parser;
use std::path::Path;
use std::process::Command;
use tracing::info;

#[derive(Debug, Parser, Default)]
#[command(about = "Run all autofixable commands: cargo update, cargo fmt, and fix lock files.")]
pub struct Options {
    /// Run in check mode, will error if any files would be modified
    #[arg(long)]
    check: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AutofixResult(String);

impl std::fmt::Display for AutofixResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl PrettyPrintable for AutofixResult {
    fn pretty_print(&self) -> String {
        self.0.clone()
    }
}

impl From<String> for AutofixResult {
    fn from(s: String) -> Self {
        AutofixResult(s)
    }
}

/// Run all autofixable commands in all workspaces:
/// 1. cargo update --workspace
/// 2. cargo clippy --fix --allow-dirty --allow-staged
/// 3. cargo fmt --all
/// 4. Fix Cargo.lock files (via fix_lock_files logic)
pub fn autofix(
    common_options: &PackageRelatedOptions,
    options: &Options,
    repo_root: &Path,
) -> anyhow::Result<AutofixResult> {
    let PackageRelatedOptions {
        cargo_main_registry,
        ..
    } = common_options;
    let Options { check } = options;

    let graph = CrateGraph::new(repo_root, cargo_main_registry.clone(), None)?;
    let workspaces: Vec<_> = graph
        .workspaces()
        .iter()
        .filter(|w| !w.path.join(".no_cargo_lock").exists())
        .map(|w| repo_root.join(&w.path))
        .collect();

    let mut results = Vec::new();

    for workspace_path in &workspaces {
        info!("Processing workspace: {workspace_path:?}");

        // Step 1: Run cargo update --workspace
        info!("Running 'cargo update --workspace' in {workspace_path:?}");
        let output = Command::new("cargo")
            .arg("update")
            .arg("--workspace")
            .current_dir(workspace_path)
            .output()?;

        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "cargo update failed in {:?}: {}",
                workspace_path,
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        // Step 2: Run cargo clippy --fix --allow-dirty --allow-staged
        info!("Running 'cargo clippy --fix --allow-dirty --allow-staged' in {workspace_path:?}");
        let output = Command::new("cargo")
            .arg("clippy")
            .arg("--fix")
            .arg("--allow-dirty")
            .arg("--allow-staged")
            .current_dir(workspace_path)
            .output()?;

        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "cargo clippy --fix failed in {:?}: {}",
                workspace_path,
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        // Step 3: Run cargo fmt --all
        info!("Running 'cargo fmt --all' in {workspace_path:?}");
        let output = Command::new("cargo")
            .arg("fmt")
            .arg("--all")
            .current_dir(workspace_path)
            .output()?;

        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "cargo fmt failed in {:?}: {}",
                workspace_path,
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        results.push(format!(
            "✓ Updated, clippy fixed, and formatted: {}",
            workspace_path.display()
        ));
    }

    // Step 4: Run fix_lock_files logic to verify consistency
    info!("Verifying lock files are consistent");
    let fix_lock_options = crate::commands::fix_lock_files::Options { check: *check };
    crate::commands::fix_lock_files::fix_lock_files(common_options, &fix_lock_options, repo_root)?;

    results.push("✓ Lock files verified".to_string());

    if *check {
        // In check mode, verify no files were modified
        let output = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(repo_root)
            .output()?;

        if !output.stdout.is_empty() {
            return Err(anyhow::anyhow!(
                "Files were modified. Please run autofix locally and commit the changes:\n{}",
                String::from_utf8_lossy(&output.stdout)
            ));
        }
        results.push("✓ No files modified (check mode)".to_string());
    }

    Ok(AutofixResult(results.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PackageRelatedOptions;
    use std::process::Command;

    #[tokio::test]
    async fn test_autofix_basic() {
        let tmp = assert_fs::TempDir::new().unwrap().into_persistent();

        // Initialize a git repo
        Command::new("git")
            .args(["init"])
            .current_dir(&tmp)
            .output()
            .unwrap();

        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&tmp)
            .output()
            .unwrap();

        Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(&tmp)
            .output()
            .unwrap();

        // Create a simple Rust crate
        Command::new("cargo")
            .args(["init", "--bin", "--name", "test-crate"])
            .current_dir(&tmp)
            .output()
            .unwrap();

        // Commit initial state
        Command::new("git")
            .args(["add", "."])
            .current_dir(&tmp)
            .output()
            .unwrap();

        Command::new("git")
            .args(["commit", "-m", "Initial commit"])
            .current_dir(&tmp)
            .output()
            .unwrap();

        let common_options = PackageRelatedOptions::default();
        let options = Options::default();

        let result = autofix(&common_options, &options, tmp.path());
        assert!(result.is_ok(), "autofix should succeed: {:?}", result);
    }
}

use crate::{
    PackageRelatedOptions, PrettyPrintable, crate_graph::CrateGraph, script::CommandOutput,
};
use clap::Parser;
use diffy::create_patch;
use git2::{Repository, build::CheckoutBuilder};
use std::path::Path;
use tracing::{debug, info};

#[derive(Debug, Parser, Default)]
#[command(about = "Fix inconsistencies in all Cargo.lock files.")]
pub struct Options {
    /// Run the fix in check mode, if set, an updated lockfile would yield an error
    #[arg(long)]
    check: bool,
}

/// Fix mistakes in all workspace `Cargo.lock` files.
///
/// Performs the following:
///
/// 1. Restore all `Cargo.lock` files to their state at `base_rev`.
///    If no `base_rev` are given, then the checks run on the current state.
///    This is useful for local fixing.
/// 2. Run `cargo update --workspace` in each workspace to ensure
///    the `Cargo.lock` files are updated to reflect any changes in
///    `Cargo.toml`s.
///
/// Because of the `--workspace` flag, only minimal updates are
/// performed. This is done to avoid letting SemVer violations from
/// dependencies slip into CI.
///
pub fn fix_workspace_lockfile(
    repo_root: &Path,
    workspace_path: &Path,
    head_rev: String,
    base_rev: Option<String>,
    check: bool,
) -> anyhow::Result<CommandOutput> {
    let lock_path = workspace_path.join("Cargo.lock");
    let orig_lockfile = match std::fs::read_to_string(&lock_path) {
        Ok(contents) => Some(contents),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(e.into());
        }
    };

    if let Some(base_rev) = base_rev {
        let repo = Repository::open(repo_root)?;

        // Do this resolution before making any changes to the repo, so e.g.
        // "HEAD" is correct.
        let head_commit = repo.revparse_single(&head_rev)?;
        let base_commit = repo.revparse_single(&base_rev)?;

        // Restore `Cargo.lock` file to its state at `base_rev`.
        debug!("checking out {}", base_commit.id());
        let mut builder = CheckoutBuilder::new();
        builder.force();
        repo.checkout_tree(&base_commit, Some(&mut builder))?;
        debug!("checking out {}", head_commit.id());
        let mut builder = CheckoutBuilder::new();
        builder.force();
        repo.checkout_tree(&head_commit, Some(&mut builder))?;
        if let Some(contents) = orig_lockfile.clone() {
            debug!(
                "Reverting {lock_path:?} to contents at {}",
                base_commit.id()
            );
            std::fs::write(&lock_path, contents)?;
        }
    }

    info!("Running 'cargo update --workspace' in {workspace_path:?}");
    let output = std::process::Command::new("cargo")
        .arg("update")
        .arg("--workspace")
        .current_dir(workspace_path)
        .output()?;

    if !output.status.success() {
        return Ok(output.into());
    }

    if check {
        debug!("Checking for changes in {:?}", lock_path);
        let updated_lockfile = match std::fs::read_to_string(&lock_path) {
            Ok(contents) => Some(contents),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(e.into());
            }
        };
        let changed = match (&orig_lockfile, &updated_lockfile) {
            (Some(orig), Some(updated)) => orig != updated,
            (Some(_), None) | (None, Some(_)) => true,
            (None, None) => false,
        };

        if changed {
            let mut diff_output = String::new();

            diff_output.push_str(&format!(
                "Diff in {} at {}:\n",
                lock_path.display(),
                head_rev
            ));
            let orig = orig_lockfile.as_deref().unwrap_or("");
            let updated = updated_lockfile.as_deref().unwrap_or("");
            let patch = create_patch(orig, updated);
            diff_output.push_str(&format!("{}", patch));
            return Ok(CommandOutput {
                stdout: "".to_string(),
                stderr: format!(
                    "Cargo.lock is out of sync. Please run 'cargo update --workspace' locally and commit the changes.\n\n{}",
                    diff_output
                ),
                success: false,
            });
        }
    }
    Ok(CommandOutput {
        stdout: "".to_string(),
        stderr: "".to_string(),
        success: true,
    })
}

pub type LockResult = String;

impl PrettyPrintable for LockResult {
    fn pretty_print(&self) -> String {
        "".to_string()
    }
}

/// Any workspaces containing a ".no_cargo_lock" sentinel file will be skipped.
pub fn fix_lock_files(
    common_options: &PackageRelatedOptions,
    options: &Options,
    repo_root: &Path,
) -> anyhow::Result<LockResult> {
    let PackageRelatedOptions {
        cargo_main_registry,
        head_rev,
        base_rev,
        ..
    } = common_options;
    let Options { check } = options;

    let graph = CrateGraph::new(repo_root, cargo_main_registry.clone(), None)?;
    let check_workspaces: Vec<_> = graph
        .workspaces()
        .iter()
        .filter(|w| !w.path.join(".no_cargo_lock").exists())
        .map(|w| repo_root.join(&w.path))
        .collect();

    for workspace_path in check_workspaces {
        fix_workspace_lockfile(
            repo_root,
            &workspace_path,
            head_rev.clone(),
            base_rev.clone(),
            *check,
        )?;
    }

    Ok("".into())
}
#[cfg(test)]
mod tests {
    use crate::utils::test::{
        commit_all_changes, commit_repo, create_complex_workspace, modify_file, stage_file,
    };

    use super::*;
    use std::fs::File;
    use std::io::prelude::*;
    use std::path::PathBuf;
    use std::process::Command;

    fn create_simple_rust_crate() -> PathBuf {
        let tmp = assert_fs::TempDir::new()
            .unwrap()
            .into_persistent()
            .to_path_buf();

        let repo = Repository::init(&tmp).expect("Failed to init repo");

        // Configure Git user info (required for commits)
        repo.config()
            .unwrap()
            .set_str("user.name", "Test User")
            .unwrap();
        repo.config()
            .unwrap()
            .set_str("user.email", "test@example.com")
            .unwrap();
        repo.config().unwrap().set_str("gpg.sign", "false").unwrap();

        Command::new("cargo")
            .arg("init")
            .arg("--bin")
            .arg("--name")
            .arg("test-bin")
            .current_dir(&tmp)
            .output()
            .expect("Failed to create simple crate");

        // Stage and commit initial crate
        commit_all_changes(&tmp, "Initial commit");
        // Create Second Commit
        modify_file(&tmp, "src/main.rs", "pub fn main() {}");
        stage_file(&tmp, "src/main.rs");
        commit_repo(&tmp, "Added new function");
        tmp
    }

    #[tokio::test]
    async fn test_fix_lockfile_no_change() {
        let repo = create_simple_rust_crate();

        let common_options = PackageRelatedOptions::default();
        let options = Options::default();
        // Call the fix_lockfile function
        let result = fix_lock_files(&common_options, &options, &repo).map_err(|e| {
            println!("Got error: {e}");
            e
        });

        assert!(result.is_ok());
        // Assert that lock file has been created.
        assert!(repo.join("Cargo.lock").exists());
    }

    #[tokio::test]
    async fn test_fix_lockfile_updates() {
        let repo = create_simple_rust_crate();
        let common_options = PackageRelatedOptions::default();
        let options = Options::default();
        // Call the fix_lockfile function to generate the first lock file
        let _ = fix_lock_files(&common_options, &options, &repo).map_err(|e| {
            println!("Got error: {e}");
            e
        });
        commit_all_changes(&repo, "init");

        // Let's update the version in Cargo.toml, this should yield a package in need of Cargo.lock update
        modify_file(
            &repo,
            "Cargo.toml",
            r#"[package]
name = "test-bin"
version = "0.2.0"
edition = "2024"
[dependencies]"#,
        );
        stage_file(&repo, "Cargo.toml");
        commit_repo(&repo, "chore: bump version");

        // Let's get the current lock file data
        let mut file = File::open(repo.join("Cargo.lock")).unwrap();
        let mut contents = String::new();
        file.read_to_string(&mut contents).unwrap();
        assert_eq!(
            contents,
            r#"# This file is automatically @generated by Cargo.
# It is not intended for manual editing.
version = 4

[[package]]
name = "test-bin"
version = "0.1.0"
"#
        );

        let common_options = PackageRelatedOptions::default();
        let options = Options::default();
        // Call the fix_lockfile function
        let result = fix_lock_files(&common_options, &options, &repo).map_err(|e| {
            println!("Got error: {e}");
            e
        });
        assert!(result.is_ok());
        // Let's get the current lock file data
        let mut file = File::open(repo.join("Cargo.lock")).unwrap();
        let mut contents = String::new();
        file.read_to_string(&mut contents).unwrap();
        assert_eq!(
            contents,
            r#"# This file is automatically @generated by Cargo.
# It is not intended for manual editing.
version = 4

[[package]]
name = "test-bin"
version = "0.2.0"
"#
        );
    }
    #[tokio::test]
    async fn test_fix_lockfile_updates_in_complex_ws() {
        let ws = create_complex_workspace();
        let graph = CrateGraph::new(&ws, "", None).unwrap();

        // Remove lock files created from running cargo-metadata.
        for workspace in graph.workspaces() {
            let _ = std::fs::remove_file(ws.join(&workspace.path).join("Cargo.lock"));
        }

        let common_options = PackageRelatedOptions::default();
        let options = Options::default();
        let _ = fix_lock_files(&common_options, &options, &ws).map_err(|e| {
            println!("Got error: {e}");
            e
        });
        // Assert that lock files have been created.
        for workspace in graph.workspaces() {
            assert!(
                ws.join(&workspace.path).join("Cargo.lock").exists(),
                "{:?}",
                workspace.path
            );
        }
    }
}

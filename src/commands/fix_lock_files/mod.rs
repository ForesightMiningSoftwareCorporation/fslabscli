use crate::{PrettyPrintable, crate_graph::CrateGraph};
use clap::Parser;
use git2::{Repository, build::CheckoutBuilder};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};
use tracing::{debug, info};

#[derive(Debug, Parser, Default)]
#[command(about = "Fix inconsistencies in all Cargo.lock files.")]
pub struct Options {
    /// The branch's head revision string.
    #[arg(long, default_value = "HEAD")]
    head_rev: String,
    /// The branch's base revision string.
    #[arg(long)]
    base_rev: Option<String>,
    #[arg(long, env, default_value = "foresight-mining-software-corporation")]
    cargo_main_registry: String,
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
    changed_lockfiles: &HashSet<PathBuf>,
    check: bool,
) -> anyhow::Result<(String, String, bool)> {
    if check && changed_lockfiles.contains(workspace_path) {
        return Err(anyhow::anyhow!(
            "cargo metadata modified Cargo.lock in check mode.",
        ));
    }
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
    assert!(output.status.success(), "{output:?}");

    if check {
        debug!("Checking for changes in {:?}", lock_path);
        let updated_lockfile = match std::fs::read_to_string(&lock_path) {
            Ok(contents) => Some(contents),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(e.into());
            }
        };
        let correct = match (orig_lockfile, updated_lockfile) {
            (Some(orig), Some(updated)) => {
                debug!(
                    "Checking for changes in {:?}: {:?}",
                    lock_path,
                    orig == updated
                );
                orig == updated
            }
            (Some(_), None) => false,
            (None, Some(_)) => false,
            (None, None) => true,
        };
        if !correct {
            return Err(anyhow::anyhow!(
                "cargo update modified Cargo.lock in check mode.",
            ));
        }
    }
    Ok(("".to_string(), "".to_string(), true))
}

pub type LockResult = String;

impl PrettyPrintable for LockResult {
    fn pretty_print(&self) -> String {
        "".to_string()
    }
}

/// Any workspaces containing a ".no_cargo_lock" sentinel file will be skipped.
pub fn fix_lock_files(options: &Options, repo_root: &Path) -> anyhow::Result<LockResult> {
    let Options {
        head_rev,
        base_rev,
        cargo_main_registry,
        check,
    } = options;

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
            &graph.changed_lockfiles,
            *check,
        )?;
    }

    Ok("".into())
}
#[cfg(test)]
mod tests {
    use crate::utils::test::{commit_all_changes, commit_repo, modify_file, stage_file};

    use super::*;
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

        let options = Options::default();
        // Call the fix_lockfile function
        let result = fix_lock_files(&options, &repo);

        assert!(result.await.is_ok());
        // Assert that lock file has been created.
        assert!(repo.join("Cargo.lock").exists());
    }
}

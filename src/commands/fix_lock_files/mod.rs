use crate::{
    PackageRelatedOptions, PrettyPrintable,
    cli_args::{DiffOptions, DiffStrategy},
    crate_graph::CrateGraph,
    script::CommandOutput,
};
use clap::Parser;
use diffy::create_patch;
use git2::Repository;
use std::path::Path;
use tracing::{debug, info};

#[derive(Debug, Parser, Default)]
#[command(about = "Fix inconsistencies in all Cargo.lock files.")]
pub struct Options {
    /// Run the fix in check mode, if set, an updated lockfile would yield an error
    #[arg(long)]
    check: bool,
    #[clap(flatten)]
    diff: DiffOptions,
}

/// Read the content of a file from a specific commit without checking it out.
///
/// This function uses git's blob reading capabilities to access file content
/// from a historical commit without modifying the working directory.
fn read_file_from_commit(
    repo: &Repository,
    commit: &git2::Object,
    file_path: &Path,
) -> anyhow::Result<Option<String>> {
    // Convert the object to a commit
    let commit = commit
        .as_commit()
        .ok_or_else(|| anyhow::anyhow!("Object is not a commit"))?;

    let tree = commit.tree()?;

    // Try to get the tree entry for this path
    match tree.get_path(file_path) {
        Ok(entry) => {
            // Get the blob object
            let blob = repo.find_blob(entry.id())?;

            // Convert blob content to string
            let content = std::str::from_utf8(blob.content())?;
            Ok(Some(content.to_string()))
        }
        Err(_) => {
            // File doesn't exist at this commit
            Ok(None)
        }
    }
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
    diff_strategy: &DiffStrategy,
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

    let repo = Repository::open(repo_root)?;
    let (base_commit, head_commit) = diff_strategy.git_commits(&repo)?;
    if let DiffStrategy::Explicit { .. } = diff_strategy {
        // Restore `Cargo.lock` file to its state at `base_rev` using git blob reading
        // instead of checkout to avoid interfering with parallel tests.
        debug!("Reading Cargo.lock from base commit {}", base_commit.id());

        // Get relative path from repo root for git tree lookup
        let lock_path_relative = lock_path.strip_prefix(repo_root)?;

        // Read the lockfile content from base_commit without checking it out
        let base_lockfile = read_file_from_commit(&repo, &base_commit, lock_path_relative)?;

        if let Some(base_contents) = base_lockfile {
            debug!(
                "Restoring {lock_path:?} to contents at {}",
                base_commit.id()
            );
            std::fs::write(&lock_path, base_contents)?;
        } else {
            debug!(
                "Cargo.lock did not exist at {}, removing if present",
                base_commit.id()
            );
            // If the file didn't exist at base_commit, remove it if it exists now
            let _ = std::fs::remove_file(&lock_path);
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
                "Diff in {} at {:?}:\n",
                lock_path.display(),
                head_commit
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
        ..
    } = common_options;
    let Options { check, diff } = options;

    let graph = CrateGraph::new(repo_root, cargo_main_registry.clone(), None)?;
    let check_workspaces: Vec<_> = graph
        .workspaces()
        .iter()
        .filter(|w| !w.path.join(".no_cargo_lock").exists())
        .map(|w| repo_root.join(&w.path))
        .collect();

    let diff_strategy = diff.strategy();

    for workspace_path in check_workspaces {
        fix_workspace_lockfile(repo_root, &workspace_path, &diff_strategy, *check)?;
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

    /// Creates a dummy Rust crate with a git history suitable for testing
    /// lockfile restoration from historical commits (we got bit with parrallel testing)
    ///
    /// Returns a PathBuf to a temporary repository with the following commit history:
    /// 1. Initial commit: version 0.1.0 with Cargo.lock
    /// 2. Update Cargo.toml to version 0.2.0
    /// 3. Update Cargo.lock to match version 0.2.0
    /// 4. Corrupt Cargo.lock (version 0.99.99) and modify src/main.rs with marker comment
    fn create_versioned_rust_crate() -> PathBuf {
        let repo_path = create_simple_rust_crate();

        // Commit 2: Update version to 0.2.0 in Cargo.toml
        modify_file(
            &repo_path,
            "Cargo.toml",
            r#"[package]
name = "test-crate"
version = "0.2.0"
edition = "2021"

[dependencies]"#,
        );
        stage_file(&repo_path, "Cargo.toml");
        commit_repo(&repo_path, "chore: bump version to 0.2.0");

        // Commit 3: Run cargo update to get the lock file in sync with 0.2.0
        let output = Command::new("cargo")
            .arg("update")
            .arg("--workspace")
            .current_dir(&repo_path)
            .output()
            .expect("Failed to run cargo update");
        assert!(
            output.status.success(),
            "cargo update failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        stage_file(&repo_path, "Cargo.lock");
        commit_repo(&repo_path, "chore: update lock file for v0.2.0");

        // Commit 4: Manually corrupt the Cargo.lock to an incorrect state
        let corrupted_lockfile = r#"# This file is automatically @generated by Cargo.
# It is not intended for manual editing.
version = 4

[[package]]
name = "test-crate"
version = "0.99.99"
"#;
        std::fs::write(repo_path.join("Cargo.lock"), corrupted_lockfile)
            .expect("Failed to write corrupted lock file");
        stage_file(&repo_path, "Cargo.lock");
        commit_repo(&repo_path, "test: corrupt lock file");

        // Also modify another file to serve as a canary that detects unwanted checkouts
        let marker_content = "// This file should not be modified by fix_workspace_lockfile\n";
        modify_file(&repo_path, "src/main.rs", marker_content);
        commit_all_changes(&repo_path, "test: add marker to main.rs");

        repo_path
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
        let ws = create_complex_workspace(true);
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

    #[tokio::test]
    async fn test_fix_lockfile_reads_from_base_commit() {
        // This test verifies that fix_workspace_lockfile correctly reads Cargo.lock
        // from git history without checking out the base commit, thus avoiding
        // interference with parallel tests and other files in the working directory.
        let repo_path = create_versioned_rust_crate();
        let repo = Repository::open(&repo_path).expect("Failed to open repo");

        // Get the initial commit SHA (HEAD~3 in this repo's history)
        let initial_commit_sha = {
            let head = repo.head().unwrap();
            let commit = head.peel_to_commit().unwrap();
            // Walk back 3 commits to get the initial commit
            let initial_commit = commit
                .parent(0)
                .unwrap()
                .parent(0)
                .unwrap()
                .parent(0)
                .unwrap();
            initial_commit.id().to_string()
        };

        // Store the main.rs content before running the fix
        let main_rs_path = repo_path.join("src/main.rs");
        let main_rs_before =
            std::fs::read_to_string(&main_rs_path).expect("Failed to read main.rs before fix");

        // Now we're at HEAD, which has:
        // - Cargo.toml with version 0.2.0
        // - Corrupted Cargo.lock with version 0.99.99
        // - Modified main.rs with marker comment

        // Call fix_workspace_lockfile with DiffStrategy::Explicit comparing HEAD to initial commit
        let diff_strategy = DiffStrategy::Explicit {
            base: initial_commit_sha.clone(),
            head: "HEAD".to_string(),
        };

        let result = fix_workspace_lockfile(&repo_path, &repo_path, &diff_strategy, false);

        assert!(
            result.is_ok(),
            "fix_workspace_lockfile failed: {:?}",
            result.err()
        );
        assert!(
            result.unwrap().success,
            "fix_workspace_lockfile returned non-success"
        );

        // Verify that main.rs was NOT modified (proving no checkout happened)
        let main_rs_after =
            std::fs::read_to_string(&main_rs_path).expect("Failed to read main.rs after fix");
        assert_eq!(
            main_rs_before, main_rs_after,
            "main.rs was modified during fix_workspace_lockfile, indicating a checkout occurred. \
             This should not happen with the blob-reading fix."
        );

        // Verify that Cargo.lock has been correctly updated
        let lockfile_content = std::fs::read_to_string(repo_path.join("Cargo.lock"))
            .expect("Failed to read Cargo.lock after fix");

        // The lock file should now reflect version 0.2.0 (from Cargo.toml at HEAD)
        assert!(
            lockfile_content.contains("version = \"0.2.0\""),
            "Cargo.lock should contain version 0.2.0 after fix, but got:\n{}",
            lockfile_content
        );

        // Ensure the corrupted version (0.99.99) is no longer present
        assert!(
            !lockfile_content.contains("0.99.99"),
            "Cargo.lock should not contain the corrupted version 0.99.99"
        );

        // Ensure the initial version (0.1.0) is also not present (it should have been updated to 0.2.0)
        assert!(
            !lockfile_content.contains("version = \"0.1.0\""),
            "Cargo.lock should not contain version 0.1.0 (it should be updated to 0.2.0)"
        );
    }

    #[tokio::test]
    async fn test_fix_lockfile_parallel_safety() {
        // This test verifies that multiple parallel invocations of fix_workspace_lockfile
        // can run safely without interfering with each other or the working directory.
        //
        // This simulates the scenario where JOB_LIMIT=3 causes multiple workspace lockfiles
        // to be fixed concurrently. With the old checkout-based approach, this would cause
        // race conditions where one task's checkout would interfere with another task's
        // file reads. With the new blob-reading approach, all tasks should succeed.

        let repo_path = create_versioned_rust_crate();
        let repo = Repository::open(&repo_path).expect("Failed to open repo");

        // Get the initial commit SHA (HEAD~3 in this repo's history)
        let initial_commit_sha = {
            let head = repo.head().unwrap();
            let commit = head.peel_to_commit().unwrap();
            // Walk back 3 commits to get the initial commit
            let initial_commit = commit
                .parent(0)
                .unwrap()
                .parent(0)
                .unwrap()
                .parent(0)
                .unwrap();
            initial_commit.id().to_string()
        };

        let main_rs_path = repo_path.join("src/main.rs");
        let expected_marker = "// This file should not be modified by fix_workspace_lockfile\n";

        // Verify the marker is present before we start
        let initial_content =
            std::fs::read_to_string(&main_rs_path).expect("Failed to read main.rs initially");
        assert!(
            initial_content.contains(expected_marker),
            "Test setup failed: marker not found in main.rs"
        );

        // Launch 3 concurrent tasks that all call fix_workspace_lockfile
        // Each task will:
        // 1. Fix the lockfile (reading from base commit)
        // 2. Simultaneously verify that main.rs still contains the marker
        // 3. This proves that no checkout happened (which would remove the marker)

        let task_count = 3;
        let mut tasks = Vec::new();

        for task_id in 0..task_count {
            let repo_path_clone = repo_path.clone();
            let initial_commit_clone = initial_commit_sha.clone();
            let main_rs_path_clone = main_rs_path.clone();
            let expected_marker_clone = expected_marker.to_string();

            let task = tokio::spawn(async move {
                println!("Task {}: Starting fix_workspace_lockfile", task_id);

                // Call fix_workspace_lockfile with DiffStrategy::Explicit
                let diff_strategy = DiffStrategy::Explicit {
                    base: initial_commit_clone,
                    head: "HEAD".to_string(),
                };

                let result = fix_workspace_lockfile(
                    &repo_path_clone,
                    &repo_path_clone,
                    &diff_strategy,
                    false,
                );

                // Check that the fix succeeded
                assert!(
                    result.is_ok(),
                    "Task {}: fix_workspace_lockfile failed: {:?}",
                    task_id,
                    result.err()
                );
                assert!(
                    result.unwrap().success,
                    "Task {}: fix_workspace_lockfile returned non-success",
                    task_id
                );

                // Verify that main.rs STILL contains the marker
                // If the old checkout-based code was running, this would intermittently fail
                // because another task's checkout would remove the marker
                let current_content = std::fs::read_to_string(&main_rs_path_clone).unwrap();
                assert!(
                    current_content.contains(&expected_marker_clone),
                    "Task {}: main.rs marker disappeared during parallel execution! \
                     This indicates a checkout occurred. Content: {:?}",
                    task_id,
                    current_content
                );

                println!(
                    "Task {}: Completed successfully, marker still present",
                    task_id
                );
                task_id
            });

            tasks.push(task);
        }

        // Wait for all tasks to complete
        let results = futures::future::join_all(tasks).await;

        // Verify all tasks succeeded
        for (i, result) in results.iter().enumerate() {
            assert!(
                result.is_ok(),
                "Task {} panicked: {:?}",
                i,
                result.as_ref().err()
            );
        }

        // Final verification: main.rs still has the marker
        let final_content =
            std::fs::read_to_string(&main_rs_path).expect("Failed to read main.rs at end");
        assert!(
            final_content.contains(expected_marker),
            "Final check: marker disappeared from main.rs"
        );

        // Verify that Cargo.lock was correctly fixed
        let lockfile_content = std::fs::read_to_string(repo_path.join("Cargo.lock"))
            .expect("Failed to read Cargo.lock after parallel fix");

        assert!(
            lockfile_content.contains("version = \"0.2.0\""),
            "Cargo.lock should contain version 0.2.0 after parallel fix"
        );
        assert!(
            !lockfile_content.contains("0.99.99"),
            "Cargo.lock should not contain the corrupted version 0.99.99"
        );
    }
}

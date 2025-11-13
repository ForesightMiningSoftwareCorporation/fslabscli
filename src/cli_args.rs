use clap::{Parser, ValueEnum};
use git2::{Object, Repository};
use serde::Serialize;
use std::fmt;

#[derive(Debug, Default, Clone, ValueEnum, Serialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum DiffStrategy {
    /// Explicit SHAs
    #[clap(skip)]
    Explicit { base: String, head: String },
    /// Compare worktree against a base branch
    #[clap(skip)]
    WorktreeVsBranch { branch: String },
    /// Compare local changes: HEAD~ vs HEAD
    /// Falls back to HEAD vs HEAD if no parent commit exists
    #[default]
    LocalChanges,
    /// No comparing, run all
    All,
}

impl DiffStrategy {
    pub fn git_commits<'r>(
        &self,
        repo: &'r Repository,
    ) -> anyhow::Result<(Object<'r>, Object<'r>)> {
        match self {
            DiffStrategy::Explicit { base, head } => {
                let head_commit = repo.revparse_single(head)?;
                let base_commit = repo.revparse_single(base)?;
                Ok((base_commit, head_commit))
            }
            DiffStrategy::LocalChanges | DiffStrategy::All => {
                // Compare HEAD~ vs HEAD (last commit changes)
                // Falls back to HEAD vs HEAD if no parent exists (single commit repo)
                let head_commit = repo.revparse_single("HEAD")?;
                let base_commit = repo
                    .revparse_single("HEAD~")
                    .unwrap_or_else(|_| head_commit.clone());
                Ok((base_commit, head_commit))
            }
            DiffStrategy::WorktreeVsBranch { branch } => {
                let head_commit = repo.revparse_single("HEAD")?;
                let base_commit = repo.revparse_single(branch)?;
                Ok((base_commit, head_commit))
            }
        }
    }
}

impl fmt::Display for DiffStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiffStrategy::All => write!(f, "all"),
            DiffStrategy::LocalChanges => write!(f, "local-changes"),
            DiffStrategy::WorktreeVsBranch { branch } => write!(f, "branch:{}", branch),
            DiffStrategy::Explicit { base, head } => write!(f, "{}..{}", base, head),
        }
    }
}

/// Relevant env vars are specified by Prow here:
/// <https://docs.prow.k8s.io/docs/jobs/#job-environment-variables>
#[derive(Debug, Parser, Default, Clone)]
pub struct DiffOptions {
    #[clap(long, env = "PULL_PULL_SHA")]
    pub head_sha: Option<String>,
    #[clap(long, env = "PULL_BASE_SHA")]
    pub base_sha: Option<String>,
    #[clap(long, env)]
    pub compare_branch: Option<String>,
    #[clap(long, env)]
    pub strategy: Option<DiffStrategy>,
}

impl DiffOptions {
    pub fn strategy(&self) -> DiffStrategy {
        if let Some(strategy) = self.strategy.clone() {
            return strategy;
        }
        match (&self.base_sha, &self.head_sha, &self.compare_branch) {
            (Some(base), Some(head), _) => DiffStrategy::Explicit {
                base: base.clone(),
                head: head.clone(),
            },
            (None, None, Some(branch)) => DiffStrategy::WorktreeVsBranch {
                branch: branch.clone(),
            },
            _ => DiffStrategy::LocalChanges,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{Oid, Signature, Time};
    use std::fs;
    use tempfile::TempDir;

    // Test helper to create a repo with commits
    fn setup_test_repo() -> (TempDir, Oid, Oid) {
        let temp_dir = TempDir::new().unwrap();
        let repo = Repository::init(temp_dir.path()).unwrap();

        // Configure repo
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "Test User").unwrap();
        config.set_str("user.email", "test@example.com").unwrap();

        let sig =
            Signature::new("Test User", "test@example.com", &Time::new(1234567890, 0)).unwrap();

        // Create first commit
        let tree_id = {
            let mut index = repo.index().unwrap();
            fs::write(temp_dir.path().join("file1.txt"), "content1").unwrap();
            index.add_path(std::path::Path::new("file1.txt")).unwrap();
            index.write().unwrap();
            index.write_tree().unwrap()
        };

        let tree = repo.find_tree(tree_id).unwrap();
        let first_commit = repo
            .commit(Some("HEAD"), &sig, &sig, "First commit", &tree, &[])
            .unwrap();

        // Create second commit
        let tree_id = {
            let mut index = repo.index().unwrap();
            fs::write(temp_dir.path().join("file2.txt"), "content2").unwrap();
            index.add_path(std::path::Path::new("file2.txt")).unwrap();
            index.write().unwrap();
            index.write_tree().unwrap()
        };

        let tree = repo.find_tree(tree_id).unwrap();
        let parent = repo.find_commit(first_commit).unwrap();
        let second_commit = repo
            .commit(Some("HEAD"), &sig, &sig, "Second commit", &tree, &[&parent])
            .unwrap();

        (temp_dir, first_commit, second_commit)
    }

    #[test]
    fn test_diff_strategy_local_changes() {
        let (temp_dir, first_oid, second_oid) = setup_test_repo();
        let repo = Repository::open(temp_dir.path()).unwrap();
        let strategy = DiffStrategy::LocalChanges;

        let (base, head) = strategy.git_commits(&repo).unwrap();

        // LocalChanges compares HEAD~ vs HEAD
        assert_eq!(base.id(), first_oid);
        assert_eq!(head.id(), second_oid);
    }

    #[test]
    fn test_diff_strategy_local_changes_single_commit() {
        // Test that LocalChanges works even with just one commit
        let temp_dir = TempDir::new().unwrap();
        let repo = Repository::init(temp_dir.path()).unwrap();

        // Configure repo
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "Test User").unwrap();
        config.set_str("user.email", "test@example.com").unwrap();

        let sig =
            Signature::new("Test User", "test@example.com", &Time::new(1234567890, 0)).unwrap();

        // Create single commit
        let tree_id = {
            let mut index = repo.index().unwrap();
            fs::write(temp_dir.path().join("file1.txt"), "content1").unwrap();
            index.add_path(std::path::Path::new("file1.txt")).unwrap();
            index.write().unwrap();
            index.write_tree().unwrap()
        };

        let tree = repo.find_tree(tree_id).unwrap();
        let commit_oid = repo
            .commit(Some("HEAD"), &sig, &sig, "Initial commit", &tree, &[])
            .unwrap();

        let strategy = DiffStrategy::LocalChanges;
        let (base, head) = strategy.git_commits(&repo).unwrap();

        // Should successfully return HEAD vs HEAD even with single commit
        assert_eq!(base.id(), commit_oid);
        assert_eq!(head.id(), commit_oid);
    }

    #[test]
    fn test_diff_strategy_explicit() {
        let (temp_dir, first_oid, second_oid) = setup_test_repo();
        let repo = Repository::open(temp_dir.path()).unwrap();
        let strategy = DiffStrategy::Explicit {
            base: first_oid.to_string(),
            head: second_oid.to_string(),
        };

        let (base, head) = strategy.git_commits(&repo).unwrap();

        assert_eq!(base.id(), first_oid);
        assert_eq!(head.id(), second_oid);
    }

    #[test]
    fn test_diff_strategy_explicit_short_sha() {
        let (temp_dir, first_oid, second_oid) = setup_test_repo();
        let repo = Repository::open(temp_dir.path()).unwrap();
        let strategy = DiffStrategy::Explicit {
            base: first_oid.to_string()[..7].to_string(),
            head: second_oid.to_string()[..7].to_string(),
        };

        let (base, head) = strategy.git_commits(&repo).unwrap();

        assert_eq!(base.id(), first_oid);
        assert_eq!(head.id(), second_oid);
    }

    #[test]
    fn test_diff_strategy_worktree_vs_branch() {
        let (temp_dir, first_oid, second_oid) = setup_test_repo();
        let repo = Repository::open(temp_dir.path()).unwrap();

        // Create a branch pointing to first commit
        repo.branch("test-branch", &repo.find_commit(first_oid).unwrap(), false)
            .unwrap();

        let strategy = DiffStrategy::WorktreeVsBranch {
            branch: "test-branch".to_string(),
        };

        let (base, head) = strategy.git_commits(&repo).unwrap();

        assert_eq!(base.id(), first_oid);
        assert_eq!(head.id(), second_oid); // HEAD is still at second commit
    }

    #[test]
    fn test_diff_strategy_explicit_invalid_sha() {
        let (temp_dir, _first_oid, _second_oid) = setup_test_repo();
        let repo = Repository::open(temp_dir.path()).unwrap();
        let strategy = DiffStrategy::Explicit {
            base: "invalid".to_string(),
            head: "also-invalid".to_string(),
        };

        let result = strategy.git_commits(&repo);
        assert!(result.is_err());
    }

    #[test]
    fn test_diff_strategy_worktree_vs_branch_invalid() {
        let (temp_dir, _first_oid, _second_oid) = setup_test_repo();
        let repo = Repository::open(temp_dir.path()).unwrap();
        let strategy = DiffStrategy::WorktreeVsBranch {
            branch: "nonexistent-branch".to_string(),
        };

        let result = strategy.git_commits(&repo);
        assert!(result.is_err());
    }

    #[test]
    fn test_display_local_changes() {
        let strategy = DiffStrategy::LocalChanges;
        assert_eq!(strategy.to_string(), "local-changes");
    }

    #[test]
    fn test_display_worktree_vs_branch() {
        let strategy = DiffStrategy::WorktreeVsBranch {
            branch: "main".to_string(),
        };
        assert_eq!(strategy.to_string(), "branch:main");
    }

    #[test]
    fn test_display_explicit() {
        let strategy = DiffStrategy::Explicit {
            base: "abc123".to_string(),
            head: "def456".to_string(),
        };
        assert_eq!(strategy.to_string(), "abc123..def456");
    }

    #[test]
    fn test_diff_options_strategy_explicit() {
        let options = DiffOptions {
            base_sha: Some("base123".to_string()),
            head_sha: Some("head456".to_string()),
            compare_branch: None,
            strategy: None,
        };

        let strategy = options.strategy();
        match strategy {
            DiffStrategy::Explicit { base, head } => {
                assert_eq!(base, "base123");
                assert_eq!(head, "head456");
            }
            _ => panic!("Expected Explicit strategy"),
        }
    }

    #[test]
    fn test_diff_options_strategy_worktree_vs_branch() {
        let options = DiffOptions {
            base_sha: None,
            head_sha: None,
            compare_branch: Some("develop".to_string()),
            strategy: None,
        };

        let strategy = options.strategy();
        match strategy {
            DiffStrategy::WorktreeVsBranch { branch } => {
                assert_eq!(branch, "develop");
            }
            _ => panic!("Expected WorktreeVsBranch strategy"),
        }
    }

    #[test]
    fn test_diff_options_strategy_local_changes_default() {
        let options = DiffOptions {
            base_sha: None,
            head_sha: None,
            compare_branch: None,
            strategy: None,
        };

        let strategy = options.strategy();
        assert!(matches!(strategy, DiffStrategy::LocalChanges));
    }

    #[test]
    fn test_diff_options_explicit_strategy_override() {
        let options = DiffOptions {
            base_sha: Some("base123".to_string()),
            head_sha: Some("head456".to_string()),
            compare_branch: Some("develop".to_string()),
            strategy: Some(DiffStrategy::LocalChanges),
        };

        let strategy = options.strategy();
        assert!(matches!(strategy, DiffStrategy::LocalChanges));
    }

    #[test]
    fn test_diff_options_explicit_sha_priority_over_branch() {
        let options = DiffOptions {
            base_sha: Some("base123".to_string()),
            head_sha: Some("head456".to_string()),
            compare_branch: Some("develop".to_string()),
            strategy: None,
        };

        let strategy = options.strategy();
        assert!(matches!(strategy, DiffStrategy::Explicit { .. }));
    }

    #[test]
    fn test_diff_options_partial_sha_ignored() {
        // Only base_sha without head_sha should fall back to LocalChanges
        let options = DiffOptions {
            base_sha: Some("base123".to_string()),
            head_sha: None,
            compare_branch: None,
            strategy: None,
        };

        let strategy = options.strategy();
        assert!(matches!(strategy, DiffStrategy::LocalChanges));
    }
}

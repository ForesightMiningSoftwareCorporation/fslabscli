use cargo_metadata::{semver::Version, Metadata, MetadataCommand, Package, PackageId};
use git2::{build::CheckoutBuilder, DiffDelta, Repository};
use ignore::gitignore::Gitignore;
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf, StripPrefixError},
};

/// The (directed acyclic) graph of crates in a multi-workspace repo.
#[derive(Clone, Debug)]
pub struct CrateGraph {
    repo_root: PathBuf,
    workspaces: Vec<Workspace>,
    dependencies: DependencyGraph,
}

impl CrateGraph {
    /// Finds all [`Workspace`]s (recursively) in `repo_root` that contain a
    /// valid cargo manifest.
    ///
    /// If a directory contains a file named ".skip_ci", then that directory
    /// will be excluded from the search.
    ///
    /// # Errors
    ///
    /// Returns error if a manifest is found that cannot be parsed.
    pub fn new(repo_root: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let repo_root = repo_root.into();
        let mut workspaces = Vec::new();
        let (ignore, err) = Gitignore::new(repo_root.join(".gitignore"));
        if let Some(err) = err {
            eprintln!("Failed to find .gitignore: {err}");
        }
        Self::new_recursive(&repo_root, &ignore, &repo_root, &mut workspaces)?;
        workspaces.sort_by(|r1, r2| r1.path.cmp(&r2.path));
        let dependencies = DependencyGraph::new(&repo_root, &workspaces);
        Ok(Self {
            repo_root,
            workspaces,
            dependencies,
        })
    }

    fn new_recursive(
        repo_root: &Path,
        ignore: &Gitignore,
        dir: &Path,
        workspaces: &mut Vec<Workspace>,
    ) -> anyhow::Result<()> {
        if let Some(name) = dir.file_name() {
            if name == ".git" {
                return Ok(());
            }
        }
        if ignore.matched(dir, true).is_ignore() {
            return Ok(());
        }
        if std::fs::exists(dir.join(".skip_ci"))? {
            return Ok(());
        }

        let manifest_path = dir.join("Cargo.toml");
        if std::fs::exists(&manifest_path)? {
            // Found a manifest. Get metadata.

            let metadata = MetadataCommand::new().current_dir(dir).exec()?;

            let has_explicit_members = if metadata.root_package().is_some() {
                metadata.workspace_members.len() > 1
            } else {
                !metadata.workspace_members.is_empty()
            };
            workspaces.push(Workspace {
                path: relative_path(repo_root, dir)
                    .expect("Subdirectory must have ancestor path prefix")
                    .into(),
                metadata,
            });

            // Assume that the workspace members are all we needed to find.
            if has_explicit_members {
                return Ok(());
            }
        }

        // No workspace manifest in this directory. Keep searching.
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                Self::new_recursive(repo_root, ignore, &entry.path(), workspaces)?;
            }
        }

        Ok(())
    }

    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    pub fn workspaces(&self) -> &[Workspace] {
        &self.workspaces
    }

    pub fn dependency_graph(&self) -> &DependencyGraph {
        &self.dependencies
    }

    /// All cargo packages in the repo.
    pub fn packages(&self) -> impl Iterator<Item = &Package> {
        self.workspaces()
            .iter()
            .flat_map(|w| w.metadata.workspace_packages())
    }

    /// Determines which packages have changed between `old_rev` and `new_rev`. (Un)Staged changes are considered
    pub fn changed_packages(&self, old_rev: &str, new_rev: &str) -> anyhow::Result<Vec<PathBuf>> {
        // Create git diff between revisions.
        let repository = Repository::open(&self.repo_root)?;
        let old_commit = repository.revparse_single(old_rev)?;
        let new_commit = repository.revparse_single(new_rev)?;
        let old_tree = old_commit.peel_to_tree()?;
        let new_tree = new_commit.peel_to_tree()?;

        // Get index and working directory state
        let index = repository.index()?;

        // Create diffs:
        // - one between old_rev and new_rev,
        // - and another between new_rev and current state staged
        // - and another between new_rev and current state unstaged
        let diff_old_new = repository.diff_tree_to_tree(Some(&old_tree), Some(&new_tree), None)?;
        let diff_new_staged = repository.diff_tree_to_index(Some(&new_tree), Some(&index), None)?;
        let diff_new_unstaged = repository.diff_index_to_workdir(Some(&index), None)?;

        // Check each package path against each file paths in git diff.
        let mut changed = Vec::new();
        for package in self.packages() {
            let package_path = package_path(&self.repo_root, package).into_owned();

            // If package_path is ".", treat it as the entire repo
            let is_repo_root = package_path == Path::new(".");

            let mut file_cb = |delta: DiffDelta, _: f32| -> bool {
                for delta_path in [delta.old_file().path(), delta.new_file().path()]
                    .into_iter()
                    .flatten()
                {
                    if is_repo_root || delta_path.starts_with(&package_path) {
                        changed.push(package_path.clone());
                        return false;
                    }
                }
                true
            };
            // Returning early from a callback will propagate an error for some
            // reason. Ignore it.
            let _ = diff_old_new.foreach(&mut file_cb, None, None, None);
            let _ = diff_new_staged.foreach(&mut file_cb, None, None, None);
            let _ = diff_new_unstaged.foreach(&mut file_cb, None, None, None);
        }
        changed.sort();
        changed.dedup(); // Remove duplicates if package changed in multiple diffs

        Ok(changed)
    }

    /// Fix mistakes in all workspace `Cargo.lock` files.
    ///
    /// Performs the following:
    ///
    /// 1. Restore all `Cargo.lock` files to their state at `base_rev`.
    /// 2. Run `cargo update --workspace` in each workspace to ensure
    ///    the `Cargo.lock` files are updated to reflect any changes in
    ///    `Cargo.toml`s.
    ///
    /// Because of the `--workspace` flag, only minimal updates are
    /// performed. This is done to avoid letting SemVer violations from
    /// dependencies slip into CI.
    ///
    /// Any workspaces containing a ".no_cargo_lock" sentinel file will be skipped.
    pub fn fix_lock_files(&self, diff: Option<DiffRevs>) -> anyhow::Result<()> {
        let repo = Repository::open(self.repo_root())?;

        let check_workspaces: Vec<_> = self
            .workspaces()
            .iter()
            .filter(|w| !w.path.join(".no_cargo_lock").exists())
            .map(|w| self.repo_root().join(&w.path))
            .collect();

        if let Some(DiffRevs { head_rev, base_rev }) = diff {
            // Do this resolution before making any changes to the repo, so e.g.
            // "HEAD" is correct.
            let head_commit = repo.revparse_single(head_rev)?;
            let base_commit = repo.revparse_single(base_rev)?;

            // Restore all `Cargo.lock` files to their state at `base_rev`.
            println!("checking out {}", base_commit.id());
            let mut builder = CheckoutBuilder::new();
            builder.force();
            repo.checkout_tree(&base_commit, Some(&mut builder))?;
            let mut orig_lockfiles = Vec::new();
            for workspace_path in &check_workspaces {
                let lock_path = workspace_path.join("Cargo.lock");
                match std::fs::read_to_string(&lock_path) {
                    Ok(contents) => {
                        orig_lockfiles.push((lock_path.clone(), contents));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        continue;
                    }
                    Err(e) => {
                        return Err(e.into());
                    }
                }
            }
            println!("checking out {}", head_commit.id());
            let mut builder = CheckoutBuilder::new();
            builder.force();
            repo.checkout_tree(&head_commit, Some(&mut builder))?;
            for (lock_path, contents) in orig_lockfiles {
                println!(
                    "Reverting {lock_path:?} to contents at {}",
                    base_commit.id()
                );
                std::fs::write(&lock_path, contents)?;
            }
        }

        // Run `cargo update -w` in each workspace to ensure the `Cargo.lock`
        // files are updated to reflect any changes in `Cargo.toml`s.
        for workspace_path in check_workspaces {
            println!("Running 'cargo update --workspace' in {workspace_path:?}");
            let output = std::process::Command::new("cargo")
                .arg("update")
                .arg("--workspace")
                .current_dir(&workspace_path)
                .output()?;
            assert!(output.status.success(), "{output:?}");
        }

        Ok(())
    }
}

pub struct DiffRevs<'a> {
    pub head_rev: &'a str,
    pub base_rev: &'a str,
}

/// A crate that either:
///
/// - is not a workspace member (a standalone package)
/// - has a manifest with a `[workspace]` table
#[derive(Clone, Debug)]
pub struct Workspace {
    pub path: PathBuf,
    pub metadata: Metadata,
}

impl Workspace {
    #[cfg(test)]
    pub fn root_package_key(&self) -> Option<PackageKey> {
        self.metadata.root_package().map(From::from)
    }

    #[cfg(test)]
    pub fn member_package_keys(&self) -> Vec<PackageKey> {
        self.metadata
            .workspace_packages()
            .into_iter()
            .map(From::from)
            .collect()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageKey {
    pub name: String,
    pub version: Version,
}

impl From<&Package> for PackageKey {
    fn from(p: &Package) -> Self {
        PackageKey {
            name: p.name.clone(),
            version: p.version.clone(),
        }
    }
}

/// The dependency graph of **local** crates from [`CrateGraph`].
#[derive(Clone, Debug, Default)]
pub struct DependencyGraph {
    path_to_id: HashMap<PathBuf, PackageId>,
    id_to_path: HashMap<PackageId, PathBuf>,

    /// "KEY depends on VALUE"
    dependencies: HashMap<PackageId, Vec<PackageId>>,
    /// "KEY is depended on by VALUE"
    reverse_dependencies: HashMap<PackageId, Vec<PackageId>>,
}

impl DependencyGraph {
    pub fn new(repo_root: &Path, workspaces: &[Workspace]) -> Self {
        let mut me = Self::default();

        for w in workspaces {
            // Create the 1:1 bidirectional map between path and package ID.
            for p in w.metadata.workspace_packages() {
                let p_dir_path = package_path(repo_root, p).into_owned();
                me.path_to_id.insert(p_dir_path.clone(), p.id.clone());
                me.id_to_path.insert(p.id.clone(), p_dir_path);
                me.dependencies.insert(p.id.clone(), Default::default());
                me.reverse_dependencies
                    .insert(p.id.clone(), Default::default());
            }

            // Create the M:N bidirectional dependency map between package IDs.
            let resolve = w.metadata.resolve.as_ref().unwrap();
            for node in &resolve.nodes {
                if me.id_to_path.contains_key(&node.id) {
                    let deps = me.dependencies.get_mut(&node.id).unwrap();
                    for d in &node.dependencies {
                        if me.id_to_path.contains_key(d) {
                            let reverse_deps = me.reverse_dependencies.get_mut(d).unwrap();
                            deps.push(d.clone());
                            reverse_deps.push(node.id.clone());
                        }
                    }
                }
            }
        }

        me
    }

    /// Given a set `seed` of **relative** paths to packages into the repo,
    /// returns the superset of packages that directly or indirectly depend on
    /// one of the packages in `seed`.
    ///
    /// # Panics
    ///
    /// If any paths in `seed` are not recognized by the dependency graph.
    pub fn reverse_closure<'a>(&self, seed: impl IntoIterator<Item = &'a Path>) -> Vec<PathBuf> {
        let mut closure = HashSet::new();
        let mut to_visit: Vec<_> = seed
            .into_iter()
            .map(|path| self.path_to_id[path].clone())
            .collect();
        while let Some(id) = to_visit.pop() {
            if closure.insert(id.clone()) {
                for dependant in &self.reverse_dependencies[&id] {
                    to_visit.push(dependant.clone());
                }
            }
        }
        let mut closure: Vec<_> = closure
            .into_iter()
            .map(|id| self.id_to_path[&id].clone())
            .collect();
        closure.sort();
        closure
    }
}

/// The path to `package`, relative to `repo_root`.
pub fn package_path<'a>(repo_root: &Path, package: &'a Package) -> Cow<'a, Path> {
    relative_path(
        repo_root,
        package.manifest_path.as_std_path().parent().unwrap(),
    )
    .expect("Workspace package manifest must be relative to repo root")
}

fn relative_path<'a>(root: &Path, path: &'a Path) -> Result<Cow<'a, Path>, StripPrefixError> {
    // In MacOs temp folders can be /var/private or /private (symlink between the two)
    let canonical_root = root
        .canonicalize()
        .expect("Failed to canonicalize root path");
    let canonical_path = path
        .canonicalize()
        .expect("Failed to canonicalize package path");

    match canonical_path.strip_prefix(&canonical_root)? {
        p if p == Path::new("") => Ok(Cow::Owned(PathBuf::from("."))),
        stripped => Ok(Cow::Owned(stripped.to_path_buf())), // Ensure we return an owned path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn test_discover_standalone_workspace() {
        let repo = initialize_repo().join("standalone");

        let graph = CrateGraph::new(&repo).unwrap();
        let workspaces = graph.workspaces();
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].path, Path::new("."));
        assert_eq!(
            workspaces[0].member_package_keys(),
            vec![PackageKey {
                name: "standalone".into(),
                version: "0.1.0".parse().unwrap()
            }]
        );
    }

    #[test]
    fn test_discover_many_workspaces() {
        let repo = initialize_repo();

        let graph = CrateGraph::new(&repo).unwrap();
        let workspaces = graph.workspaces();
        assert_eq!(workspaces.len(), 5);
        let mut i = workspaces.iter();
        let bar = i.next().unwrap();
        let bar_nested = i.next().unwrap();
        let baz = i.next().unwrap();
        let foo = i.next().unwrap();
        let _standalone = i.next().unwrap();

        // bar is a standalone package (implicit workspace).
        assert_eq!(bar.path, Path::new("bar"));
        assert_eq!(
            bar.root_package_key(),
            Some(PackageKey {
                name: "bar".into(),
                version: "0.1.0".parse().unwrap()
            })
        );

        // bar only has a root package, but it contains a nested workspace.
        assert_eq!(bar_nested.path, Path::new("bar").join("bar_nested"));
        assert_eq!(
            bar_nested.root_package_key(),
            Some(PackageKey {
                name: "bar_nested".into(),
                version: "0.1.0".parse().unwrap()
            })
        );

        // baz is a workspace with one member.
        assert_eq!(baz.path, Path::new("baz"));
        assert_eq!(baz.root_package_key(), None);
        assert_eq!(
            baz.member_package_keys(),
            vec![PackageKey {
                name: "baz_member1".into(),
                version: "0.1.0".parse().unwrap()
            }]
        );

        // foo is a workspace with a root package and one member.
        assert_eq!(foo.path, Path::new("foo"));
        let foo_package_key = PackageKey {
            name: "foo".into(),
            version: "0.1.0".parse().unwrap(),
        };
        assert_eq!(foo.root_package_key(), Some(foo_package_key.clone()));
        assert_eq!(
            foo.member_package_keys(),
            vec![
                foo_package_key,
                PackageKey {
                    name: "foo_member1".into(),
                    version: "0.1.0".parse().unwrap(),
                }
            ]
        );

        // nothing depends on foo
        let closure = graph.dependency_graph().reverse_closure([Path::new("foo")]);
        assert_eq!(closure, [Path::new("foo")]);

        // foo --> baz/member1 --> bar
        let closure = graph.dependency_graph().reverse_closure([Path::new("bar")]);
        assert_eq!(
            closure,
            [
                PathBuf::from("bar"),
                Path::new("baz").join("baz_member1"),
                PathBuf::from("foo"),
            ]
        );
    }

    #[test]
    fn test_detect_changed_packages() {
        let repo = initialize_repo();
        let graph = CrateGraph::new(&repo).unwrap();

        // These revision strings rely on an understanding of the test repo's git log.
        // We know that the most recent revision makes changes to files in foo and bar.
        let changed = graph.changed_packages("HEAD~", "HEAD").unwrap();
        assert_eq!(changed, [Path::new("bar"), Path::new("foo")]);
    }

    #[test]
    fn test_detect_changed_package_single_rust_crate() {
        let repo = create_simple_rust_crate();
        let graph = CrateGraph::new(&repo).unwrap();

        let changed = graph.changed_packages("HEAD~", "HEAD").unwrap();
        assert_eq!(changed, [Path::new(".")]);
    }

    #[test]
    fn test_detect_changed_package_unstaged_file() {
        let repo = create_simple_rust_crate();

        let graph = CrateGraph::new(&repo).unwrap();
        modify_file(&repo, "src/lib.rs", "pub fn new_function_again() {}");

        let changed = graph.changed_packages("HEAD", "HEAD").unwrap();
        assert_eq!(changed, [Path::new(".")]);
    }

    #[test]
    fn test_detect_changed_package_staged_file() {
        let repo = create_simple_rust_crate();

        let graph = CrateGraph::new(&repo).unwrap();
        modify_file(&repo, "src/lib.rs", "pub fn new_function_again() {}");
        stage_file(&repo, "src/lib.rs");

        let changed = graph.changed_packages("HEAD", "HEAD").unwrap();
        assert_eq!(changed, [Path::new(".")]);
    }

    #[test]
    fn test_fix_lock_files() {
        let repo = initialize_repo();
        let graph = CrateGraph::new(&repo).unwrap();

        // Remove lock files created from running cargo-metadata.
        for workspace in graph.workspaces() {
            std::fs::remove_file(repo.join(&workspace.path).join("Cargo.lock")).unwrap();
        }

        // This diff shouldn't actually affect the lock files, but it at least
        // gives better code coverage.
        let diff = DiffRevs {
            head_rev: "HEAD",
            base_rev: "HEAD~",
        };
        graph.fix_lock_files(Some(diff)).unwrap();

        // Assert that lock files have been created.
        for workspace in graph.workspaces() {
            assert!(
                repo.join(&workspace.path).join("Cargo.lock").exists(),
                "{:?}",
                workspace.path
            );
        }
    }

    fn initialize_repo() -> PathBuf {
        let tmp = assert_fs::TempDir::new()
            .unwrap()
            .into_persistent()
            .to_path_buf();
        println!("Initializing test repo in {tmp:?}");
        let test_data = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test_data");
        let script = test_data.join("create_repo.sh");
        let output = Command::new("bash")
            .arg(script)
            .arg(test_data)
            .current_dir(&tmp)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        tmp
    }

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
            .arg("--lib")
            .arg("--name")
            .arg("test-lib")
            .current_dir(&tmp)
            .output()
            .expect("Failed to create simple crate");

        // Stage and commit initial crate
        commit_all_changes(&tmp, "Initial commit");
        // Create Second Commit
        modify_file(&tmp, "src/lib.rs", "pub fn new_function() {}");
        stage_file(&tmp, "src/lib.rs");
        commit_repo(&tmp, "Added new function");
        tmp
    }
    fn commit_all_changes(repo_path: &PathBuf, message: &str) {
        stage_all(repo_path);
        commit_repo(repo_path, message);
    }

    fn modify_file(repo_path: &Path, file_path: &str, content: &str) {
        let full_path = repo_path.join(file_path);

        // Ensure the directory exists
        std::fs::create_dir_all(full_path.parent().unwrap()).expect("Failed to create directories");

        // Modify the file
        std::fs::write(&full_path, content).expect("Failed to write to file");
    }

    fn stage_file(repo_path: &PathBuf, file_path: &str) {
        let repo = Repository::open(repo_path).expect("Failed to open repo");
        let mut index = repo.index().unwrap();
        index
            .add_all([file_path].iter(), git2::IndexAddOption::DEFAULT, None)
            .expect("Failed to add files to index");
        index.write().expect("Failed to write index");
    }

    fn stage_all(repo_path: &PathBuf) {
        stage_file(repo_path, "*");
    }

    fn commit_repo(repo_path: &PathBuf, commit_message: &str) {
        let repo = Repository::open(repo_path).expect("Failed to open repo");
        let mut index = repo.index().unwrap();

        let oid = index.write_tree().unwrap();
        let signature = repo.signature().unwrap();
        let tree = repo.find_tree(oid).unwrap();
        let parent_commit = repo
            .head()
            .ok()
            .and_then(|r| r.target())
            .and_then(|oid| repo.find_commit(oid).ok());

        if let Some(parent) = parent_commit {
            repo.commit(
                Some("HEAD"),
                &signature,
                &signature,
                commit_message,
                &tree,
                &[&parent],
            )
            .unwrap();
        } else {
            repo.commit(
                Some("HEAD"),
                &signature,
                &signature,
                commit_message,
                &tree,
                &[],
            )
            .unwrap();
        };
    }
}

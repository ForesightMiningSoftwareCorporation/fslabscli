#[cfg(test)]
use cargo_metadata::semver::Version;
use cargo_metadata::{CargoOpt, DependencyKind, Metadata, MetadataCommand, Package, PackageId};
use git2::{DiffDelta, Object, Repository};
use ignore::gitignore::Gitignore;
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf, StripPrefixError},
};

use crate::{cli_args::DiffStrategy, utils::get_registry_env};

/// The (directed acyclic) graph of crates in a multi-workspace repo.
#[derive(Clone, Debug)]
pub struct CrateGraph {
    repo_root: PathBuf,
    workspaces: Vec<Workspace>,
    pub dependencies: DependencyGraph,
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
    pub fn new(
        repo_root: impl Into<PathBuf>,
        main_registry: impl Into<String> + Clone,
        dep_kind: Option<DependencyKind>,
    ) -> anyhow::Result<Self> {
        let repo_root = repo_root.into();
        let mut workspaces = Vec::new();
        let (ignore, err) = Gitignore::new(repo_root.join(".gitignore"));
        if let Some(err) = err {
            eprintln!("Failed to find .gitignore: {err}");
        }
        let envs = get_registry_env(main_registry.clone().into());
        Self::new_recursive(&repo_root, &ignore, &repo_root, &mut workspaces, &envs)?;
        workspaces.sort_by(|r1, r2| r1.path.cmp(&r2.path));
        let dependencies = DependencyGraph::new(&repo_root, &workspaces, dep_kind);
        if let Some(cycles) = dependencies.detect_cycles() {
            return Err(anyhow::anyhow!("Cycle detected: {:?}", cycles));
        }
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
        envs: &HashMap<String, String>,
    ) -> anyhow::Result<()> {
        if let Some(name) = dir.file_name()
            && name == ".git"
        {
            return Ok(());
        }
        if ignore.matched(dir, true).is_ignore() {
            return Ok(());
        }
        if std::fs::exists(dir.join(".skip_ci"))? {
            return Ok(());
        }

        let manifest_path = dir.join("Cargo.toml");
        let lock_path = dir.join("Cargo.lock");
        let orig_lock_content = match std::fs::exists(&lock_path)? {
            true => Some(std::fs::read_to_string(&lock_path)?),
            false => None,
        };

        if std::fs::exists(&manifest_path)? {
            // Found a manifest. Get metadata.
            let mut command = MetadataCommand::new();
            command.current_dir(dir);
            command.features(CargoOpt::AllFeatures);
            for (k, v) in envs {
                command.env(k, v);
            }
            let metadata = command.exec()?;

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
            // crate_graph runs `cargo metadata` under the hood. This can updates the Cargo.lock
            // we want to track that behavior
            let updated_lock_content = match std::fs::exists(&lock_path)? {
                true => Some(std::fs::read_to_string(&lock_path)?),
                false => None,
            };
            match (orig_lock_content, updated_lock_content) {
                (Some(orig), Some(updated)) => {
                    if orig != updated {
                        // We need to revert the old file
                        std::fs::write(&lock_path, orig)?;
                    }
                }
                (Some(orig), None) => {
                    // We need to revert the old file
                    std::fs::write(&lock_path, orig)?;
                }
                (None, Some(_)) => {
                    // We need to delete the new file that got created
                    std::fs::remove_file(&lock_path)?;
                }
                (None, None) => {} // Nothing to do
            };
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
                Self::new_recursive(repo_root, ignore, &entry.path(), workspaces, envs)?;
            }
        }

        Ok(())
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
    pub fn changed_packages<'r>(
        &self,
        repository: &'r Repository,
        old_commit: Object<'r>,
        new_commit: Object<'r>,
        diff_strategy: &DiffStrategy,
    ) -> anyhow::Result<Vec<PathBuf>> {
        // Create git diff between revisions.
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
        let mut packages = self.packages().collect::<Vec<_>>();
        // Sort packages so that we look at the subpackages first:
        // - workspace/crate_a/subcrate
        // - workspace/crate_a
        // - workspace/crate_b
        // - workspace
        packages.sort_by_key(|package| package_path(&self.repo_root, package).iter().count());
        packages.reverse();

        // For each change, find the most specific package modified
        let mut file_cb = |delta: DiffDelta, _: f32| -> bool {
            for delta_path in [delta.old_file().path(), delta.new_file().path()]
                .into_iter()
                .flatten()
            {
                for package in &packages {
                    let package_path = package_path(&self.repo_root, package).into_owned();

                    // If package_path is ".", treat it as the entire repo
                    let is_repo_root = package_path == Path::new(".");

                    if delta_path.ends_with("rust-toolchain.toml") {
                        // We have a special case, the `rust-toolchain.toml` file, if it changed, everything should be considered as changed
                        changed.push(package_path.clone());
                        continue;
                    }
                    if is_repo_root
                        || delta_path.starts_with(&package_path)
                        || delta_path.ends_with("rust-toolchain.toml")
                    {
                        changed.push(package_path.clone());
                        // Stop processing this change, we found the most specific package impacted
                        // Returning true will continue matching other file changed in case a commit targets several packages
                        return true;
                    }
                }
            }
            true
        };
        // Returning early from a callback will propagate an error for some
        // reason. Ignore it.
        let _ = diff_old_new.foreach(&mut file_cb, None, None, None);

        // Only check changes on staged and unstaged when not using the explicit strategy
        match diff_strategy {
            DiffStrategy::Explicit { .. } => {}
            _ => {
                let _ = diff_new_staged.foreach(&mut file_cb, None, None, None);
                let _ = diff_new_unstaged.foreach(&mut file_cb, None, None, None);
            }
        }

        changed.sort();
        changed.dedup(); // Remove duplicates if package changed in multiple diffs

        Ok(changed)
    }
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
#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageKey {
    pub name: String,
    pub version: Version,
}

#[cfg(test)]
impl From<&Package> for PackageKey {
    fn from(p: &Package) -> Self {
        PackageKey {
            name: p.name.to_string(),
            version: p.version.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct DependencyInstance {
    pub kind: DependencyKind,
    // Refer to a path only dep
    pub is_local: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Dependency {
    pub package_id: PackageId,
    pub instances: Vec<DependencyInstance>,
}

/// The dependency graph of **local** crates from [`CrateGraph`].
#[derive(Clone, Debug, Default)]
pub struct DependencyGraph {
    path_to_id: HashMap<PathBuf, PackageId>,
    id_to_path: HashMap<PackageId, PathBuf>,
    id_to_package: HashMap<PackageId, Package>,

    /// "KEY depends on VALUE"
    pub dependencies: HashMap<PackageId, Vec<Dependency>>,
    /// "KEY is depended on by VALUE"
    reverse_dependencies: HashMap<PackageId, Vec<PackageId>>,
}

impl DependencyGraph {
    pub fn new(
        repo_root: &Path,
        workspaces: &[Workspace],
        dep_kind: Option<DependencyKind>,
    ) -> Self {
        let mut me = Self::default();

        for w in workspaces {
            // Create the 1:1 bidirectional map between path and package ID.
            for p in w.metadata.workspace_packages() {
                let p_dir_path = package_path(repo_root, p).into_owned();
                me.path_to_id.insert(p_dir_path.clone(), p.id.clone());
                me.id_to_path.insert(p.id.clone(), p_dir_path);
                me.id_to_package.insert(p.id.clone(), p.clone());
                me.dependencies.insert(p.id.clone(), Default::default());
                me.reverse_dependencies
                    .insert(p.id.clone(), Default::default());
            }
        }

        for w in workspaces {
            // Create the M:N bidirectional dependency map between package IDs.
            let resolve = w.metadata.resolve.as_ref().unwrap();
            for node in &resolve.nodes {
                if me.id_to_path.contains_key(&node.id) {
                    let self_package = me.id_to_package.get(&node.id);
                    let deps = me.dependencies.get_mut(&node.id).unwrap();
                    for node_dep in &node.deps {
                        let dep_id = &node_dep.pkg;
                        let instances = node_dep
                            .dep_kinds
                            .iter()
                            .map(|k| {
                                let dep_package = me.id_to_package.get(dep_id);
                                let is_local = match dep_package {
                                    Some(p) => match p.source {
                                        Some(_) => false,
                                        None => self_package
                                            .and_then(|p| {
                                                p.dependencies
                                                    .iter()
                                                    .find(|dependency| {
                                                        dependency.rename.as_ref()
                                                            == Some(&node_dep.name)
                                                            || (dependency.rename.is_none()
                                                                && dependency.name == node_dep.name)
                                                            || format!("{}", node_dep.pkg)
                                                                .starts_with(&dependency.name)
                                                    })
                                                    .map(|c| c.registry.is_none())
                                            })
                                            .unwrap_or(false),
                                    },
                                    None => true,
                                };
                                DependencyInstance {
                                    kind: k.kind,
                                    is_local,
                                }
                            })
                            .collect();
                        let is_accepted_dep = match dep_kind {
                            Some(kind) => {
                                let mut is_accepted_dep = false;
                                for dep_kind in &node_dep.dep_kinds {
                                    if dep_kind.kind == kind {
                                        is_accepted_dep = true;
                                    }
                                }
                                is_accepted_dep
                            }
                            None => true,
                        };
                        if is_accepted_dep && me.id_to_path.contains_key(dep_id) {
                            let reverse_deps = me.reverse_dependencies.get_mut(dep_id).unwrap();
                            let dep = Dependency {
                                package_id: dep_id.clone(),
                                instances,
                            };
                            deps.push(dep);
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

    pub fn get_transitive_dependencies(&self, root: PackageId) -> HashSet<PackageId> {
        let mut visited = HashSet::new();
        let mut stack = vec![root];
        while let Some(current_package) = stack.pop() {
            if let Some(deps) = self.dependencies.get(&current_package) {
                for dep in deps {
                    let package_id = dep.package_id.clone();
                    if !visited.contains(&package_id) {
                        visited.insert(package_id.clone());
                        stack.push(package_id.clone());
                    }
                }
            }
        }
        visited
    }

    pub fn detect_cycles(&self) -> Option<Vec<PackageId>> {
        // used to prevent duplicate cycle trasversal
        let mut visited = HashSet::new();
        // used to detect if there is a cycle in the current trasversal
        let mut recursion_stack = HashSet::new();
        let mut cycle_path = Vec::new();

        // Helper function for DFS traversal, returns true if a cycle is detected
        fn dfs(
            graph: &DependencyGraph,
            package_id: &PackageId,
            visited: &mut HashSet<PackageId>,
            recursion_stack: &mut HashSet<PackageId>,
            cycle_path: &mut Vec<PackageId>,
        ) -> bool {
            if recursion_stack.contains(package_id) {
                // Cycle detected, record the cycle path
                cycle_path.push(package_id.clone());
                return true;
            }

            if visited.contains(package_id) {
                return false;
            }

            // Mark the package as visited
            visited.insert(package_id.clone());
            recursion_stack.insert(package_id.clone());

            // Traverse the dependencies
            if let Some(deps) = graph.dependencies.get(package_id) {
                for dep in deps {
                    if dep
                        .instances
                        .iter()
                        .all(|k| k.kind == DependencyKind::Development && k.is_local)
                    {
                        // if it's only a dev dep, we can ignore it
                        continue;
                    }
                    if dfs(graph, &dep.package_id, visited, recursion_stack, cycle_path) {
                        cycle_path.push(package_id.clone());
                        return true;
                    }
                }
            }

            // Backtrack
            recursion_stack.remove(package_id);
            false
        }

        // Try to detect cycles for each package in the graph
        for package in self.dependencies.keys() {
            if !visited.contains(&package.clone())
                && dfs(
                    self,
                    package,
                    &mut visited,
                    &mut recursion_stack,
                    &mut cycle_path,
                )
            {
                cycle_path.reverse();
                return Some(cycle_path);
            }
        }

        None // No cycle detected
    }
}

/// The path to `package`, relative to `repo_root`.
fn package_path<'a>(repo_root: &Path, package: &'a Package) -> Cow<'a, Path> {
    relative_path(
        repo_root,
        package.manifest_path.as_std_path().parent().unwrap(),
    )
    .expect("Workspace package manifest must be relative to repo root")
}

fn relative_path<'a>(root: &Path, path: &'a Path) -> Result<Cow<'a, Path>, StripPrefixError> {
    // In MacOs temp folders can be /var/private or /private (symlink between the two)
    let canonical_root = dunce::canonicalize(root).expect("Failed to canonicalize root path");
    let canonical_path = dunce::canonicalize(path).expect("Failed to canonicalize package path");

    match canonical_path.strip_prefix(&canonical_root)? {
        p if p == Path::new("") => Ok(Cow::Owned(PathBuf::from("."))),
        stripped => Ok(Cow::Owned(stripped.to_path_buf())), // Ensure we return an owned path
    }
}

/// Finds the root directory of a Git repository given any path inside it.
/// Returns `None` if no `.git` directory is found.
pub fn find_git_root(start: impl AsRef<Path>) -> Option<PathBuf> {
    let mut current = std::fs::canonicalize(start).ok()?;
    loop {
        // Check for `.git` directory or file (could be a file in worktrees or submodules)
        let git_dir = current.join(".git");
        if git_dir.exists() {
            return Some(current);
        }

        // Move up one directory level
        if !current.pop() {
            // Reached filesystem root
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::utils::test::{
        FAKE_REGISTRY, commit_all_changes, commit_repo, create_complex_workspace, modify_file,
        stage_file,
    };

    use super::*;
    use std::{fs::OpenOptions, io::Write, process::Command};

    #[test]
    fn test_discover_standalone_workspace() {
        let repo = initialize_repo().join("standalone");

        let graph = CrateGraph::new(&repo, "", None).unwrap();
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

        let graph = CrateGraph::new(&repo, "", None).unwrap();
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
        let repo_root = initialize_repo();
        let graph = CrateGraph::new(&repo_root, "", None).unwrap();
        let repo = Repository::open(repo_root).unwrap();
        // Use LocalChanges strategy to compare HEAD~ vs HEAD
        let diff_strategy = DiffStrategy::LocalChanges;
        let (base_commit, head_commit) = diff_strategy.git_commits(&repo).unwrap();

        // These revision strings rely on an understanding of the test repo's git log.
        // We know that the most recent revision makes changes to files in foo and bar.
        let changed = graph
            .changed_packages(&repo, base_commit, head_commit, &diff_strategy)
            .unwrap();
        assert_eq!(changed, [Path::new("bar"), Path::new("foo")]);
    }

    #[test]
    fn test_detect_changed_package_single_rust_crate() {
        let repo_root = create_simple_rust_crate();
        let graph = CrateGraph::new(&repo_root, "", None).unwrap();
        let repo = Repository::open(repo_root).unwrap();
        // Use LocalChanges strategy to compare HEAD~ vs HEAD
        let diff_strategy = DiffStrategy::LocalChanges;
        let (base_commit, head_commit) = diff_strategy.git_commits(&repo).unwrap();
        let changed = graph
            .changed_packages(&repo, base_commit, head_commit, &diff_strategy)
            .unwrap();

        assert_eq!(changed, [Path::new(".")]);
    }

    #[test]
    fn test_detect_changed_package_unstaged_file() {
        let repo_root = create_simple_rust_crate();
        let graph = CrateGraph::new(&repo_root, "", None).unwrap();
        modify_file(&repo_root, "src/lib.rs", "pub fn new_function_again() {}");
        let repo = Repository::open(repo_root).unwrap();
        let diff_strategy = DiffStrategy::WorktreeVsBranch {
            branch: "HEAD".to_string(),
        };
        let (base_commit, head_commit) = diff_strategy.git_commits(&repo).unwrap();
        let changed = graph
            .changed_packages(&repo, base_commit, head_commit, &diff_strategy)
            .unwrap();

        assert_eq!(changed, [Path::new(".")]);
    }

    #[test]
    fn test_detect_changed_package_staged_file() {
        let repo_root = create_simple_rust_crate();
        let graph = CrateGraph::new(&repo_root, "", None).unwrap();
        modify_file(&repo_root, "src/lib.rs", "pub fn new_function_again() {}");
        stage_file(&repo_root, "src/lib.rs");

        let repo = Repository::open(repo_root).unwrap();
        let diff_strategy = DiffStrategy::WorktreeVsBranch {
            branch: "HEAD".to_string(),
        };
        let (base_commit, head_commit) = diff_strategy.git_commits(&repo).unwrap();
        let changed = graph
            .changed_packages(&repo, base_commit, head_commit, &diff_strategy)
            .unwrap();
        assert_eq!(changed, [Path::new(".")]);
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
    #[test]
    fn test_get_transitive_dependencies() {
        // Set up a simple graph
        let mut graph = DependencyGraph::default();

        // Example data (add your actual data)
        let package_1 = PackageId {
            repr: "1".to_string(),
        };
        let package_2 = PackageId {
            repr: "2".to_string(),
        };
        let package_3 = PackageId {
            repr: "3".to_string(),
        };
        let package_4 = PackageId {
            repr: "4".to_string(),
        };

        // Package 1 depends on Package 2 and Package 3
        graph.dependencies.insert(
            package_1.clone(),
            vec![
                Dependency {
                    package_id: package_2.clone(),
                    instances: vec![],
                },
                Dependency {
                    package_id: package_3.clone(),
                    instances: vec![],
                },
            ],
        );

        // Package 2 depends on Package 4
        graph.dependencies.insert(
            package_2.clone(),
            vec![Dependency {
                package_id: package_4.clone(),
                instances: vec![],
            }],
        );

        // Package 3 has no dependencies
        graph.dependencies.insert(package_3.clone(), vec![]);

        // Package 4 has no dependencies
        graph.dependencies.insert(package_4.clone(), vec![]);

        // Test: Get all transitive dependencies of Package 1
        let transitive_deps = graph.get_transitive_dependencies(package_1);

        // Expected transitive dependencies for Package 1: {2, 3, 4}
        let expected_deps: HashSet<PackageId> =
            vec![package_2, package_3, package_4].into_iter().collect();

        // Assert that the returned transitive dependencies match the expected ones
        assert_eq!(transitive_deps, expected_deps);
    }

    #[test]
    fn test_no_cycles_dont_fail() {
        let repo = create_complex_workspace(true);
        let graph = CrateGraph::new(&repo, "", None);
        assert!(graph.is_ok());
    }

    #[test]
    /// Tests if cycle between direct dependencies are detected
    fn test_simple_cycles_are_detected() {
        let repo = create_complex_workspace(true);
        // Let's create a cycle dependencies, we already have crates_g ->  workspace_a/crates_b,
        // Let's add workspace_a/crates_b -> crates_g
        Command::new("cargo")
            .arg("add")
            .arg("--offline")
            .arg("--registry")
            .arg(FAKE_REGISTRY)
            .arg("--path")
            .arg("../../../crates_g")
            .arg("crates_g")
            .current_dir(repo.join("workspace_a/crates/crates_b"))
            .output()
            .expect("Failed to add workspace_a__crates_b");
        commit_all_changes(&repo, "Add simple cycle");
        let graph = CrateGraph::new(&repo, "", None);
        assert!(graph.is_err());
        let error = graph.unwrap_err();
        assert!(&format!("{}", error).starts_with("`cargo metadata` exited with an error:"));
    }

    #[test]
    /// Tests if cycle between transitive dependencies are detected
    fn test_transitive_cycles_are_detected() {
        let repo = create_complex_workspace(true);
        // Let's create a cycle dependencies,
        // we have crates_g -> workspace_d/crates_e -> workspace_a/crates_a
        // So we can add workspace_a/crates_a -> crates_g
        // to create a transitive cycle
        Command::new("cargo")
            .arg("add")
            .arg("--offline")
            .arg("--registry")
            .arg(FAKE_REGISTRY)
            .arg("--path")
            .arg("../../../crates_g")
            .arg("crates_g")
            .current_dir(repo.join("workspace_a/crates/crates_a"))
            .output()
            .expect("Failed to add workspace_a__crates_b");
        commit_all_changes(&repo, "Add simple cycle");
        println!("Got repo: {}", repo.display());
        let graph = CrateGraph::new(&repo, "", None);
        assert!(graph.is_err());
        let error = graph.unwrap_err();
        assert!(&format!("{}", error).starts_with("`cargo metadata` exited with an error:"));
    }

    #[test]
    /// Path only Dev-dependencies should not create a cycle
    fn test_cycles_without_dev_dependencies() {
        let repo = create_complex_workspace(true);
        // Let's create a cycle dependencies,
        // we have crates_g -> workspace_d/crates_e -> workspace_a/crates_a
        // So we can add workspace_a/crates_a -> crates_g
        // to create a transitive cycle
        Command::new("cargo")
            .arg("add")
            .arg("--offline")
            .arg("--dev")
            .arg("--path")
            .arg("../../../crates_g")
            .current_dir(repo.join("workspace_a/crates/crates_a"))
            .output()
            .expect("Failed to add workspace_a__crates_b");
        commit_all_changes(&repo, "Add simple cycle");
        println!("Got repo: {}", repo.display());
        let graph = CrateGraph::new(&repo, "", None);
        assert!(graph.is_ok());
    }

    #[test]
    /// Path only Dev-dependencies should not create a cycle except if they are version pinned
    fn test_cycles_without_dev_dependencies_but_pinned() {
        let repo = create_complex_workspace(true);
        // Let's create a cycle dependencies,
        // we have crates_g -> workspace_d/crates_e -> workspace_a/crates_a
        // So we can add workspace_a/crates_a -> crates_g
        // to create a transitive cycle
        let mut crates_a_cargo_toml = OpenOptions::new()
            .append(true)
            .open(repo.join("workspace_a/crates/crates_a/Cargo.toml"))
            .unwrap();

        writeln!(
            crates_a_cargo_toml,
            r#"[dev-dependencies]
crates_g = {{ version = "0.1.0", path = "../../../crates_g", registry = "fake-registry" }}"#
        )
        .unwrap();
        commit_all_changes(&repo, "Add simple cycle");
        let graph = CrateGraph::new(&repo, "", None);
        assert!(graph.is_err());
    }
}

#[cfg(test)]
use cargo_metadata::semver::Version;
use cargo_metadata::{CargoOpt, DependencyKind, Metadata, MetadataCommand, Package, PackageId};
use gix::Repository;
use gix::bstr::ByteSlice;
use ignore::gitignore::Gitignore;
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf, StripPrefixError},
};

use crate::{cli_args::DiffStrategy, utils::get_registry_env};

/// Controls how `cargo metadata` resolves features for dependency analysis.
pub enum FeatureResolution {
    /// Only run with `--all-features` (used for publishing).
    AllFeaturesOnly,
    /// Run twice: once with `--all-features` (for publishing deps), once with default features (for change detection).
    DualGraph,
}

/// The (directed acyclic) graph of crates in a multi-workspace repo.
#[derive(Clone, Debug)]
pub struct CrateGraph {
    repo_root: PathBuf,
    workspaces: Vec<Workspace>,
    pub dependencies: DependencyGraph,
    default_dependencies: Option<DependencyGraph>,
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
        feature_resolution: FeatureResolution,
    ) -> anyhow::Result<Self> {
        Self::new_with_skip_ci(repo_root, main_registry, dep_kind, feature_resolution, true)
    }

    /// Finds all Cargo workspaces for a release, including directories marked
    /// with `.skip_ci`. The marker only suppresses test and change-discovery CI;
    /// explicitly publishable crates must remain visible to release planning.
    pub fn new_for_publish(
        repo_root: impl Into<PathBuf>,
        main_registry: impl Into<String> + Clone,
        dep_kind: Option<DependencyKind>,
        feature_resolution: FeatureResolution,
    ) -> anyhow::Result<Self> {
        Self::new_with_skip_ci(
            repo_root,
            main_registry,
            dep_kind,
            feature_resolution,
            false,
        )
    }

    fn new_with_skip_ci(
        repo_root: impl Into<PathBuf>,
        main_registry: impl Into<String> + Clone,
        dep_kind: Option<DependencyKind>,
        feature_resolution: FeatureResolution,
        honor_skip_ci: bool,
    ) -> anyhow::Result<Self> {
        let repo_root = repo_root.into();
        let mut workspaces = Vec::new();
        let (ignore, err) = Gitignore::new(repo_root.join(".gitignore"));
        if let Some(err) = err {
            eprintln!("Failed to find .gitignore: {err}");
        }
        let envs = get_registry_env(main_registry.clone().into());
        Self::new_recursive(
            &repo_root,
            &ignore,
            &repo_root,
            &mut workspaces,
            &envs,
            matches!(feature_resolution, FeatureResolution::DualGraph),
            honor_skip_ci,
        )?;
        workspaces.sort_by(|r1, r2| r1.path.cmp(&r2.path));
        let dependencies = DependencyGraph::new(&repo_root, &workspaces, dep_kind, false);
        let default_dependencies = if matches!(feature_resolution, FeatureResolution::DualGraph) {
            Some(DependencyGraph::new(
                &repo_root,
                &workspaces,
                dep_kind,
                true,
            ))
        } else {
            None
        };
        if let Some(cycles) = dependencies.detect_cycles() {
            return Err(anyhow::anyhow!("Cycle detected: {:?}", cycles));
        }
        Ok(Self {
            repo_root,
            workspaces,
            dependencies,
            default_dependencies,
        })
    }

    fn new_recursive(
        repo_root: &Path,
        ignore: &Gitignore,
        dir: &Path,
        workspaces: &mut Vec<Workspace>,
        envs: &HashMap<String, String>,
        dual_graph: bool,
        honor_skip_ci: bool,
    ) -> anyhow::Result<()> {
        if let Some(name) = dir.file_name()
            && name == ".git"
        {
            return Ok(());
        }
        if ignore.matched(dir, true).is_ignore() {
            return Ok(());
        }
        if honor_skip_ci && std::fs::exists(dir.join(".skip_ci"))? {
            return Ok(());
        }

        let manifest_path = dir.join("Cargo.toml");

        if std::fs::exists(&manifest_path)? {
            // Found a manifest. Get metadata with all features.
            let mut command = MetadataCommand::new();
            command.current_dir(dir);
            command.features(CargoOpt::AllFeatures);
            for (k, v) in envs {
                command.env(k, v);
            }
            let metadata = command.exec()?;

            let default_metadata = if dual_graph {
                let mut default_command = MetadataCommand::new();
                default_command.current_dir(dir);
                for (k, v) in envs {
                    default_command.env(k, v);
                }
                Some(default_command.exec()?)
            } else {
                None
            };

            let has_explicit_members = if metadata.root_package().is_some() {
                metadata.workspace_members.len() > 1
            } else {
                !metadata.workspace_members.is_empty()
            };
            // Use the actual workspace root from cargo metadata, not the
            // directory we found the manifest in. A crate that is a member of
            // a parent workspace will report the parent as workspace_root;
            // using `dir` would create a workspace entry at the wrong path
            // (e.g. pointing at a member's deleted Cargo.lock instead of the
            // root workspace's Cargo.lock).
            let ws_root = metadata.workspace_root.as_std_path();
            let ws_path: PathBuf = relative_path(repo_root, ws_root)
                .expect("Workspace root must be within the repo root")
                .into();
            // Save/restore the workspace root's Cargo.lock, since
            // `cargo metadata` may modify it as a side effect.
            let lock_path = ws_root.join("Cargo.lock");
            let orig_lock_content = match std::fs::exists(&lock_path)? {
                true => Some(std::fs::read_to_string(&lock_path)?),
                false => None,
            };
            // Compute the set of top-level subdirectories covered by
            // workspace members before metadata is moved into the
            // Workspace struct.
            let covered: Option<HashSet<PathBuf>> = if has_explicit_members {
                let dir_str = dir.to_str().unwrap_or("");
                Some(
                    metadata
                        .workspace_members
                        .iter()
                        .filter_map(|id| {
                            let pkg = metadata.packages.iter().find(|p| &p.id == id)?;
                            let pkg_dir = pkg.manifest_path.parent()?;
                            let rel = pkg_dir.strip_prefix(dir_str).ok()?;
                            rel.components().next().map(|c| dir.join(c.as_str()))
                        })
                        .collect(),
                )
            } else {
                None
            };
            if !workspaces.iter().any(|w| w.path == ws_path) {
                workspaces.push(Workspace {
                    path: ws_path,
                    metadata,
                    default_metadata,
                });
            }
            // crate_graph runs `cargo metadata` under the hood. This can update the Cargo.lock;
            // we want to revert that behavior.
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
            if let Some(covered) = covered {
                // Recurse into subdirectories not covered by workspace
                // members, so excluded sub-workspaces (e.g. fdk_apps) are
                // still discovered.
                for entry in std::fs::read_dir(dir)? {
                    let entry = entry?;
                    if entry.metadata()?.is_dir() && !covered.contains(&entry.path()) {
                        Self::new_recursive(
                            repo_root,
                            ignore,
                            &entry.path(),
                            workspaces,
                            envs,
                            dual_graph,
                            honor_skip_ci,
                        )?;
                    }
                }
                return Ok(());
            }
        }

        // No workspace manifest in this directory. Keep searching.
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                Self::new_recursive(
                    repo_root,
                    ignore,
                    &entry.path(),
                    workspaces,
                    envs,
                    dual_graph,
                    honor_skip_ci,
                )?;
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

    pub fn default_dependency_graph(&self) -> &DependencyGraph {
        self.default_dependencies
            .as_ref()
            .unwrap_or(&self.dependencies)
    }

    /// All cargo packages in the repo.
    pub fn packages(&self) -> impl Iterator<Item = &Package> {
        self.workspaces()
            .iter()
            .flat_map(|w| w.metadata.workspace_packages())
    }

    /// Determines which packages have changed between `old_rev` and `new_rev`. (Un)Staged changes are considered
    pub fn changed_packages(
        &self,
        repository: &Repository,
        old_commit_id: gix::ObjectId,
        new_commit_id: gix::ObjectId,
        diff_strategy: &DiffStrategy,
    ) -> anyhow::Result<Vec<PathBuf>> {
        let old_tree = repository.find_commit(old_commit_id)?.tree()?;
        let new_tree = repository.find_commit(new_commit_id)?.tree()?;

        let mut changed = Vec::new();
        let mut packages = self.packages().collect::<Vec<_>>();
        packages.sort_by_key(|package| package_path(&self.repo_root, package).iter().count());
        packages.reverse();

        // delta_path is always relative to repo_root (from gix tree diff or git diff --name-only)
        let mut record_change = |delta_path: &Path| {
            // Scope detection for .cargo/config.toml: affects all packages under the
            // directory containing .cargo/. Matches Cargo's own directory-scoping semantics
            // where config lookup ascends the tree but doesn't descend into siblings.
            // Path::ends_with is component-based, so ".cargo/config.toml" matches the
            // last two path components, not a string suffix.
            let cargo_config_scope = if delta_path.ends_with(".cargo/config.toml") {
                delta_path.parent().and_then(|p| p.parent())
            } else {
                None
            };

            for package in &packages {
                let pkg_path = package_path(&self.repo_root, package).into_owned();
                let is_repo_root = pkg_path == Path::new(".");
                if delta_path.ends_with("rust-toolchain.toml") {
                    changed.push(pkg_path.clone());
                    continue;
                }
                if let Some(scope) = cargo_config_scope {
                    // Empty scope means root-level .cargo/config.toml → all packages affected.
                    if scope.as_os_str().is_empty() || pkg_path.starts_with(scope) {
                        changed.push(pkg_path.clone());
                    }
                    // continue (not return): a config.toml may affect multiple packages
                    continue;
                }
                if is_repo_root || delta_path.starts_with(&pkg_path) {
                    changed.push(pkg_path.clone());
                    return;
                }
            }
        };

        // Tree-to-tree diff (gix native)
        old_tree
            .changes()?
            .for_each_to_obtain_tree(&new_tree, |change| {
                match change.location().to_path() {
                    Ok(path) => {
                        record_change(path);
                    }
                    Err(e) => {
                        tracing::warn!("Skipping non-UTF8 path in diff: {}", e);
                    }
                }
                Ok::<_, std::convert::Infallible>(std::ops::ControlFlow::Continue(()))
            })?;

        // Staged + unstaged (git CLI, only for non-Explicit strategies)
        match diff_strategy {
            DiffStrategy::Explicit { .. } => {}
            _ => {
                // Staged: diff new_commit tree vs current index
                let output = std::process::Command::new("git")
                    .args([
                        "diff",
                        "--cached",
                        "--name-only",
                        &new_commit_id.to_string(),
                    ])
                    .current_dir(&self.repo_root)
                    .output()?;
                if output.status.success() {
                    for line in String::from_utf8_lossy(&output.stdout).lines() {
                        if !line.is_empty() {
                            record_change(Path::new(line));
                        }
                    }
                }

                // Unstaged: diff index vs workdir
                let output = std::process::Command::new("git")
                    .args(["diff", "--name-only"])
                    .current_dir(&self.repo_root)
                    .output()?;
                if output.status.success() {
                    for line in String::from_utf8_lossy(&output.stdout).lines() {
                        if !line.is_empty() {
                            record_change(Path::new(line));
                        }
                    }
                }
            }
        }

        changed.sort();
        changed.dedup();
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
    default_metadata: Option<Metadata>,
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
        use_default_features: bool,
    ) -> Self {
        let mut me = Self::default();

        for w in workspaces {
            // Create the 1:1 bidirectional map between path and package ID.
            for p in w.metadata.workspace_packages() {
                let p_dir_path = package_path(repo_root, p).into_owned();
                tracing::debug!(
                    package_id = %p.id,
                    path = %p_dir_path.display(),
                    "registering package"
                );
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
            let metadata_for_resolve = if use_default_features {
                w.default_metadata.as_ref().unwrap_or(&w.metadata)
            } else {
                &w.metadata
            };
            let resolve = metadata_for_resolve.resolve.as_ref().unwrap();
            for node in &resolve.nodes {
                if !me.id_to_path.contains_key(&node.id) {
                    tracing::debug!(
                        node_id = %node.id,
                        "skipping resolve node: not a registered workspace package"
                    );
                } else if me.id_to_path.contains_key(&node.id) {
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
                            tracing::debug!(
                                source_id = %node.id,
                                dep_id = %dep_id,
                                dep_name = %node_dep.name,
                                "adding dependency edge"
                            );
                            let reverse_deps = me.reverse_dependencies.get_mut(dep_id).unwrap();
                            let dep = Dependency {
                                package_id: dep_id.clone(),
                                instances,
                            };
                            deps.push(dep);
                            reverse_deps.push(node.id.clone());
                        } else if is_accepted_dep && !me.id_to_path.contains_key(dep_id) {
                            tracing::debug!(
                                source_id = %node.id,
                                dep_id = %dep_id,
                                dep_name = %node_dep.name,
                                "skipping external dependency edge"
                            );
                        }
                    }
                }
            }
        }

        let total_edges: usize = me.dependencies.values().map(|deps| deps.len()).sum();
        tracing::debug!(total_edges, "dependency graph construction complete");

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
        FAKE_REGISTRY, commit_all_changes, commit_repo, create_complex_workspace, init_repo,
        initialize_workspace, modify_file, stage_file,
    };

    use super::*;
    use std::{fs::OpenOptions, io::Write, process::Command};

    #[test]
    fn test_discover_standalone_workspace() {
        let repo = initialize_repo().join("standalone");

        let graph = CrateGraph::new(&repo, "", None, FeatureResolution::AllFeaturesOnly).unwrap();
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
    fn test_publish_discovery_includes_skip_ci_workspace() {
        let repo = create_simple_rust_crate();
        std::fs::write(repo.join(".skip_ci"), "").unwrap();

        let ci_graph =
            CrateGraph::new(&repo, "", None, FeatureResolution::AllFeaturesOnly).unwrap();
        let publish_graph =
            CrateGraph::new_for_publish(&repo, "", None, FeatureResolution::AllFeaturesOnly)
                .unwrap();

        assert!(ci_graph.workspaces().is_empty());
        assert_eq!(publish_graph.workspaces().len(), 1);
        assert_eq!(
            publish_graph.workspaces()[0].root_package_key(),
            Some(PackageKey {
                name: "test-lib".into(),
                version: "0.1.0".parse().unwrap(),
            })
        );
    }

    #[test]
    fn test_discover_many_workspaces() {
        let repo = initialize_repo();

        let graph = CrateGraph::new(&repo, "", None, FeatureResolution::AllFeaturesOnly).unwrap();
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
        let graph =
            CrateGraph::new(&repo_root, "", None, FeatureResolution::AllFeaturesOnly).unwrap();
        let repo = gix::open(repo_root).unwrap();
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
        let graph =
            CrateGraph::new(&repo_root, "", None, FeatureResolution::AllFeaturesOnly).unwrap();
        let repo = gix::open(repo_root).unwrap();
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
        let graph =
            CrateGraph::new(&repo_root, "", None, FeatureResolution::AllFeaturesOnly).unwrap();
        modify_file(&repo_root, "src/lib.rs", "pub fn new_function_again() {}");
        let repo = gix::open(repo_root).unwrap();
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
        let graph =
            CrateGraph::new(&repo_root, "", None, FeatureResolution::AllFeaturesOnly).unwrap();
        modify_file(&repo_root, "src/lib.rs", "pub fn new_function_again() {}");
        stage_file(&repo_root, "src/lib.rs");

        let repo = gix::open(repo_root).unwrap();
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

        init_repo(&tmp);

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
        let graph = CrateGraph::new(&repo, "", None, FeatureResolution::AllFeaturesOnly);
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
        let graph = CrateGraph::new(&repo, "", None, FeatureResolution::AllFeaturesOnly);
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
        let graph = CrateGraph::new(&repo, "", None, FeatureResolution::AllFeaturesOnly);
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
        let graph = CrateGraph::new(&repo, "", None, FeatureResolution::AllFeaturesOnly);
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
        let graph = CrateGraph::new(&repo, "", None, FeatureResolution::AllFeaturesOnly);
        assert!(graph.is_err());
    }

    // --- Dev-dependency handling tests ---
    //
    // These tests pin down the behaviour of dev-dep edges in the graph so the
    // team stops flip-flopping on whether dev deps belong in the publish DAG.
    // The authoritative answer is:
    //   • Path-only dev deps  → excluded from publish ordering (cycle-safe).
    //   • Registry dev deps   → included in publish ordering (cargo publish needs them).

    #[test]
    /// A local dev-dep edge must appear in the graph when dep_kind is None, and
    /// the stored DependencyInstance must record kind=Development and is_local=true.
    fn test_dev_dep_included_when_no_kind_filter() {
        let repo = create_complex_workspace(true);

        // Add workspace_a/crates_b as a dev-dep of workspace_a/crates_c (path-only,
        // no version pin → is_local == true).
        Command::new("cargo")
            .args([
                "add",
                "--offline",
                "--dev",
                "--path",
                "../crates_b",
                "workspace_a__crates_b",
            ])
            .current_dir(repo.join("workspace_a/crates/crates_c"))
            .output()
            .expect("Failed to add dev dep");

        commit_all_changes(&repo, "Add path dev-dep crates_c -> crates_b");

        let graph = CrateGraph::new(&repo, "", None, FeatureResolution::AllFeaturesOnly).unwrap();
        let dep_graph = graph.dependency_graph();

        // Locate crates_c and crates_b by path.
        let crates_c_path = Path::new("workspace_a").join("crates").join("crates_c");
        let crates_b_path = Path::new("workspace_a").join("crates").join("crates_b");

        let crates_c_id = &dep_graph.path_to_id[&crates_c_path];
        let crates_b_id = &dep_graph.path_to_id[&crates_b_path];

        let crates_c_deps = dep_graph.dependencies.get(crates_c_id).unwrap();
        let dev_dep = crates_c_deps
            .iter()
            .find(|d| &d.package_id == crates_b_id)
            .expect("dev-dep edge from crates_c to crates_b must exist when dep_kind=None");

        // The instance must be flagged as Development + local.
        assert!(
            dev_dep
                .instances
                .iter()
                .any(|i| i.kind == DependencyKind::Development && i.is_local),
            "Expected a Development+local instance, got: {:?}",
            dev_dep.instances
        );
    }

    #[test]
    /// When dep_kind=Some(Normal), dev-dep edges must be absent from the graph.
    /// This mirrors how `cargo publish` ordering used to be built — but note that
    /// excluding dev deps here can cause publish failures for registry dev deps.
    fn test_dev_dep_excluded_when_normal_kind_filter() {
        let repo = create_complex_workspace(true);

        // Same path dev-dep as the test above.
        Command::new("cargo")
            .args([
                "add",
                "--offline",
                "--dev",
                "--path",
                "../crates_b",
                "workspace_a__crates_b",
            ])
            .current_dir(repo.join("workspace_a/crates/crates_c"))
            .output()
            .expect("Failed to add dev dep");

        commit_all_changes(&repo, "Add path dev-dep crates_c -> crates_b");

        let graph = CrateGraph::new(
            &repo,
            "",
            Some(DependencyKind::Normal),
            FeatureResolution::AllFeaturesOnly,
        )
        .unwrap();
        let dep_graph = graph.dependency_graph();

        let crates_c_path = Path::new("workspace_a").join("crates").join("crates_c");
        let crates_b_path = Path::new("workspace_a").join("crates").join("crates_b");

        let crates_c_id = &dep_graph.path_to_id[&crates_c_path];
        let crates_b_id = &dep_graph.path_to_id[&crates_b_path];

        let crates_c_deps = dep_graph.dependencies.get(crates_c_id).unwrap();
        let dev_dep_edge = crates_c_deps.iter().find(|d| &d.package_id == crates_b_id);

        assert!(
            dev_dep_edge.is_none(),
            "Dev-dep edge must be absent when dep_kind=Some(Normal), got: {:?}",
            dev_dep_edge
        );
    }

    #[test]
    /// A dev-dep that specifies a registry (not path-only) must be recorded with
    /// is_local == false. Registry dev-deps must be published before the dependant,
    /// so they must survive the publish-ordering filter.
    fn test_registry_dev_dep_not_local() {
        let repo = create_complex_workspace(true);

        // Write a dev-dependency with both path + registry directly so that the
        // registry field is set, making is_local == false.
        let mut crates_c_cargo_toml = OpenOptions::new()
            .append(true)
            .open(repo.join("workspace_a/crates/crates_c/Cargo.toml"))
            .unwrap();

        writeln!(
            crates_c_cargo_toml,
            r#"[dev-dependencies]
workspace_a__crates_b = {{ version = "0.1.0", path = "../crates_b", registry = "fake-registry" }}"#
        )
        .unwrap();

        commit_all_changes(&repo, "Add registry dev-dep crates_c -> crates_b");

        let graph = CrateGraph::new(&repo, "", None, FeatureResolution::AllFeaturesOnly).unwrap();
        let dep_graph = graph.dependency_graph();

        let crates_c_path = Path::new("workspace_a").join("crates").join("crates_c");
        let crates_b_path = Path::new("workspace_a").join("crates").join("crates_b");

        let crates_c_id = &dep_graph.path_to_id[&crates_c_path];
        let crates_b_id = &dep_graph.path_to_id[&crates_b_path];

        let crates_c_deps = dep_graph.dependencies.get(crates_c_id).unwrap();
        let dep = crates_c_deps
            .iter()
            .find(|d| &d.package_id == crates_b_id)
            .expect("registry dev-dep edge from crates_c to crates_b must exist");

        assert!(
            dep.instances
                .iter()
                .any(|i| i.kind == DependencyKind::Development && !i.is_local),
            "Expected a Development+non-local instance for a registry dev-dep, got: {:?}",
            dep.instances
        );
    }

    #[test]
    /// reverse_closure must walk through dev-dep edges when the graph was built
    /// with dep_kind=None. This is critical: if crate A dev-depends on crate B,
    /// a change to B should mark A as needing a re-test.
    fn test_reverse_closure_includes_dev_dep_dependents() {
        let repo = create_complex_workspace(true);

        // workspace_a/crates_c dev-depends on workspace_a/crates_b (path-only).
        Command::new("cargo")
            .args([
                "add",
                "--offline",
                "--dev",
                "--path",
                "../crates_b",
                "workspace_a__crates_b",
            ])
            .current_dir(repo.join("workspace_a/crates/crates_c"))
            .output()
            .expect("Failed to add dev dep");

        commit_all_changes(&repo, "Add path dev-dep crates_c -> crates_b");

        // Build graph with all dep kinds so the dev-dep edge is present.
        let graph = CrateGraph::new(&repo, "", None, FeatureResolution::AllFeaturesOnly).unwrap();
        let dep_graph = graph.dependency_graph();

        let crates_b_path = Path::new("workspace_a").join("crates").join("crates_b");
        let crates_c_path = Path::new("workspace_a").join("crates").join("crates_c");

        // A change to crates_b must propagate to crates_c via the dev-dep edge.
        let closure = dep_graph.reverse_closure([crates_b_path.as_path()]);

        assert!(
            closure.contains(&crates_c_path),
            "reverse_closure of crates_b must include crates_c (dev-dep dependent), got: {closure:?}"
        );
        assert!(
            closure.contains(&crates_b_path),
            "reverse_closure must always include the seed package itself"
        );
    }

    #[test]
    fn test_dual_graph_narrows_reverse_closure() {
        // Create a workspace with two crates where crate_a optionally depends on crate_b.
        // With --all-features: crate_a -> crate_b (edge exists)
        // With default features: no edge between them
        let tmp = assert_fs::TempDir::new()
            .unwrap()
            .into_persistent()
            .to_path_buf();

        init_repo(&tmp);
        initialize_workspace(&tmp, "ws", vec!["crate_a", "crate_b"], vec![], false);

        // Make crate_b an optional dependency of crate_a, gated behind a feature.
        // cargo init emits an empty [dependencies] section, so we inject the optional dep into
        // that existing section rather than appending a duplicate [dependencies] header.
        let crate_a_toml = tmp.join("ws/crates/crate_a/Cargo.toml");
        let original = std::fs::read_to_string(&crate_a_toml).unwrap();
        let patched = original.replacen(
            "[dependencies]",
            &format!(
                "[dependencies]\nws__crate_b = {{ path = \"../crate_b\", optional = true, registry = \"{FAKE_REGISTRY}\" }}\n\n[features]\nwith_b = [\"dep:ws__crate_b\"]"
            ),
            1,
        );
        std::fs::write(&crate_a_toml, patched).unwrap();

        commit_all_changes(&tmp, "workspace with optional dep");

        let graph = CrateGraph::new(&tmp, "", None, FeatureResolution::DualGraph).unwrap();

        let crate_b_path = Path::new("ws").join("crates").join("crate_b");

        // All-features graph: changing crate_b should pull in crate_a via reverse closure
        let all_features_closure = graph
            .dependency_graph()
            .reverse_closure([crate_b_path.as_path()]);

        // Default-features graph: changing crate_b should NOT pull in crate_a
        let default_closure = graph
            .default_dependency_graph()
            .reverse_closure([crate_b_path.as_path()]);

        // The all-features closure should include both crate_a and crate_b
        let crate_a_path = Path::new("ws").join("crates").join("crate_a");
        assert!(
            all_features_closure.contains(&crate_a_path),
            "all-features closure should include crate_a (depends on crate_b via optional dep), got: {all_features_closure:?}"
        );
        assert!(all_features_closure.contains(&crate_b_path));

        // The default closure should only include crate_b (no edge to crate_a)
        assert!(
            !default_closure.contains(&crate_a_path),
            "default closure should NOT include crate_a (optional dep not enabled), got: {default_closure:?}"
        );
        assert!(default_closure.contains(&crate_b_path));

        // The default closure is strictly smaller
        assert!(
            default_closure.len() < all_features_closure.len(),
            "default closure ({}) should be smaller than all-features closure ({})",
            default_closure.len(),
            all_features_closure.len()
        );
    }

    /// When a workspace has explicit members, subdirectories not covered by those members
    /// should still be recursed into, so that independent sub-workspaces may be discovered.
    #[test]
    fn test_excluded_subworkspace_discovered_in_explicit_member_workspace() {
        let tmp = assert_fs::TempDir::new()
            .unwrap()
            .into_persistent()
            .to_path_buf();

        init_repo(&tmp);

        // Create a parent workspace with explicit members in "crates/*"
        initialize_workspace(&tmp, "mono", vec!["app_a"], vec![], false);

        // Add exclude for the sub-workspace so cargo doesn't complain
        // about an unrelated manifest inside the workspace directory.
        {
            let cargo_toml = tmp.join("mono").join("Cargo.toml");
            let content = std::fs::read_to_string(&cargo_toml).unwrap();
            let content = content.replace(
                "members = [\"crates/*\"]",
                "members = [\"crates/*\"]\nexclude = [\"fdk_apps\"]",
            );
            std::fs::write(&cargo_toml, content).unwrap();
        }

        // Create an independent sub-workspace inside "mono/" that is
        // excluded from the parent workspace (simulates the fdk_apps case).
        let excluded_dir = tmp.join("mono").join("fdk_apps");
        std::fs::create_dir_all(&excluded_dir).unwrap();
        Command::new("cargo")
            .arg("init")
            .arg("--lib")
            .arg("--name")
            .arg("fdk_apps")
            .arg("--registry")
            .arg(FAKE_REGISTRY)
            .current_dir(&excluded_dir)
            .output()
            .expect("Failed to create fdk_apps crate");

        commit_all_changes(&tmp, "Initial commit");

        let graph = CrateGraph::new(&tmp, "", None, FeatureResolution::AllFeaturesOnly).unwrap();
        let workspaces = graph.workspaces();

        let ws_paths: Vec<&Path> = workspaces.iter().map(|w| w.path.as_path()).collect();

        assert!(
            ws_paths.contains(&Path::new("mono")),
            "parent workspace 'mono' should be discovered, got: {ws_paths:?}"
        );
        assert!(
            ws_paths.contains(&Path::new("mono").join("fdk_apps").as_path()),
            "excluded sub-workspace 'mono/fdk_apps' should be discovered, got: {ws_paths:?}"
        );
    }
}

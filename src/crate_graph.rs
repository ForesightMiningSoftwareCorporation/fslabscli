use cargo_metadata::{semver::Version, Metadata, MetadataCommand, Package};
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

    #[cfg(test)]
    pub fn workspaces(&self) -> &[Workspace] {
        &self.workspaces
    }

    pub fn dependency_graph(&self) -> &DependencyGraph {
        &self.dependencies
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
                let manifest_path = p.manifest_path.as_std_path();
                let p_dir_path = relative_path(repo_root, manifest_path.parent().unwrap())
                    .expect("Workspace package manifest must be relative to repo root")
                    .into_owned();
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

fn relative_path<'a>(root: &Path, path: &'a Path) -> Result<Cow<'a, Path>, StripPrefixError> {
    match path.strip_prefix(root)? {
        p if p == Path::new("") => Ok(Cow::Owned(PathBuf::from("."))),
        stripped => Ok(Cow::Borrowed(stripped)),
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

    fn initialize_repo() -> PathBuf {
        let tmp = assert_fs::TempDir::new()
            .unwrap()
            .into_persistent()
            .to_path_buf();
        let test_data = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test_data");
        let script = test_data.join("create_repo.sh");
        Command::new("sh")
            .arg(script)
            .arg(test_data)
            .current_dir(&tmp)
            .output()
            .unwrap();
        tmp
    }
}

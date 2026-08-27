use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Context;
use gix::bstr::ByteSlice;
use serde::Serialize;

use crate::utils::cargo::CrateChecker;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MarkedCargoPackage {
    pub name: String,
    pub manifest_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CargoPublishPolicySnapshot {
    pub base_marked: BTreeMap<String, MarkedCargoPackage>,
    pub head_marked: BTreeMap<String, MarkedCargoPackage>,
    pub newly_marked: BTreeSet<String>,
}

fn manifest_is_marked(value: &toml::Value, path: &str) -> anyhow::Result<bool> {
    let publish = value
        .get("package")
        .and_then(|package| package.get("metadata"))
        .and_then(|metadata| metadata.get("fslabs"))
        .and_then(|fslabs| fslabs.get("publish"))
        .and_then(|publish| publish.get("cargo"))
        .and_then(|cargo| cargo.get("publish"));
    match publish {
        Some(value) => value.as_bool().with_context(|| {
            format!("package.metadata.fslabs.publish.cargo.publish in {path} must be a boolean")
        }),
        None => Ok(false),
    }
}

pub fn marked_cargo_packages_at(
    repo: &gix::Repository,
    commit_id: gix::ObjectId,
) -> anyhow::Result<BTreeMap<String, MarkedCargoPackage>> {
    let tree = repo.find_commit(commit_id)?.tree()?;
    let mut marked = BTreeMap::new();

    for entry in tree.traverse().breadthfirst.files()? {
        let path = entry
            .filepath
            .to_str()
            .context("Cargo manifest path is not UTF-8")?;
        if Path::new(path).file_name().and_then(|name| name.to_str()) != Some("Cargo.toml") {
            continue;
        }
        if !entry.mode.is_blob() {
            anyhow::bail!("Cargo manifest {path} is not a regular file");
        }

        let blob = repo.find_blob(entry.oid)?;
        let manifest = std::str::from_utf8(&blob.data)
            .with_context(|| format!("Cargo manifest {path} is not UTF-8"))?;
        let value: toml::Value =
            toml::from_str(manifest).with_context(|| format!("Could not parse {path}"))?;
        if !manifest_is_marked(&value, path)? {
            continue;
        }

        let name = value
            .get("package")
            .and_then(|package| package.get("name"))
            .and_then(toml::Value::as_str)
            .with_context(|| format!("Marked Cargo manifest {path} has no package.name"))?
            .to_string();
        let package = MarkedCargoPackage {
            name: name.clone(),
            manifest_path: path.to_string(),
        };
        if let Some(previous) = marked.insert(name.clone(), package) {
            anyhow::bail!(
                "Marked Cargo package {name} is defined by both {} and {path}",
                previous.manifest_path
            );
        }
    }

    Ok(marked)
}

pub fn inspect_cargo_publish_policy(
    repo: &gix::Repository,
    base_commit: gix::ObjectId,
    head_commit: gix::ObjectId,
) -> anyhow::Result<CargoPublishPolicySnapshot> {
    let base_marked = marked_cargo_packages_at(repo, base_commit)?;
    let head_marked = marked_cargo_packages_at(repo, head_commit)?;
    let newly_marked = head_marked
        .keys()
        .filter(|name| !base_marked.contains_key(*name))
        .cloned()
        .collect();

    Ok(CargoPublishPolicySnapshot {
        base_marked,
        head_marked,
        newly_marked,
    })
}

pub async fn missing_marked_package_names<C: CrateChecker>(
    cargo: &C,
    registry: &str,
    marked: &BTreeMap<String, MarkedCargoPackage>,
) -> anyhow::Result<BTreeSet<String>> {
    let mut missing = BTreeSet::new();
    for name in marked.keys() {
        let exists = cargo
            .check_crate_name_exists(registry.to_string(), name.clone())
            .await
            .with_context(|| {
                format!("Could not check whether Cargo package {name} exists in {registry}")
            })?;
        if !exists {
            missing.insert(name.clone());
        }
    }
    Ok(missing)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;
    use crate::utils::cargo::tests::MockCargo;

    fn git(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo)
            .env("GIT_AUTHOR_NAME", "Test User")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test User")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn write_manifest(repo: &Path, relative_path: &str, name: &str, publish: Option<bool>) {
        let path = repo.join(relative_path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let publish = publish
            .map(|publish| {
                format!("\n[package.metadata.fslabs.publish.cargo]\npublish = {publish}\n")
            })
            .unwrap_or_default();
        fs::write(
            path,
            format!("[package]\nname = \"{name}\"\nversion = \"1.0.0\"\n{publish}"),
        )
        .unwrap();
    }

    fn commit(repo: &Path, message: &str) -> gix::ObjectId {
        git(repo, &["add", "."]);
        git(
            repo,
            &["-c", "commit.gpgsign=false", "commit", "-m", message],
        );
        git(repo, &["rev-parse", "HEAD"]).parse().unwrap()
    }

    #[test]
    fn detects_new_and_newly_enabled_marked_packages() {
        let temp = TempDir::new().unwrap();
        git(temp.path(), &["init"]);
        write_manifest(temp.path(), "existing/Cargo.toml", "existing", Some(true));
        write_manifest(temp.path(), "disabled/Cargo.toml", "disabled", Some(false));
        let base = commit(temp.path(), "base");

        write_manifest(temp.path(), "disabled/Cargo.toml", "disabled", Some(true));
        write_manifest(temp.path(), "new/Cargo.toml", "new", Some(true));
        write_manifest(temp.path(), "ordinary/Cargo.toml", "ordinary", None);
        let head = commit(temp.path(), "head");

        let repo = gix::open(temp.path()).unwrap();
        let snapshot = inspect_cargo_publish_policy(&repo, base, head).unwrap();

        assert_eq!(
            snapshot.newly_marked,
            BTreeSet::from(["disabled".to_string(), "new".to_string()])
        );
        assert!(snapshot.base_marked.contains_key("existing"));
        assert!(snapshot.head_marked.contains_key("existing"));
    }

    #[test]
    fn already_marked_package_changes_do_not_require_new_approval() {
        let temp = TempDir::new().unwrap();
        git(temp.path(), &["init"]);
        write_manifest(temp.path(), "crate/Cargo.toml", "crate", Some(true));
        let base = commit(temp.path(), "base");

        fs::write(temp.path().join("crate/src.rs"), "pub fn changed() {}\n").unwrap();
        let head = commit(temp.path(), "head");

        let repo = gix::open(temp.path()).unwrap();
        let snapshot = inspect_cargo_publish_policy(&repo, base, head).unwrap();

        assert!(snapshot.newly_marked.is_empty());
    }

    #[test]
    fn moved_marked_package_keeps_its_existing_identity() {
        let temp = TempDir::new().unwrap();
        git(temp.path(), &["init"]);
        write_manifest(temp.path(), "old/Cargo.toml", "crate", Some(true));
        let base = commit(temp.path(), "base");

        fs::create_dir_all(temp.path().join("new")).unwrap();
        fs::rename(
            temp.path().join("old/Cargo.toml"),
            temp.path().join("new/Cargo.toml"),
        )
        .unwrap();
        let head = commit(temp.path(), "head");

        let repo = gix::open(temp.path()).unwrap();
        let snapshot = inspect_cargo_publish_policy(&repo, base, head).unwrap();

        assert!(snapshot.newly_marked.is_empty());
    }

    #[test]
    fn non_boolean_publish_setting_fails_closed() {
        let temp = TempDir::new().unwrap();
        git(temp.path(), &["init"]);
        let path = temp.path().join("crate/Cargo.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "[package]\nname = \"crate\"\nversion = \"1.0.0\"\n\n[package.metadata.fslabs.publish.cargo]\npublish = \"yes\"\n",
        )
        .unwrap();
        let commit = commit(temp.path(), "invalid metadata");

        let repo = gix::open(temp.path()).unwrap();
        let error = marked_cargo_packages_at(&repo, commit).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("package.metadata.fslabs.publish.cargo.publish")
        );
    }

    #[tokio::test]
    async fn reports_missing_marked_package_names() {
        let marked = BTreeMap::from([
            (
                "missing".to_string(),
                MarkedCargoPackage {
                    name: "missing".to_string(),
                    manifest_path: "missing/Cargo.toml".to_string(),
                },
            ),
            (
                "present".to_string(),
                MarkedCargoPackage {
                    name: "present".to_string(),
                    manifest_path: "present/Cargo.toml".to_string(),
                },
            ),
        ]);
        let mut cargo = MockCargo::new();
        cargo
            .expect_check_crate_name_exists()
            .times(2)
            .withf(|registry, _| registry == "fsl")
            .returning(|_, name| Ok(name == "present"));

        let missing = missing_marked_package_names(&cargo, "fsl", &marked)
            .await
            .unwrap();

        assert_eq!(missing, BTreeSet::from(["missing".to_string()]));
    }
}

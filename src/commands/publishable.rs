use std::fmt::{Display, Formatter};
use std::fs::read_dir;
use std::path::{Path, PathBuf};

use anyhow::Context;
use cargo_metadata::{MetadataCommand, Package};
use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_json::from_value;

#[derive(Debug, Parser)]
#[clap(about = "Check directory for crates that need to be published.")]
pub struct Options {}

#[derive(Serialize)]
pub struct Result {
    pub workspace: String,
    pub package: String,
    pub version: String,
    pub path: PathBuf,
    pub publish: PackageMetadataFslabsCiPublish,
}

fn default_false() -> bool { false }

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct PackageMetadataFslabsCiPublish {
    #[serde(default = "default_false")]
    pub docker: bool,
    #[serde(default = "default_false")]
    pub private_registry: bool,
    #[serde(default = "default_false")]
    pub public_registry: bool,
    #[serde(default = "default_false")]
    pub npm_napi: bool,
    #[serde(default = "default_false")]
    pub binary: bool,
}

#[derive(Deserialize, Default)]
struct PackageMetadataFslabsCi {
    pub publish: PackageMetadataFslabsCiPublish,
}

#[derive(Deserialize, Default)]
struct PackageMetadata {
    pub fslabs: PackageMetadataFslabsCi,
}

impl Result {
    pub fn new(workspace: String, package: Package) -> anyhow::Result<Self> {
        let path = package.manifest_path.canonicalize()?.parent().unwrap().to_path_buf();
        let metadata: PackageMetadata = from_value(package.metadata).unwrap_or_else(|_| PackageMetadata::default());
        Ok(Self {
            workspace,
            package: package.name,
            version: package.version.to_string(),
            path,
            publish: metadata.fslabs.publish,
        })
    }

    pub async fn check_publishable(mut self) -> anyhow::Result<Self> {
        if self.publish.docker {
            log::debug!("Checking if version {} of {} already exists", self.version, self.package);
        }
        Ok(self)
    }
}

impl Display for Result {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f,
               "{} -- {} -- {}: docker: {}, private registry: {}, public registry: {}, npm_napi: {}, binary: {}",
               self.workspace, self.package, self.version,
               self.publish.docker,
               self.publish.private_registry,
               self.publish.public_registry,
               self.publish.npm_napi,
               self.publish.binary)
    }
}

#[derive(Serialize)]
pub struct Results(Vec<Result>);

impl Display for Results {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        for v in &self.0 {
            write!(f, "{}\n", v)?;
        }
        Ok(())
    }
}

pub async fn publishable(_options: Options, working_directory: PathBuf) -> anyhow::Result<Results> {
    log::info!("Check directory for crates that need publishing");
    let path = match working_directory.is_absolute() {
        true => working_directory.clone(),
        false => working_directory.canonicalize().with_context(|| format!("Failed to get absolute path from {:?}", working_directory))?,
    };

    let mut results = vec![];
    log::debug!("Base directory: {:?}", path);
    // 1. Find all workspaces to investigate
    let roots = get_cargo_roots(path).with_context(|| format!("Failed to get roots from {:?}", working_directory))?;
    // 2. For each workspace, find if one of the subcrates needs publishing
    for root in roots {
        log::debug!("Checking publishing for: {:?}", root);
        if let Some(workspace_name) = root.file_name() {
            let workspace_metadata = MetadataCommand::new()
                .current_dir(root.clone())
                .no_deps()
                .exec()
                .unwrap();
            for package in workspace_metadata.packages {
                let result = Result::new(workspace_name.to_string_lossy().to_string(), package)?.check_publishable().await?;
                results.push(result);
            }
        }
    }
    Ok(Results(results))
}

fn get_cargo_roots(root: PathBuf) -> anyhow::Result<Vec<PathBuf>> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if Path::exists(root.join("Cargo.toml").as_path()) {
        roots.push(root);
        return Ok(roots);
    }
    for entry in read_dir(root)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            let mut sub_roots = get_cargo_roots(entry.path())?;
            roots.append(&mut sub_roots);
        }
    }
    roots.sort();
    Ok(roots)
}

async fn check_docker_image_exists(
    docker_registry_url: String,
    docker_registry_username: String,
    docker_registry_password: String,
    image_name: String,
    image_tag: String,
) -> anyhow::Result<bool> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::fs::create_dir_all;

    use assert_fs::TempDir;

    use super::*;

    #[test]
    fn test_get_cargo_roots_simple_crate() {
        // Create fake directory structure
        let dir = TempDir::new().expect("Could not create temp dir");
        let path = dir.path();
        let file_path = dir.path().join("Cargo.toml");
        fs::File::create(file_path).expect("Could not create root Cargo.toml");
        let roots = get_cargo_roots(path.to_path_buf()).expect("Could not get roots");
        let expected_results = vec![
            path
        ];
        assert_eq!(roots, expected_results);
    }

    #[test]
    fn test_get_cargo_roots_simple_workspace() {
        // Create fake directory structure
        let dir = TempDir::new().expect("Could not create temp dir");
        let path = dir.path();
        fs::File::create(dir.path().join("Cargo.toml")).expect("Could not create root Cargo.toml");
        create_dir_all(dir.path().join("crates/subcrate_a")).expect("Could not create subdir");
        fs::File::create(dir.path().join("crates/subcrate_a/Cargo.toml")).expect("Could not create root Cargo.toml");
        create_dir_all(dir.path().join("crates/subcrate_b")).expect("Could not create subdir");
        fs::File::create(dir.path().join("crates/subcrate_b/Cargo.toml")).expect("Could not create root Cargo.toml");
        let roots = get_cargo_roots(path.to_path_buf()).expect("Could not get roots");
        let expected_results = vec![
            path
        ];
        assert_eq!(roots, expected_results);
    }

    #[test]
    fn test_get_cargo_roots_complex_monorepo() {
        // Create fake directory structure
        // dir
        //  - subdir_a/Cargo.toml
        //  - subdir_b/Cargo_toml
        //  - subdir_b/crates/subcrate_a/Cargo.toml
        //  - subdir_b/crates/subcrate_b/Cargo.toml
        //  - subdir_c
        //  - subdir_d/subdir_a/Cargo.toml
        //  - subdir_d/subdir_b/Cargo.tom
        //  - subdir_d/subdir_b/crates/subcrate_a/Cargo.toml
        //  - subdir_d/subdir_b/crates/subcrate_b/Cargo.toml
        let dir = TempDir::new().expect("Could not create temp dir");
        create_dir_all(dir.path().join("subdir_a")).expect("Could not create subdir");
        create_dir_all(dir.path().join("subdir_b/crates/subcrate_a")).expect("Could not create subdir");
        create_dir_all(dir.path().join("subdir_b/crates/subcrate_b")).expect("Could not create subdir");
        create_dir_all(dir.path().join("subdir_c")).expect("Could not create subdir");
        create_dir_all(dir.path().join("subdir_d/subdir_a")).expect("Could not create subdir");
        create_dir_all(dir.path().join("subdir_d/subdir_b/crates/subcrate_a")).expect("Could not create subdir");
        create_dir_all(dir.path().join("subdir_d/subdir_b/crates/subcrate_b")).expect("Could not create subdir");
        fs::File::create(dir.path().join("subdir_a/Cargo.toml")).expect("Could not create root Cargo.toml");
        fs::File::create(dir.path().join("subdir_b/Cargo.toml")).expect("Could not create root Cargo.toml");
        fs::File::create(dir.path().join("subdir_b/crates/subcrate_a/Cargo.toml")).expect("Could not create root Cargo.toml");
        fs::File::create(dir.path().join("subdir_b/crates/subcrate_b/Cargo.toml")).expect("Could not create root Cargo.toml");
        fs::File::create(dir.path().join("subdir_d/subdir_a/Cargo.toml")).expect("Could not create root Cargo.toml");
        fs::File::create(dir.path().join("subdir_d/subdir_b/Cargo.toml")).expect("Could not create root Cargo.toml");
        fs::File::create(dir.path().join("subdir_d/subdir_b/crates/subcrate_a/Cargo.toml")).expect("Could not create root Cargo.toml");
        fs::File::create(dir.path().join("subdir_d/subdir_b/crates/subcrate_b/Cargo.toml")).expect("Could not create root Cargo.toml");

        let path = dir.path();
        let roots = get_cargo_roots(path.to_path_buf()).expect("Could not get roots");
        let expected_results = vec![
            path.join("subdir_a").to_path_buf(),
            path.join("subdir_b").to_path_buf(),
            path.join("subdir_d/subdir_a").to_path_buf(),
            path.join("subdir_d/subdir_b").to_path_buf(),
        ];
        assert_eq!(roots, expected_results);
    }

    #[tokio::test]
    async fn test_docker_image_exists() {
        let image = "my_image".to_string();
        let
    }
}
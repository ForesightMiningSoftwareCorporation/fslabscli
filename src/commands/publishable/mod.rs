use std::fmt::{Display, Formatter};
use std::fs::read_dir;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Context;
use cargo_metadata::{MetadataCommand, Package};
use clap::Parser;
use console::{Emoji, style};
use indicatif::{HumanDuration, ProgressBar, ProgressStyle};
use oci_distribution::Reference;
use serde::{Deserialize, Serialize};
use serde_json::from_value;

use docker::PackageMetadataFslabsCiPublishDocker;
use npm::{Npm, PackageMetadataFslabsCiPublishNpmNapi};

use crate::commands::publishable::cargo::PackageMetadataFslabsCiPublishCargo;

mod docker;
mod npm;
mod cargo;

static LOOKING_GLASS: Emoji<'_, '_> = Emoji("🔍  ", "");
static TRUCK: Emoji<'_, '_> = Emoji("🚚  ", "");
static PAPER: Emoji<'_, '_> = Emoji("📃  ", "");
static SPARKLE: Emoji<'_, '_> = Emoji("✨ ", ":-)");

#[derive(Debug, Parser)]
#[command(about = "Check directory for crates that need to be published.")]
pub struct Options {
    #[arg(long)]
    docker_registry: Option<String>,
    #[arg(long)]
    docker_registry_username: Option<String>,
    #[arg(long)]
    docker_registry_password: Option<String>,
    #[arg(long)]
    npm_registry_url: Option<String>,
    #[arg(long)]
    npm_registry_token: Option<String>,
    #[arg(long)]
    npm_registry_npmrc_path: Option<String>,
    #[arg(long, default_value_t = true)]
    hide_unpublishable: bool,
    #[arg(long, default_value_t = false)]
    progress: bool,
    #[arg(long, default_value_t = false)]
    fail_unit_error: bool,
}

#[derive(Serialize, Clone)]
pub struct Result {
    pub workspace: String,
    pub package: String,
    pub version: String,
    pub path: PathBuf,
    pub publish: PackageMetadataFslabsCiPublish,
}

fn default_false() -> bool { false }

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct PackageMetadataFslabsCiPublish {
    #[serde(default = "PackageMetadataFslabsCiPublishDocker::default")]
    pub docker: PackageMetadataFslabsCiPublishDocker,
    #[serde(default = "PackageMetadataFslabsCiPublishCargo::default")]
    pub cargo: PackageMetadataFslabsCiPublishCargo,
    #[serde(default = "PackageMetadataFslabsCiPublishNpmNapi::default")]
    pub npm_napi: PackageMetadataFslabsCiPublishNpmNapi,
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
        let mut publish = metadata.fslabs.publish;
        // Let's parse cargo publishing from main metadata
        publish.cargo.registry = package.publish;
        Ok(Self {
            workspace,
            package: package.name,
            version: package.version.to_string(),
            path,
            publish,
        })
    }

    pub async fn check_publishable(mut self, options: &Options, npm: &Npm) -> anyhow::Result<Self> {
        if self.publish.docker.publish {
            log::debug!("Docker: checking if version {} of {} already exists", self.version, self.package);
            let docker_registry = match options.docker_registry.clone() {
                Some(r) => r,
                None => match self.publish.docker.repository.clone() {
                    Some(r) => r,
                    None => anyhow::bail!("Tried to check docker image without setting the registry"),
                }
            };
            let image: Reference = format!("{}/{}:{}", docker_registry, self.package.clone(), self.version.clone()).parse()?;
            self.publish.docker.check(
                &image,
                options.docker_registry_username.clone(),
                options.docker_registry_password.clone(),
                None,
            ).await?;
        }
        if self.publish.npm_napi.publish {
            let npm_package_prefix = match self.publish.npm_napi.scope.clone() {
                Some(s) => format!("@{}/", s),
                None => "".to_string(),
            };
            let package_name = format!("{}{}", npm_package_prefix, self.package.clone());
            log::debug!("NPM: checking if version {} of {} already exists", self.version, package_name);
            self.publish.npm_napi.check(npm, package_name.clone(), self.version.clone()).await?;
        }
        Ok(self)
    }
}

impl Display for Result {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f,
               "{} -- {} -- {}: docker: {}, cargo: {}, npm_napi: {}, binary: {}",
               self.workspace, self.package, self.version,
               self.publish.docker.publish,
               self.publish.cargo.publish,
               self.publish.npm_napi.publish,
               self.publish.binary)
    }
}

#[derive(Serialize)]
pub struct Results(Vec<Result>);

impl Display for Results {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        for v in &self.0 {
            writeln!(f, "{}", v)?;
        }
        Ok(())
    }
}

pub async fn publishable(options: Options, working_directory: PathBuf) -> anyhow::Result<Results> {
    log::info!("Check directory for crates that need publishing");
    let started = Instant::now();
    let path = match working_directory.is_absolute() {
        true => working_directory.clone(),
        false => working_directory.canonicalize().with_context(|| format!("Failed to get absolute path from {:?}", working_directory))?,
    };

    log::debug!("Base directory: {:?}", path);
    // 1. Find all workspaces to investigate
    if options.progress {
        println!(
            "{} {}Resolving workspaces...",
            style("[1/4]").bold().dim(),
            LOOKING_GLASS
        );
    }
    let roots = get_cargo_roots(path).with_context(|| format!("Failed to get roots from {:?}", working_directory))?;
    let mut packages = vec![];
    // 2. For each workspace, find if one of the subcrates needs publishing
    if options.progress {
        println!(
            "{} {}Resolving packages...",
            style("[2/4]").bold().dim(),
            TRUCK
        );
    }
    for root in roots {
        log::debug!("Checking publishing for: {:?}", root);
        if let Some(workspace_name) = root.file_name() {
            let workspace_metadata = MetadataCommand::new()
                .current_dir(root.clone())
                .no_deps()
                .exec()
                .unwrap();
            for package in workspace_metadata.packages {
                match Result::new(workspace_name.to_string_lossy().to_string(), package.clone()) {
                    Ok(r) => packages.push(r),
                    Err(e) => {
                        let error_msg = format!("Could not check package {}: {}", package.name, e);
                        if options.fail_unit_error {
                            anyhow::bail!(error_msg)
                        } else {
                            log::warn!("{}", error_msg);
                            continue;
                        }
                    }
                }
            }
        }
    }
    if options.progress {
        println!(
            "{} {}Checking published status...",
            style("[3/3]").bold().dim(),
            PAPER
        );
    }
    // TODO: switch to an ASYNC_ONCE or something
    let npm = Npm::new(options.npm_registry_url.clone(), options.npm_registry_token.clone(), options.npm_registry_npmrc_path.clone(), true)?;
    let mut pb: Option<ProgressBar> = None;
    if options.progress {
        pb = Some(ProgressBar::new(packages.len() as u64).with_style(ProgressStyle::with_template("{spinner} {wide_msg} {pos}/{len}")?));
    }
    let mut results: Vec<Result> = vec![];
    for package in packages {
        if let Some(ref pb) = pb {
            pb.set_message(format!("{} : {}", package.workspace, package.package));
            pb.inc(1);
        }
        match package.clone().check_publishable(&options, &npm).await {
            Ok(result) => {
                let should_add = match options.hide_unpublishable {
                    true => vec![result.publish.docker.publish, result.publish.binary, result.publish.npm_napi.publish, result.publish.cargo.publish].into_iter().any(|x| x),
                    false => false,
                };
                if should_add {
                    results.push(result);
                }
            }
            Err(e) => {
                let error_msg = format!("Could not check package {} -- {}: {}", package.workspace.clone(), package.package.clone(), e);
                if options.fail_unit_error {
                    anyhow::bail!(error_msg)
                } else {
                    log::warn!("{}", error_msg);
                    continue;
                }
            }
        }
    }
    if options.progress {
        println!("{} Done in {}", SPARKLE, HumanDuration(started.elapsed()));
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
}
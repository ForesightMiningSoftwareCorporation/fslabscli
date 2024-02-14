use std::fmt::{Display, Formatter};
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Context;
use cargo_metadata::{MetadataCommand, Package};
use clap::Parser;
use console::{Emoji, style};
use indicatif::{HumanDuration, ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};
use serde_json::from_value;

use cargo::{Cargo, PackageMetadataFslabsCiPublishCargo};
use docker::PackageMetadataFslabsCiPublishDocker;
use npm::{Npm, PackageMetadataFslabsCiPublishNpmNapi};

use crate::utils;

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
    #[arg(long)]
    cargo_registry: Option<String>,
    #[arg(long)]
    cargo_registry_url: Option<String>,
    #[arg(long)]
    cargo_registry_user_agent: Option<String>,
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

    pub async fn check_publishable(mut self, options: &Options, npm: &Npm, cargo: &Cargo) -> anyhow::Result<Self> {
        self.publish.docker.check(
            self.package.clone(),
            self.version.clone(),
            options.docker_registry.clone(),
            options.docker_registry_username.clone(),
            options.docker_registry_password.clone(),
            None,
        ).await?;
        self.publish.npm_napi.check(self.package.clone(), self.version.clone(), npm).await?;
        self.publish.cargo.check(self.package.clone(), self.version.clone(), cargo).await?;
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
    let roots = utils::get_cargo_roots(path).with_context(|| format!("Failed to get roots from {:?}", working_directory))?;
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
    let mut cargo = Cargo::new(None)?;
    if let (Some(private_registry), Some(private_registry_url)) = (options.cargo_registry.clone(), options.cargo_registry_url.clone()) {
        cargo.add_registry(private_registry, private_registry_url, options.cargo_registry_user_agent.clone())?;
    }
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
        match package.clone().check_publishable(&options, &npm, &cargo).await {
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
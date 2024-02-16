use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Context;
use cargo_metadata::{DependencyKind, MetadataCommand, Package};
use clap::Parser;
use console::{style, Emoji};
use git2::{DiffDelta, DiffOptions, Repository};
use indexmap::IndexMap;
use indicatif::{HumanDuration, ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};
use serde_json::from_value;
use serde_yaml::Value;

use cargo::{Cargo, PackageMetadataFslabsCiPublishCargo};
use docker::PackageMetadataFslabsCiPublishDocker;
use npm::{Npm, PackageMetadataFslabsCiPublishNpmNapi};

use crate::utils;

mod cargo;
mod docker;
mod npm;

static LOOKING_GLASS: Emoji<'_, '_> = Emoji("🔍  ", "");
static TRUCK: Emoji<'_, '_> = Emoji("🚚  ", "");
static PAPER: Emoji<'_, '_> = Emoji("📃  ", "");
static SPARKLE: Emoji<'_, '_> = Emoji("✨ ", ":-)");

#[derive(Debug, Parser, Default)]
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
    #[arg(long, default_value_t = false)]
    progress: bool,
    #[arg(long, default_value_t = false)]
    pub(crate) check_publish: bool,
    #[arg(long, default_value_t = false)]
    pub(crate) check_changed: bool,
    #[arg(long, default_value = "HEAD")]
    changed_head_ref: String,
    #[arg(long, default_value = "HEAD~")]
    changed_base_ref: String,
    #[arg(long, default_value_t = false)]
    fail_unit_error: bool,
}

impl Options {
    pub fn new() -> Self {
        Self::default()
    }
}

#[derive(Serialize, Clone, Default, Debug)]
pub struct ResultDependency {
    pub package: String,
    pub version: String,
}

#[derive(Serialize, Clone, Default, Debug)]
pub struct Result {
    pub workspace: String,
    pub package: String,
    pub version: String,
    pub path: PathBuf,
    pub publish_detail: PackageMetadataFslabsCiPublish,
    pub publish: bool,
    pub dependencies: Vec<ResultDependency>,
    pub dependant: Vec<ResultDependency>,
    pub changed: bool,
    pub dependencies_changed: bool,
    pub ci_args: Option<IndexMap<String, Value>>,
}

fn default_false() -> bool {
    false
}

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
    pub args: Option<IndexMap<String, Value>>,
}

#[derive(Deserialize, Default)]
struct PackageMetadata {
    pub fslabs: PackageMetadataFslabsCi,
}

impl Result {
    pub fn new(workspace: String, package: Package, root_dir: PathBuf) -> anyhow::Result<Self> {
        let path = package
            .manifest_path
            .canonicalize()?
            .parent()
            .unwrap()
            .to_path_buf();
        let metadata: PackageMetadata =
            from_value(package.metadata).unwrap_or_else(|_| PackageMetadata::default());
        let mut publish = metadata.fslabs.publish;
        // Let's parse cargo publishing from main metadata
        publish.cargo.registry = package.publish;
        let dependencies = package
            .dependencies
            .into_iter()
            .filter(|p| p.kind == DependencyKind::Normal)
            .map(|d| ResultDependency {
                package: d.name,
                version: d.req.to_string(),
            })
            .collect();
        let mut path = path.strip_prefix(root_dir)?.to_path_buf();
        if path.to_string_lossy().is_empty() {
            path = PathBuf::from(".");
        }
        Ok(Self {
            workspace,
            package: package.name,
            version: package.version.to_string(),
            path,
            publish_detail: publish,
            ci_args: metadata.fslabs.args,
            dependencies,
            ..Default::default()
        })
    }

    pub async fn check_publishable(
        &mut self,
        options: &Options,
        npm: &Npm,
        cargo: &Cargo,
    ) -> anyhow::Result<()> {
        self.publish_detail
            .docker
            .check(
                self.package.clone(),
                self.version.clone(),
                options.docker_registry.clone(),
                options.docker_registry_username.clone(),
                options.docker_registry_password.clone(),
                None,
            )
            .await?;
        self.publish_detail
            .npm_napi
            .check(self.package.clone(), self.version.clone(), npm)
            .await?;
        self.publish_detail
            .cargo
            .check(self.package.clone(), self.version.clone(), cargo)
            .await?;
        Ok(())
    }
}

impl Display for Result {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} -- {} -- {}: docker: {}, cargo: {}, npm_napi: {}, binary: {}",
            self.workspace,
            self.package,
            self.version,
            self.publish_detail.docker.publish,
            self.publish_detail.cargo.publish,
            self.publish_detail.npm_napi.publish,
            self.publish_detail.binary
        )
    }
}

#[derive(Serialize)]
pub struct Results(pub(crate) HashMap<String, Result>);

impl Display for Results {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        for (k, v) in &self.0 {
            writeln!(f, "{}: {}", k, v)?;
        }
        Ok(())
    }
}

pub async fn check_workspace(
    options: Box<Options>,
    working_directory: PathBuf,
) -> anyhow::Result<Results> {
    log::info!("Check directory for crates that need publishing");
    let started = Instant::now();
    let path = match working_directory.is_absolute() {
        true => working_directory.clone(),
        false => working_directory
            .canonicalize()
            .with_context(|| format!("Failed to get absolute path from {:?}", working_directory))?,
    };

    log::debug!("Base directory: {:?}", path);
    // 1. Find all workspaces to investigate
    if options.progress {
        println!(
            "{} {}Resolving workspaces...",
            style("[1/7]").bold().dim(),
            LOOKING_GLASS
        );
    }
    let roots = utils::get_cargo_roots(path)
        .with_context(|| format!("Failed to get roots from {:?}", working_directory))?;
    let mut packages: HashMap<String, Result> = HashMap::new();
    // 2. For each workspace, find if one of the subcrates needs publishing
    if options.progress {
        println!(
            "{} {}Resolving packages...",
            style("[2/7]").bold().dim(),
            TRUCK
        );
    }
    for root in roots {
        if let Some(workspace_name) = root.file_name() {
            let workspace_metadata = MetadataCommand::new()
                .current_dir(root.clone())
                .no_deps()
                .exec()
                .unwrap();
            for package in workspace_metadata.packages {
                match Result::new(
                    workspace_name.to_string_lossy().to_string(),
                    package.clone(),
                    working_directory.clone(),
                ) {
                    Ok(r) => {
                        packages.insert(r.package.clone(), r);
                    }
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
            style("[3/7]").bold().dim(),
            PAPER
        );
    }

    let package_keys: Vec<String> = packages.keys().cloned().collect();

    // TODO: switch to an ASYNC_ONCE or something
    let npm = Npm::new(
        options.npm_registry_url.clone(),
        options.npm_registry_token.clone(),
        options.npm_registry_npmrc_path.clone(),
        true,
    )?;
    let mut cargo = Cargo::new(None)?;
    if let (Some(private_registry), Some(private_registry_url)) = (
        options.cargo_registry.clone(),
        options.cargo_registry_url.clone(),
    ) {
        cargo.add_registry(
            private_registry,
            private_registry_url,
            options.cargo_registry_user_agent.clone(),
        )?;
    }
    let mut pb: Option<ProgressBar> = None;
    if options.progress {
        pb = Some(ProgressBar::new(packages.len() as u64).with_style(
            ProgressStyle::with_template("{spinner} {wide_msg} {pos}/{len}")?,
        ));
    }
    for package_key in package_keys.clone() {
        if let Some(ref pb) = pb {
            pb.inc(1);
        }
        if let Some(package) = packages.get_mut(&package_key) {
            if let Some(ref pb) = pb {
                pb.set_message(format!("{} : {}", package.workspace, package.package));
            }
            if options.check_publish {
                match package.check_publishable(&options, &npm, &cargo).await {
                    Ok(_) => {}
                    Err(e) => {
                        let error_msg = format!(
                            "Could not check package {} -- {}: {}",
                            package.workspace.clone(),
                            package.package.clone(),
                            e
                        );
                        if options.fail_unit_error {
                            anyhow::bail!(error_msg)
                        } else {
                            log::warn!("{}", error_msg);
                            continue;
                        }
                    }
                }
            }

            package.publish = vec![
                package.publish_detail.docker.publish,
                package.publish_detail.cargo.publish,
                package.publish_detail.npm_napi.publish,
                package.publish_detail.binary,
            ]
            .into_iter()
            .any(|x| x);
        }
    }

    if options.progress {
        println!(
            "{} {}Filtering packages dependencies...",
            style("[4/7]").bold().dim(),
            TRUCK
        );
    }
    let mut pb: Option<ProgressBar> = None;
    if options.progress {
        pb = Some(ProgressBar::new(packages.len() as u64).with_style(
            ProgressStyle::with_template("{spinner} {wide_msg} {pos}/{len}")?,
        ));
    }
    for package_key in package_keys.clone() {
        if let Some(ref pb) = pb {
            pb.inc(1);
        }
        // Loop through all the dependencies, if we don't know of it, skip it
        if let Some(package) = packages.get_mut(&package_key) {
            if let Some(ref pb) = pb {
                pb.set_message(format!("{} : {}", package.workspace, package.package));
            }
            package
                .dependencies
                .retain(|d| package_keys.contains(&d.package));
        }
    }
    // 4 Feed Dependent
    if options.progress {
        println!(
            "{} {}Feeding packages dependant...",
            style("[5/7]").bold().dim(),
            TRUCK
        );
    }

    if options.progress {
        pb = Some(ProgressBar::new(packages.len() as u64).with_style(
            ProgressStyle::with_template("{spinner} {wide_msg} {pos}/{len}")?,
        ));
    }
    let package_keys: Vec<String> = packages.keys().cloned().collect();
    for package_key in package_keys.clone() {
        if let Some(ref pb) = pb {
            pb.inc(1);
        }
        // Loop through all the dependencies, if we don't know of it, skip it
        if let Some(package) = packages.get(&package_key).cloned() {
            if let Some(ref pb) = pb {
                pb.set_message(format!("{} : {}", package.workspace, package.package));
            }
            // for each dependency we need to edit it and add ourself as a dependeant
            for dependency in package.dependencies.clone() {
                if let Some(dependant) = packages.get_mut(&dependency.package) {
                    dependant.dependant.push(ResultDependency {
                        package: package.package.clone(),
                        version: package.version.clone(),
                    });
                }
            }
        }
    }

    if options.progress {
        println!(
            "{} {}Checking if packages changed...",
            style("[6/7]").bold().dim(),
            TRUCK
        );
    }
    if options.check_changed {
        let repository = Repository::open(working_directory.clone())?;
        // Get the commits objects based on the head ref and base ref
        let head_commit = repository.revparse_single(&options.changed_head_ref)?;
        let base_commit = repository.revparse_single(&options.changed_base_ref)?;
        // Get the tree for the commits
        let head_tree = head_commit.peel_to_tree()?;
        let base_tree = base_commit.peel_to_tree()?;
        if options.progress {
            pb = Some(ProgressBar::new(packages.len() as u64).with_style(
                ProgressStyle::with_template("{spinner} {wide_msg} {pos}/{len}")?,
            ));
        }

        // Check changed from a git pov
        for package_key in package_keys.clone() {
            if let Some(ref pb) = pb {
                pb.inc(1);
            }
            if let Some(package) = packages.get_mut(&package_key) {
                if let Some(ref pb) = pb {
                    pb.set_message(format!("{} : {}", package.workspace, package.package));
                }
                // let Ok(folder_entry) = head_tree.get_path(package_folder) else {
                //     continue;
                // };

                let package_folder = match &package.path.to_string_lossy().to_string() == "." {
                    true => "".to_string(),
                    false => package.path.clone().to_string_lossy().to_string(),
                };
                let mut diff_options = DiffOptions::new();
                diff_options.include_unmodified(true);
                let Ok(diff) = repository.diff_tree_to_tree(
                    Some(&base_tree),
                    Some(&head_tree),
                    Some(&mut diff_options),
                ) else {
                    continue;
                };
                let mut file_cb = |delta: DiffDelta, _: f32| -> bool {
                    let check_old_file = match delta.old_file().path() {
                        Some(p) => package_folder.is_empty() || p.starts_with(&package_folder),
                        None => false,
                    };
                    let check_new_file = match delta.new_file().path() {
                        Some(p) => package_folder.is_empty() || p.starts_with(&package_folder),
                        None => false,
                    };
                    if check_old_file || check_new_file {
                        let old_oid = delta.old_file().id();
                        let new_oid = delta.new_file().id();
                        if old_oid != new_oid {
                            package.changed = true;
                            return false;
                        }
                    }
                    true
                };
                if diff.foreach(&mut file_cb, None, None, None).is_err() {
                    continue;
                }
            }
        }
        // Now that git changes has been checked, we should loop through all package, if it has changed, we should mark
        // all it's dependant recursively as changed
    }
    if options.progress {
        println!(
            "{} {}Marking packages dependency as changed...",
            style("[7/7]").bold().dim(),
            TRUCK
        );
    }
    if options.check_changed {
        if options.progress {
            pb = Some(ProgressBar::new(packages.len() as u64).with_style(
                ProgressStyle::with_template("{spinner} {wide_msg} {pos}/{len}")?,
            ));
        }

        // Check changed from a git pov
        for package_key in package_keys.clone() {
            if let Some(ref pb) = pb {
                pb.inc(1);
            }
            if let Some(package) = packages.get(&package_key) {
                if let Some(ref pb) = pb {
                    pb.set_message(format!("{} : {}", package.workspace, package.package));
                }
                if !package.changed {
                    continue;
                }
                if package.dependencies_changed {
                    // We already treated it's tree
                    continue;
                }
                let dependant: Vec<String> = package
                    .dependant
                    .iter()
                    .map(|p| p.package.clone())
                    .collect();
                mark_dependants_as_changed(&mut packages, &dependant);
            }
        }
    }
    if options.progress {
        println!("{} Done in {}", SPARKLE, HumanDuration(started.elapsed()));
    }

    Ok(Results(packages))
}

fn mark_dependants_as_changed(all_packages: &mut HashMap<String, Result>, changed: &Vec<String>) {
    for package_key in changed {
        if let Some(package) = all_packages.get_mut(package_key) {
            if package.dependencies_changed {
                // already treated
                continue;
            }
            package.dependencies_changed = true;
            let dependant: Vec<String> = package
                .dependant
                .iter()
                .map(|p| p.package.clone())
                .collect();
            mark_dependants_as_changed(all_packages, &dependant);
        }
    }
}

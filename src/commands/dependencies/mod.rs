use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Context;
use cargo_metadata::{MetadataCommand, Package};
use clap::Parser;
use console::{Emoji, style};
use indicatif::{HumanDuration, ProgressBar, ProgressStyle};
use serde::Serialize;

use crate::utils::get_cargo_roots;

static LOOKING_GLASS: Emoji<'_, '_> = Emoji("🔍  ", "");
static TRUCK: Emoji<'_, '_> = Emoji("🚚  ", "");
static SPARKLE: Emoji<'_, '_> = Emoji("✨ ", ":-)");

#[derive(Debug, Parser)]
#[command(about = "Check dependencies for crates in directory.")]
pub struct Options {
    #[arg(long, default_value_t = false)]
    progress: bool,
    #[arg(long, default_value_t = false)]
    fail_unit_error: bool,
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
    pub dependencies: Vec<ResultDependency>,
    pub dependant: Vec<ResultDependency>,
}


impl Result {
    pub fn new(workspace: String, package: Package) -> anyhow::Result<Self> {
        let path = package.manifest_path.canonicalize()?.parent().unwrap().to_path_buf();
        let dependencies = package.dependencies.into_iter().map(|d| ResultDependency {
            package: d.name,
            version: d.req.to_string(),
        }).collect();
        Ok(Self {
            workspace,
            package: package.name,
            version: package.version.to_string(),
            path,
            dependencies,
            ..Default::default()
        })
    }
}

impl Display for Result {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f,
               "{} -- {} -- {}",
               self.workspace, self.package, self.version, )
    }
}

#[derive(Serialize)]
pub struct Results(Vec<Result>);

impl Display for Results {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        for v in &self.0 {
            writeln!(f, "{:?}", v)?;
        }
        Ok(())
    }
}

pub async fn dependencies(options: Options, working_directory: PathBuf) -> anyhow::Result<Results> {
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
    let mut packages: HashMap<String, Result> = HashMap::new();
    // 2. For each workspace, find if one of the subcrates needs publishing
    if options.progress {
        println!(
            "{} {}Fetching packages informations...",
            style("[2/4]").bold().dim(),
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
                match Result::new(workspace_name.to_string_lossy().to_string(), package.clone()) {
                    Ok(r) => {
                        packages.insert(r.package.clone(), r.clone());
                    }
                    Err(e) => {
                        let error_msg = format!("Could not check package {}: {}", package.name, e);
                        if options.fail_unit_error {
                            anyhow::bail!(error_msg);
                        } else {
                            log::warn!("{}", error_msg);
                        };
                    }
                }
            }
        }
    }
    // 3. Filter dependencies we now of
    if options.progress {
        println!(
            "{} {}Filtering packages dependencies...",
            style("[3/4]").bold().dim(),
            TRUCK
        );
    }
    let mut pb: Option<ProgressBar> = None;
    if options.progress {
        pb = Some(ProgressBar::new(packages.len() as u64).with_style(ProgressStyle::with_template("{spinner} {wide_msg} {pos}/{len}")?));
    }
    let package_keys: Vec<String> = packages.keys().cloned().collect();
    for package_key in package_keys.clone() {
        if let Some(ref pb) = pb {
            pb.inc(1);
        }
        // Loop through all the dependencies, if we don't know of it, skip it
        if let Some(package) = packages.get_mut(&package_key) {
            if let Some(ref pb) = pb {
                pb.set_message(format!("{} : {}", package.workspace, package.package));
            }
            package.dependencies.retain(|d| package_keys.contains(&d.package));
        }
    }
    // 4 Feed Dependent
    if options.progress {
        println!(
            "{} {}Feeding packages dependant...",
            style("[4/4]").bold().dim(),
            TRUCK
        );
    }

    if options.progress {
        pb = Some(ProgressBar::new(packages.len() as u64).with_style(ProgressStyle::with_template("{spinner} {wide_msg} {pos}/{len}")?));
    }
    let package_keys: Vec<String> = packages.keys().cloned().collect();
    for package_key in package_keys.clone() {
        if let Some(ref pb) = pb {
            pb.inc(1);
        }
        // Loop through all the dependencies, if we don't know of it, skip it
        if let Some(package) = packages.get(&package_key).map(|c| c.clone()) {
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
    // let mut pb: Option<ProgressBar> = None;
    // if options.progress {
    //     pb = Some(ProgressBar::new(packages.len() as u64).with_style(ProgressStyle::with_template("{spinner} {wide_msg} {pos}/{len}")?));
    // }
    // let mut results: HashMap<String, Result> = HashMap::from(packages.clone());
    // for package_key in packages.keys() {
    //     if let Some(ref pb) = pb {
    //         pb.inc(1);
    //     }
    //     // Loop through all the dependencies, if we don't know of it, skip it
    //     let Some(mut package) = packages.get(&package_key.clone()).map(|c| c.clone()) else {
    //         continue;
    //     };
    //     if let Some(ref pb) = pb {
    //         pb.set_message(format!("{} : {}", package.workspace, package.package));
    //     }
    //     package.dependencies = package.dependencies.iter().filter(|d| packages.contains_key(&d.package)).map(|d| d.clone()).collect();
    //     results.insert(package_key.clone(), package.clone());
    // }
    if options.progress {
        println!("{} Done in {}", SPARKLE, HumanDuration(started.elapsed()));
    }
    Ok(Results(packages.values().map(|d| d.clone()).collect()))
}


#[cfg(test)]
mod tests {}
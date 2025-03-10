use anyhow::Context;
use cargo_metadata::{DependencyKind, PackageId};
use clap::Parser;
use octocrab::Octocrab;
use serde::Serialize;
use std::collections::HashMap;
use std::io::Read;
use std::{
    fmt::{Display, Formatter},
    fs,
    fs::File,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};
use tokio::sync::Semaphore;

use crate::{
    PrettyPrintable,
    commands::check_workspace::{
        Options as CheckWorkspaceOptions, Result as Package, check_workspace,
    },
    crate_graph::CrateGraph,
    utils::execute_command,
};

#[derive(Debug, Parser, Default, Clone)]
#[command(about = "Run rust tests")]
pub struct Options {
    #[clap(long, env, default_value = ".")]
    artifacts: PathBuf,
    #[clap(long, env)]
    pull_base_ref: String,
    #[arg(long, env)]
    repo_owner: String,
    #[arg(long, env)]
    repo_name: String,
    #[arg(long, env)]
    github_token: String,
    #[arg(long, env, default_value = "5")]
    job_limit: usize,
}

#[derive(Serialize)]
pub struct PublishResult {}

impl Display for PublishResult {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "")
    }
}

impl PrettyPrintable for PublishResult {
    fn pretty_print(&self) -> String {
        "".to_string()
    }
}

/// copy_files copy files from src_dir to dest_dir and returns which files were copied
fn copy_files(src_dir: &PathBuf, dest_dir: &PathBuf) -> anyhow::Result<Vec<PathBuf>> {
    let mut copied_paths = Vec::new();

    for entry in fs::read_dir(src_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() {
            let filename = path.file_name().unwrap().to_str().unwrap();
            let dest_path = Path::new(dest_dir).join(filename);
            fs::copy(path, &dest_path)?;
            copied_paths.push(dest_path);
        }
    }

    Ok(copied_paths)
}

async fn publish_package(
    repo_root: PathBuf,
    package: Package,
    semaphore: Arc<Semaphore>,
    dependencies: Option<Vec<PackageId>>,
    statuses: Arc<RwLock<HashMap<PackageId, Option<bool>>>>,
    output_dir: PathBuf,
) {
    if let Some(ref package_id) = package.package_id {
        loop {
            let mut mark_failed = false;
            let mut process = true;
            {
                if let Some(ref deps) = dependencies {
                    for dep_id in deps {
                        let map = statuses.read().expect("RwLock poisoned");
                        if let Some(dep_status) = map.get(dep_id) {
                            match dep_status {
                                Some(success) => {
                                    if !success {
                                        mark_failed = true;
                                        process = false;
                                    }
                                }
                                None => {
                                    process = false;
                                }
                            }
                        }
                    }
                }
            }
            if mark_failed {
                let mut map = statuses.write().expect("RwLock posoned");
                *map.entry(package_id.clone()).or_insert(None) = Some(false);
                drop(map);
                return;
            }
            if process {
                break;
            }
            // Add a small delay to allow other tasks to make progress
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        // Acquire a permit from the semaphore to limit the number of concurrent tasks
        let permit = semaphore.acquire().await;
        let success = do_publish_package(repo_root.clone(), package.clone(), output_dir)
            .await
            .is_ok();
        let mut map = statuses.write().expect("RwLock poisoned");
        *map.entry(package_id.clone()).or_insert(None) = Some(success);
        drop(permit);
    }
}

async fn do_publish_package(
    repo_root: PathBuf,
    package: Package,
    output_dir: PathBuf,
) -> anyhow::Result<()> {
    let _workspace_name = &package.workspace;
    let package_name = &package.package;
    let _package_version = &package.version;
    let package_path = repo_root.join(&package.path);
    let mut overall_success = true;
    if package.publish_detail.nix_binary.publish {
        let (_stdout, _stderr, mut success) = execute_command(
            "nix build .#release",
            &package_path,
            &HashMap::new(),
            Some(tracing::Level::DEBUG),
            Some(tracing::Level::DEBUG),
        )
        .await;
        if success {
            // Let's copy the artifacts to the
            success = copy_files(&package_path.join("result/bin"), &output_dir).is_ok();
        }
        overall_success = success;
    }
    match overall_success {
        true => {
            println!("Published package {}", package_name);
            Ok(())
        }
        false => Err(anyhow::anyhow!(
            "Could not publish package {}",
            package_name
        )),
    }
}

pub async fn publish(options: Box<Options>, repo_root: PathBuf) -> anyhow::Result<PublishResult> {
    let check_workspace_options = CheckWorkspaceOptions::new()
        .with_check_publish(true)
        .with_cargo_default_publish(true)
        .with_cargo_registry("foresight-mining-software-corporation".to_string())
        .with_cargo_registry_url("https://shipyard.rs/api/v1/shipyard/krates/by-name/".to_string())
        .with_cargo_registry_user_agent("shipyard ${SHIPYARD_TOKEN}".to_string())
        .with_ignore_dev_dependencies(true);

    let crates = CrateGraph::new(repo_root.clone(), Some(DependencyKind::Normal))?;
    let dependency_graph = crates.dependency_graph();
    let members = check_workspace(Box::new(check_workspace_options), repo_root.clone())
        .await
        .map_err(|e| {
            tracing::error!("Check directory for crates that need publishing: {}", e);
            e
        })
        .with_context(|| "Could not get directory information")?;

    let semaphore = Arc::new(Semaphore::new(options.job_limit));

    let mut handles = vec![];
    let mut init_status: HashMap<PackageId, Option<bool>> = HashMap::new();
    for member_id in members.0.keys() {
        init_status.insert(member_id.clone(), None);
    }
    let publish_status = Arc::new(RwLock::new(init_status));

    let artifact_dir = options.artifacts.join("output");
    fs::create_dir_all(&artifact_dir)?;
    // Spawn a task for each object
    for (_, member) in members.0 {
        if let Some(ref member_id) = member.package_id {
            let o = artifact_dir.clone();
            let m = member.clone();
            let r = repo_root.clone();
            let s = Arc::clone(&semaphore);
            let status = Arc::clone(&publish_status);
            let task_handle = tokio::spawn(publish_package(
                r,
                m,
                s,
                dependency_graph.dependencies.get(member_id).cloned(),
                status,
                o,
            ));
            handles.push(task_handle);
        }
    }
    futures::future::join_all(handles).await;

    // Send initial tasks to workers
    // We have a github token we should try to upload the artifact to the releas
    let octocrab = Octocrab::builder()
        .personal_token(options.github_token)
        .build()?;

    let repo = octocrab.repos(&options.repo_owner, &options.repo_name);
    let repo_releases = repo.releases();
    if let Ok(release) = repo_releases.get_by_tag(&options.pull_base_ref).await {
        tracing::info!("Updating github release {}", release.id);
        let paths = fs::read_dir(artifact_dir)?;
        for raw_artifact in paths {
            if let Ok(artifact) = raw_artifact {
                let artifact_path = artifact.path();
                if let Some(artifact_name) = artifact_path.file_name() {
                    if let Some(artifact_name) = artifact_name.to_str() {
                        tracing::debug!("Uploading github artifact {:?}", artifact_name);
                        if let Ok(mut file) = File::open(&artifact_path) {
                            if let Ok(metadata) = fs::metadata(&artifact_path) {
                                let mut data: Vec<u8> = vec![0; metadata.len() as usize];
                                if file.read(&mut data).is_ok() {
                                    let _ = repo_releases
                                        .upload_asset(
                                            release.id.into_inner(),
                                            artifact_name,
                                            data.into(),
                                        )
                                        .send()
                                        .await;
                                }
                            }
                        }
                    }
                }
            }
        }
    } else {
        tracing::info!("Could not find a github release to update, not doing anything");
    }

    Ok(PublishResult {})
}

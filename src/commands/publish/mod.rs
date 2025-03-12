use anyhow::Context;
use cargo_metadata::PackageId;
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

use crate::utils::github::{generate_github_app_token, InstallationRetrievalMode};
use crate::{
    commands::check_workspace::{
        check_workspace, Options as CheckWorkspaceOptions, Result as Package,
    },
    utils::{cargo::Cargo, execute_command},
    PrettyPrintable,
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
    github_app_id: u64,
    #[arg(long, env)]
    github_app_private_key: PathBuf,
    #[arg(long, env, default_value = "1")]
    job_limit: usize,
    #[arg(long, env)]
    ghcr_oci_url: Option<String>,
    #[arg(long, env)]
    ghcr_oci_username: Option<String>,
    #[arg(long, env)]
    ghcr_oci_password: Option<String>,
    #[arg(long, env)]
    docker_hub_username: Option<String>,
    #[arg(long, env)]
    docker_hub_password: Option<String>,
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
    cargo: Arc<Cargo>,
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
        let success = do_publish_package(repo_root.clone(), package.clone(), output_dir, cargo)
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
    cargo: Arc<Cargo>,
) -> anyhow::Result<()> {
    if !package.publish {
        return Ok(());
    }
    let _workspace_name = &package.workspace;
    let package_name = &package.package;
    println!("Publishing package {}", package_name);
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
    if package.publish_detail.cargo.publish {
        let additional_args = package.publish_detail.additional_args.unwrap_or_default();
        for (registry_name, registry_publish) in package.publish_detail.cargo.registries_publish {
            if !registry_publish {
                continue;
            }
            if cargo.get_registry(&registry_name).is_none() {
                continue;
            }
            let args = [
                additional_args.clone(),
                "--registry".to_string(),
                registry_name.clone(),
            ];
            let (_stdout, _stderr, success) = execute_command(
                &format!("cargo publish {}", args.join(" ")),
                &package_path,
                &HashMap::new(),
                Some(tracing::Level::DEBUG),
                Some(tracing::Level::DEBUG),
            )
            .await;
            tracing::info!("STDOUT: {}", _stdout);
            tracing::info!("STDERR: {}", _stderr);
            overall_success = success;
        }
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
        .with_ignore_dev_dependencies(true);

    // We might need to log to some docker registries
    if options.docker_hub_username.is_some() && options.docker_hub_password.is_some() {
        let (_stdout, stderr, success) = execute_command(
            "echo \"$DOCKER_HUB_PASSWORD\" | docker login registry-1.docker.io --username $DOCKER_HUB_USERNAME --password-stdin >/dev/null",
            &repo_root,
            &HashMap::new(),
            Some(tracing::Level::DEBUG),
            Some(tracing::Level::DEBUG),
        )
        .await;
        if !success {
            return Err(anyhow::anyhow!(stderr));
        }
    }
    if options.ghcr_oci_url.is_some()
        && options.ghcr_oci_username.is_some()
        && options.ghcr_oci_password.is_some()
    {
        let (_stdout, stderr, success) = execute_command(
            "echo \"${GHCR_OCI_PASSWORD}\" | docker login \"${GHCR_OCI_URL#oci://}\" --username \"${GHCR_OCI_USERNAME}\" --password-stdin >/dev/null",
            &repo_root,
            &HashMap::new(),
            Some(tracing::Level::DEBUG),
            Some(tracing::Level::DEBUG),
        )
        .await;
        if !success {
            return Err(anyhow::anyhow!(stderr));
        }
    }

    let results = check_workspace(Box::new(check_workspace_options), repo_root.clone())
        .await
        .map_err(|e| {
            tracing::error!("Check directory for crates that need publishing: {}", e);
            e
        })
        .with_context(|| "Could not get directory information")?;

    let cargo = Arc::new(Cargo::new(&results.crate_graph)?);
    let semaphore = Arc::new(Semaphore::new(options.job_limit));

    let mut handles = vec![];
    let mut init_status: HashMap<PackageId, Option<bool>> = HashMap::new();
    for member_id in results.members.keys() {
        init_status.insert(member_id.clone(), None);
    }
    let publish_status = Arc::new(RwLock::new(init_status));

    let artifact_dir = options.artifacts.join("output");
    fs::create_dir_all(&artifact_dir)?;
    // Spawn a task for each object
    for (_, member) in results.members {
        if let Some(ref member_id) = member.package_id {
            let o = artifact_dir.clone();
            let m = member.clone();
            let r = repo_root.clone();
            let s = Arc::clone(&semaphore);
            let c = Arc::clone(&cargo);
            let status = Arc::clone(&publish_status);
            let task_handle = tokio::spawn(publish_package(
                r,
                m,
                s,
                results
                    .crate_graph
                    .dependency_graph()
                    .dependencies
                    .get(member_id)
                    .cloned(),
                status,
                o,
                c,
            ));
            handles.push(task_handle);
        }
    }
    futures::future::join_all(handles).await;

    let github_token = generate_github_app_token(
        options.github_app_id,
        options.github_app_private_key.clone(),
        InstallationRetrievalMode::Organization,
        Some(options.repo_owner.clone()),
    )
    .await?;
    let octocrab = Octocrab::builder().personal_token(github_token).build()?;

    let repo = octocrab.repos(&options.repo_owner, &options.repo_name);
    let repo_releases = repo.releases();
    if let Ok(release) = repo_releases.get_by_tag(&options.pull_base_ref).await {
        let paths = fs::read_dir(&artifact_dir)?;
        for artifact in paths.flatten() {
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
    } else {
        tracing::info!("Could not find a github release to update, not doing anything");
    }

    Ok(PublishResult {})
}

use anyhow::Context;
use cargo_metadata::PackageId;
use clap::Parser;
use git2::Repository;
use junit_report::{Duration, ReportBuilder, TestCase, TestSuiteBuilder};
use octocrab::Octocrab;
use octocrab::params::repos::Reference;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::time::SystemTime;
use std::{
    env,
    fmt::{Display, Formatter},
    fs,
    fs::File,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};
use tokio::sync::Semaphore;
use tracing::{debug, info};

use crate::utils::get_registry_env;
use crate::utils::github::{InstallationRetrievalMode, generate_github_app_token};
use crate::{
    PrettyPrintable,
    commands::check_workspace::{
        Options as CheckWorkspaceOptions, Result as Package, check_workspace,
    },
    utils::{
        cargo::{Cargo, patch_crate_for_registry},
        execute_command,
    },
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
    github_app_id: Option<u64>,
    #[arg(long, env)]
    github_app_private_key: Option<PathBuf>,
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
    #[arg(long, env)]
    npm_ghcr_scope: Option<String>,
    #[arg(long, env)]
    npm_ghcr_token: Option<String>,
    #[arg(long, env, default_value = "foresight-mining-software-corporation")]
    cargo_main_registry: String,
    #[arg(long, env, default_value = "false")]
    dry_run: bool,
    #[arg(long, env, default_value = "false")]
    handle_tags: bool,
}

#[derive(Serialize, Default, Clone)]
pub struct PublishDetailResult {
    pub name: String,
    pub key: String,
    pub should_publish: bool,
    pub success: bool,
    pub error: String,
    pub stderr: String,
    pub stdout: String,
    pub start_time: Option<SystemTime>,
    pub end_time: Option<SystemTime>,
}

static SKIPPED: &str = "-";
static SUCCESS: &str = "✔";
static FAILED: &str = "✘";

impl PublishDetailResult {
    pub fn get_status(&self) -> String {
        if !self.should_publish {
            SKIPPED.to_string()
        } else if self.success {
            SUCCESS.to_string()
        } else {
            FAILED.to_string()
        }
    }
    pub fn get_junit_testcase(&self) -> TestCase {
        match self.should_publish {
            true => {
                let duration = match (self.start_time, self.end_time) {
                    (Some(s), Some(e)) => Duration::seconds_f64(
                        e.duration_since(s)
                            .map(|d| d.as_secs_f64())
                            .unwrap_or_else(|_| 0.0),
                    ),
                    _ => Duration::default(),
                };
                let mut tc = match self.success {
                    true => TestCase::success(&self.name, duration),
                    false => TestCase::failure(&self.name, duration, &self.name, "required"),
                };
                // stdout and stderr are inverted because we want to still output stderr on console
                tc.set_system_out(&self.stderr);
                tc.set_system_err(&self.stdout);
                tc
            }
            false => TestCase::skipped(&self.name),
        }
    }
}
#[derive(Serialize, Default, Clone)]
pub struct PublishResult {
    pub should_publish: bool,
    pub success: bool,
    pub docker: PublishDetailResult,
    pub cargo: HashMap<String, PublishDetailResult>, // HashMap on Registries
    pub nix_binary: PublishDetailResult,
    pub git_tag: PublishDetailResult,
    pub start_time: Option<SystemTime>,
    pub end_time: Option<SystemTime>,
}

impl PublishResult {
    pub fn new(package: &Package, registries: HashSet<String>, options: Box<Options>) -> Self {
        let mut s = Self {
            should_publish: package.publish,
            docker: PublishDetailResult {
                name: "docker build && docker push".to_string(),
                key: "docker".to_string(),
                should_publish: package.publish_detail.docker.publish,
                ..Default::default()
            },
            nix_binary: PublishDetailResult {
                name: "nix build .#release".to_string(),
                key: "nix".to_string(),
                should_publish: package.publish_detail.nix_binary.publish,
                ..Default::default()
            },
            git_tag: PublishDetailResult {
                name: "git tag".to_string(),
                key: "git".to_string(),
                should_publish: options.handle_tags,
                ..Default::default()
            },
            ..Default::default()
        };

        for registry_name in &registries {
            s.cargo.insert(
                registry_name.clone(),
                PublishDetailResult {
                    should_publish: *package
                        .publish_detail
                        .cargo
                        .registries_publish
                        .get(registry_name)
                        .unwrap_or(&false),
                    name: format!("cargo publish -r {}", registry_name),
                    key: format!("cargo_{}", registry_name),
                    ..Default::default()
                },
            );
        }

        s
    }

    pub fn with_failed(mut self, failed: bool) -> Self {
        self.success = !failed;
        self
    }
}

#[derive(Serialize, Default)]
pub struct PublishResults {
    pub published_members: HashMap<PackageId, PublishResult>,
    pub all_members: HashMap<PackageId, Package>,
}

impl PublishResults {
    fn craft_junit(&self, output_dir: &Path) -> anyhow::Result<()> {
        let mut registries = HashMap::new();
        for package in self.all_members.values() {
            for registry_name in package.publish_detail.cargo.registries_publish.keys() {
                registries.insert(registry_name, registry_name.len());
            }
        }
        let mut junit_report = ReportBuilder::new().build();
        for (package_id, package) in &self.all_members {
            let workspace_name = &package.workspace;
            let package_name = &package.package;
            let package_version = &package.version;
            let ts_name = format!("{workspace_name} - {package_name} - {package_version}");
            let mut ts = TestSuiteBuilder::new(&ts_name).build();
            if let Some(publish_result) = self.published_members.get(package_id) {
                let mut results = vec![
                    &publish_result.nix_binary,
                    &publish_result.docker,
                    &publish_result.git_tag,
                ];
                for cargo in publish_result.cargo.values() {
                    results.push(cargo);
                }
                ts.add_testcases(results.into_iter().map(|r| r.get_junit_testcase()));
                junit_report.add_testsuite(ts);
            }
        }
        let mut junit_file = File::create(output_dir.join("junit.rust.xml"))?;
        junit_report.write_xml(&mut junit_file)?;
        Ok(())
    }
    fn store_logs(&self, output_dir: &Path) -> anyhow::Result<()> {
        let logs_dir = output_dir.join("logs");
        fs::create_dir_all(&logs_dir)?;
        for (package_id, package) in &self.all_members {
            let package_name = &package.package;
            let package_version = &package.version;
            let file_prefix = format!("{package_name}__{package_version}");
            if let Some(publish_result) = self.published_members.get(package_id) {
                let mut results = vec![&publish_result.nix_binary, &publish_result.docker];
                for cargo in publish_result.cargo.values() {
                    results.push(cargo);
                }
                for r in results {
                    // stdout and stderr are inverted because we want to still output stderr on console
                    if !r.stderr.is_empty() {
                        let mut stdout_file = File::create(
                            logs_dir.join(format!("{file_prefix}_{}.out.log", r.key)),
                        )?;
                        stdout_file.write_all(r.stderr.as_bytes())?;
                    }
                    if !r.stdout.is_empty() {
                        let mut stderr_file = File::create(
                            logs_dir.join(format!("{file_prefix}_{}.err.log", r.key)),
                        )?;
                        stderr_file.write_all(r.stdout.as_bytes())?;
                    }
                }
            }
        }
        Ok(())
    }
}

impl Display for PublishResults {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut registries = HashMap::new();
        for package in self.all_members.values() {
            for registry_name in package.publish_detail.cargo.registries_publish.keys() {
                registries.insert(registry_name, registry_name.len());
            }
        }
        let cargo_size: usize = registries.values().sum::<usize>() + 9;
        let empty_cargo_reg_headers = &registries
            .clone()
            .into_values()
            .map(|size| format!("{:─^width$}", "", width = size + 4))
            .collect::<Vec<String>>()
            .join("┼");
        let empty_last_cargo_reg_headers = &registries
            .clone()
            .into_values()
            .map(|size| format!("{:─^width$}", "", width = size + 4))
            .collect::<Vec<String>>()
            .join("┴");
        let cargo_reg_headers = &registries
            .clone()
            .into_iter()
            .map(|(registry_name, size)| format!("{:^width$}", registry_name, width = size + 4))
            .collect::<Vec<String>>()
            .join("│");
        writeln!(
            f,
            "┌{:─^60}┬{:─^20}┬{:─^15}┬{:─^width$}┬{:─^15}┐",
            " Package ",
            " Version ",
            " Docker ",
            " Cargo ",
            " Nix Binary ",
            width = cargo_size
        )?;

        writeln!(
            f,
            "│{:60}│{:20}│{:15}│{:^width$}│{:15}│",
            "",
            "",
            "",
            cargo_reg_headers,
            "",
            width = cargo_size
        )?;
        for (package_name, publish_result) in self.published_members.clone().into_iter() {
            let mut id = package_name.to_string().clone();
            id = id.as_str().rsplit_once('/').unwrap().1.to_string();
            let (name, mut version) = id.split_once('#').unwrap();
            if version.contains('@') {
                version = version.split_once('@').unwrap().1;
            }
            let cargo_reg = &registries
                .clone()
                .into_iter()
                .map(|(registry_name, size)| {
                    let s = match publish_result.cargo.get(registry_name) {
                        Some(s) => format!(" {} ", s.get_status()),
                        None => SKIPPED.to_string(),
                    };
                    format!("{:^width$}", s, width = size + 4)
                })
                .collect::<Vec<String>>()
                .join("│");
            writeln!(
                f,
                "├{:─^60}┼{:─^20}┼{:─^15}┼{:─^width$}┼{:─^15}┤",
                "",
                "",
                "",
                empty_cargo_reg_headers,
                "",
                width = cargo_size
            )?;

            writeln!(
                f,
                "│{:^60}│{:^20}│{:^15}│{:^width$}│{:^15}│",
                name,
                version,
                publish_result.docker.get_status(),
                cargo_reg,
                publish_result.nix_binary.get_status(),
                width = cargo_size,
            )?;
        }
        writeln!(
            f,
            "└{:─^60}┴{:─^20}┴{:─^15}┴{:─^width$}┴{:─^15}┘",
            "",
            "",
            "",
            empty_last_cargo_reg_headers,
            "",
            width = cargo_size
        )?;
        Ok(())
    }
}

impl PrettyPrintable for PublishResults {
    fn pretty_print(&self) -> String {
        self.to_string()
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

/// publish_package handles the dependencies waiting and stuff like that
#[allow(clippy::too_many_arguments)]
async fn publish_package(
    repo_root: PathBuf,
    package: Package,
    semaphore: Arc<Semaphore>,
    dependencies: Option<Vec<PackageId>>,
    statuses: Arc<RwLock<HashMap<PackageId, Option<PublishResult>>>>,
    output_dir: PathBuf,
    cargo: Arc<Cargo>,
    options: Box<Options>,
    registries: HashSet<String>,
) {
    if let Some(ref package_id) = package.package_id {
        loop {
            // println!("Looping on package: {}", package.package);
            let mut mark_failed = false;
            let mut process = true;
            {
                if let Some(ref deps) = dependencies {
                    for dep_id in deps {
                        let map = statuses.read().expect("RwLock poisoned");
                        if let Some(dep_result) = map.get(dep_id) {
                            match dep_result {
                                Some(result) => {
                                    if result.should_publish && !result.success {
                                        // Dep should have published, but has not done so succesfully
                                        mark_failed = true;
                                        process = false;
                                    }
                                }
                                None => {
                                    // Dep should not yet published
                                    process = false;
                                }
                            }
                        }
                    }
                }
            }
            if mark_failed {
                let mut map = statuses.write().expect("RwLock posoned");
                let failed_result =
                    PublishResult::new(&package, registries, options).with_failed(true);
                *map.entry(package_id.clone()).or_insert(None) = Some(failed_result);
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
        debug!("Doing package: {}", package.package);
        let success = do_publish_package(
            repo_root.clone(),
            package.clone(),
            output_dir,
            cargo,
            options,
            registries,
        )
        .await;
        debug!("Done package: {}", package.package);
        let mut map = statuses.write().expect("RwLock poisoned");
        *map.entry(package_id.clone()).or_insert(None) = Some(success);
        drop(permit);
    }
}

/// Actual Publish
async fn do_publish_package(
    repo_root: PathBuf,
    package: Package,
    output_dir: PathBuf,
    cargo: Arc<Cargo>,
    options: Box<Options>,
    registries: HashSet<String>,
) -> PublishResult {
    let mut result = PublishResult::new(&package, registries, options.clone());
    result.start_time = Some(SystemTime::now());
    if !package.publish {
        result.end_time = Some(SystemTime::now());
        return result;
    }
    // let workspace_name = &package.workspace;
    let package_version = &package.version;
    let package_name = &package.package;
    let package_path = repo_root.join(&package.path);
    let mut is_failed = false;
    if !is_failed && package.publish_detail.nix_binary.publish {
        result.nix_binary.start_time = Some(SystemTime::now());
        if options.dry_run {
            result.nix_binary.success = true;
        } else {
            if let (Ok(atticd_url), Ok(atticd_cache), Ok(atticd_token)) = (
                env::var("ATTICD_URL"),
                env::var("ATTICD_CACHE"),
                env::var("ATTICD_TOKEN"),
            ) {
                info!("Login to atticd");
                let (stdout, stderr, success) = execute_command(
                    &format!("attic login central {}/ {}", atticd_url, atticd_token),
                    &repo_root,
                    &HashMap::new(),
                    &HashSet::new(),
                    Some(tracing::Level::DEBUG),
                    Some(tracing::Level::DEBUG),
                )
                .await;
                result.nix_binary.success = success;
                result.nix_binary.stdout = stdout;
                result.nix_binary.stderr = stderr;
                is_failed = !success;
                if !is_failed {
                    let (stdout, stderr, success) = execute_command(
                        &format!("attic use central:{}", atticd_cache),
                        &package_path,
                        &HashMap::new(),
                        &HashSet::new(),
                        Some(tracing::Level::DEBUG),
                        Some(tracing::Level::DEBUG),
                    )
                    .await;
                    result.nix_binary.success = success;
                    result.nix_binary.stdout = format!("{}\n{}", result.nix_binary.stdout, stdout);
                    result.nix_binary.stderr = format!("{}\n{}", result.nix_binary.stderr, stderr);
                    is_failed = !success;
                }
            }
            if !is_failed {
                let (stdout, stderr, mut success) = execute_command(
                    "nix build .#release",
                    &package_path,
                    &HashMap::new(),
                    &HashSet::new(),
                    Some(tracing::Level::INFO),
                    Some(tracing::Level::INFO),
                )
                .await;
                if success {
                    // Let's copy the artifacts to the
                    success = copy_files(&package_path.join("result/bin"), &output_dir).is_ok();
                }
                result.nix_binary.success = success;
                result.nix_binary.stdout = format!("{}\n{}", result.nix_binary.stdout, stdout);
                result.nix_binary.stderr = format!("{}\n{}", result.nix_binary.stderr, stderr);
                is_failed = !success;
            }
            if !is_failed {
                if let Ok(atticd_cache) = env::var("ATTICD_CACHE") {
                    // Let's push the store to cachix by rebuilding and pushing
                    info!("Pushing to atticd");
                    let (stdout, stderr, success) = execute_command(
                    &format!(
                        "attic push {} $(nix-store -qR --include-outputs $(nix-store -qd ./result) | grep -v '\\.drv$')",
                        atticd_cache
                    ),
                    &package_path,
                    &HashMap::new(),
                    &HashSet::new(),
                    Some(tracing::Level::INFO),
                    Some(tracing::Level::INFO),
                )
                .await;
                    result.nix_binary.success = success;
                    result.nix_binary.stdout = format!("{}\n{}", result.nix_binary.stdout, stdout);
                    result.nix_binary.stderr = format!("{}\n{}", result.nix_binary.stderr, stderr);
                    is_failed = !success;
                }
            }
        }
        result.nix_binary.end_time = Some(SystemTime::now());
    }
    if !is_failed && package.publish_detail.cargo.publish {
        let additional_args = package.publish_detail.additional_args.unwrap_or_default();
        for (registry_name, registry_publish) in package.publish_detail.cargo.registries_publish {
            let mut r = PublishDetailResult {
                start_time: Some(SystemTime::now()),
                ..Default::default()
            };
            let mut run = true;
            if !registry_publish {
                run = false;
            } else {
                r.should_publish = true;
            }
            if is_failed {
                run = false;
            }
            if cargo.get_registry(&registry_name).is_none() {
                run = false;
            }
            if run {
                let registry_prefix =
                    format!("CARGO_REGISTRIES_{}", registry_name.replace("-", "_")).to_uppercase();

                if std::env::var(format!("{}_INDEX", registry_prefix)).is_ok() {
                    // For each reg we need to
                    // 1. Ensure registry is in `publish = []`
                    // 2. Find and replace `main_registry` to `current_registry` in Cargo.toml
                    // 3. Ensure there are Cargo.lock
                    // 4. Publish with --allow-dirty
                    if patch_crate_for_registry(&repo_root, &package_path, registry_name.clone())
                        .is_ok()
                    {
                        // to publish to a registry we need
                        // - index url
                        // - user agent if set
                        // - ssh key
                        let envs = get_registry_env(registry_name.clone());
                        let mut blacklist_envs = HashSet::from([
                            "GIT_SSH_COMMAND".to_string(),
                            "SSH_AUTH_SOCK".to_string(),
                        ]);
                        for (key, _) in std::env::vars() {
                            if key.starts_with("CARGO_REGISTRIES_") {
                                blacklist_envs.insert(key);
                            }
                        }

                        let mut args = vec![
                            additional_args.clone(),
                            "--registry".to_string(),
                            registry_name.clone(),
                            "--allow-dirty".to_string(),
                        ];
                        if options.dry_run {
                            args.push("--dry-run".to_string())
                        }
                        let (stdout, stderr, success) = execute_command(
                            &format!("cargo publish {}", args.join(" ")),
                            &package_path,
                            &envs,
                            &blacklist_envs,
                            Some(tracing::Level::INFO),
                            Some(tracing::Level::INFO),
                        )
                        .await;
                        r.success = success;
                        r.stdout = stdout;
                        r.stderr = stderr;
                    } else {
                        r.success = false;
                        r.stderr = format!(
                            "registry {} not setup correctly, missing index, private_key, and token",
                            registry_name
                        );
                    }
                    // Path back to the main registry
                    if !patch_crate_for_registry(
                        &repo_root,
                        &package_path,
                        options.cargo_main_registry.clone(),
                    )
                    .is_ok()
                    {
                        r.success = false;
                        r.stderr = format!(
                            "registry {} not setup correctly, missing index, private_key, and token",
                            options.cargo_main_registry.clone()
                        );
                    }
                }
                is_failed = !r.success;
            }
            r.end_time = Some(SystemTime::now());
            result.cargo.insert(registry_name.clone(), r);
        }
    }
    if !is_failed && package.publish_detail.docker.publish {
        result.docker.start_time = Some(SystemTime::now());
        if options.dry_run {
            result.docker.success = true;
        } else {
            let registry = &package
                .publish_detail
                .docker
                .repository
                .unwrap_or_else(|| "ghcr.io/foresightminingsoftwarecorporation".to_string());
            let context = &package
                .publish_detail
                .docker
                .context
                .unwrap_or_else(|| ".".to_string());
            let dockerfile = &package
                .publish_detail
                .docker
                .dockerfile
                .map(PathBuf::from)
                .unwrap_or_else(|| package_path.join("Dockerfile"))
                .to_str()
                .unwrap()
                .to_string();
            let image_name = format!("{}/{}:{}", registry, package_name, package_version);
            let image_latest = format!("{}/{}:latest", registry, package_name);
            let mut args = vec![
                "-t".to_string(),
                image_name.to_string(),
                // "--cache-to".to_string(),
                // format!("type=registry,ref={}/{}-cache", registry, package_name),
                // "--cache-from".to_string(),
                // format!("type=registry,ref={}/{}-cache", registry, package_name),
                "-f".to_string(),
                dockerfile.clone(),
            ];
            let mut envs = HashMap::new();
            let mut blacklist_envs =
                HashSet::from(["GIT_SSH_COMMAND".to_string(), "SSH_AUTH_SOCK".to_string()]);
            for (key, _) in std::env::vars() {
                if key.starts_with("CARGO_REGISTRIES_") {
                    blacklist_envs.insert(key);
                }
            }

            if let (Some(_), Some(npm_ghcr_token)) = (
                options.npm_ghcr_scope.clone(),
                options.npm_ghcr_token.clone(),
            ) {
                envs.insert("NPM_GHCR_TOKEN".to_string(), npm_ghcr_token);
                args.push("--secret id=node_auth_token,env=NPM_GHCR_TOKEN".to_string());
            }
            let main_registry_prefix = format!(
                "CARGO_REGISTRIES_{}",
                options.cargo_main_registry.replace("-", "_")
            )
            .to_uppercase();
            if let Ok(ssh_key) = env::var(format!("{}_PRIVATE_KEY", main_registry_prefix)) {
                args.push("--ssh".to_string());
                args.push(format!(
                    "{}={}",
                    options.cargo_main_registry.clone(),
                    ssh_key
                ));
            }

            if let (Ok(user_agent), Ok(token)) = (
                env::var(format!("{}_USER_AGENT", main_registry_prefix)),
                env::var(format!("{}_TOKEN", main_registry_prefix)),
            ) {
                let user_agent_env = format!("{}_USER_AGENT", main_registry_prefix);
                let token_env = format!("{}_TOKEN", main_registry_prefix);
                let name_env = format!("{}_NAME", main_registry_prefix);
                envs.insert(user_agent_env.clone(), user_agent);
                envs.insert(token_env.clone(), token);
                envs.insert(name_env.clone(), options.cargo_main_registry.clone());
                args.push(format!(
                    "--secret id=cargo_private_registry_user_agent,env={}",
                    user_agent_env
                ));
                args.push(format!(
                    "--secret id=cargo_private_registry_token,env={}",
                    token_env
                ));
                args.push(format!(
                    "--secret id=cargo_private_registry_name,env={}",
                    name_env
                ));
            }
            args.push(context.clone());
            // First we build
            let (stdout, stderr, success) = execute_command(
                &format!("docker build {}", args.join(" ")),
                &repo_root,
                &envs,
                &blacklist_envs,
                Some(tracing::Level::INFO),
                Some(tracing::Level::INFO),
            )
            .await;
            result.docker.success = success;
            result.docker.stdout = stdout;
            result.docker.stderr = stderr;
            is_failed = !success;
            if !is_failed {
                // Tag as latest
                let (stdout, stderr, success) = execute_command(
                    &format!("docker tag {} {}", image_name, image_latest),
                    &repo_root,
                    &HashMap::new(),
                    &HashSet::new(),
                    Some(tracing::Level::INFO),
                    Some(tracing::Level::INFO),
                )
                .await;
                result.docker.success = success;
                result.docker.stdout = format!("{}\n{}", result.docker.stdout, stdout);
                result.docker.stderr = format!("{}\n{}", result.docker.stderr, stderr);
                is_failed = !success;
                if !is_failed {
                    // Push image
                    let (stdout, stderr, success) = execute_command(
                        &format!("docker push {}", image_name),
                        &repo_root,
                        &HashMap::new(),
                        &HashSet::new(),
                        Some(tracing::Level::INFO),
                        Some(tracing::Level::INFO),
                    )
                    .await;
                    result.docker.success = success;
                    result.docker.stdout = format!("{}\n{}", result.docker.stdout, stdout);
                    result.docker.stderr = format!("{}\n{}", result.docker.stderr, stderr);
                    is_failed = !success;
                    if !is_failed {
                        // Push latest
                        let (stdout, stderr, success) = execute_command(
                            &format!("docker push {}", image_latest),
                            &repo_root,
                            &HashMap::new(),
                            &HashSet::new(),
                            Some(tracing::Level::INFO),
                            Some(tracing::Level::INFO),
                        )
                        .await;
                        result.docker.success = success;
                        result.docker.stdout = format!("{}\n{}", result.docker.stdout, stdout);
                        result.docker.stderr = format!("{}\n{}", result.docker.stderr, stderr);
                        is_failed = !success;
                    }
                }
            }
        }
        result.docker.end_time = Some(SystemTime::now());
    }
    if !is_failed && result.git_tag.should_publish {
        result.git_tag.start_time = Some(SystemTime::now());
        let tagged: anyhow::Result<()> = async {
            let tag = format!("{}-{}", package.package, package.version);
            if let (Some(github_app_id), Some(github_app_private_key)) =
                (options.github_app_id, options.github_app_private_key)
            {
                result.git_tag.stdout = format!("{}\nRetrieving git HEAD", result.git_tag.stdout);
                let Some(head) = Repository::open(&repo_root)
                    .ok()
                    .as_ref()
                    .and_then(|r| r.head().ok())
                    .as_ref()
                    .and_then(|head| head.peel_to_commit().ok())
                    .map(|head| head.id().to_string())
                else {
                    return Err(anyhow::Error::msg("Failed to get git HEAD"));
                };
                result.git_tag.stdout = format!("{}\nHEAD: {}", result.git_tag.stdout, head);

                result.git_tag.stdout =
                    format!("{}\nGenerating GitHub token", result.git_tag.stdout);
                let github_token = generate_github_app_token(
                    github_app_id,
                    github_app_private_key.clone(),
                    InstallationRetrievalMode::Organization,
                    Some(options.repo_owner.clone()),
                )
                .await?;
                let octocrab = Octocrab::builder().personal_token(github_token).build()?;
                let repo = octocrab.repos(&options.repo_owner, &options.repo_name);
                result.git_tag.stdout = format!(
                    "{}\nCreating tag {} at {}",
                    result.git_tag.stdout, tag, head
                );
                repo.create_ref(&Reference::Tag(tag), head).await?;
            } else {
                tracing::debug!("Github credentials not set, not doing anything");
            }
            Ok(())
        }
        .await;
        if let Err(err) = tagged {
            result.git_tag.stderr = format!("{}\n{}", result.git_tag.stderr, err);
            result.git_tag.success = false;
            is_failed = true;
        } else {
            result.git_tag.success = true;
        }
        result.git_tag.end_time = Some(SystemTime::now());
    }
    result.success = !is_failed;
    result.end_time = Some(SystemTime::now());
    result
}

/// login handles the custom logic of login to the 3rd party provider
/// - Docker, we may need to login to multiple docker registries
/// - Cargo, we may need to login to multiple registries
pub async fn login(options: Box<Options>, repo_root: &PathBuf) -> anyhow::Result<()> {
    // We might need to log to some docker registries
    if options.docker_hub_username.is_some() && options.docker_hub_password.is_some() {
        let (_stdout, stderr, success) = execute_command(
            "echo \"$DOCKER_HUB_PASSWORD\" | docker login registry-1.docker.io --username $DOCKER_HUB_USERNAME --password-stdin >/dev/null",
            repo_root,
            &HashMap::new(),
&HashSet::new(),
            Some(tracing::Level::INFO),
            Some(tracing::Level::INFO),
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
            repo_root,
            &HashMap::new(),
            &HashSet::new(),
            Some(tracing::Level::INFO),
            Some(tracing::Level::INFO),
        )
        .await;
        if !success {
            return Err(anyhow::anyhow!(stderr));
        }
    }
    Ok(())
}

pub async fn report_publish_to_github(
    options: Box<Options>,
    artifact_dir: &PathBuf,
) -> anyhow::Result<()> {
    if let (Some(github_app_id), Some(github_app_private_key)) =
        (options.github_app_id, options.github_app_private_key)
    {
        let github_token = generate_github_app_token(
            github_app_id,
            github_app_private_key.clone(),
            InstallationRetrievalMode::Organization,
            Some(options.repo_owner.clone()),
        )
        .await?;
        let octocrab = Octocrab::builder().personal_token(github_token).build()?;

        let repo = octocrab.repos(&options.repo_owner, &options.repo_name);
        let repo_releases = repo.releases();
        if let Ok(release) = repo_releases.get_by_tag(&options.pull_base_ref).await {
            let paths = fs::read_dir(artifact_dir)?;
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
            tracing::debug!("Could not find a github release to update, not doing anything");
        }
    } else {
        tracing::debug!("Github credentials not set, not doing anything");
    }
    Ok(())
}

pub async fn publish(options: Box<Options>, repo_root: PathBuf) -> anyhow::Result<PublishResults> {
    // Login to whatever need login to
    login(options.clone(), &repo_root)
        .await
        .with_context(|| "Could not login")?;

    // Check workspace information
    let check_workspace_options = CheckWorkspaceOptions::new()
        .with_check_publish(true)
        .with_progress(true)
        .with_cargo_main_registry(options.cargo_main_registry.clone())
        .with_ignore_dev_dependencies(true);
    // .with_ignore_dev_dependencies(false);

    let results = check_workspace(Box::new(check_workspace_options), repo_root.clone())
        .await
        .map_err(|e| {
            tracing::error!("Check directory for crates that need publishing: {}", e);
            e
        })
        .with_context(|| "Could not get directory information")?;

    let mut registries = HashSet::new();
    for member in results.members.values() {
        if let Some(r) = member.publish_detail.cargo.registries.clone() {
            registries.extend(r);
        }
    }
    let cargo = Arc::new(Cargo::new(&registries)?);
    let semaphore = Arc::new(Semaphore::new(options.job_limit));

    let mut handles = vec![];
    let mut status: HashMap<PackageId, Option<PublishResult>> = HashMap::new();
    for member_id in results.members.keys() {
        status.insert(member_id.clone(), None);
    }
    let publish_status = Arc::new(RwLock::new(status));

    let artifact_dir = options.artifacts.clone().join("output");
    fs::create_dir_all(&artifact_dir)?;

    let mut registries = HashSet::new();
    for package in results.members.values() {
        for registry_name in package.publish_detail.cargo.registries_publish.keys() {
            registries.insert(registry_name.clone());
        }
    }
    // Spawn a task for each object
    for member in results.members.values() {
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
                options.clone(),
                registries.clone(),
            ));
            handles.push(task_handle);
        }
    }
    futures::future::join_all(handles).await;

    // Report publish result to github
    report_publish_to_github(options.clone(), &artifact_dir)
        .await
        .with_context(|| "Issue reporting to GitHub")?;

    let mut published_members = HashMap::new();
    let lock = publish_status.read().expect("RwLock Poisoned");
    for (k, v) in lock.iter() {
        if let Some(v) = v {
            if v.should_publish {
                published_members.insert(k.clone(), v.clone());
            }
        }
    }
    let r = PublishResults {
        published_members,
        all_members: results.members.clone(),
    };
    // Store logs
    r.store_logs(&options.artifacts)?;
    // Craft Junit Results
    r.craft_junit(&options.artifacts)?;
    Ok(r)
}

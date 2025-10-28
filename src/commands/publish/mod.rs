use anyhow::Context;
use cargo_metadata::{DependencyKind, PackageId};
use clap::Parser;
use git2::Repository;
use junit_report::{Duration, ReportBuilder, TestCase, TestSuiteBuilder};
use octocrab::Octocrab;
use octocrab::params::repos::Reference;
use opendal::{Operator, services};
use regex::Regex;
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
use walkdir::WalkDir;

use crate::PackageRelatedOptions;
use crate::script::{CommandOutput, Script};
use crate::utils::get_registry_env;
use crate::utils::github::{InstallationRetrievalMode, generate_github_app_token};
use crate::{
    PrettyPrintable,
    commands::check_workspace::{
        Options as CheckWorkspaceOptions, Result as Package, check_workspace,
    },
    crate_graph::Dependency,
    utils::cargo::{Cargo, patch_crate_for_registry},
};

#[derive(Debug, Parser, Default, Clone)]
#[command(about = "Run rust tests")]
pub struct Options {
    #[clap(long, env, default_value = ".")]
    artifacts: PathBuf,
    #[clap(long, env)]
    base_rev_regex: Option<String>,
    #[arg(long, env)]
    repo_owner: String,
    #[arg(long, env)]
    repo_name: String,
    #[arg(long, env)]
    github_app_id: Option<u64>,
    #[arg(long, env)]
    github_app_private_key: Option<PathBuf>,
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
    #[arg(long, env)]
    s3_access_key_id: Option<String>,
    #[arg(long, env)]
    s3_secret_access_key: Option<String>,
    #[arg(long, env)]
    s3_endpoint: Option<String>,
    #[arg(long, env, default_value = "false")]
    dry_run: bool,
    #[arg(long, env, default_value = "false")]
    handle_tags: bool,
    #[arg(long, default_value_t = false)]
    autopublish_cargo: bool,
    /// Pattern for matching release tags (e.g., "v*" or "cargo-fslabscli-*")
    /// Used to filter which tags are considered for GitHub release lookup
    #[arg(long, env, default_value = "v*")]
    tag_pattern: String,
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

    fn update_from_command(&mut self, command_output: CommandOutput) {
        self.success = command_output.success;
        if self.stdout.is_empty() {
            self.stdout = command_output.stdout;
        } else {
            self.stdout = format!("{}\n{}", self.stdout, command_output.stdout)
        }
        if self.stderr.is_empty() {
            self.stderr = command_output.stderr;
        } else {
            self.stderr = format!("{}\n{}", self.stderr, command_output.stderr)
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
    pub s3: PublishDetailResult,
    pub start_time: Option<SystemTime>,
    pub end_time: Option<SystemTime>,
}

impl PublishResult {
    pub fn new(package: &Package, registries: HashSet<String>, options: &Options) -> Self {
        let mut s = Self {
            should_publish: package.publish,
            docker: PublishDetailResult {
                name: "docker buildx build && docker push".to_string(),
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
            s3: PublishDetailResult {
                name: "s3".to_string(),
                key: "s3".to_string(),
                should_publish: package.publish_detail.s3.publish,
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
                    name: format!("cargo publish -r {registry_name}"),
                    key: format!("cargo_{registry_name}"),
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
                    &publish_result.s3,
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
            "┌{:─^60}┬{:─^20}┬{:─^15}┬{:─^width$}┬{:─^15}┬{:─^15}┐",
            " Package ",
            " Version ",
            " Docker ",
            " Cargo ",
            " Nix Binary ",
            " S3 ",
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
            let mut cargo_reg = registries
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
                .join("│")
                .clone();

            if cargo_reg.is_empty() {
                cargo_reg = format!("{:^width$}", "-", width = cargo_size);
            }
            writeln!(
                f,
                "├{:─^60}┼{:─^20}┼{:─^15}┼{:─^width$}┼{:─^15}┼{:─^15}┤",
                "",
                "",
                "",
                empty_cargo_reg_headers,
                "",
                "",
                width = cargo_size
            )?;

            writeln!(
                f,
                "│{:^60}│{:^20}│{:^15}│{:^width$}│{:^15}│{:^15}│",
                name,
                version,
                publish_result.docker.get_status(),
                cargo_reg,
                publish_result.nix_binary.get_status(),
                publish_result.s3.get_status(),
                width = cargo_size,
            )?;
        }
        writeln!(
            f,
            "└{:─^60}┴{:─^20}┴{:─^15}┴{:─^width$}┴{:─^15}┴{:─^15}┘",
            "",
            "",
            "",
            empty_last_cargo_reg_headers,
            "",
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
    dependencies: Option<Vec<Dependency>>,
    statuses: Arc<RwLock<HashMap<PackageId, Option<PublishResult>>>>,
    output_dir: PathBuf,
    cargo: Arc<Cargo>,
    common_options: Arc<PackageRelatedOptions>,
    options: Arc<Options>,
    registries: HashSet<String>,
) {
    if let Some(ref package_id) = package.package_id {
        loop {
            let mut mark_failed = false;
            let mut process = true;
            {
                if let Some(ref deps) = dependencies {
                    for dep in deps {
                        let map = statuses.read().expect("RwLock poisoned");
                        if let Some(dep_result) = map.get(&dep.package_id) {
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
                    PublishResult::new(&package, registries, &options).with_failed(true);
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
            &common_options,
            &options,
            registries,
        )
        .await;
        debug!("Done package: {}", package.package);
        let mut map = statuses.write().expect("RwLock poisoned");
        *map.entry(package_id.clone()).or_insert(None) = Some(success);
        drop(permit);
    }
}

pub async fn create_s3_client(
    bucket_name: Option<String>,
    bucket_region: Option<String>,
    access_key_id: Option<String>,
    secret_access_key: Option<String>,
    endpoint: Option<String>,
) -> anyhow::Result<Operator> {
    if let (Some(bucket), Some(region), Some(access_key_id), Some(secret_access_key)) =
        (bucket_name, bucket_region, access_key_id, secret_access_key)
    {
        let mut builder = services::S3::default()
            .bucket(&bucket)
            .region(&region)
            .access_key_id(&access_key_id)
            .secret_access_key(&secret_access_key);

        if let Some(endpoint) = endpoint {
            builder = builder.endpoint(&endpoint);
        }

        let op = Operator::new(builder)?.finish();
        Ok(op)
    } else {
        anyhow::bail!("missing credentials for s3 storage backend")
    }
}

/// Actual Publish
async fn do_publish_package(
    repo_root: PathBuf,
    package: Package,
    output_dir: PathBuf,
    cargo: Arc<Cargo>,
    common_options: &PackageRelatedOptions,
    options: &Options,
    registries: HashSet<String>,
) -> PublishResult {
    let mut result = PublishResult::new(&package, registries, options);
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
    if !is_failed && package.publish_detail.s3.publish {
        result.s3.start_time = Some(SystemTime::now());
        if options.dry_run {
            result.s3.success = true;
        } else {
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
            }
            let main_registry_prefix = format!(
                "CARGO_REGISTRIES_{}",
                common_options.cargo_main_registry.replace("-", "_")
            )
            .to_uppercase();

            if let (Ok(user_agent), Ok(token)) = (
                env::var(format!("{main_registry_prefix}_USER_AGENT")),
                env::var(format!("{main_registry_prefix}_TOKEN")),
            ) {
                let user_agent_env = format!("{main_registry_prefix}_USER_AGENT");
                let token_env = format!("{main_registry_prefix}_TOKEN");
                let name_env = format!("{main_registry_prefix}_NAME");
                envs.insert(user_agent_env.clone(), user_agent);
                envs.insert(token_env.clone(), token);
                envs.insert(name_env.clone(), common_options.cargo_main_registry.clone());
            }
            // First we build
            let build_command = package.publish_detail.s3.build_command;
            let command_output = Script::new(&build_command)
                .current_dir(&package_path)
                .env_removals(&blacklist_envs)
                .envs(&envs)
                .log_stdout(tracing::Level::INFO)
                .log_stderr(tracing::Level::INFO)
                .execute()
                .await;
            result.s3.update_from_command(command_output);
            is_failed = !result.s3.success;
            if !is_failed {
                match create_s3_client(
                    package.publish_detail.s3.bucket_name.clone(),
                    package.publish_detail.s3.bucket_region.clone(),
                    options.s3_access_key_id.clone(),
                    options.s3_secret_access_key.clone(),
                    options.s3_endpoint.clone(),
                )
                .await
                {
                    Ok(store_client) => {
                        // Let's upload the output dir to s3
                        let prefix = package.publish_detail.s3.bucket_prefix;
                        let build_dir = package_path.join(
                            package
                                .publish_detail
                                .s3
                                .output_dir
                                .unwrap_or("".to_string()),
                        );
                        for entry in WalkDir::new(&build_dir) {
                            match entry {
                                Ok(entry) if entry.file_type().is_file() => {
                                    let path = entry.path();
                                    let relative = match path.strip_prefix(&build_dir) {
                                        Ok(r) => r,
                                        Err(e) => {
                                            result.s3.success = false;
                                            result.s3.stderr = format!(
                                                "{}\nPath strip error: {}",
                                                result.s3.stderr, e
                                            );
                                            is_failed = true;
                                            break;
                                        }
                                    };
                                    let key = match &prefix {
                                        Some(p) => format!("{}/{}", p, relative.display()),
                                        None => relative.display().to_string(),
                                    };
                                    match fs::read(path) {
                                        Ok(bytes) => match store_client.write(&key, bytes).await {
                                            Ok(_) => {
                                                result.s3.stdout = format!(
                                                    "{}\nUploaded: {}",
                                                    result.s3.stdout, key
                                                );
                                            }
                                            Err(e) => {
                                                result.s3.success = false;
                                                result.s3.stderr = format!(
                                                    "{}\nUpload failed {}: {}",
                                                    result.s3.stderr, key, e
                                                );
                                                is_failed = true;
                                                break;
                                            }
                                        },
                                        Err(e) => {
                                            result.s3.success = false;
                                            result.s3.stderr = format!(
                                                "{}\nRead failed {}: {}",
                                                result.s3.stderr,
                                                path.display(),
                                                e
                                            );
                                            is_failed = true;
                                            break;
                                        }
                                    }
                                }
                                Ok(_) => {} // directory, skip
                                Err(e) => {
                                    result.s3.success = false;
                                    result.s3.stderr =
                                        format!("{}\nWalk error: {}", result.s3.stderr, e);
                                    is_failed = true;
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        result.nix_binary.success = false;
                        result.nix_binary.stderr = format!("{}\n{}", result.s3.stderr, e);
                        is_failed = true;
                    }
                }
            }
        }
        result.s3.end_time = Some(SystemTime::now());
    }
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
                let command_output =
                    Script::new(format!("attic login central {atticd_url}/ {atticd_token}"))
                        .current_dir(&repo_root)
                        .log_stdout(tracing::Level::DEBUG)
                        .log_stderr(tracing::Level::DEBUG)
                        .execute()
                        .await;
                result.nix_binary.update_from_command(command_output);
                is_failed = !result.nix_binary.success;
                if !is_failed {
                    let command_output = Script::new(format!("attic use central:{atticd_cache}"))
                        .current_dir(&package_path)
                        .log_stdout(tracing::Level::DEBUG)
                        .log_stderr(tracing::Level::DEBUG)
                        .execute()
                        .await;
                    result.nix_binary.update_from_command(command_output);
                    is_failed = !result.nix_binary.success;
                }
            }
            if !is_failed {
                let mut command_output = Script::new("nix build .#release")
                    .current_dir(&package_path)
                    .log_stdout(tracing::Level::INFO)
                    .log_stderr(tracing::Level::INFO)
                    .execute()
                    .await;
                if command_output.success {
                    // Let's copy the artifacts to the
                    command_output.success =
                        copy_files(&package_path.join("result/bin"), &output_dir).is_ok();
                }
                result.nix_binary.update_from_command(command_output);
                is_failed = !result.nix_binary.success;
            }
            if !is_failed && let Ok(atticd_cache) = env::var("ATTICD_CACHE") {
                // Let's push the store to cachix by rebuilding and pushing
                info!("Pushing to atticd");
                let command_output = Script::new(format!(
                    "attic push {atticd_cache} $(nix-store -qR --include-outputs $(nix-store -qd ./result) | grep -v '\\.drv$')"
                ))
                    .current_dir(&package_path)
                    .log_stdout(tracing::Level::INFO)
                    .log_stderr(tracing::Level::INFO)
                    .execute().await;
                result.nix_binary.update_from_command(command_output);
                is_failed = !result.nix_binary.success;
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
                if !options.dry_run {
                    let registry_prefix =
                        format!("CARGO_REGISTRIES_{}", registry_name.replace("-", "_"))
                            .to_uppercase();

                    if std::env::var(format!("{registry_prefix}_INDEX")).is_ok() {
                        // For each reg we need to
                        // 1. Ensure registry is in `publish = []`
                        // 2. Find and replace `main_registry` to `current_registry` in Cargo.toml
                        // 3. Ensure there are Cargo.lock
                        // 4. Publish with --allow-dirty
                        if patch_crate_for_registry(
                            &repo_root,
                            &package_path,
                            registry_name.clone(),
                        )
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
                            let command_output = Script::new(format!(
                                "printenv | grep CARGO && cargo publish {}",
                                args.join(" ")
                            ))
                            .current_dir(&package_path)
                            .env_removals(&blacklist_envs)
                            .envs(&envs)
                            .log_stdout(tracing::Level::INFO)
                            .log_stderr(tracing::Level::INFO)
                            .execute()
                            .await;
                            r.update_from_command(command_output);
                        } else {
                            r.success = false;
                            r.stderr = format!(
                                "registry {registry_name} not setup correctly, missing index, private_key, and token"
                            );
                        }
                        // Path back to the main registry
                        if patch_crate_for_registry(
                            &repo_root,
                            &package_path,
                            common_options.cargo_main_registry.clone(),
                        )
                        .is_err()
                        {
                            r.success = false;
                            r.stderr = format!(
                                "registry {} not setup correctly, missing index, private_key, and token",
                                common_options.cargo_main_registry.clone()
                            );
                        }
                    }
                    is_failed = !r.success;
                } else {
                    r.success = true;
                }
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
            let image_name = format!("{registry}/{package_name}:{package_version}");
            let image_latest = format!("{registry}/{package_name}:latest");
            let cache_ref = format!("{registry}/{package_name}-buildcache");
            let mut args = vec![
                "-t".to_string(),
                image_name.to_string(),
                "-t".to_string(),
                image_latest.to_string(),
                "--cache-from".to_string(),
                format!("type=registry,ref={}", cache_ref),
                "--cache-to".to_string(),
                format!("type=registry,ref={},mode=max", cache_ref),
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
                common_options.cargo_main_registry.replace("-", "_")
            )
            .to_uppercase();
            if let Ok(ssh_key) = env::var(format!("{main_registry_prefix}_PRIVATE_KEY")) {
                args.push("--ssh".to_string());
                args.push(format!(
                    "{}={}",
                    common_options.cargo_main_registry.clone(),
                    ssh_key
                ));
            }

            if let (Ok(user_agent), Ok(token)) = (
                env::var(format!("{main_registry_prefix}_USER_AGENT")),
                env::var(format!("{main_registry_prefix}_TOKEN")),
            ) {
                let user_agent_env = format!("{main_registry_prefix}_USER_AGENT");
                let token_env = format!("{main_registry_prefix}_TOKEN");
                let name_env = format!("{main_registry_prefix}_NAME");
                envs.insert(user_agent_env.clone(), user_agent);
                envs.insert(token_env.clone(), token);
                envs.insert(name_env.clone(), common_options.cargo_main_registry.clone());
                args.push(format!(
                    "--secret id=cargo_private_registry_user_agent,env={user_agent_env}"
                ));
                args.push(format!(
                    "--secret id=cargo_private_registry_token,env={token_env}"
                ));
                args.push(format!(
                    "--secret id=cargo_private_registry_name,env={name_env}"
                ));
            }
            args.push(context.clone());
            // First we build
            let command_output = Script::new(format!(
                "docker buildx build --push --progress plain {}",
                args.join(" ")
            ))
            .current_dir(&repo_root)
            .env_removals(&blacklist_envs)
            .envs(&envs)
            .log_stdout(tracing::Level::INFO)
            .log_stderr(tracing::Level::INFO)
            .execute()
            .await;
            result.docker.update_from_command(command_output);
            is_failed = !result.docker.success;
        }
        result.docker.end_time = Some(SystemTime::now());
    }
    if !is_failed && result.git_tag.should_publish {
        result.git_tag.start_time = Some(SystemTime::now());
        let tagged: anyhow::Result<()> = async {
            let tag = format!("{}-{}", package.package, package.version);
            if let (Some(github_app_id), Some(github_app_private_key)) = (
                options.github_app_id,
                options.github_app_private_key.clone(),
            ) {
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
pub async fn login(options: &Options, repo_root: &PathBuf) -> anyhow::Result<()> {
    // We might need to log to some docker registries
    if options.docker_hub_username.is_some() && options.docker_hub_password.is_some() {
        let command_output = Script::new(
            "echo \"$DOCKER_HUB_PASSWORD\" | docker login registry-1.docker.io --username $DOCKER_HUB_USERNAME --password-stdin >/dev/null",
        )
            .current_dir(repo_root)
            .log_stdout(tracing::Level::INFO)
            .log_stderr(tracing::Level::INFO)
            .execute().await;
        if !command_output.success {
            return Err(anyhow::anyhow!(command_output.stderr));
        }
    }
    if options.ghcr_oci_url.is_some()
        && options.ghcr_oci_username.is_some()
        && options.ghcr_oci_password.is_some()
    {
        let command_output = Script::new(
            "echo \"${GHCR_OCI_PASSWORD}\" | docker login \"${GHCR_OCI_URL#oci://}\" --username \"${GHCR_OCI_USERNAME}\" --password-stdin >/dev/null",
        )
            .current_dir(repo_root)
            .log_stdout(tracing::Level::INFO)
            .log_stderr(tracing::Level::INFO)
        .execute().await;
        if !command_output.success {
            return Err(anyhow::anyhow!(command_output.stderr));
        }
    }
    Ok(())
}

/// Resolves a commit SHA (or git reference) to a git tag name.
/// This is used to find the GitHub release tag associated with a commit.
/// Uses git2's describe functionality for efficient exact-match tag lookup.
/// Filters tags by the provided pattern (e.g., "v*" or "cargo-fslabscli-*")
fn resolve_commit_to_tag(
    repo_root: &PathBuf,
    commit_ref: &str,
    tag_pattern: &str,
) -> anyhow::Result<String> {
    let repo = Repository::open(repo_root)
        .with_context(|| format!("Failed to open git repository at {:?}", repo_root))?;

    // Resolve the reference to a commit
    let obj = repo
        .revparse_single(commit_ref)
        .with_context(|| format!("Failed to resolve git reference: {}", commit_ref))?;

    // Use git2's describe with exact match option
    let mut describe_options = git2::DescribeOptions::new();
    describe_options
        .describe_tags()
        .show_commit_oid_as_fallback(false)
        .pattern(tag_pattern);

    let describe_result = obj.describe(&describe_options).with_context(|| {
        format!(
            "No tag matching pattern '{}' found for commit {}",
            tag_pattern, commit_ref
        )
    })?;

    // Format without any suffix (just the tag name)
    let mut format_options = git2::DescribeFormatOptions::new();
    format_options.abbreviated_size(0);

    let tag = describe_result
        .format(Some(&format_options))
        .with_context(|| "Failed to format describe result")?;

    Ok(tag)
}

pub async fn report_publish_to_github(
    common_options: &PackageRelatedOptions,
    options: &Options,
    artifact_dir: &PathBuf,
    repo_root: &PathBuf,
) -> anyhow::Result<()> {
    if let (Some(github_app_id), Some(github_app_private_key)) = (
        options.github_app_id,
        options.github_app_private_key.clone(),
    ) {
        let github_token = generate_github_app_token(
            github_app_id,
            github_app_private_key.clone(),
            InstallationRetrievalMode::Organization,
            Some(options.repo_owner.clone()),
        )
        .await?;
        let octocrab = Octocrab::builder().personal_token(github_token).build()?;

        // Resolve the base_rev (commit SHA) to a git tag using the configured pattern
        let base_rev = common_options.base_rev.as_deref().unwrap_or("HEAD~");
        let release_tag = resolve_commit_to_tag(repo_root, base_rev, &options.tag_pattern)?;

        tracing::info!(
            "Resolved commit {} to tag: {} (pattern: {})",
            base_rev,
            release_tag,
            options.tag_pattern
        );

        let repo = octocrab.repos(&options.repo_owner, &options.repo_name);
        let repo_releases = repo.releases();
        let release = repo_releases
            .get_by_tag(&release_tag)
            .await
            .with_context(|| "Could not find a release".to_string())?;
        let paths = fs::read_dir(artifact_dir)?;
        for artifact in paths.flatten() {
            let artifact_path = artifact.path();
            if let Some(artifact_name) = artifact_path.file_name()
                && let Some(artifact_name) = artifact_name.to_str()
            {
                tracing::debug!("Uploading github artifact {:?}", artifact_name);
                if let Ok(mut file) = File::open(&artifact_path)
                    && let Ok(metadata) = fs::metadata(&artifact_path)
                {
                    let mut data: Vec<u8> = vec![0; metadata.len() as usize];
                    if file.read(&mut data).is_ok() {
                        let _ = repo_releases
                            .upload_asset(release.id.into_inner(), artifact_name, data.into())
                            .send()
                            .await;
                    }
                }
            }
        }
    } else {
        tracing::debug!("Github credentials not set, not doing anything");
    }
    Ok(())
}

pub async fn publish(
    common_options: &mut PackageRelatedOptions,
    options: &Options,
    repo_root: PathBuf,
) -> anyhow::Result<PublishResults> {
    // Login to whatever need login to
    login(options, &repo_root)
        .await
        .with_context(|| "Could not login")?;

    // For publishing we have a special case for the whitelist.
    // If the push regex is set, then we need to consider only the package that
    // match the first capturing group
    tracing::info!(
        "Got whitelist, regex, baseref: {:?} -- {:?} -- {:?}",
        common_options.whitelist,
        options.base_rev_regex,
        common_options.base_rev
    );
    let mut whitelist = common_options.whitelist.clone();

    let base_rev = common_options.base_rev.as_deref().unwrap_or("HEAD~");
    if let Some(regex) = &options.base_rev_regex {
        let re = Regex::new(regex)?;
        if let Some(captures) = re.captures(base_rev)
            && let Some(package_name_match) = captures.get(1)
        {
            whitelist.push(package_name_match.as_str().to_string());
        }
    }
    common_options.whitelist = whitelist;

    // Check workspace information
    let check_workspace_options = CheckWorkspaceOptions::new()
        .with_check_publish(true)
        .with_autopublish_cargo(options.autopublish_cargo)
        .with_ignore_dev_dependencies(true);

    let results = check_workspace(common_options, &check_workspace_options, repo_root.clone())
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
    let semaphore = Arc::new(Semaphore::new(common_options.job_limit));

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
    // Filters members based on regex
    // Spawn a task for each object
    for (member_id, member) in &results.members {
        let dependencies = results
            .crate_graph
            .dependency_graph()
            .dependencies
            .get(member_id)
            .map(|deps| {
                // We should remove the dev and path only dependencies from the tree
                deps.iter()
                    .filter(|d| {
                        d.instances
                            .iter()
                            .any(|k| k.kind != DependencyKind::Development || !k.is_local)
                    })
                    .cloned()
                    .collect::<Vec<Dependency>>()
            });
        let task_handle = tokio::spawn(publish_package(
            repo_root.clone(),
            member.clone(),
            semaphore.clone(),
            dependencies,
            publish_status.clone(),
            artifact_dir.clone(),
            cargo.clone(),
            Arc::new(common_options.clone()),
            Arc::new(options.clone()),
            registries.clone(),
        ));
        handles.push(task_handle);
    }
    futures::future::join_all(handles).await;

    // Report publish result to github
    if let Err(e) =
        report_publish_to_github(common_options, options, &artifact_dir, &repo_root).await
    {
        tracing::error!("Issue reporting to Github {:?}", e);
    }

    let mut global_success = true;
    let mut published_members = HashMap::new();
    let lock = publish_status.read().expect("RwLock Poisoned");
    for (k, v) in lock.iter() {
        if let Some(v) = v
            && v.should_publish
        {
            published_members.insert(k.clone(), v.clone());
            global_success &= v.success;
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
    match global_success {
        true => Ok(r),
        false => Err(anyhow::anyhow!("publishing failed")),
    }
}

#[cfg(test)]
mod tests {
    use git2::{Repository, Signature};
    use std::path::PathBuf;

    use super::resolve_commit_to_tag;
    use crate::utils::test::{commit_all_changes, modify_file};

    /// Helper function to create a test git repository with initial commit
    fn create_test_repo() -> (assert_fs::TempDir, PathBuf) {
        let temp_dir = assert_fs::TempDir::new().expect("Failed to create temp directory");
        let repo_path = temp_dir.path().to_path_buf();

        let repo = Repository::init(&repo_path).expect("Failed to init repository");

        // Configure Git user info (required for commits)
        repo.config()
            .unwrap()
            .set_str("user.name", "Test User")
            .unwrap();
        repo.config()
            .unwrap()
            .set_str("user.email", "test@example.com")
            .unwrap();

        // Create initial file and commit
        modify_file(&repo_path, "README.md", "# Test Repository");
        commit_all_changes(&repo_path, "Initial commit");

        (temp_dir, repo_path)
    }

    /// Helper function to create a commit in the repository
    fn create_commit(repo_path: &PathBuf, message: &str) -> git2::Oid {
        // Modify a file to have something to commit
        modify_file(repo_path, "test.txt", &format!("Content for {}", message));
        commit_all_changes(repo_path, message);

        // Get the commit OID
        let repo = Repository::open(repo_path).expect("Failed to open repository");
        repo.head()
            .expect("Failed to get HEAD")
            .target()
            .expect("HEAD has no target")
    }

    /// Helper function to create a lightweight tag pointing to a commit
    fn create_tag(repo_path: &PathBuf, tag_name: &str, commit_oid: git2::Oid) {
        let repo = Repository::open(repo_path).expect("Failed to open repository");
        let commit = repo.find_commit(commit_oid).expect("Failed to find commit");
        let obj = commit.as_object();

        repo.tag_lightweight(tag_name, obj, false)
            .expect("Failed to create tag");
    }

    #[test]
    fn test_resolve_commit_with_cargo_fslabscli_tag() {
        // Test: A commit with cargo-fslabscli-* tag (project's actual format)
        let (_temp_dir, repo_path) = create_test_repo();
        let commit_oid = create_commit(&repo_path, "Add feature");
        create_tag(&repo_path, "cargo-fslabscli-2.29.1", commit_oid);

        let result = resolve_commit_to_tag(&repo_path, "HEAD", "cargo-fslabscli-*");
        assert!(result.is_ok(), "Should successfully resolve to tag");
        assert_eq!(result.unwrap(), "cargo-fslabscli-2.29.1");
    }

    #[test]
    fn test_resolve_commit_with_version_tag() {
        // Test: A commit with a version tag (v-prefixed) should return that tag
        let (_temp_dir, repo_path) = create_test_repo();
        let commit_oid = create_commit(&repo_path, "Add feature");
        create_tag(&repo_path, "v2.29.1", commit_oid);

        let result = resolve_commit_to_tag(&repo_path, "HEAD", "v*");
        assert!(result.is_ok(), "Should successfully resolve to tag");
        assert_eq!(result.unwrap(), "v2.29.1");
    }

    #[test]
    fn test_resolve_commit_with_multiple_tags_filters_by_pattern() {
        // Test: When multiple tags exist, pattern matching filters correctly
        let (_temp_dir, repo_path) = create_test_repo();
        let commit_oid = create_commit(&repo_path, "Release commit");

        // Create tags with different patterns
        create_tag(&repo_path, "latest", commit_oid);
        create_tag(&repo_path, "release-1.0.0", commit_oid);
        create_tag(&repo_path, "v1.0.0", commit_oid);
        create_tag(&repo_path, "stable", commit_oid);

        // Should find only v-prefixed tag when pattern is "v*"
        let result = resolve_commit_to_tag(&repo_path, "HEAD", "v*");
        assert!(result.is_ok(), "Should successfully resolve to tag");
        assert_eq!(result.unwrap(), "v1.0.0", "Should return v-prefixed tag");
    }

    #[test]
    fn test_resolve_commit_filters_by_exact_pattern() {
        // Test: Pattern matching is exact - only matches the specified pattern
        let (_temp_dir, repo_path) = create_test_repo();
        let commit_oid = create_commit(&repo_path, "Release commit");

        // Create both tag formats
        create_tag(&repo_path, "v2.0.0", commit_oid);
        create_tag(&repo_path, "cargo-fslabscli-2.29.1", commit_oid);

        // When searching for "v*", should only return v-prefixed tag
        let result = resolve_commit_to_tag(&repo_path, "HEAD", "v*");
        assert!(result.is_ok(), "Should successfully resolve to tag");
        assert_eq!(
            result.unwrap(),
            "v2.0.0",
            "Should return only v-prefixed tag"
        );

        // When searching for "cargo-fslabscli-*", should only return that tag
        let result = resolve_commit_to_tag(&repo_path, "HEAD", "cargo-fslabscli-*");
        assert!(result.is_ok(), "Should successfully resolve to tag");
        assert_eq!(result.unwrap(), "cargo-fslabscli-2.29.1");
    }

    #[test]
    fn test_resolve_commit_with_pattern_mismatch_returns_error() {
        // Test: When no tags match the pattern, should return error
        let (_temp_dir, repo_path) = create_test_repo();
        let commit_oid = create_commit(&repo_path, "Tagged commit");

        create_tag(&repo_path, "latest", commit_oid);
        create_tag(&repo_path, "stable", commit_oid);

        // Try to find v* tags when only "latest" and "stable" exist
        let result = resolve_commit_to_tag(&repo_path, "HEAD", "v*");
        assert!(
            result.is_err(),
            "Should return error when no tags match pattern"
        );

        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("No tag matching pattern"),
            "Error message should mention pattern mismatch, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_resolve_commit_with_no_tags_returns_error() {
        // Test: A commit with no tags should return an error
        let (_temp_dir, repo_path) = create_test_repo();
        create_commit(&repo_path, "Untagged commit");

        let result = resolve_commit_to_tag(&repo_path, "HEAD", "v*");
        assert!(result.is_err(), "Should return error for untagged commit");

        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("No tag matching pattern"),
            "Error message should mention no tags matching pattern, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_resolve_invalid_commit_reference_returns_error() {
        // Test: An invalid reference should return an error
        let (_temp_dir, repo_path) = create_test_repo();

        let result = resolve_commit_to_tag(&repo_path, "nonexistent-ref", "v*");
        assert!(result.is_err(), "Should return error for invalid reference");

        let err_msg = result.unwrap_err().to_string();
        // Should contain error about resolving the reference
        assert!(
            err_msg.contains("Failed to resolve git reference"),
            "Error message should indicate failure, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_resolve_commit_sha_directly() {
        // Test: Should be able to resolve using a commit SHA directly
        let (_temp_dir, repo_path) = create_test_repo();
        let commit_oid = create_commit(&repo_path, "Feature commit");
        create_tag(&repo_path, "v3.0.0", commit_oid);

        let commit_sha = commit_oid.to_string();
        let result = resolve_commit_to_tag(&repo_path, &commit_sha, "v*");
        assert!(result.is_ok(), "Should resolve commit SHA to tag");
        assert_eq!(result.unwrap(), "v3.0.0");
    }

    #[test]
    fn test_resolve_commit_sha_short_form() {
        // Test: Should be able to resolve using a short commit SHA
        let (_temp_dir, repo_path) = create_test_repo();
        let commit_oid = create_commit(&repo_path, "Short SHA test");
        create_tag(&repo_path, "v4.0.0", commit_oid);

        let commit_sha = commit_oid.to_string();
        let short_sha = &commit_sha[..7]; // Use first 7 characters
        let result = resolve_commit_to_tag(&repo_path, short_sha, "v*");
        assert!(result.is_ok(), "Should resolve short commit SHA to tag");
        assert_eq!(result.unwrap(), "v4.0.0");
    }

    #[test]
    fn test_resolve_head_reference() {
        // Test: Should resolve HEAD reference to tag
        let (_temp_dir, repo_path) = create_test_repo();
        let commit_oid = create_commit(&repo_path, "HEAD commit");
        create_tag(&repo_path, "v5.0.0", commit_oid);

        let result = resolve_commit_to_tag(&repo_path, "HEAD", "v*");
        assert!(result.is_ok(), "Should resolve HEAD to tag");
        assert_eq!(result.unwrap(), "v5.0.0");
    }

    #[test]
    fn test_resolve_branch_reference() {
        // Test: Should resolve a branch reference to tag
        let (_temp_dir, repo_path) = create_test_repo();
        let repo = Repository::open(&repo_path).expect("Failed to open repository");

        // Create a commit and tag it
        let commit_oid = create_commit(&repo_path, "Branch commit");
        create_tag(&repo_path, "v6.0.0", commit_oid);

        // Create a branch pointing to this commit
        let commit = repo.find_commit(commit_oid).expect("Failed to find commit");
        repo.branch("feature-branch", &commit, false)
            .expect("Failed to create branch");

        let result = resolve_commit_to_tag(&repo_path, "feature-branch", "v*");
        assert!(result.is_ok(), "Should resolve branch reference to tag");
        assert_eq!(result.unwrap(), "v6.0.0");
    }

    #[test]
    fn test_resolve_older_commit_with_tag() {
        // Test: Should resolve to tag on an older commit (not HEAD)
        let (_temp_dir, repo_path) = create_test_repo();

        // Create first commit with tag
        let first_commit_oid = create_commit(&repo_path, "First release");
        create_tag(&repo_path, "v1.0.0", first_commit_oid);

        // Create second commit (HEAD) without tag
        create_commit(&repo_path, "Second commit");

        // Should still be able to resolve the first commit by its SHA
        let commit_sha = first_commit_oid.to_string();
        let result = resolve_commit_to_tag(&repo_path, &commit_sha, "v*");
        assert!(result.is_ok(), "Should resolve older commit to tag");
        assert_eq!(result.unwrap(), "v1.0.0");
    }

    #[test]
    fn test_multiple_version_tags_returns_matching_tag() {
        // Test: When multiple v-prefixed tags exist, returns one that matches the pattern
        // Note: git describe doesn't guarantee which tag when multiple match the same commit
        let (_temp_dir, repo_path) = create_test_repo();
        let commit_oid = create_commit(&repo_path, "Multi-version commit");

        // Create multiple tags
        create_tag(&repo_path, "v1.0.0", commit_oid);
        create_tag(&repo_path, "v2.0.0", commit_oid);
        create_tag(&repo_path, "v1.0.1", commit_oid);

        let result = resolve_commit_to_tag(&repo_path, "HEAD", "v*");
        assert!(result.is_ok(), "Should successfully resolve to tag");

        // Should return one of the v-prefixed tags
        let tag = result.unwrap();
        assert!(
            tag.starts_with("v"),
            "Should return a v-prefixed tag, got: {}",
            tag
        );

        // Verify it's deterministic by calling again (should return same tag)
        let result2 = resolve_commit_to_tag(&repo_path, "HEAD", "v*");
        assert_eq!(result2.unwrap(), tag, "Should return same tag consistently");
    }

    #[test]
    fn test_resolve_head_tilde_reference() {
        // Test: Should resolve HEAD~ reference to tag on parent commit
        let (_temp_dir, repo_path) = create_test_repo();

        // First commit with tag
        let first_commit_oid = create_commit(&repo_path, "First release");
        create_tag(&repo_path, "v1.0.0", first_commit_oid);

        // Second commit (becomes HEAD)
        create_commit(&repo_path, "Second commit");

        // Should resolve HEAD~ to the first commit's tag
        let result = resolve_commit_to_tag(&repo_path, "HEAD~", "v*");
        assert!(result.is_ok(), "Should resolve HEAD~ to tag");
        assert_eq!(result.unwrap(), "v1.0.0");
    }

    #[test]
    fn test_annotated_tag_resolution() {
        // Test: Should resolve annotated tags (not just lightweight tags)
        let (_temp_dir, repo_path) = create_test_repo();
        let repo = Repository::open(&repo_path).expect("Failed to open repository");

        let commit_oid = create_commit(&repo_path, "Annotated tag commit");
        let commit = repo.find_commit(commit_oid).expect("Failed to find commit");
        let obj = commit.as_object();

        // Create an annotated tag
        let sig =
            Signature::now("Test User", "test@example.com").expect("Failed to create signature");
        repo.tag("v7.0.0", obj, &sig, "Release v7.0.0", false)
            .expect("Failed to create annotated tag");

        let result = resolve_commit_to_tag(&repo_path, "HEAD", "v*");
        assert!(result.is_ok(), "Should resolve annotated tag");
        assert_eq!(result.unwrap(), "v7.0.0");
    }

    #[test]
    fn test_invalid_repo_path_returns_error() {
        // Test: Invalid repository path should return error
        let invalid_path = PathBuf::from("/nonexistent/path/to/repo");

        let result = resolve_commit_to_tag(&invalid_path, "HEAD", "v*");
        assert!(result.is_err(), "Should return error for invalid repo path");

        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Failed to open git repository"),
            "Error message should mention repository opening failure, got: {}",
            err_msg
        );
    }
}

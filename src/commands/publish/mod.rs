use anyhow::Context;
use aws_sdk_cloudfront as cloudfront;
use cargo_metadata::{DependencyKind, PackageId};
use clap::Parser;
use http_body_util::{BodyExt, Empty};
use hyper::body::Bytes;
use hyper::{Method, Request, StatusCode, Uri};
use junit_report::{Duration, ReportBuilder, TestCase, TestSuiteBuilder};
use mime_guess;
use octocrab::Octocrab;
use octocrab::params::repos::Reference;
use opendal::{Operator, services};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::Write;
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
use crate::commands::release_utils::{format_tag, upload_artifacts_to_release};
use crate::script::{CommandOutput, Script};
use crate::utils::get_registry_env;
use crate::utils::github::{InstallationRetrievalMode, generate_github_app_token};
use crate::{
    PrettyPrintable,
    commands::check_workspace::{
        Options as CheckWorkspaceOptions, Result as Package, check_workspace,
    },
    crate_graph::Dependency,
    utils::cargo::{Cargo, CargoRegistry, patch_crate_for_registry},
};

#[derive(Debug, Parser, Default, Clone)]
#[command(about = "Run rust tests")]
pub struct Options {
    #[clap(long, env = "PULL_BASE_REF", alias = "pull-base-ref")]
    base_rev: Option<String>,
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
    #[arg(long, env)]
    cloudfront_distribution_id: Option<String>,
    #[arg(long, env, default_value = "false")]
    dry_run: bool,
    #[arg(long, env, default_value = "false")]
    handle_tags: bool,
    #[arg(long, default_value_t = false)]
    autopublish_cargo: bool,
    /// Add every crate with package.metadata.fslabs.publish.cargo.publish=true
    /// and its Cargo dependency closure to the tag-selected release plan.
    #[arg(long, env, default_value_t = false)]
    publish_all_marked_cargo: bool,
    /// Restrict --publish-all-marked-cargo to releases whose captured root or
    /// explicit whitelist contains this package.
    #[arg(long, env, requires = "publish_all_marked_cargo")]
    publish_all_marked_cargo_for: Option<String>,
    /// After Cargo publication, ensure every planned crate belongs to this
    /// Kellnr group. Requires --cargo-target-registry.
    #[arg(long, env)]
    ensure_cargo_group: Option<String>,
    /// Pattern for matching release tags (e.g., "v*" or "cargo-fslabscli-*")
    /// Used to filter which tags are considered for GitHub release lookup
    #[arg(long, env, default_value = "v*")]
    tag_pattern: String,
    /// Skip crate-exists check, allowing republish of already-published versions.
    /// Only use with registries that support overwrites (e.g., kellnr).
    #[arg(long, env, default_value = "false")]
    force_publish: bool,
    /// Template string for git tag format.
    /// Supports `{package_name}` and `{version}` as placeholders.
    /// Example: `--tag-format "v{version}"` produces `v2.43.0`.
    #[arg(long, env, default_value = "{package_name}-{version}")]
    tag_format: String,
    /// When set, target a draft GitHub release instead of a published one.
    #[arg(long, env, default_value = "false")]
    draft: bool,
    /// Skip git tag resolution and use this tag directly.
    /// Intended for Prow/CI invocations on push to main where no git tag exists yet.
    #[arg(long, env)]
    release_tag: Option<String>,
    /// Override which publish steps to run, ignoring Cargo.toml metadata.
    /// Can be repeated: --publish-steps nix --publish-steps cargo
    /// When omitted, all steps enabled in Cargo.toml metadata run (current behavior).
    #[arg(long, env, value_delimiter = ',')]
    pub publish_steps: Option<Vec<PublishStep>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum PublishStep {
    S3,
    Cargo,
    Nix,
    Docker,
    Tags,
    Github,
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
                    false => {
                        TestCase::failure(&self.name, duration, "publish_failure", &self.stderr)
                    }
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
    pub s3: HashMap<String, PublishDetailResult>,
    pub start_time: Option<SystemTime>,
    pub end_time: Option<SystemTime>,
}

impl PublishResult {
    pub fn new(package: &Package, registries: HashSet<String>, options: &Options) -> Self {
        let crate_name = &package.package;
        let crate_version = &package.version;
        let cargo_only = package.cargo_only;
        let mut s = Self {
            should_publish: package.publish,
            docker: PublishDetailResult {
                name: format!("{crate_name}@{crate_version} docker buildx build && docker push"),
                key: "docker".to_string(),
                should_publish: !cargo_only && package.publish_detail.docker.publish,
                ..Default::default()
            },
            nix_binary: PublishDetailResult {
                name: format!("{crate_name}@{crate_version} nix build .#release --fallback"),
                key: "nix".to_string(),
                should_publish: !cargo_only && package.publish_detail.nix_binary.publish,
                ..Default::default()
            },
            git_tag: PublishDetailResult {
                name: format!("{crate_name}@{crate_version} git tag"),
                key: "git".to_string(),
                should_publish: !cargo_only && options.handle_tags,
                ..Default::default()
            },
            ..Default::default()
        };

        for (dest_name, _dest) in package.publish_detail.s3.resolved_destinations() {
            s.s3.insert(
                dest_name.clone(),
                PublishDetailResult {
                    name: format!("{crate_name}@{crate_version} s3:{dest_name}"),
                    key: format!("s3_{dest_name}"),
                    should_publish: !cargo_only && package.publish_detail.s3.publish,
                    ..Default::default()
                },
            );
        }

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
                    name: format!("{crate_name}@{crate_version} cargo publish -r {registry_name}"),
                    key: format!("cargo_{registry_name}"),
                    ..Default::default()
                },
            );
        }

        s
    }

    pub fn with_failed(mut self, failed: bool) -> Self {
        self.success = !failed;
        if failed {
            for detail in [&mut self.docker, &mut self.nix_binary, &mut self.git_tag] {
                if detail.should_publish && detail.stderr.is_empty() {
                    detail.success = false;
                    detail.stderr = "skipped: a dependency failed to publish".to_string();
                }
            }
            for detail in self.cargo.values_mut() {
                if detail.should_publish && detail.stderr.is_empty() {
                    detail.success = false;
                    detail.stderr = "skipped: a dependency failed to publish".to_string();
                }
            }
            for detail in self.s3.values_mut() {
                if detail.should_publish && detail.stderr.is_empty() {
                    detail.success = false;
                    detail.stderr = "skipped: a dependency failed to publish".to_string();
                }
            }
        }
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
                for s3_result in publish_result.s3.values() {
                    results.push(s3_result);
                }
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
                let mut results: Vec<&PublishDetailResult> =
                    vec![&publish_result.nix_binary, &publish_result.docker];
                for s3_result in publish_result.s3.values() {
                    results.push(s3_result);
                }
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
                {
                    let s3_statuses: Vec<_> = publish_result.s3.values().collect();
                    if s3_statuses.len() == 1 {
                        s3_statuses[0].get_status()
                    } else if s3_statuses.is_empty() {
                        "-".to_string()
                    } else if s3_statuses
                        .iter()
                        .filter(|s| s.should_publish)
                        .all(|s| s.success)
                    {
                        format!("✓ ({})", s3_statuses.len())
                    } else {
                        let failed = s3_statuses
                            .iter()
                            .filter(|s| s.should_publish && !s.success)
                            .count();
                        format!("✗ ({}/{})", failed, s3_statuses.len())
                    }
                },
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
    member_paths: Arc<Vec<PathBuf>>,
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
        let success = do_publish_package(DoPublishParams {
            repo_root: repo_root.clone(),
            package: package.clone(),
            output_dir,
            cargo,
            common_options: common_options.as_ref().clone(),
            options: options.as_ref().clone(),
            registries,
            member_paths,
        })
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

/// Creates a CloudFront invalidation for the specified paths
pub async fn create_cloudfront_invalidation(
    distribution_id: &str,
    paths: Vec<String>,
    region: Option<String>,
) -> anyhow::Result<String> {
    if paths.is_empty() {
        return Ok("No paths to invalidate".to_string());
    }

    let mut config_loader = aws_config::from_env();
    if let Some(region) = region {
        config_loader = config_loader.region(aws_config::Region::new(region));
    }
    let config = config_loader.load().await;

    let client = cloudfront::Client::new(&config);

    // CloudFront paths must start with /
    let invalidation_paths: Vec<String> = paths
        .into_iter()
        .map(|p| {
            if p.starts_with('/') {
                p
            } else {
                format!("/{}", p)
            }
        })
        .collect();

    // Create unique caller reference using timestamp
    let caller_reference = format!(
        "fslabscli-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    );

    let invalidation_batch = cloudfront::types::InvalidationBatch::builder()
        .paths(
            cloudfront::types::Paths::builder()
                .quantity(invalidation_paths.len() as i32)
                .set_items(Some(invalidation_paths.clone()))
                .build()
                .context("Failed to build invalidation paths")?,
        )
        .caller_reference(&caller_reference)
        .build()
        .context("Failed to build invalidation batch")?;

    let response = client
        .create_invalidation()
        .distribution_id(distribution_id)
        .invalidation_batch(invalidation_batch)
        .send()
        .await
        .context("Failed to create CloudFront invalidation")?;

    let invalidation_id = response
        .invalidation()
        .map(|i| i.id().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    Ok(format!(
        "Created CloudFront invalidation {} for {} paths",
        invalidation_id,
        invalidation_paths.len()
    ))
}

/// Uploads the contents of `build_dir` to a single S3 destination, then optionally
/// sync-deletes stale objects and invalidates CloudFront.
async fn publish_to_s3_destination(
    dest_name: &str,
    dest: &crate::commands::check_workspace::S3Destination,
    build_dir: &Path,
    options: &Options,
    result: &mut PublishDetailResult,
) {
    let prefix = dest.credentials_env_prefix.as_deref().unwrap_or("S3");
    let access_key_id = std::env::var(format!("{}_ACCESS_KEY_ID", prefix))
        .ok()
        .or_else(|| options.s3_access_key_id.clone());
    let secret_access_key = std::env::var(format!("{}_SECRET_ACCESS_KEY", prefix))
        .ok()
        .or_else(|| options.s3_secret_access_key.clone());
    let endpoint = std::env::var(format!("{}_ENDPOINT", prefix))
        .ok()
        .or_else(|| options.s3_endpoint.clone());

    let mut uploaded_paths: Vec<String> = Vec::new();

    match create_s3_client(
        dest.bucket_name.clone(),
        dest.bucket_region.clone(),
        access_key_id,
        secret_access_key,
        endpoint,
    )
    .await
    {
        Ok(store_client) => {
            result.success = true;
            let prefix_str = dest.bucket_prefix.as_ref();

            for entry in WalkDir::new(build_dir) {
                match entry {
                    Ok(entry) if entry.file_type().is_file() => {
                        let path = entry.path();
                        let relative = match path.strip_prefix(build_dir) {
                            Ok(r) => r,
                            Err(e) => {
                                result.success = false;
                                result.stderr =
                                    format!("{}\nPath strip error: {}", result.stderr, e);
                                return;
                            }
                        };
                        let key = match prefix_str {
                            Some(p) => format!("{}/{}", p, relative.display()),
                            None => relative.display().to_string(),
                        };
                        match fs::read(path) {
                            Ok(bytes) => {
                                let content_type = mime_guess::from_path(path)
                                    .first_or_octet_stream()
                                    .to_string();
                                // These cache settings assume that assets are deployed with unique
                                // names, which is true for most build systems (including Trunk).
                                // The exceptions are the entry files, which point to the other
                                // files, and must not be cached so aggressively. Usually, those
                                // files are html files or service workers.
                                let cache_control =
                                    if path.extension().is_some_and(|ext| ext == "html")
                                        || path
                                            .file_name()
                                            .is_some_and(|file_name| file_name == "sw.js")
                                    {
                                        "public, max-age=0, must-revalidate"
                                    } else {
                                        "public, max-age=31536000, immutable"
                                    };
                                match store_client
                                    .write_with(&key, bytes)
                                    .content_type(&content_type)
                                    .cache_control(cache_control)
                                    .await
                                {
                                    Ok(_) => {
                                        result.stdout = format!(
                                            "{}\nUploaded: {} (Content-Type: {}, Cache-Control: {})",
                                            result.stdout, key, content_type, cache_control
                                        );
                                        uploaded_paths.push(key.clone());
                                    }
                                    Err(e) => {
                                        result.success = false;
                                        result.stderr = format!(
                                            "{}\nUpload failed {}: {}",
                                            result.stderr, key, e
                                        );
                                        return;
                                    }
                                }
                            }
                            Err(e) => {
                                result.success = false;
                                result.stderr = format!(
                                    "{}\nRead failed {}: {}",
                                    result.stderr,
                                    path.display(),
                                    e
                                );
                                return;
                            }
                        }
                    }
                    Ok(_) => {} // directory, skip
                    Err(e) => {
                        result.success = false;
                        result.stderr = format!("{}\nWalk error: {}", result.stderr, e);
                        return;
                    }
                }
            }

            // Sync delete: remove stale objects not present in this upload
            if dest.sync_delete.unwrap_or(false) {
                match prefix_str {
                    None => {
                        result.stderr = format!(
                            "{}\nsync_delete requires bucket_prefix to be set; skipping deletion to avoid wiping entire bucket",
                            result.stderr
                        );
                        result.success = false;
                        return;
                    }
                    Some(p) => {
                        let list_prefix = format!("{}/", p);
                        info!(
                            "Sync-deleting stale S3 objects under prefix: {} (dest: {})",
                            list_prefix, dest_name
                        );
                        let uploaded_set: HashSet<&str> =
                            uploaded_paths.iter().map(|s| s.as_str()).collect();
                        match store_client.list(&list_prefix).await {
                            Ok(entries) => {
                                for entry in entries {
                                    let entry_path = entry.path().to_string();
                                    if entry_path.ends_with('/') {
                                        continue;
                                    }
                                    if !uploaded_set.contains(entry_path.as_str()) {
                                        match store_client.delete(&entry_path).await {
                                            Ok(_) => {
                                                result.stdout = format!(
                                                    "{}\nDeleted stale: {}",
                                                    result.stdout, entry_path
                                                );
                                            }
                                            Err(e) => {
                                                result.success = false;
                                                result.stderr = format!(
                                                    "{}\nDelete failed {}: {}",
                                                    result.stderr, entry_path, e
                                                );
                                                return;
                                            }
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                result.success = false;
                                result.stderr = format!(
                                    "{}\nList failed for sync_delete: {}",
                                    result.stderr, e
                                );
                                return;
                            }
                        }
                    }
                }
            }

            // CloudFront invalidation — CLI flag takes precedence over per-destination config
            let distribution_id = options
                .cloudfront_distribution_id
                .as_ref()
                .or(dest.cloudfront_distribution_id.as_ref());

            if let Some(distribution_id) = distribution_id {
                info!(
                    "Creating CloudFront invalidation for distribution: {} (dest: {})",
                    distribution_id, dest_name
                );
                match create_cloudfront_invalidation(
                    distribution_id,
                    uploaded_paths,
                    dest.bucket_region.clone(),
                )
                .await
                {
                    Ok(msg) => {
                        result.stdout = format!("{}\n{}", result.stdout, msg);
                        info!("{}", msg);
                    }
                    Err(e) => {
                        let err_msg = format!("CloudFront invalidation warning: {}", e);
                        result.stderr = format!("{}\n{}", result.stderr, err_msg);
                        tracing::warn!("{}", err_msg);
                    }
                }
            }
        }
        Err(e) => {
            result.success = false;
            result.stderr = format!("{}\n{}", result.stderr, e);
        }
    }
}

struct DoPublishParams {
    repo_root: PathBuf,
    package: Package,
    output_dir: PathBuf,
    cargo: Arc<Cargo>,
    common_options: PackageRelatedOptions,
    options: Options,
    registries: HashSet<String>,
    member_paths: Arc<Vec<PathBuf>>,
}

/// Returns true if the given publish step should execute.
///
/// When `--publish-steps` is provided, only the explicitly listed steps run.
/// When omitted, falls back to `metadata_enabled` (the Cargo.toml-derived value).
fn should_run_step(options: &Options, step: PublishStep, metadata_enabled: bool) -> bool {
    match &options.publish_steps {
        Some(steps) => steps.contains(&step),
        None => metadata_enabled,
    }
}

fn should_expand_marked_cargo(options: &Options, whitelist: &[String]) -> bool {
    options.publish_all_marked_cargo
        && options
            .publish_all_marked_cargo_for
            .as_ref()
            .is_none_or(|required_root| whitelist.contains(required_root))
}

fn should_run_package_step(
    options: &Options,
    cargo_only: bool,
    step: PublishStep,
    metadata_enabled: bool,
) -> bool {
    (!cargo_only || step == PublishStep::Cargo) && should_run_step(options, step, metadata_enabled)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CargoReleaseAction {
    Publish,
    AlreadyPresent,
    SkipDevelopmentVersion,
}

impl Display for CargoReleaseAction {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Publish => write!(f, "publish"),
            Self::AlreadyPresent => write!(f, "already-present"),
            Self::SkipDevelopmentVersion => write!(f, "skip-development-version"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CargoReleasePlanEntry {
    package: String,
    version: String,
    registry: String,
    action: CargoReleaseAction,
    source: &'static str,
}

fn cargo_release_plan(
    members: &HashMap<PackageId, Package>,
    target_registry: Option<&str>,
) -> anyhow::Result<Vec<CargoReleasePlanEntry>> {
    let mut packages = members
        .values()
        .filter(|package| package.cargo_selected)
        .collect::<Vec<_>>();
    packages.sort_by(|a, b| {
        (&a.package, &a.version, &a.workspace).cmp(&(&b.package, &b.version, &b.workspace))
    });

    let mut plan = Vec::new();
    for package in packages {
        let mut registries = package
            .publish_detail
            .cargo
            .registries
            .clone()
            .unwrap_or_default()
            .into_iter()
            .filter(|registry| target_registry.is_none_or(|target| registry == target))
            .collect::<Vec<_>>();
        registries.sort();

        if registries.is_empty() {
            anyhow::bail!(
                "Cargo release plan selected {} v{} but found no target registry",
                package.package,
                package.version
            );
        }

        let source = match (package.cargo_root, package.cargo_only) {
            (true, true) => "metadata-only-root",
            (true, false) => "tag-scope-root",
            (false, true) => "metadata-only-dependency",
            (false, false) => "tag-scope-dependency",
        };
        for registry in registries {
            let action = if package.version.ends_with("dev") {
                CargoReleaseAction::SkipDevelopmentVersion
            } else if *package
                .publish_detail
                .cargo
                .registries_publish
                .get(&registry)
                .with_context(|| {
                    format!(
                        "Cargo release plan has no existence-check result for {} v{} in registry {}",
                        package.package, package.version, registry
                    )
                })?
            {
                CargoReleaseAction::Publish
            } else {
                CargoReleaseAction::AlreadyPresent
            };
            plan.push(CargoReleasePlanEntry {
                package: package.package.clone(),
                version: package.version.clone(),
                registry,
                action,
                source,
            });
        }
    }

    Ok(plan)
}

fn log_cargo_release_plan(plan: &[CargoReleasePlanEntry]) {
    info!("Cargo release plan: {} entries", plan.len());
    for entry in plan {
        info!(
            "CARGO RELEASE PLAN {}@{} registry={} action={} source={}",
            entry.package, entry.version, entry.registry, entry.action, entry.source
        );
    }
}

const KELLNR_INDEX_SUFFIX: &str = "/api/v1/crates/";
const KELLNR_MAX_ATTEMPTS: u32 = 3;
const KELLNR_REQUEST_TIMEOUT_SECS: u64 = 15;
const KELLNR_MAX_RETRY_DELAY_SECS: u64 = 10;

#[derive(Debug, Deserialize)]
struct KellnrMutationResponse {
    ok: bool,
    msg: String,
}

#[derive(Debug, Deserialize)]
struct KellnrGroupList {
    groups: Vec<KellnrGroup>,
}

#[derive(Debug, Deserialize)]
struct KellnrGroup {
    #[allow(dead_code)]
    id: i32,
    name: String,
}

fn kellnr_api_base_url(registry: &CargoRegistry) -> anyhow::Result<url::Url> {
    let index = registry
        .index
        .as_deref()
        .context("Kellnr group reconciliation requires a Cargo registry index")?;
    let sparse_index = index
        .strip_prefix("sparse+")
        .context("Kellnr group reconciliation requires a sparse Cargo registry")?;
    let mut url = url::Url::parse(sparse_index).context("Invalid sparse Cargo registry URL")?;
    if !matches!(url.scheme(), "http" | "https") {
        anyhow::bail!("Kellnr group reconciliation requires an HTTP(S) registry URL");
    }
    if url.query().is_some() || url.fragment().is_some() {
        anyhow::bail!("Kellnr sparse registry URL must not contain a query or fragment");
    }
    let base_path = url
        .path()
        .strip_suffix(KELLNR_INDEX_SUFFIX)
        .context("Kellnr sparse registry URL must end with /api/v1/crates/")?;
    let base_path = if base_path.is_empty() {
        "/".to_string()
    } else {
        format!("{base_path}/")
    };
    url.set_path(&base_path);
    Ok(url)
}

fn kellnr_crate_group_url(
    base_url: &url::Url,
    crate_name: &str,
    group: Option<&str>,
) -> anyhow::Result<url::Url> {
    let mut url = base_url.clone();
    let mut segments = url
        .path_segments_mut()
        .map_err(|_| anyhow::anyhow!("Kellnr API URL cannot contain path segments"))?;
    segments
        .pop_if_empty()
        .extend(["api", "v1", "crates", crate_name, "crate_groups"]);
    if let Some(group) = group {
        segments.push(group);
    }
    drop(segments);
    Ok(url)
}

fn kellnr_retry_delay(headers: &hyper::HeaderMap, attempt: u32) -> u64 {
    headers
        .get(hyper::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(attempt as u64)
        .min(KELLNR_MAX_RETRY_DELAY_SECS)
}

async fn kellnr_request(
    cargo: &Cargo,
    registry_name: &str,
    method: Method,
    url: &url::Url,
) -> anyhow::Result<Bytes> {
    let registry = cargo
        .get_registry(registry_name)
        .with_context(|| format!("Unknown Cargo registry {registry_name}"))?;
    let token = registry
        .token
        .as_deref()
        .with_context(|| format!("Cargo registry {registry_name} has no authentication token"))?;
    let client = cargo
        .http_client()
        .context("HTTP client required for Kellnr group reconciliation")?;
    let uri: Uri = url
        .as_str()
        .parse()
        .context("Invalid Kellnr group API URL")?;

    for attempt in 1..=KELLNR_MAX_ATTEMPTS {
        let mut request = Request::builder()
            .method(method.clone())
            .uri(uri.clone())
            .header(hyper::header::AUTHORIZATION, token);
        if let Some(user_agent) = &registry.user_agent {
            request = request.header(hyper::header::USER_AGENT, user_agent);
        }
        let request = request
            .body(Empty::default())
            .context("Could not build Kellnr group API request")?;

        let response = match tokio::time::timeout(
            std::time::Duration::from_secs(KELLNR_REQUEST_TIMEOUT_SECS),
            client.request(request),
        )
        .await
        {
            Ok(Ok(response)) => response,
            Ok(Err(_)) | Err(_) if attempt < KELLNR_MAX_ATTEMPTS => {
                tracing::warn!(attempt, "Kellnr request failed, retrying");
                tokio::time::sleep(std::time::Duration::from_secs(attempt as u64)).await;
                continue;
            }
            Ok(Err(_)) | Err(_) => {
                anyhow::bail!("Kellnr request failed after {KELLNR_MAX_ATTEMPTS} attempts")
            }
        };

        let status = response.status();
        let retry_delay = kellnr_retry_delay(response.headers(), attempt);
        let body = match tokio::time::timeout(
            std::time::Duration::from_secs(KELLNR_REQUEST_TIMEOUT_SECS),
            response.into_body().collect(),
        )
        .await
        {
            Ok(Ok(body)) => body.to_bytes(),
            Ok(Err(_)) | Err(_) if attempt < KELLNR_MAX_ATTEMPTS => {
                tracing::warn!(attempt, "Kellnr response failed, retrying");
                tokio::time::sleep(std::time::Duration::from_secs(attempt as u64)).await;
                continue;
            }
            Ok(Err(_)) | Err(_) => {
                anyhow::bail!("Kellnr response failed after {KELLNR_MAX_ATTEMPTS} attempts")
            }
        };

        if status.is_success() {
            return Ok(body);
        }
        let retryable = status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
        if retryable && attempt < KELLNR_MAX_ATTEMPTS {
            tracing::warn!(attempt, status = %status, "Kellnr request failed, retrying");
            tokio::time::sleep(std::time::Duration::from_secs(retry_delay)).await;
            continue;
        }
        anyhow::bail!("Kellnr group API returned HTTP {status}");
    }

    unreachable!("bounded Kellnr retry loop always returns")
}

async fn ensure_cargo_group(
    cargo: &Cargo,
    plan: &[CargoReleasePlanEntry],
    registry_name: &str,
    group: &str,
) -> anyhow::Result<()> {
    validate_cargo_group_configuration(cargo, registry_name, group)?;
    let registry = cargo
        .get_registry(registry_name)
        .with_context(|| format!("Unknown Cargo registry {registry_name}"))?;
    let base_url = kellnr_api_base_url(registry)?;
    let mut crates = plan
        .iter()
        .filter(|entry| {
            entry.registry == registry_name && entry.action == CargoReleaseAction::Publish
        })
        .map(|entry| entry.package.as_str())
        .collect::<Vec<_>>();
    crates.sort_unstable();
    crates.dedup();

    info!(
        "Kellnr group reconciliation plan: {} crates, group={}",
        crates.len(),
        group
    );
    for crate_name in crates {
        let put_url = kellnr_crate_group_url(&base_url, crate_name, Some(group))?;
        let put_body = kellnr_request(cargo, registry_name, Method::PUT, &put_url).await?;
        let mutation: KellnrMutationResponse = serde_json::from_slice(&put_body)
            .context("Could not parse Kellnr group mutation response")?;
        if !mutation.ok {
            anyhow::bail!(
                "Kellnr rejected group {} for crate {}: {}",
                group,
                crate_name,
                mutation.msg
            );
        }

        let get_url = kellnr_crate_group_url(&base_url, crate_name, None)?;
        let get_body = kellnr_request(cargo, registry_name, Method::GET, &get_url).await?;
        let groups: KellnrGroupList = serde_json::from_slice(&get_body)
            .context("Could not parse Kellnr group verification response")?;
        if !groups.groups.iter().any(|existing| existing.name == group) {
            anyhow::bail!(
                "Kellnr group verification failed for crate {} and group {}",
                crate_name,
                group
            );
        }
        info!(
            "Kellnr group verified: crate={} group={}",
            crate_name, group
        );
    }

    Ok(())
}

fn validate_cargo_group_configuration(
    cargo: &Cargo,
    registry_name: &str,
    group: &str,
) -> anyhow::Result<()> {
    if group.trim().is_empty() {
        anyhow::bail!("Cargo group name must not be empty");
    }
    let registry = cargo
        .get_registry(registry_name)
        .with_context(|| format!("Unknown Cargo registry {registry_name}"))?;
    kellnr_api_base_url(registry)?;
    if registry.token.is_none() {
        anyhow::bail!("Cargo registry {registry_name} has no authentication token");
    }
    cargo
        .http_client()
        .context("HTTP client required for Kellnr group reconciliation")?;
    Ok(())
}

/// Actual Publish
async fn do_publish_package(params: DoPublishParams) -> PublishResult {
    let DoPublishParams {
        repo_root,
        package,
        output_dir,
        cargo,
        common_options,
        options,
        registries,
        member_paths,
    } = params;
    let mut result = PublishResult::new(&package, registries.clone(), &options);
    result.start_time = Some(SystemTime::now());
    if !package.publish {
        result.end_time = Some(SystemTime::now());
        return result;
    }
    // let workspace_name = &package.workspace;
    let package_version = &package.version;
    let package_name = &package.package;
    let cargo_only = package.cargo_only;
    let package_path = repo_root.join(&package.path);
    let mut is_failed = false;
    if !is_failed
        && should_run_package_step(
            &options,
            cargo_only,
            PublishStep::S3,
            package.publish_detail.s3.publish,
        )
    {
        // Extract what we need before any moves
        let s3_build_command = package.publish_detail.s3.build_command.clone();
        let s3_output_dir = package.publish_detail.s3.output_dir.clone();
        let s3_destinations = package.publish_detail.s3.resolved_destinations();

        if options.dry_run {
            for dest_result in result.s3.values_mut() {
                dest_result.start_time = Some(SystemTime::now());
                dest_result.success = true;
                dest_result.end_time = Some(SystemTime::now());
            }
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
                envs.insert(user_agent_env, user_agent);
                envs.insert(token_env, token);
                envs.insert(name_env, common_options.cargo_main_registry.clone());
            }

            // Mark start time for all destinations
            for dest_result in result.s3.values_mut() {
                dest_result.start_time = Some(SystemTime::now());
            }

            // Build once for all destinations
            let command_output = Script::new(&s3_build_command, true)
                .current_dir(&package_path)
                .env_removals(&blacklist_envs)
                .envs(&envs)
                .log_stdout(tracing::Level::INFO)
                .log_stderr(tracing::Level::INFO)
                .execute()
                .await;

            let build_success = command_output.success;

            // Propagate build output to all destination results
            for dest_result in result.s3.values_mut() {
                dest_result.update_from_command(command_output.clone());
            }

            if build_success {
                let build_dir = package_path.join(s3_output_dir.as_deref().unwrap_or(""));

                for (dest_name, dest) in &s3_destinations {
                    if let Some(dest_result) = result.s3.get_mut(dest_name) {
                        publish_to_s3_destination(
                            dest_name,
                            dest,
                            &build_dir,
                            &options,
                            dest_result,
                        )
                        .await;

                        if !dest_result.success {
                            is_failed = true;
                        }
                    }
                }
            } else {
                is_failed = true;
            }

            for dest_result in result.s3.values_mut() {
                dest_result.end_time = Some(SystemTime::now());
            }
        }
    }
    if !is_failed
        && should_run_package_step(
            &options,
            cargo_only,
            PublishStep::Cargo,
            package.publish_detail.cargo.publish,
        )
    {
        let additional_args = package.publish_detail.additional_args.unwrap_or_default();

        if let Some(original_registry) = cargo.get_registry(&common_options.cargo_main_registry) {
            for (registry_name, registry_publish) in package.publish_detail.cargo.registries_publish
            {
                let mut r = PublishDetailResult {
                    name: format!(
                        "{package_name}@{package_version} cargo publish -r {registry_name}"
                    ),
                    key: format!("cargo_{registry_name}"),
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
                if let Some(ref target) = common_options.cargo_target_registry
                    && &registry_name != target
                {
                    run = false;
                    r.should_publish = false;
                }
                if run && let Some(target_registry) = cargo.get_registry(&registry_name) {
                    if target_registry.index.is_none() {
                        tracing::warn!(
                            "Skipping cargo publish for {} to registry {}: no index configured",
                            package_name,
                            registry_name
                        );
                        r.success = false;
                        r.stderr = format!("Registry {} has no index configured", registry_name);
                        r.end_time = Some(SystemTime::now());
                        result.cargo.insert(registry_name.clone(), r);
                        continue;
                    }
                    // For each reg we need to
                    // 1. Ensure registry is in `publish = []`
                    // 2. Find and replace `main_registry` to `current_registry` in Cargo.toml
                    // 3. Ensure there are Cargo.lock
                    // 4. Publish with --allow-dirty

                    // Save workspace Cargo.lock before patching — cargo publish may modify it.
                    // Walk up from package_path to find the workspace root Cargo.lock.
                    let workspace_lock_path = {
                        let mut dir = package_path.clone();
                        loop {
                            let candidate = dir.join("Cargo.lock");
                            if candidate.exists() {
                                break Some(candidate);
                            }
                            if !dir.pop() || dir < repo_root {
                                break None;
                            }
                        }
                    };
                    let saved_lock_content = workspace_lock_path
                        .as_ref()
                        .and_then(|p| std::fs::read_to_string(p).ok());

                    if let Err(patch_err) = patch_crate_for_registry(
                        &repo_root,
                        &package_path,
                        original_registry,
                        target_registry,
                        cargo.http_client(),
                        &member_paths,
                    ) {
                        tracing::error!(
                            registry = %registry_name,
                            error = %patch_err,
                            "patch_crate_for_registry failed"
                        );
                        r.success = false;
                        r.stderr = format!(
                            "patch_crate_for_registry failed for registry {registry_name}: {patch_err}"
                        );
                    } else {
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
                        let command_output =
                            Script::new(format!("cargo publish {}", args.join(" ")), true)
                                .current_dir(&package_path)
                                .env_removals(&blacklist_envs)
                                .envs(&envs)
                                .log_stdout(tracing::Level::INFO)
                                .log_stderr(tracing::Level::INFO)
                                .execute()
                                .await;
                        r.update_from_command(command_output);
                    }

                    // Restore workspace Cargo.lock before patch-back to avoid lock file corruption
                    if let (Some(lock_path), Some(content)) =
                        (&workspace_lock_path, &saved_lock_content)
                    {
                        let _ = std::fs::write(lock_path, content);
                    }

                    // Patch back to the main registry
                    if let Err(patch_back_err) = patch_crate_for_registry(
                        &repo_root,
                        &package_path,
                        target_registry,
                        original_registry,
                        cargo.http_client(),
                        &member_paths,
                    ) {
                        tracing::error!(
                            registry = %common_options.cargo_main_registry,
                            error = %patch_back_err,
                            "patch_crate_for_registry failed to restore registry"
                        );
                        r.success = false;
                        if r.stderr.is_empty() {
                            r.stderr = format!(
                                "patch_crate_for_registry failed restoring registry {}: {patch_back_err}",
                                common_options.cargo_main_registry,
                            );
                        } else {
                            r.stderr = format!(
                                "{}\npatch_crate_for_registry failed restoring registry {}: {patch_back_err}",
                                r.stderr, common_options.cargo_main_registry,
                            );
                        }
                    }
                    is_failed = !r.success;
                }
                r.end_time = Some(SystemTime::now());
                result.cargo.insert(registry_name.clone(), r);
            }
        }
    }
    if !is_failed
        && should_run_package_step(
            &options,
            cargo_only,
            PublishStep::Nix,
            package.publish_detail.nix_binary.publish,
        )
    {
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
                let command_output = Script::new(
                    format!("attic login central {atticd_url}/ {atticd_token}"),
                    false,
                )
                .current_dir(&repo_root)
                .log_stdout(tracing::Level::DEBUG)
                .log_stderr(tracing::Level::DEBUG)
                .execute()
                .await;
                result.nix_binary.update_from_command(command_output);
                is_failed = !result.nix_binary.success;
                if !is_failed {
                    let command_output =
                        Script::new(format!("attic use central:{atticd_cache}"), true)
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
                let mut command_output = Script::new("nix build .#release --fallback", true)
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
                ), true)
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
    if !is_failed
        && should_run_package_step(
            &options,
            cargo_only,
            PublishStep::Docker,
            package.publish_detail.docker.publish,
        )
    {
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

            if let Ok(token) = env::var(format!("{main_registry_prefix}_TOKEN")) {
                let token_env = format!("{main_registry_prefix}_TOKEN");
                let name_env = format!("{main_registry_prefix}_NAME");
                envs.insert(token_env.clone(), token);
                envs.insert(name_env.clone(), common_options.cargo_main_registry.clone());
                args.push(format!(
                    "--secret id=cargo_private_registry_token,env={token_env}"
                ));
                args.push(format!(
                    "--secret id=cargo_private_registry_name,env={name_env}"
                ));

                // USER_AGENT is only set for git-based registries, not sparse ones (e.g. FSL)
                if let Ok(user_agent) = env::var(format!("{main_registry_prefix}_USER_AGENT")) {
                    let user_agent_env = format!("{main_registry_prefix}_USER_AGENT");
                    envs.insert(user_agent_env.clone(), user_agent);
                    args.push(format!(
                        "--secret id=cargo_private_registry_user_agent,env={user_agent_env}"
                    ));
                }
            }
            args.push(context.clone());
            // First we build
            let command_output = Script::new(
                format!(
                    "docker buildx build --push --progress plain {}",
                    args.join(" ")
                ),
                true,
            )
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
    if !is_failed
        && should_run_package_step(
            &options,
            cargo_only,
            PublishStep::Tags,
            result.git_tag.should_publish,
        )
    {
        result.git_tag.start_time = Some(SystemTime::now());
        let tagged: anyhow::Result<()> = async {
            let tag = format_tag(&options.tag_format, &package.package, &package.version);
            if let (Some(github_app_id), Some(github_app_private_key)) = (
                options.github_app_id,
                options.github_app_private_key.clone(),
            ) {
                result.git_tag.stdout = format!("{}\nRetrieving git HEAD", result.git_tag.stdout);
                let Some(head) = gix::open(&repo_root)
                    .ok()
                    .and_then(|r| r.head_commit().map(|commit| commit.id().to_string()).ok())
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
                match repo
                    .create_ref(&Reference::Tag(tag.clone()), head.clone())
                    .await
                {
                    Ok(_) => {}
                    Err(create_err) => {
                        // Tag may already exist — check the existing ref and reconcile.
                        match repo.get_ref(&Reference::Tag(tag.clone())).await {
                            Ok(existing_ref) => {
                                let existing_sha = match &existing_ref.object {
                                    octocrab::models::repos::Object::Commit { sha, .. } => {
                                        sha.clone()
                                    }
                                    octocrab::models::repos::Object::Tag { sha, .. } => sha.clone(),
                                    _ => {
                                        return Err(anyhow::anyhow!(
                                            "Unexpected ref object type for tag {}",
                                            tag
                                        ));
                                    }
                                };
                                if existing_sha == head {
                                    result.git_tag.stdout = format!(
                                        "{}\nTag {} already exists at {}, skipping",
                                        result.git_tag.stdout, tag, head
                                    );
                                } else {
                                    result.git_tag.stdout = format!(
                                        "{}\nTag {} already exists at {}, updating to {}",
                                        result.git_tag.stdout, tag, existing_sha, head
                                    );
                                    repo.delete_ref(&Reference::Tag(tag.clone())).await?;
                                    repo.create_ref(&Reference::Tag(tag), head).await?;
                                }
                            }
                            Err(_) => {
                                // The get_ref also failed; surface the original create error.
                                return Err(create_err.into());
                            }
                        }
                    }
                }
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
            false
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
            false
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
/// Uses git CLI's describe functionality for efficient exact-match tag lookup.
/// Filters tags by the provided pattern (e.g., "v*" or "cargo-fslabscli-*")
fn resolve_commit_to_tag(
    repo_root: &Path,
    commit_ref: &str,
    tag_pattern: &str,
) -> anyhow::Result<String> {
    let output = std::process::Command::new("git")
        .args([
            "describe",
            "--tags",
            "--exact-match",
            &format!("--match={}", tag_pattern),
            commit_ref,
        ])
        .current_dir(repo_root)
        .output()
        .with_context(|| format!("Failed to run git describe for {}", commit_ref))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "No tag matching pattern '{}' found for commit {}: {}",
            tag_pattern,
            commit_ref,
            stderr.trim()
        );
    }

    let tag = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(tag)
}

pub async fn report_publish_to_github(
    _common_options: &PackageRelatedOptions,
    options: &Options,
    artifact_dir: &Path,
    repo_root: &Path,
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

        // Determine the release tag to upload artifacts to.
        // --release-tag takes precedence; otherwise resolve from git history.
        let release_tag = if let Some(explicit_tag) = &options.release_tag {
            tracing::info!("Using explicit release tag: {}", explicit_tag);
            explicit_tag.clone()
        } else {
            // If base_rev is not set, use HEAD to find the most recent tag
            let base_rev = options.base_rev.as_deref().unwrap_or("HEAD");

            // Check if base_rev is already a tag name by trying to verify it exists as a tag
            let repo = gix::open(repo_root)
                .with_context(|| format!("Failed to open git repository at {:?}", repo_root))?;

            if repo
                .find_reference(&format!("refs/tags/{}", base_rev))
                .is_ok()
            {
                // base_rev is already a valid tag name, use it directly
                tracing::info!("Using {} as release tag (already a valid tag)", base_rev);
                base_rev.to_string()
            } else {
                // base_rev is a commit reference, resolve it to a tag
                match resolve_commit_to_tag(repo_root, base_rev, &options.tag_pattern) {
                    Ok(resolved_tag) => {
                        tracing::info!(
                            "Resolved commit {} to tag: {} (pattern: {})",
                            base_rev,
                            resolved_tag,
                            options.tag_pattern
                        );
                        resolved_tag
                    }
                    Err(e) if options.draft => {
                        // Draft mode without a git tag (e.g. --publish-steps nix,github skips cargo/tagging).
                        // Compute the expected release tag from Cargo.toml + tag_format.
                        tracing::info!(
                            "No tag found on {} ({}); computing tag from Cargo.toml for draft release",
                            base_rev,
                            e
                        );
                        let manifest: toml::Value = toml::from_str(
                            &std::fs::read_to_string(repo_root.join("Cargo.toml"))
                                .with_context(|| "Failed to read workspace Cargo.toml")?,
                        )
                        .with_context(|| "Failed to parse workspace Cargo.toml")?;
                        let version = manifest["package"]["version"].as_str().unwrap_or("0.0.0");
                        let package_name = manifest["package"]["name"].as_str().unwrap_or("");
                        let tag = format_tag(&options.tag_format, package_name, version);
                        tracing::info!("Computed draft release tag: {}", tag);
                        tag
                    }
                    Err(e) => return Err(e),
                }
            }
        };

        let repo = octocrab.repos(&options.repo_owner, &options.repo_name);
        let repo_releases = repo.releases();
        let release = match repo_releases.get_by_tag(&release_tag).await {
            Ok(release) => release,
            Err(octocrab::Error::GitHub { source, .. })
                if source.status_code == http::StatusCode::NOT_FOUND =>
            {
                if options.draft {
                    // get_by_tag only returns published releases; drafts always 404 there.
                    // List all releases (including drafts) and match by tag_name.
                    tracing::info!("Looking for draft release with tag {}", release_tag);
                    let first_page = repo_releases
                        .list()
                        .per_page(100)
                        .send()
                        .await
                        .with_context(|| "Failed to list releases to find draft")?;
                    let all_releases = octocrab
                        .all_pages::<octocrab::models::repos::Release>(first_page)
                        .await
                        .with_context(|| "Failed to paginate releases to find draft")?;
                    let existing_draft = all_releases
                        .into_iter()
                        .find(|r| r.draft && r.tag_name == release_tag);
                    match existing_draft {
                        Some(draft_release) => {
                            tracing::info!(
                                "Found existing draft release id={} for tag {}",
                                draft_release.id,
                                release_tag
                            );
                            draft_release
                        }
                        None => {
                            tracing::info!(
                                "No draft release found for tag {}, creating one",
                                release_tag
                            );
                            {
                                let notes = repo_releases
                                    .generate_release_notes(&release_tag)
                                    .target_commitish("main")
                                    .send()
                                    .await
                                    .ok();
                                let mut builder = repo_releases
                                    .create(&release_tag)
                                    .name(&release_tag)
                                    .draft(true);
                                if let Some(notes) = &notes {
                                    builder = builder.body(&notes.body);
                                }
                                builder.send().await.with_context(|| {
                                    format!(
                                        "Failed to create draft release for tag: {}",
                                        release_tag
                                    )
                                })?
                            }
                        }
                    }
                } else {
                    tracing::info!("No existing release for tag {}, creating one", release_tag);
                    {
                        let notes = repo_releases
                            .generate_release_notes(&release_tag)
                            .send()
                            .await
                            .ok();
                        let mut builder = repo_releases.create(&release_tag).name(&release_tag);
                        if let Some(notes) = &notes {
                            builder = builder.body(&notes.body);
                        }
                        builder.send().await.with_context(|| {
                            format!("Failed to create release for tag: {}", release_tag)
                        })?
                    }
                }
            }
            Err(e) => {
                return Err(e).context(format!("Failed to fetch release for tag: {}", release_tag));
            }
        };
        upload_artifacts_to_release(&repo, release.id.into_inner(), artifact_dir)
            .await
            .with_context(|| format!("Failed to upload artifacts to release {}", release_tag))?;
    } else {
        tracing::debug!("Github credentials not set, not doing anything");
    }
    Ok(())
}

/// Returns true if the dependency should be included in publish ordering.
/// Registry dev deps (is_local=false) are kept because cargo publish verifies them.
/// Path-only dev deps (is_local=true) are stripped — they can form cycles and aren't on the registry.
fn is_publish_ordered_dep(dep: &crate::crate_graph::Dependency) -> bool {
    dep.instances
        .iter()
        .any(|k| k.kind != DependencyKind::Development || !k.is_local)
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
        options.base_rev
    );

    let base_rev = options.base_rev.as_deref().unwrap_or("HEAD~");
    let mut whitelist = if options.base_rev_regex.is_some() {
        // When using regex, start with empty whitelist
        // Only packages matching the regex will be added
        Vec::new()
    } else {
        common_options.whitelist.clone()
    };

    if let Some(regex) = &options.base_rev_regex {
        let re = Regex::new(regex)?;
        if let Some(captures) = re.captures(base_rev)
            && let Some(package_name_match) = captures.get(1)
        {
            whitelist.push(package_name_match.as_str().to_string());
            tracing::info!(
                "Regex '{}' matched base_rev '{}'. Adding '{}' to whitelist",
                regex,
                base_rev,
                package_name_match.as_str()
            );
        } else {
            tracing::warn!(
                "base_rev_regex '{}' did not match base_rev '{}'. No packages will be published.",
                regex,
                base_rev
            );
            return Ok(PublishResults {
                published_members: HashMap::new(),
                all_members: HashMap::new(),
            });
        }
    }
    common_options.whitelist = whitelist;
    let expand_marked_cargo = should_expand_marked_cargo(options, &common_options.whitelist);
    if options.publish_all_marked_cargo && !expand_marked_cargo {
        tracing::info!(
            required_root = ?options.publish_all_marked_cargo_for,
            whitelist = ?common_options.whitelist,
            "Skipping marked Cargo expansion because the release root filter did not match"
        );
    }

    // Dev dependencies must be included in the dependency graph so publish ordering is
    // correct. `cargo publish` validates that all declared dependencies exist on the
    // registry, including dev deps. If crate A is a registry dev-dependency of crate B,
    // A must be published before B. The publish ordering filter below strips path-only
    // dev deps (never on a registry) while keeping registry dev deps in the ordering.
    // Cycle detection in crate_graph.rs skips local dev-only deps to allow valid cycles.
    //
    // WARNING: Do not set ignore_dev_dependencies to true — it removes dev deps from
    // the ordering graph and causes publish failures for registry dev-dependencies.
    let check_workspace_options = CheckWorkspaceOptions::new()
        .with_check_publish(true)
        .with_autopublish_cargo(options.autopublish_cargo)
        .with_publish_all_marked_cargo(expand_marked_cargo)
        .with_release_discovery(expand_marked_cargo)
        .with_fail_cargo_check_error(true)
        .with_ignore_dev_dependencies(false)
        .with_force_publish(options.force_publish);

    let results =
        check_workspace::<Cargo>(common_options, &check_workspace_options, repo_root.clone())
            .await
            .map_err(|e| {
                tracing::error!("Check directory for crates that need publishing: {}", e);
                e
            })
            .with_context(|| "Could not get directory information")?;

    let cargo_release_plan = cargo_release_plan(
        &results.members,
        common_options.cargo_target_registry.as_deref(),
    )?;
    log_cargo_release_plan(&cargo_release_plan);

    let mut registries = HashSet::new();
    for member in results.members.values() {
        if let Some(r) = member.publish_detail.cargo.registries.clone() {
            registries.extend(r);
        }
    }
    let cargo = Arc::new(Cargo::new(&registries, true)?);
    if let Some(group) = options.ensure_cargo_group.as_deref()
        && !options.dry_run
    {
        let registry_name = common_options
            .cargo_target_registry
            .as_deref()
            .context("--ensure-cargo-group requires --cargo-target-registry")?;
        validate_cargo_group_configuration(cargo.as_ref(), registry_name, group)?;
    }
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
    let member_paths: Vec<PathBuf> = results
        .members
        .values()
        .map(|m| repo_root.join(&m.path))
        .collect();
    let member_paths = Arc::new(member_paths);
    // Filters members based on regex
    // Spawn a task for each object
    for (member_id, member) in &results.members {
        let dependencies = results
            .crate_graph
            .dependency_graph()
            .dependencies
            .get(member_id)
            .map(|deps| {
                // Strip path-only dev deps (never on the registry and may form cycles).
                // Registry dev deps (is_local=false) are retained for correct publish ordering.
                deps.iter()
                    .filter(|d| is_publish_ordered_dep(d))
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
            member_paths.clone(),
        ));
        handles.push(task_handle);
    }
    futures::future::join_all(handles).await;

    let (global_success, published_members) = {
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
        (global_success, published_members)
    };

    if global_success
        && !options.dry_run
        && let Some(group) = options.ensure_cargo_group.as_deref()
    {
        let registry_name = common_options
            .cargo_target_registry
            .as_deref()
            .context("--ensure-cargo-group requires --cargo-target-registry")?;
        ensure_cargo_group(cargo.as_ref(), &cargo_release_plan, registry_name, group)
            .await
            .context("Could not reconcile Kellnr Cargo groups")?;
    }

    // Report publish result to github
    if should_run_step(options, PublishStep::Github, true) {
        report_publish_to_github(common_options, options, &artifact_dir, &repo_root)
            .await
            .with_context(|| "Failed to report publish results to GitHub")?;
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
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use cargo_metadata::PackageId;
    use clap::Parser;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, Request as WiremockRequest, Respond, ResponseTemplate};

    use super::{
        CargoReleaseAction, CargoReleasePlanEntry, Options, PublishStep, cargo_release_plan,
        ensure_cargo_group, kellnr_api_base_url, kellnr_crate_group_url, kellnr_retry_delay,
        resolve_commit_to_tag, should_expand_marked_cargo, should_run_package_step,
    };
    use crate::commands::check_workspace::{PackageMetadataFslabsCiPublish, Result as Package};
    use crate::utils::cargo::{Cargo, CargoRegistry};
    use crate::utils::test::{commit_all_changes, init_repo, modify_file};

    fn cargo_plan_package(
        name: &str,
        registry_publish: HashMap<String, bool>,
        cargo_root: bool,
        cargo_only: bool,
    ) -> (PackageId, Package) {
        let package_id = PackageId {
            repr: format!("{name} 1.0.0"),
        };
        let registries = registry_publish.keys().cloned().collect::<HashSet<_>>();
        let mut publish_detail = PackageMetadataFslabsCiPublish::default();
        publish_detail.cargo.registries = Some(registries);
        publish_detail.cargo.registries_publish = registry_publish;
        let mut package = Package::default();
        package.package = name.to_string();
        package.package_id = Some(package_id.clone());
        package.version = "1.0.0".to_string();
        package.publish_detail = publish_detail;
        package.cargo_selected = true;
        package.cargo_root = cargo_root;
        package.cargo_only = cargo_only;
        (package_id, package)
    }

    const TEST_CARGO_TOKEN: &str = "test-cargo-token";

    fn kellnr_test_cargo(mock_server: &MockServer) -> Cargo {
        let mut cargo = Cargo::default();
        cargo.add_registry(CargoRegistry {
            name: "fsl".to_string(),
            index: Some(format!("sparse+{}/api/v1/crates/", mock_server.uri())),
            token: Some(TEST_CARGO_TOKEN.to_string()),
            ..Default::default()
        });
        cargo
    }

    fn kellnr_test_plan(crate_name: &str) -> Vec<CargoReleasePlanEntry> {
        vec![CargoReleasePlanEntry {
            package: crate_name.to_string(),
            version: "1.0.0".to_string(),
            registry: "fsl".to_string(),
            action: CargoReleaseAction::Publish,
            source: "metadata-only-root",
        }]
    }

    async fn mount_kellnr_success(
        mock_server: &MockServer,
        crate_name: &str,
        group: &str,
        expected_calls: u64,
    ) {
        Mock::given(method("PUT"))
            .and(path(format!(
                "/api/v1/crates/{crate_name}/crate_groups/{group}"
            )))
            .and(header("authorization", TEST_CARGO_TOKEN))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "msg": "Added groups to crate."
            })))
            .expect(expected_calls)
            .mount(mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/crates/{crate_name}/crate_groups")))
            .and(header("authorization", TEST_CARGO_TOKEN))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "groups": [{"id": 1, "name": group}]
            })))
            .expect(expected_calls)
            .mount(mock_server)
            .await;
    }

    #[derive(Clone)]
    struct FailOnceThenSucceed {
        calls: Arc<AtomicUsize>,
    }

    impl Respond for FailOnceThenSucceed {
        fn respond(&self, _request: &WiremockRequest) -> ResponseTemplate {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(503).insert_header("Retry-After", "0")
            } else {
                ResponseTemplate::new(200).set_body_json(json!({
                    "ok": true,
                    "msg": "Added groups to crate."
                }))
            }
        }
    }

    #[test]
    fn test_publish_all_marked_cargo_flag_parses() {
        let options = Options::try_parse_from([
            "publish",
            "--repo-owner",
            "fsl",
            "--repo-name",
            "fsl_libs",
            "--publish-all-marked-cargo",
            "--publish-all-marked-cargo-for",
            "fdk",
            "--ensure-cargo-group",
            "fdk",
        ])
        .unwrap();

        assert!(options.publish_all_marked_cargo);
        assert_eq!(options.publish_all_marked_cargo_for.as_deref(), Some("fdk"));
        assert_eq!(options.ensure_cargo_group.as_deref(), Some("fdk"));

        assert!(
            Options::try_parse_from([
                "publish",
                "--repo-owner",
                "fsl",
                "--repo-name",
                "fsl_libs",
                "--publish-all-marked-cargo-for",
                "fdk",
            ])
            .is_err()
        );
    }

    #[test]
    fn test_publish_all_marked_cargo_for_requires_matching_release_root() {
        let filtered = Options {
            publish_all_marked_cargo: true,
            publish_all_marked_cargo_for: Some("fdk".to_string()),
            ..Default::default()
        };

        assert!(should_expand_marked_cargo(&filtered, &["fdk".to_string()]));
        assert!(!should_expand_marked_cargo(
            &filtered,
            &["spatial_studio".to_string()]
        ));
        assert!(!should_expand_marked_cargo(&filtered, &[]));

        let generic = Options {
            publish_all_marked_cargo: true,
            ..Default::default()
        };
        assert!(should_expand_marked_cargo(
            &generic,
            &["spatial_studio".to_string()]
        ));

        let disabled = Options {
            publish_all_marked_cargo_for: Some("fdk".to_string()),
            ..Default::default()
        };
        assert!(!should_expand_marked_cargo(&disabled, &["fdk".to_string()]));
    }

    #[test]
    fn test_cargo_only_scope_rejects_explicit_non_cargo_step_override() {
        let options = Options {
            publish_steps: Some(vec![PublishStep::Cargo, PublishStep::Nix]),
            ..Default::default()
        };
        assert!(should_run_package_step(
            &options,
            true,
            PublishStep::Cargo,
            false
        ));
        assert!(!should_run_package_step(
            &options,
            true,
            PublishStep::Nix,
            true
        ));
    }

    #[test]
    fn test_cargo_release_plan_is_sorted_and_includes_existing_versions() {
        let (package_b_id, package_b) = cargo_plan_package(
            "package_b",
            HashMap::from([("fsl".to_string(), true)]),
            true,
            true,
        );
        let (package_a_id, package_a) = cargo_plan_package(
            "package_a",
            HashMap::from([("fsl".to_string(), false), ("other".to_string(), true)]),
            false,
            false,
        );
        let members = HashMap::from([(package_b_id, package_b), (package_a_id, package_a)]);

        let plan = cargo_release_plan(&members, Some("fsl")).unwrap();

        assert_eq!(
            plan,
            vec![
                CargoReleasePlanEntry {
                    package: "package_a".to_string(),
                    version: "1.0.0".to_string(),
                    registry: "fsl".to_string(),
                    action: CargoReleaseAction::AlreadyPresent,
                    source: "tag-scope-dependency",
                },
                CargoReleasePlanEntry {
                    package: "package_b".to_string(),
                    version: "1.0.0".to_string(),
                    registry: "fsl".to_string(),
                    action: CargoReleaseAction::Publish,
                    source: "metadata-only-root",
                },
            ]
        );
    }

    #[test]
    fn test_kellnr_urls_are_derived_from_sparse_index_and_encode_segments() {
        let registry = CargoRegistry {
            index: Some("sparse+https://crates.example.com/kellnr/api/v1/crates/".to_string()),
            ..Default::default()
        };

        let base_url = kellnr_api_base_url(&registry).unwrap();
        let group_url = kellnr_crate_group_url(&base_url, "crate name", Some("fdk/admin")).unwrap();

        assert_eq!(base_url.as_str(), "https://crates.example.com/kellnr/");
        assert_eq!(
            group_url.as_str(),
            "https://crates.example.com/kellnr/api/v1/crates/crate%20name/crate_groups/fdk%2Fadmin"
        );
    }

    #[test]
    fn test_kellnr_url_rejects_non_kellnr_sparse_index() {
        let registry = CargoRegistry {
            index: Some("sparse+https://index.crates.io/".to_string()),
            ..Default::default()
        };

        let error = kellnr_api_base_url(&registry).unwrap_err();

        assert!(error.to_string().contains("must end with /api/v1/crates/"));
    }

    #[test]
    fn test_kellnr_retry_after_is_bounded() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert(hyper::header::RETRY_AFTER, "3600".parse().unwrap());

        assert_eq!(kellnr_retry_delay(&headers, 1), 10);
        assert_eq!(kellnr_retry_delay(&hyper::HeaderMap::new(), 2), 2);
    }

    #[tokio::test]
    async fn test_ensure_cargo_group_uses_raw_token_and_verifies_membership() {
        let mock_server = MockServer::start().await;
        mount_kellnr_success(&mock_server, "dagger_cli", "fdk", 1).await;
        let cargo = kellnr_test_cargo(&mock_server);

        ensure_cargo_group(&cargo, &kellnr_test_plan("dagger_cli"), "fsl", "fdk")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_ensure_cargo_group_is_idempotent_across_repeated_runs() {
        let mock_server = MockServer::start().await;
        mount_kellnr_success(&mock_server, "dagger_microservice", "fdk", 2).await;
        let cargo = kellnr_test_cargo(&mock_server);
        let plan = kellnr_test_plan("dagger_microservice");

        ensure_cargo_group(&cargo, &plan, "fsl", "fdk")
            .await
            .unwrap();
        ensure_cargo_group(&cargo, &plan, "fsl", "fdk")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_ensure_cargo_group_skips_already_present_crates() {
        let mock_server = MockServer::start().await;
        let cargo = kellnr_test_cargo(&mock_server);
        let plan = vec![CargoReleasePlanEntry {
            package: "dagger_cli".to_string(),
            version: "1.0.0".to_string(),
            registry: "fsl".to_string(),
            action: CargoReleaseAction::AlreadyPresent,
            source: "metadata-only-root",
        }];

        ensure_cargo_group(&cargo, &plan, "fsl", "fdk")
            .await
            .unwrap();

        assert!(mock_server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_ensure_cargo_group_does_not_retry_forbidden_response() {
        let mock_server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/api/v1/crates/dagger_cli/crate_groups/fdk"))
            .and(header("authorization", TEST_CARGO_TOKEN))
            .respond_with(ResponseTemplate::new(403))
            .expect(1)
            .mount(&mock_server)
            .await;
        let cargo = kellnr_test_cargo(&mock_server);

        let error = ensure_cargo_group(&cargo, &kellnr_test_plan("dagger_cli"), "fsl", "fdk")
            .await
            .unwrap_err();

        assert!(error.to_string().contains("HTTP 403"));
        assert!(!error.to_string().contains(TEST_CARGO_TOKEN));
    }

    #[tokio::test]
    async fn test_ensure_cargo_group_retries_transient_server_error() {
        let mock_server = MockServer::start().await;
        let calls = Arc::new(AtomicUsize::new(0));
        Mock::given(method("PUT"))
            .and(path("/api/v1/crates/dagger_cli/crate_groups/fdk"))
            .and(header("authorization", TEST_CARGO_TOKEN))
            .respond_with(FailOnceThenSucceed {
                calls: calls.clone(),
            })
            .expect(2)
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/crates/dagger_cli/crate_groups"))
            .and(header("authorization", TEST_CARGO_TOKEN))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "groups": [{"id": 1, "name": "fdk"}]
            })))
            .expect(1)
            .mount(&mock_server)
            .await;
        let cargo = kellnr_test_cargo(&mock_server);

        ensure_cargo_group(&cargo, &kellnr_test_plan("dagger_cli"), "fsl", "fdk")
            .await
            .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_ensure_cargo_group_fails_when_get_does_not_verify_group() {
        let mock_server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/api/v1/crates/dagger_cli/crate_groups/fdk"))
            .and(header("authorization", TEST_CARGO_TOKEN))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "msg": "Added groups to crate."
            })))
            .expect(1)
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/crates/dagger_cli/crate_groups"))
            .and(header("authorization", TEST_CARGO_TOKEN))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "groups": [{"id": 2, "name": "other"}]
            })))
            .expect(1)
            .mount(&mock_server)
            .await;
        let cargo = kellnr_test_cargo(&mock_server);

        let error = ensure_cargo_group(&cargo, &kellnr_test_plan("dagger_cli"), "fsl", "fdk")
            .await
            .unwrap_err();

        assert!(error.to_string().contains("verification failed"));
    }

    /// Helper function to create a test git repository with initial commit
    fn create_test_repo() -> (assert_fs::TempDir, PathBuf) {
        let temp_dir = assert_fs::TempDir::new().expect("Failed to create temp directory");
        let repo_path = temp_dir.path().to_path_buf();

        init_repo(&repo_path);

        // Create initial file and commit
        modify_file(&repo_path, "README.md", "# Test Repository");
        commit_all_changes(&repo_path, "Initial commit");

        (temp_dir, repo_path)
    }

    /// Helper function to create a commit in the repository
    fn create_commit(repo_path: &PathBuf, message: &str) -> String {
        // Modify a file to have something to commit
        modify_file(repo_path, "test.txt", &format!("Content for {}", message));
        commit_all_changes(repo_path, message);

        // Get the commit SHA
        let output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(repo_path)
            .output()
            .expect("Failed to get HEAD SHA");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// Helper function to create a lightweight tag pointing to a commit
    fn create_tag(repo_path: &PathBuf, tag_name: &str, commit_sha: &str) {
        let output = Command::new("git")
            .args(["tag", tag_name, commit_sha])
            .current_dir(repo_path)
            .output()
            .expect("Failed to create tag");
        assert!(output.status.success(), "git tag failed: {:?}", output);
    }

    #[test]
    fn test_resolve_commit_with_cargo_fslabscli_tag() {
        // Test: A commit with cargo-fslabscli-* tag (project's actual format)
        let (_temp_dir, repo_path) = create_test_repo();
        let commit_oid = create_commit(&repo_path, "Add feature");
        create_tag(&repo_path, "cargo-fslabscli-2.29.1", &commit_oid);

        let result = resolve_commit_to_tag(&repo_path, "HEAD", "cargo-fslabscli-*");
        assert!(result.is_ok(), "Should successfully resolve to tag");
        assert_eq!(result.unwrap(), "cargo-fslabscli-2.29.1");
    }

    #[test]
    fn test_resolve_commit_with_version_tag() {
        // Test: A commit with a version tag (v-prefixed) should return that tag
        let (_temp_dir, repo_path) = create_test_repo();
        let commit_oid = create_commit(&repo_path, "Add feature");
        create_tag(&repo_path, "v2.29.1", &commit_oid);

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
        create_tag(&repo_path, "latest", &commit_oid);
        create_tag(&repo_path, "release-1.0.0", &commit_oid);
        create_tag(&repo_path, "v1.0.0", &commit_oid);
        create_tag(&repo_path, "stable", &commit_oid);

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
        create_tag(&repo_path, "v2.0.0", &commit_oid);
        create_tag(&repo_path, "cargo-fslabscli-2.29.1", &commit_oid);

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

        create_tag(&repo_path, "latest", &commit_oid);
        create_tag(&repo_path, "stable", &commit_oid);

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
            err_msg.contains("No tag matching pattern"),
            "Error message should indicate failure, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_resolve_commit_sha_directly() {
        // Test: Should be able to resolve using a commit SHA directly
        let (_temp_dir, repo_path) = create_test_repo();
        let commit_oid = create_commit(&repo_path, "Feature commit");
        create_tag(&repo_path, "v3.0.0", &commit_oid);

        let result = resolve_commit_to_tag(&repo_path, &commit_oid, "v*");
        assert!(result.is_ok(), "Should resolve commit SHA to tag");
        assert_eq!(result.unwrap(), "v3.0.0");
    }

    #[test]
    fn test_resolve_commit_sha_short_form() {
        // Test: Should be able to resolve using a short commit SHA
        let (_temp_dir, repo_path) = create_test_repo();
        let commit_oid = create_commit(&repo_path, "Short SHA test");
        create_tag(&repo_path, "v4.0.0", &commit_oid);

        let short_sha = &commit_oid[..7]; // Use first 7 characters
        let result = resolve_commit_to_tag(&repo_path, short_sha, "v*");
        assert!(result.is_ok(), "Should resolve short commit SHA to tag");
        assert_eq!(result.unwrap(), "v4.0.0");
    }

    #[test]
    fn test_resolve_head_reference() {
        // Test: Should resolve HEAD reference to tag
        let (_temp_dir, repo_path) = create_test_repo();
        let commit_oid = create_commit(&repo_path, "HEAD commit");
        create_tag(&repo_path, "v5.0.0", &commit_oid);

        let result = resolve_commit_to_tag(&repo_path, "HEAD", "v*");
        assert!(result.is_ok(), "Should resolve HEAD to tag");
        assert_eq!(result.unwrap(), "v5.0.0");
    }

    #[test]
    fn test_resolve_branch_reference() {
        // Test: Should resolve a branch reference to tag
        let (_temp_dir, repo_path) = create_test_repo();

        // Create a commit and tag it
        let commit_oid = create_commit(&repo_path, "Branch commit");
        create_tag(&repo_path, "v6.0.0", &commit_oid);

        // Create a branch pointing to this commit
        let output = Command::new("git")
            .args(["branch", "feature-branch", &commit_oid])
            .current_dir(&repo_path)
            .output()
            .expect("Failed to create branch");
        assert!(output.status.success(), "git branch failed: {:?}", output);

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
        create_tag(&repo_path, "v1.0.0", &first_commit_oid);

        // Create second commit (HEAD) without tag
        create_commit(&repo_path, "Second commit");

        // Should still be able to resolve the first commit by its SHA
        let result = resolve_commit_to_tag(&repo_path, &first_commit_oid, "v*");
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
        create_tag(&repo_path, "v1.0.0", &commit_oid);
        create_tag(&repo_path, "v2.0.0", &commit_oid);
        create_tag(&repo_path, "v1.0.1", &commit_oid);

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
        create_tag(&repo_path, "v1.0.0", &first_commit_oid);

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

        let commit_oid = create_commit(&repo_path, "Annotated tag commit");

        // Create an annotated tag
        let output = Command::new("git")
            .env("GIT_COMMITTER_NAME", "Test User")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .args(["tag", "-a", "v7.0.0", "-m", "Release v7.0.0", &commit_oid])
            .current_dir(&repo_path)
            .output()
            .expect("Failed to create annotated tag");
        assert!(output.status.success(), "git tag failed: {:?}", output);

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
            err_msg.contains("Failed to run git describe"),
            "Error message should mention repository opening failure, got: {}",
            err_msg
        );
    }

    // Content-type detection tests
    #[test]
    fn test_content_type_detection_html() {
        use std::path::Path;
        let path = Path::new("index.html");
        let content_type = mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string();
        assert_eq!(content_type, "text/html");
    }

    #[test]
    fn test_content_type_detection_javascript() {
        use std::path::Path;
        let path = Path::new("script.js");
        let content_type = mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string();
        assert_eq!(content_type, "text/javascript");
    }

    #[test]
    fn test_content_type_detection_css() {
        use std::path::Path;
        let path = Path::new("styles.css");
        let content_type = mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string();
        assert_eq!(content_type, "text/css");
    }

    #[test]
    fn test_content_type_detection_json() {
        use std::path::Path;
        let path = Path::new("data.json");
        let content_type = mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string();
        assert_eq!(content_type, "application/json");
    }

    #[test]
    fn test_content_type_detection_png() {
        use std::path::Path;
        let path = Path::new("image.png");
        let content_type = mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string();
        assert_eq!(content_type, "image/png");
    }

    #[test]
    fn test_content_type_detection_jpeg() {
        use std::path::Path;
        let path = Path::new("photo.jpg");
        let content_type = mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string();
        assert_eq!(content_type, "image/jpeg");
    }

    #[test]
    fn test_content_type_detection_wasm() {
        use std::path::Path;
        let path = Path::new("module.wasm");
        let content_type = mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string();
        assert_eq!(content_type, "application/wasm");
    }

    #[test]
    fn test_content_type_detection_unknown_extension() {
        use std::path::Path;
        let path = Path::new("file.xyz123unknown");
        let content_type = mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string();
        assert_eq!(content_type, "application/octet-stream");
    }

    // --- Publish ordering filter tests ---
    //
    // The filter at lines 1540-1545 decides which dependency edges survive into
    // the publish DAG.  The rule:
    //
    //   KEEP  if ANY instance satisfies: kind != Development  OR  !is_local
    //   STRIP if ALL instances satisfy:  kind == Development  AND  is_local
    //
    // Rationale:
    //   • Path-only dev deps can be circular and do not need to be on the
    //     registry before the dependant is published.
    //   • Registry dev deps (is_local = false) MUST be on the registry first,
    //     so they must be kept in the ordering graph.
    //   • Normal deps (local or not) must always be ordered first.

    /// Helper: apply the same filter predicate used in the publish ordering.
    fn apply_publish_filter(
        deps: &[crate::crate_graph::Dependency],
    ) -> Vec<crate::crate_graph::Dependency> {
        deps.iter()
            .filter(|d| super::is_publish_ordered_dep(d))
            .cloned()
            .collect()
    }

    #[test]
    /// A registry dev-dep (is_local=false) must survive the publish filter so
    /// that it is published before its dependant.
    fn test_publish_filter_keeps_registry_dev_dep() {
        use crate::crate_graph::{Dependency, DependencyInstance};
        use cargo_metadata::{DependencyKind, PackageId};

        // Arrange: dev-dep that lives in a registry, not just by path.
        let dep = Dependency {
            package_id: PackageId {
                repr: "registry-dep 0.1.0".to_string(),
            },
            instances: vec![DependencyInstance {
                kind: DependencyKind::Development,
                is_local: false,
            }],
        };

        // Act
        let kept = apply_publish_filter(&[dep]);

        // Assert: the dependency is retained because it must be on the registry.
        assert_eq!(
            kept.len(),
            1,
            "Registry dev-dep must be kept; it must be published before the dependant"
        );
    }

    #[test]
    /// A path-only dev-dep (is_local=true) must be stripped from the publish
    /// ordering — these deps are path-only, never published, and can form cycles.
    fn test_publish_filter_strips_local_dev_dep() {
        use crate::crate_graph::{Dependency, DependencyInstance};
        use cargo_metadata::{DependencyKind, PackageId};

        // Arrange: dev-dep that is only referenced by local path.
        let dep = Dependency {
            package_id: PackageId {
                repr: "local-dev-dep 0.1.0".to_string(),
            },
            instances: vec![DependencyInstance {
                kind: DependencyKind::Development,
                is_local: true,
            }],
        };

        // Act
        let kept = apply_publish_filter(&[dep]);

        // Assert: stripped — it would not be on the registry anyway.
        assert!(
            kept.is_empty(),
            "Path-only dev-dep must be stripped from publish ordering"
        );
    }

    #[test]
    /// A dependency that appears as both Normal+local and Development+local must
    /// be kept: the Normal instance means the dep must be published first.
    fn test_publish_filter_keeps_mixed_dep() {
        use crate::crate_graph::{Dependency, DependencyInstance};
        use cargo_metadata::{DependencyKind, PackageId};

        // Arrange: same crate used as both a normal dep and a dev dep (both local).
        let dep = Dependency {
            package_id: PackageId {
                repr: "mixed-dep 0.1.0".to_string(),
            },
            instances: vec![
                DependencyInstance {
                    kind: DependencyKind::Normal,
                    is_local: true,
                },
                DependencyInstance {
                    kind: DependencyKind::Development,
                    is_local: true,
                },
            ],
        };

        // Act
        let kept = apply_publish_filter(&[dep]);

        // Assert: kept because the Normal instance requires publish ordering.
        assert_eq!(
            kept.len(),
            1,
            "Dep with a Normal instance must be kept regardless of dev-dep instances"
        );
    }

    #[test]
    /// A regular local path dependency (Normal+local) must always be kept —
    /// it must be published before anything that depends on it.
    fn test_publish_filter_keeps_normal_dep() {
        use crate::crate_graph::{Dependency, DependencyInstance};
        use cargo_metadata::{DependencyKind, PackageId};

        // Arrange: ordinary local path dependency.
        let dep = Dependency {
            package_id: PackageId {
                repr: "normal-dep 0.1.0".to_string(),
            },
            instances: vec![DependencyInstance {
                kind: DependencyKind::Normal,
                is_local: true,
            }],
        };

        // Act
        let kept = apply_publish_filter(&[dep]);

        // Assert: normal deps are never stripped.
        assert_eq!(
            kept.len(),
            1,
            "Normal dep must always survive the publish filter"
        );
    }

    // --- format_tag tests ---

    #[test]
    fn test_format_tag_default_template() {
        // Arrange
        let template = "{package_name}-{version}";

        // Act
        let result = super::format_tag(template, "cargo-fslabscli", "2.43.0");

        // Assert
        assert_eq!(result, "cargo-fslabscli-2.43.0");
    }

    #[test]
    fn test_format_tag_version_prefix() {
        // Arrange
        let template = "v{version}";

        // Act
        let result = super::format_tag(template, "cargo-fslabscli", "2.43.0");

        // Assert
        assert_eq!(result, "v2.43.0");
    }

    #[test]
    fn test_format_tag_custom_prefix_with_v() {
        // Arrange
        let template = "{package_name}-v{version}";

        // Act
        let result = super::format_tag(template, "cargo-fslabscli", "2.43.0");

        // Assert
        assert_eq!(result, "cargo-fslabscli-v2.43.0");
    }

    #[test]
    fn test_format_tag_no_placeholders() {
        // Arrange
        let template = "release-latest";

        // Act
        let result = super::format_tag(template, "cargo-fslabscli", "2.43.0");

        // Assert
        assert_eq!(result, "release-latest");
    }

    #[test]
    fn test_format_tag_empty_package_name() {
        // Arrange
        let template = "{package_name}-{version}";

        // Act
        let result = super::format_tag(template, "", "2.43.0");

        // Assert
        assert_eq!(result, "-2.43.0");
    }

    #[test]
    fn test_format_tag_empty_version() {
        // Arrange
        let template = "v{version}";

        // Act
        let result = super::format_tag(template, "cargo-fslabscli", "");

        // Assert
        assert_eq!(result, "v");
    }

    #[test]
    fn test_format_tag_version_placeholder_repeated() {
        // Arrange
        let template = "{version}-{version}";

        // Act
        let result = super::format_tag(template, "cargo-fslabscli", "2.43.0");

        // Assert
        assert_eq!(result, "2.43.0-2.43.0");
    }
}

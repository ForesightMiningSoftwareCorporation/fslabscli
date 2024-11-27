use chrono::{prelude::*, Duration};
use core::result::Result as CoreResult;
use ignore::WalkBuilder;
use std::cmp;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Instant;

use anyhow::Context;
use cargo_metadata::{MetadataCommand, Package};
use clap::Parser;
use console::{style, Emoji};
use futures_util::StreamExt;
use git2::{DiffDelta, DiffOptions, Repository};
use indexmap::IndexMap;
use indicatif::{HumanDuration, ProgressBar, ProgressStyle};
use object_store::{path::Path as BSPath, ObjectStore};
use rust_toolchain_file::toml::Parser as ToolchainParser;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::from_value;
use serde_yaml::Value;
use strum_macros::EnumString;

use crate::commands::check_workspace::binary::BinaryStore;
use crate::commands::check_workspace::docker::Docker;
use binary::PackageMetadataFslabsCiPublishBinary;
use cargo::{Cargo, PackageMetadataFslabsCiPublishCargo};
use docker::PackageMetadataFslabsCiPublishDocker;
use npm::{Npm, PackageMetadataFslabsCiPublishNpmNapi};

use crate::{utils, PrettyPrintable};

mod binary;
mod cargo;
mod docker;
mod npm;

static LOOKING_GLASS: Emoji<'_, '_> = Emoji("🔍  ", "");
static TRUCK: Emoji<'_, '_> = Emoji("🚚  ", "");
static PAPER: Emoji<'_, '_> = Emoji("📃  ", "");
static SPARKLE: Emoji<'_, '_> = Emoji("✨ ", ":-)");

const DEFAULT_TOOLCHAIN: &str = "1.76";
const CUSTOM_EPOCH: &str = "2024-01-01";

#[derive(Deserialize, Serialize, Clone, Default, Debug, EnumString, PartialEq)]
#[serde(rename_all = "camelCase")]
#[strum(serialize_all = "camelCase")]
pub enum ReleaseChannel {
    #[default]
    Nightly,
    Alpha,
    Beta,
    Prod,
}

// Custom serialization function
fn serialize_multiline_as_escaped<S>(
    value: &Option<String>,
    serializer: S,
) -> CoreResult<S::Ok, S::Error>
where
    S: Serializer,
{
    if let Some(ref v) = value {
        let escaped = v.replace('\n', "\\n");
        serializer.serialize_some(&escaped)
    } else {
        serializer.serialize_none()
    }
}

// Custom serialization function for `IndexMap<String, Value>`
fn serialize_indexmap_multiline_as_escaped<S>(
    map: &Option<IndexMap<String, Value>>,
    serializer: S,
) -> CoreResult<S::Ok, S::Error>
where
    S: Serializer,
{
    if let Some(map) = map {
        let escaped_map: IndexMap<_, _> = map
            .iter()
            .map(|(key, value)| {
                let escaped_value = match value {
                    Value::String(s) => Value::String(s.replace('\n', "\\n")),
                    _ => value.clone(),
                };
                (key.clone(), escaped_value)
            })
            .collect();

        serializer.serialize_some(&escaped_map)
    } else {
        serializer.serialize_none()
    }
}

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
    cargo_default_publish: bool,
    #[arg(long, env)]
    binary_store_storage_account: Option<String>,
    #[arg(long, env)]
    binary_store_container_name: Option<String>,
    #[arg(long, env)]
    binary_store_access_key: Option<String>,
    #[arg(long)]
    release_channel: Option<String>,
    #[arg(long)]
    toolchain: Option<String>,
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

    pub fn with_cargo_default_publish(mut self, cargo_default_publish: bool) -> Self {
        self.cargo_default_publish = cargo_default_publish;
        self
    }
}

#[derive(Deserialize, Serialize, Clone, Default, Debug)]
pub struct ResultDependency {
    pub package: Option<String>,
    pub path: Option<PathBuf>,
    pub version: String,
    #[serde(default)]
    pub publishable: bool,
    #[serde(default)]
    pub publishable_details: HashMap<String, bool>,
    pub guid_suffix: Option<String>,
}

#[derive(Serialize, Clone, Default, Debug)]
pub struct Result {
    pub workspace: String,
    pub package: String,
    pub version: String,
    pub path: PathBuf,
    pub publish_detail: PackageMetadataFslabsCiPublish,
    pub publish: bool,
    // #[serde(skip_serializing)]
    pub dependencies: Vec<ResultDependency>,
    // #[serde(skip_serializing)]
    pub dependant: Vec<ResultDependency>,
    pub changed: bool,
    pub dependencies_changed: bool,
    pub perform_test: bool,
    pub test_detail: PackageMetadataFslabsCiTest,
    pub toolchain: String,
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct PackageMetadataFslabsCiPublish {
    #[serde(default = "PackageMetadataFslabsCiPublishDocker::default")]
    pub docker: PackageMetadataFslabsCiPublishDocker,
    #[serde(default = "PackageMetadataFslabsCiPublishCargo::default")]
    pub cargo: PackageMetadataFslabsCiPublishCargo,
    #[serde(default = "PackageMetadataFslabsCiPublishNpmNapi::default")]
    pub npm_napi: PackageMetadataFslabsCiPublishNpmNapi,
    #[serde(default = "PackageMetadataFslabsCiPublishBinary::default")]
    pub binary: PackageMetadataFslabsCiPublishBinary,
    #[serde(default)]
    pub args: Option<IndexMap<String, Value>>, // This could be generate_workflow::PublishWorkflowArgs but keeping it like this, we can have new args without having to update fslabscli
    #[serde(default, serialize_with = "serialize_multiline_as_escaped")]
    pub additional_args: Option<String>,
    #[serde(default)]
    pub env: Option<IndexMap<String, String>>,
    #[serde(default = "ReleaseChannel::default")]
    pub release_channel: ReleaseChannel,
    #[serde(default)]
    pub ci_runner: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct PackageMetadataFslabsCiTest {
    #[serde(default, serialize_with = "serialize_indexmap_multiline_as_escaped")]
    pub args: Option<IndexMap<String, Value>>, // This could be generate_workflow::TestWorkflowArgs but keeping it like this, we can have new args without having to update fslabscli
    pub env: Option<IndexMap<String, String>>,
    pub skip: Option<bool>,
}

#[derive(Deserialize, Default, Debug, Clone)]
struct PackageMetadataFslabsCi {
    pub publish: Option<PackageMetadataFslabsCiPublish>,
    #[serde(default)]
    pub test: Option<PackageMetadataFslabsCiTest>,
}

#[derive(Deserialize, Default, Debug, Clone)]
struct PackageMetadata {
    pub fslabs: PackageMetadataFslabsCi,
}

fn get_toolchain(path: &Path) -> anyhow::Result<String> {
    Ok(
        ToolchainParser::new(&fs::read_to_string(path.join("rust-toolchain.toml"))?)
            .parse()?
            .toolchain()
            .spec()
            .ok_or_else(|| anyhow::anyhow!("no spec"))?
            .channel()
            .ok_or_else(|| anyhow::anyhow!("no channel"))?
            .name()
            .to_string(),
    )
}

fn get_blob_name(
    app_name: &str,
    app_version: &str,
    toolchain: &str,
    release_channel: &ReleaseChannel,
) -> (String, String) {
    (
        format!("{app_name}/{release_channel:?}").to_lowercase(),
        format!("{app_name}-x86_64-pc-windows-msvc-{toolchain}-v{app_version}-signed.exe")
            .to_lowercase(),
    )
}

impl Result {
    pub async fn new(
        workspace: String,
        package: Package,
        root_dir: PathBuf,
    ) -> anyhow::Result<Self> {
        let path = package
            .manifest_path
            .canonicalize()?
            .parent()
            .unwrap()
            .to_path_buf();

        let metadata: PackageMetadata =
            from_value(package.metadata.clone()).unwrap_or_else(|_| PackageMetadata::default());
        let mut publish = metadata.clone().fslabs.publish.unwrap_or_default();
        publish.cargo.registry = match package.publish.clone() {
            Some(r) => Some(r.clone()),
            None => {
                // Should be public registry, double check this is wanted
                if publish.cargo.allow_public {
                    Some(vec!["public".to_string()])
                } else {
                    Some(vec![])
                }
            }
        };

        publish.cargo.publish = publish
            .cargo
            .registry
            .clone()
            .map(|r| r.len() == 1)
            .unwrap_or(false);

        let dependencies: Vec<ResultDependency> = package
            .dependencies
            .into_iter()
            // Somehow, we now need to put the devedependencies in the tree
            // .filter(|p| p.kind == DependencyKind::Normal)
            .map(|d| ResultDependency {
                package: Some(d.name),
                path: d.path.map(|p| p.into()),
                version: d.req.to_string(),
                publishable: false,
                publishable_details: HashMap::new(),
                guid_suffix: None,
            })
            // Add subapps
            .chain(
                publish
                    .binary
                    .installer
                    .sub_apps
                    .clone()
                    .into_iter()
                    .map(|(k, mut v)| {
                        if v.package.is_none() {
                            v.package = Some(k);
                        }
                        v
                    })
                    .collect::<Vec<ResultDependency>>(),
            )
            .collect();
        let mut path = path.strip_prefix(&root_dir)?.to_path_buf();
        if path.to_string_lossy().is_empty() {
            path = PathBuf::from(".");
        }

        // Deduct version based on if it's nightly or not
        //
        Ok(Self {
            workspace,
            package: package.name,
            version: package.version.to_string(),
            path,
            publish_detail: publish,
            test_detail: metadata.fslabs.test.unwrap_or_default(),
            dependencies,
            ..Default::default()
        })
    }

    pub fn update_release_channel(&mut self, release_channel: Option<&str>) {
        // Deduct release channel
        let release_channel: ReleaseChannel = match release_channel {
            Some(r) => ReleaseChannel::from_str(r).unwrap_or_default(),
            None => {
                // Parse from the environment
                std::env::var("GITHUB_REF")
                    .map(|r| {
                        // Regarding installer and launcher, we need to check the tag of their counterpart
                        let mut check_key = self.package.clone();
                        if check_key.ends_with("_launcher") {
                            check_key = check_key.replace("_launcher", "");
                        }
                        if check_key.ends_with("_installer") {
                            check_key = check_key.replace("_installer", "");
                        }
                        if r.starts_with(&format!("refs/tags/{}-alpha", check_key)) {
                            ReleaseChannel::Alpha
                        } else if r.starts_with(&format!("refs/tags/{}-beta", check_key)) {
                            ReleaseChannel::Beta
                        } else if r.starts_with(&format!("refs/tags/{}-prod", check_key)) {
                            ReleaseChannel::Prod
                        } else {
                            ReleaseChannel::Nightly
                        }
                    })
                    .unwrap_or_default()
            }
        };
        self.publish_detail.release_channel = release_channel.clone();
    }

    pub fn update_ci_runner(&mut self, toolchain: &str) {
        self.publish_detail.ci_runner =
            Some(format!("rust-{}-scale-set", toolchain.replace('.', "-")));
    }

    pub fn update_toolchain(&mut self, toolchain: &str) {
        self.toolchain = toolchain.to_string();
    }

    pub async fn update_runtime_information(
        &mut self,
        release_channel: Option<&str>,
        toolchain: &str,
        version_timestamp: &str,
        package_versions: &HashMap<String, String>,
        object_store: &Option<BinaryStore>,
    ) -> anyhow::Result<()> {
        self.update_release_channel(release_channel);
        self.update_ci_runner(toolchain);
        self.update_toolchain(toolchain);

        if self.publish_detail.binary.publish {
            let rc_version = match self.publish_detail.release_channel {
                ReleaseChannel::Nightly => {
                    // Nightly version should be current date
                    if self.package.ends_with("_launcher") {
                        self.version.to_string()
                    } else {
                        format!("{}.{}", self.version, version_timestamp)
                    }
                }
                _ => self.version.to_string(),
            };
            self.publish_detail.binary.rc_version = Some(rc_version.clone());
            self.publish_detail.binary.name = match self.publish_detail.release_channel {
                ReleaseChannel::Nightly => format!("{} Nightly", self.publish_detail.binary.name),
                ReleaseChannel::Alpha => format!("{} Alpha", self.publish_detail.binary.name),
                ReleaseChannel::Beta => format!("{} Beta", self.publish_detail.binary.name),
                ReleaseChannel::Prod => self.publish_detail.binary.name.clone(),
            };
            self.publish_detail.binary.fallback_name =
                Some(self.publish_detail.binary.name.replace(' ', "_"));

            // Compute blob names
            let (package_blob_dir, package_blob_name) = get_blob_name(
                &self.package,
                &rc_version,
                toolchain,
                &self.publish_detail.release_channel,
            );

            self.publish_detail.binary.blob_dir = Some(package_blob_dir);
            self.publish_detail.binary.blob_name = Some(package_blob_name);
            if self.publish_detail.binary.installer.publish {
                // Expiry
                let now = Utc::now();
                let future = now + Duration::hours(24);
                self.publish_detail.binary.installer.sas_expiry =
                    Some(format!("{}", future.format("%FT%TZ")));
                //  Get Guid Prefix and Upgrade code
                let (upgrade_code, guid_prefix) = match self.publish_detail.release_channel {
                    ReleaseChannel::Nightly => (
                        self.publish_detail
                            .binary
                            .installer
                            .nightly
                            .upgrade_code
                            .clone(),
                        self.publish_detail
                            .binary
                            .installer
                            .nightly
                            .guid_prefix
                            .clone(),
                    ),
                    ReleaseChannel::Alpha => (
                        self.publish_detail
                            .binary
                            .installer
                            .alpha
                            .upgrade_code
                            .clone(),
                        self.publish_detail
                            .binary
                            .installer
                            .alpha
                            .guid_prefix
                            .clone(),
                    ),
                    ReleaseChannel::Beta => (
                        self.publish_detail
                            .binary
                            .installer
                            .beta
                            .upgrade_code
                            .clone(),
                        self.publish_detail
                            .binary
                            .installer
                            .beta
                            .guid_prefix
                            .clone(),
                    ),
                    ReleaseChannel::Prod => (
                        self.publish_detail
                            .binary
                            .installer
                            .prod
                            .upgrade_code
                            .clone(),
                        self.publish_detail
                            .binary
                            .installer
                            .prod
                            .guid_prefix
                            .clone(),
                    ),
                };

                self.publish_detail.binary.installer.upgrade_code = upgrade_code;
                self.publish_detail.binary.installer.guid_prefix = guid_prefix;

                let (installer_blob_dir, _) = get_blob_name(
                    format!("{}_installer", self.package).as_ref(),
                    &rc_version,
                    toolchain,
                    &self.publish_detail.release_channel,
                );
                self.publish_detail.binary.installer.installer_blob_dir = Some(installer_blob_dir);
                let launcher_name = format!(
                    "{}{}{}",
                    self.publish_detail.binary.launcher.prefix,
                    self.package,
                    self.publish_detail.binary.launcher.suffix
                );
                if let Some(launcher_version) = package_versions.get(&launcher_name) {
                    let (launcher_blob_dir, launcher_name) = get_blob_name(
                        &launcher_name,
                        launcher_version,
                        toolchain,
                        &self.publish_detail.release_channel,
                    );
                    self.publish_detail.binary.installer.launcher_blob_dir =
                        Some(launcher_blob_dir);
                    self.publish_detail.binary.installer.launcher_blob_name = Some(launcher_name);
                    if let Some(ref fallback_name) = self.publish_detail.binary.fallback_name {
                        self.publish_detail.binary.installer.installer_blob_name = Some(
                            format!("{}.{}.{}.msi", fallback_name, launcher_version, rc_version)
                                .to_lowercase(),
                        );
                        self.publish_detail
                            .binary
                            .installer
                            .installer_blob_signed_name = Some(
                            format!(
                                "{}.{}.{}-signed.msi",
                                fallback_name, launcher_version, rc_version
                            )
                            .to_lowercase(),
                        );
                    }
                }
                // subapps
                let mut lines: Vec<String> = vec![];
                for (s, v) in &self.publish_detail.binary.installer.sub_apps {
                    let sub_app_full_blob_name: String;
                    if self.publish_detail.release_channel == ReleaseChannel::Nightly {
                        //need to add the nightly epoch to the suffix
                        if let Some(subapp_version) = package_versions.get(s) {
                            let (subapp_dir, subapp_name) = get_blob_name(
                                s,
                                &format!("{}.{}", subapp_version, version_timestamp),
                                toolchain,
                                &self.publish_detail.release_channel,
                            );
                            sub_app_full_blob_name = format!("{}/{}", subapp_dir, subapp_name);
                        } else {
                            continue;
                        }
                    } else if let Some(store) = object_store {
                        let suffix = "-signed.exe".to_string();
                        let (sub_app_dir, sub_app_name) = get_blob_name(
                            s,
                            &v.version,
                            toolchain,
                            &self.publish_detail.release_channel,
                        );
                        // we need to remove the `-signed.exe` suffix, we don't need it here
                        let sub_app_name = sub_app_name
                            .strip_suffix(&"-signed.exe")
                            .unwrap_or_else(|| &sub_app_name);
                        let mut list_stream = store
                            .get_client()
                            .list(Some(&BSPath::from(sub_app_dir.to_string())));
                        // Print a line about each object
                        let mut candidates = vec![];
                        while let Some(meta) = list_stream.next().await.transpose().unwrap() {
                            let filename = format!("{}", meta.location);
                            if filename.starts_with(&format!("{}/{}", sub_app_dir, sub_app_name))
                                && filename.ends_with(&suffix)
                            {
                                candidates.push(filename);
                            }
                        }
                        candidates.sort_by_key(|c| {
                            // We should do a semver check and whatever, but regex will probably do
                            c.replace(&sub_app_dir, "").replace("-signed.exe", "")
                        });
                        candidates.reverse();
                        if let Some(c) = candidates.first() {
                            sub_app_full_blob_name = c.to_string();
                        } else {
                            continue;
                        }
                    } else {
                        continue;
                    }
                    lines.push(format!("az storage blob download --container-name orica-cont-prod-update-001 --name {} --file target/x86_64-pc-windows-msvc/release/{}.exe", sub_app_full_blob_name, s));
                }
                self.publish_detail
                    .binary
                    .installer
                    .sub_apps_download_script = Some(lines.join("\n"));
            }
        }
        Ok(())
    }

    pub async fn check_publishable(
        &mut self,
        npm: &Npm,
        cargo: &Cargo,
        docker: &mut Docker,
        binary_store: &Option<BinaryStore>,
    ) -> anyhow::Result<()> {
        match self
            .publish_detail
            .docker
            .check(self.package.clone(), self.version.clone(), docker)
            .await
        {
            Ok(_) => {}
            Err(e) => self.publish_detail.docker.error = Some(e.to_string()),
        };
        match self
            .publish_detail
            .npm_napi
            .check(self.package.clone(), self.version.clone(), npm)
            .await
        {
            Ok(_) => {}
            Err(e) => self.publish_detail.npm_napi.error = Some(e.to_string()),
        };
        match self
            .publish_detail
            .cargo
            .check(self.package.clone(), self.version.clone(), cargo)
            .await
        {
            Ok(_) => {}
            Err(e) => self.publish_detail.cargo.error = Some(e.to_string()),
        };
        match self.publish_detail.binary.check(binary_store).await {
            Ok(_) => {}
            Err(e) => {
                self.publish_detail.binary.error = Some(e.to_string());
            }
        };

        Ok(())
    }
}

impl Display for Result {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} -- {} -- {}: docker: {}, cargo: {}, npm_napi: {}, binary: {}, publish: {}",
            self.workspace,
            self.package,
            self.version,
            self.publish_detail.docker.publish,
            self.publish_detail.cargo.publish,
            self.publish_detail.npm_napi.publish,
            self.publish_detail.binary.publish,
            self.publish
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

fn bool_to_emoji(value: bool) -> &'static str {
    if value {
        "x"
    } else {
        ""
    }
}
impl PrettyPrintable for Results {
    fn pretty_print(&self) -> String {
        let mut results: Vec<&Result> = self.0.values().collect();
        results.sort_by(|a, b| {
            // Compare primary keys first
            match a.workspace.cmp(&b.workspace) {
                // If primary keys are equal, compare backup keys
                Ordering::Equal => a.package.cmp(&b.package),
                // Otherwise, return the ordering of the primary keys
                other => other,
            }
        });
        // We need to calculate pad ots for `workspace` `package` `version`
        let workspace_len = results.iter().map(|v| v.workspace.len()).max().unwrap_or(0);
        let package_len = results.iter().map(|v| v.package.len()).max().unwrap_or(0);
        let version_len = cmp::max(
            results.iter().map(|v| v.version.len()).max().unwrap_or(0),
            7,
        );
        let out: Vec<String> = vec![
            format!("|-{:-^workspace_len$}-|-{:-^package_len$}-|-{:-^version_len$}-|-{:-^35}-|-{:-^5}-|", "-", "-", "-", "-", "-"),
            format!("| {:^workspace_len$} | {:^package_len$} | {:^version_len$} | {:^35} | {:^5} |", "Workspace", "Package", "Version", "Publish", "Tests"),
            format!("| {:workspace_len$} | {:package_len$} | {:version_len$} | docker | cargo | npm | binary | any | {:^5} |", "", "", "", ""),
            format!("|-{:-^workspace_len$}-|-{:-^package_len$}-|-{:-^version_len$}-|-{:-^35}-|-{:-^5}-|", "-", "-", "-", "-", "-")];
        [out,
         results.iter()
            .map(|v| {
                format!(
                    "| {:workspace_len$} | {:package_len$} | {:version_len$} | {:^6} | {:^5} | {:^3} | {:^6} | {:^3} | {:^5} | ",
                    v.workspace, v.package, v.version,
                    bool_to_emoji(v.publish_detail.docker.publish),
                    bool_to_emoji(v.publish_detail.cargo.publish),
                    bool_to_emoji(v.publish_detail.npm_napi.publish),
                    bool_to_emoji(v.publish_detail.binary.publish),
                    bool_to_emoji(v.publish),
                    bool_to_emoji(v.perform_test )
                )
            })
            .collect::<Vec<String>>()].concat().join("\n")
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

    let toolchain =
        get_toolchain(&working_directory).unwrap_or_else(|_| DEFAULT_TOOLCHAIN.to_string());
    let now = Utc::now().date_naive();
    let epoch = NaiveDate::parse_from_str(CUSTOM_EPOCH, "%Y-%m-%d").unwrap(); // I'm confident about this
    let timestamp = format!("{}", (now - epoch).num_days());

    let binary_store = BinaryStore::new(
        options.binary_store_storage_account,
        options.binary_store_container_name,
        options.binary_store_access_key,
    )?;
    log::debug!("Base directory: {:?}", path);
    // 1. Find all workspaces to investigate
    if options.progress {
        println!(
            "{} {}Resolving workspaces...",
            style("[1/9]").bold().dim(),
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
            style("[2/9]").bold().dim(),
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
                )
                .await
                {
                    Ok(package) => {
                        packages.insert(package.package.clone(), package);
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

    let package_keys: Vec<String> = packages.keys().cloned().collect();

    // 5. Compute Runtime information
    if options.progress {
        println!(
            "{} {}Compute runtime information...",
            style("[5/9]").bold().dim(),
            TRUCK
        );
    }

    let mut pb: Option<ProgressBar> = None;
    if options.progress {
        pb = Some(ProgressBar::new(packages.len() as u64).with_style(
            ProgressStyle::with_template("{spinner} {wide_msg} {pos}/{len}")?,
        ));
    }
    let mut package_versions: HashMap<String, String> = HashMap::new();
    package_versions.extend(packages.iter().map(|(k, v)| (k.clone(), v.version.clone())));
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
                .update_runtime_information(
                    options.release_channel.as_deref(),
                    &toolchain,
                    &timestamp,
                    &package_versions,
                    &binary_store,
                )
                .await?;
        }
    }

    // Check Release status
    if options.progress {
        println!(
            "{} {}Checking published status...",
            style("[6/9]").bold().dim(),
            PAPER
        );
    }
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
    let mut docker = Docker::new(None)?;
    if let (Some(docker_registry), Some(docker_username), Some(docker_password)) = (
        options.docker_registry.clone(),
        options.docker_registry_username.clone(),
        options.docker_registry_password.clone(),
    ) {
        docker.add_registry_auth(docker_registry, docker_username, docker_password)
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
                match package
                    .check_publishable(&npm, &cargo, &mut docker, &binary_store)
                    .await
                {
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
                package.publish_detail.binary.publish,
            ]
            .into_iter()
            .any(|x| x);

            // If we are in a tag, we are only looking for the packages that build a launcher or installer. Otherwise, we are looking at all the packages
            let package_key = package.package.clone();
            if package.publish {
                if let Ok(env_string) = std::env::var("GITHUB_REF") {
                    // Regarding installer and launcher, we need to check the tag of their counterpart
                    if env_string.starts_with("refs/tags") {
                        let mut check_key = package_key.clone();
                        if package_key.ends_with("_launcher") {
                            check_key = check_key.replace("_launcher", "");
                        }
                        if package_key.ends_with("_installer") {
                            check_key = check_key.replace("_installer", "");
                        }
                        if !env_string.starts_with(&format!("refs/tags/{}", check_key)) {
                            package.publish = false;
                        }
                    }
                }
            }
        }
    }

    if options.progress {
        println!(
            "{} {}Resolving packages dependencies...",
            style("[3/9]").bold().dim(),
            TRUCK
        );
    }
    let mut pb: Option<ProgressBar> = None;
    if options.progress {
        pb = Some(ProgressBar::new(packages.len() as u64).with_style(
            ProgressStyle::with_template("{spinner} {wide_msg} {pos}/{len}")?,
        ));
    }
    let publish_status: HashMap<String, bool> = packages
        .clone()
        .into_iter()
        .map(|(k, v)| (k, v.publish))
        .collect();
    for package_key in package_keys.clone() {
        if let Some(ref pb) = pb {
            pb.inc(1);
        }
        // Loop through all the dependencies, if we don't know of it, skip it
        if let Some(package) = packages.get_mut(&package_key) {
            if let Some(ref pb) = pb {
                pb.set_message(format!("{} : {}", package.workspace, package.package));
            }
            package.dependencies.retain(|d| {
                d.package
                    .as_ref()
                    .map_or(false, |p| package_keys.contains(p))
            });
            for dep in &mut package.dependencies {
                if let Some(package_name) = &dep.package {
                    if let Some(dep_p) = publish_status.get(package_name) {
                        dep.publishable = *dep_p;
                    }
                }
            }
        }
    }
    // 4 Feed Dependent
    if options.progress {
        println!(
            "{} {}Feeding packages dependant...",
            style("[7/9]").bold().dim(),
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
                if let Some(package_name) = dependency.package {
                    if let Some(dependant) = packages.get_mut(&package_name) {
                        dependant.dependant.push(ResultDependency {
                            package: Some(package.package.clone()),
                            version: package.version.clone(),
                            path: Some(package.path.clone()),
                            publishable: package.publish,
                            publishable_details: HashMap::from([
                                ("docker".to_string(), package.publish_detail.docker.publish),
                                ("cargo".to_string(), package.publish_detail.cargo.publish),
                                (
                                    "npm_napi".to_string(),
                                    package.publish_detail.npm_napi.publish,
                                ),
                                ("binary".to_string(), package.publish_detail.binary.publish),
                            ]),
                            guid_suffix: None,
                        });
                    }
                }
            }
        }
    }

    if options.progress {
        println!(
            "{} {}Checking if packages changed...",
            style("[8/9]").bold().dim(),
            TRUCK
        );
    }
    if options.check_changed {
        // Look for a .fslabscliignore file
        let walker = WalkBuilder::new(working_directory.clone())
            .add_custom_ignore_filename(".fslabscliignore")
            .build();

        let non_ignored_paths: Vec<PathBuf> = walker
            .filter_map(|t| t.ok())
            .map(|e| e.into_path())
            .collect();
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
                if options.check_publish && package.publish {
                    // mark package as changed
                    package.changed = true;
                    continue;
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
                let check_path = |path: Option<&Path>| -> bool {
                    match path {
                        Some(p) => {
                            if package_folder.is_empty() || p.starts_with(&package_folder) {
                                let fp = working_directory.join(p);
                                return non_ignored_paths.iter().any(|r| r == &fp);
                            }
                            false
                        }
                        None => false,
                    }
                };
                let mut file_cb = |delta: DiffDelta, _: f32| -> bool {
                    let check_old_file = check_path(delta.old_file().path());
                    let check_new_file = check_path(delta.new_file().path());
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
            style("[9/9]").bold().dim(),
            TRUCK
        );
    }
    if options.check_changed {
        if options.progress {
            pb = Some(ProgressBar::new(packages.len() as u64).with_style(
                ProgressStyle::with_template("{spinner} {wide_msg} {pos}/{len}")?,
            ));
        }

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
                    .filter_map(|p| p.package.clone())
                    .collect();
                mark_dependants_as_changed(&mut packages, &dependant);
            }
        }
    }
    for package_key in package_keys.clone() {
        if let Some(package) = packages.get_mut(&package_key) {
            if package.changed || package.dependencies_changed || package.publish {
                package.perform_test = true;
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
                .filter_map(|p| p.package.clone())
                .collect();
            mark_dependants_as_changed(all_packages, &dependant);
        }
    }
}

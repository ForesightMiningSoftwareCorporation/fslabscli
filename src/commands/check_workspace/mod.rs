use chrono::{Duration, prelude::*};
use core::result::Result as CoreResult;
use std::cmp;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Instant;

use anyhow::Context;
use cargo_metadata::{DependencyKind, Package, PackageId};
use clap::Parser;
use console::{Emoji, style};
use futures_util::StreamExt;
use indexmap::IndexMap;
use indicatif::{HumanDuration, ProgressBar, ProgressStyle};
use object_store::{ObjectStore, path::Path as BSPath};
use rust_toolchain_file::toml::Parser as ToolchainParser;
use serde::ser::{Serialize as SerSerialize, SerializeStruct, Serializer as SerSerializer};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::from_value;
use serde_yaml::Value;
use strum_macros::EnumString;

use crate::commands::check_workspace::binary::BinaryStore;
use crate::commands::check_workspace::docker::Docker;
use crate::crate_graph::CrateGraph;
use crate::utils::cargo::Cargo;
use binary::PackageMetadataFslabsCiPublishBinary;
use cargo::PackageMetadataFslabsCiPublishCargo;
use docker::PackageMetadataFslabsCiPublishDocker;
use nix_binary::PackageMetadataFslabsCiPublishNixBinary;
use npm::{Npm, PackageMetadataFslabsCiPublishNpmNapi};

use crate::{PackageRelatedOptions, PrettyPrintable};

mod binary;
mod cargo;
mod docker;
mod nix_binary;
mod npm;

static LOOKING_GLASS: Emoji<'_, '_> = Emoji("🔍  ", "");
static TRUCK: Emoji<'_, '_> = Emoji("🚚  ", "");
static PAPER: Emoji<'_, '_> = Emoji("📃  ", "");
static SPARKLE: Emoji<'_, '_> = Emoji("✨ ", ":-)");

const DEFAULT_TOOLCHAIN: &str = "1.88";
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
    if let Some(v) = value {
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
    #[arg(long, default_value_t = false)]
    skip_docker: bool,
    #[arg(long)]
    docker_registry: Option<String>,
    #[arg(long)]
    docker_registry_username: Option<String>,
    #[arg(long)]
    docker_registry_password: Option<String>,
    #[arg(long, default_value_t = false)]
    skip_npm: bool,
    #[arg(long)]
    npm_registry_url: Option<String>,
    #[arg(long)]
    npm_registry_token: Option<String>,
    #[arg(long)]
    npm_registry_npmrc_path: Option<String>,
    #[arg(long, default_value_t = false)]
    skip_cargo: bool,
    #[arg(long, default_value_t = false)]
    autopublish_cargo: bool,
    #[arg(long, default_value_t = false)]
    skip_binary: bool,
    #[arg(long, env)]
    binary_store_storage_account: Option<String>,
    #[arg(long, env)]
    binary_store_container_name: Option<String>,
    #[arg(long, env)]
    binary_store_access_key: Option<String>,
    #[arg(long, env)]
    release_channel: Option<String>,
    #[arg(long)]
    toolchain: Option<String>,
    #[arg(long, default_value_t = false)]
    pub(crate) check_publish: bool,
    #[arg(long, default_value_t = false)]
    pub(crate) check_changed: bool,
    #[arg(long, default_value_t = false)]
    fail_unit_error: bool,
    #[arg(long, default_value_t = false)]
    hide_dependencies: bool,
    #[arg(long, default_value_t = false)]
    ignore_dev_dependencies: bool,
}

impl Options {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_check_changed(mut self, check_changed: bool) -> Self {
        self.check_changed = check_changed;
        self
    }

    pub fn with_check_publish(mut self, check_publish: bool) -> Self {
        self.check_publish = check_publish;
        self
    }

    pub fn with_ignore_dev_dependencies(mut self, ignore_dev_dependencies: bool) -> Self {
        self.ignore_dev_dependencies = ignore_dev_dependencies;
        self
    }
}

fn default_dependency_kind() -> DependencyKind {
    DependencyKind::Normal
}
#[derive(Deserialize, Serialize, Clone, Default, Debug)]
pub struct ResultDependency {
    pub package: Option<String>,
    pub package_id: Option<PackageId>,
    pub rename: Option<String>,
    pub path: Option<PathBuf>,
    #[serde(default = "default_dependency_kind")]
    pub kind: DependencyKind,
    pub version: String,
    #[serde(default)]
    pub publishable: bool,
    #[serde(default)]
    pub publishable_details: HashMap<String, bool>,
    pub guid_suffix: Option<String>,
}

#[derive(Clone, Default, Debug)]
pub struct Result {
    pub workspace: String,
    pub package: String,
    pub package_id: Option<PackageId>,
    pub version: String,
    pub path: PathBuf,
    pub publish_detail: PackageMetadataFslabsCiPublish,
    pub publish: bool,
    hide_dependencies: bool,
    pub dependencies: Vec<ResultDependency>,
    pub changed: bool,
    pub dependencies_changed: bool,
    pub perform_test: bool,
    pub test_detail: PackageMetadataFslabsCiTest,
    pub toolchain: String,
}

impl SerSerialize for Result {
    fn serialize<S>(&self, serializer: S) -> CoreResult<S::Ok, S::Error>
    where
        S: SerSerializer,
    {
        let mut fields = 12;
        if !(self.hide_dependencies) {
            fields += 1;
        }
        let mut state = serializer.serialize_struct("Result", fields)?;
        state.serialize_field("workspace", &self.workspace)?;
        state.serialize_field("package", &self.package)?;
        state.serialize_field("version", &self.version)?;
        state.serialize_field("path", &self.path)?;
        state.serialize_field("publish_detail", &self.publish_detail)?;
        state.serialize_field("publish", &self.publish)?;
        if !(self.hide_dependencies) {
            state.serialize_field("dependencies", &self.dependencies)?;
        }
        state.serialize_field("changed", &self.changed)?;
        state.serialize_field("dependencies_changed", &self.dependencies_changed)?;
        state.serialize_field("perform_test", &self.perform_test)?;
        state.serialize_field("test_detail", &self.test_detail)?;
        state.serialize_field("toolchain", &self.toolchain)?;
        state.end()
    }
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
    #[serde(default = "PackageMetadataFslabsCiPublishNixBinary::default")]
    pub nix_binary: PackageMetadataFslabsCiPublishNixBinary,
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
    pub fn new(
        workspace: String,
        package: Package,
        root_dir: PathBuf,
        hide_dependencies: bool,
        dep_to_id: &HashMap<String, PackageId>,
    ) -> anyhow::Result<Self> {
        let path = dunce::canonicalize(package.manifest_path)?
            .parent()
            .unwrap()
            .to_path_buf();

        let metadata: PackageMetadata =
            from_value(package.metadata.clone()).unwrap_or_else(|_| PackageMetadata::default());
        let mut publish = metadata.clone().fslabs.publish.unwrap_or_default();
        let mut registries = publish.cargo.registries.unwrap_or_default();
        for r in package.publish.unwrap_or_default() {
            registries.insert(r.clone());
        }
        if publish.cargo.allow_public {
            registries.insert("crates.io".to_string());
        }
        publish.cargo.registries = Some(registries);

        publish.cargo.publish = publish.cargo.publish
            && publish
                .cargo
                .registries
                .clone()
                .map(|r| !r.is_empty())
                .unwrap_or(false);

        let dependencies: Vec<ResultDependency> = package
            .dependencies
            .into_iter()
            // Somehow, we now need to put the devedependencies in the tree
            // .filter(|p| p.kind == DependencyKind::Normal)
            .map(|d| ResultDependency {
                package: Some(d.name.clone()),
                package_id: dep_to_id.get(&d.name).cloned(),
                rename: d.rename,
                kind: d.kind,
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
            package_id: Some(package.id.clone()),
            package: package.name.to_string(),
            version: package.version.to_string(),
            path,
            publish_detail: publish,
            test_detail: metadata.fslabs.test.unwrap_or_default(),
            hide_dependencies,
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
                        if r.starts_with(&format!("refs/tags/{check_key}-alpha")) {
                            ReleaseChannel::Alpha
                        } else if r.starts_with(&format!("refs/tags/{check_key}-beta")) {
                            ReleaseChannel::Beta
                        } else if r.starts_with(&format!("refs/tags/{check_key}-prod")) {
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
        package_versions: &HashMap<PackageId, String>,
        object_store: &Option<BinaryStore>,
        dep_to_id: &HashMap<String, PackageId>,
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
                if let Some(launcher_package_id) = dep_to_id.get(&launcher_name) {
                    if let Some(launcher_version) = package_versions.get(launcher_package_id) {
                        let (launcher_blob_dir, launcher_name) = get_blob_name(
                            &launcher_name,
                            launcher_version,
                            toolchain,
                            &self.publish_detail.release_channel,
                        );
                        self.publish_detail.binary.installer.launcher_blob_dir =
                            Some(launcher_blob_dir);
                        self.publish_detail.binary.installer.launcher_blob_name =
                            Some(launcher_name);
                        if let Some(ref fallback_name) = self.publish_detail.binary.fallback_name {
                            self.publish_detail.binary.installer.installer_blob_name = Some(
                                format!("{fallback_name}.{launcher_version}.{rc_version}.msi")
                                    .to_lowercase(),
                            );
                            self.publish_detail
                                .binary
                                .installer
                                .installer_blob_signed_name = Some(
                                format!(
                                    "{fallback_name}.{launcher_version}.{rc_version}-signed.msi"
                                )
                                .to_lowercase(),
                            );
                        }
                    }
                }
                // subapps
                let mut lines: Vec<String> = vec![];
                for (s, v) in &self.publish_detail.binary.installer.sub_apps {
                    let sub_app_full_blob_name: String;
                    if self.publish_detail.release_channel == ReleaseChannel::Nightly {
                        //need to add the nightly epoch to the suffix
                        if let Some(subapp_package_id) = dep_to_id.get(s) {
                            if let Some(subapp_version) = package_versions.get(subapp_package_id) {
                                let (subapp_dir, subapp_name) = get_blob_name(
                                    s,
                                    &format!("{subapp_version}.{version_timestamp}"),
                                    toolchain,
                                    &self.publish_detail.release_channel,
                                );
                                sub_app_full_blob_name = format!("{subapp_dir}/{subapp_name}");
                            } else {
                                continue;
                            }
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
                            if filename.starts_with(&format!("{sub_app_dir}/{sub_app_name}"))
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
                    lines.push(format!("az storage blob download --container-name orica-cont-prod-update-001 --name {sub_app_full_blob_name} --file target/x86_64-pc-windows-msvc/release/{s}.exe"));
                }
                self.publish_detail
                    .binary
                    .installer
                    .sub_apps_download_script = Some(lines.join("\n"));
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn check_publishable(
        &mut self,
        skip_npm: bool,
        npm: &Npm,
        skip_cargo: bool,
        cargo: &Cargo,
        force_cargo: bool,
        skip_docker: bool,
        docker: &mut Docker,
        skip_binary: bool,
        binary_store: &Option<BinaryStore>,
    ) -> anyhow::Result<()> {
        if !skip_docker {
            match self
                .publish_detail
                .docker
                .check(self.package.clone(), self.version.clone(), docker)
                .await
            {
                Ok(_) => {}
                Err(e) => self.publish_detail.docker.error = Some(e.to_string()),
            };
        }
        if !skip_npm {
            match self
                .publish_detail
                .npm_napi
                .check(self.package.clone(), self.version.clone(), npm)
                .await
            {
                Ok(_) => {}
                Err(e) => self.publish_detail.npm_napi.error = Some(e.to_string()),
            };
        }
        if !skip_cargo {
            match self
                .publish_detail
                .cargo
                .check(
                    self.package.clone(),
                    self.version.clone(),
                    cargo,
                    force_cargo,
                )
                .await
            {
                Ok(_) => {}
                Err(e) => self.publish_detail.cargo.error = Some(e.to_string()),
            };
        }
        if !skip_binary {
            match self.publish_detail.binary.check(binary_store).await {
                Ok(_) => {}
                Err(e) => {
                    self.publish_detail.binary.error = Some(e.to_string());
                }
            };
            match self.publish_detail.nix_binary.check().await {
                Ok(_) => {}
                Err(e) => {
                    self.publish_detail.nix_binary.error = Some(e.to_string());
                }
            };
        }

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
pub struct Results {
    pub members: HashMap<PackageId, Result>,
    #[serde(skip)]
    pub crate_graph: CrateGraph,
}

impl Display for Results {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        for (k, v) in &self.members {
            writeln!(f, "{k}: {v}")?;
        }
        Ok(())
    }
}

fn bool_to_emoji(value: bool) -> &'static str {
    if value { "x" } else { "" }
}
impl PrettyPrintable for Results {
    fn pretty_print(&self) -> String {
        let mut results: Vec<&Result> = self.members.values().collect();
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
            format!(
                "|-{:-^workspace_len$}-|-{:-^package_len$}-|-{:-^version_len$}-|-{:-^35}-|-{:-^5}-|",
                "-", "-", "-", "-", "-"
            ),
            format!(
                "| {:^workspace_len$} | {:^package_len$} | {:^version_len$} | {:^35} | {:^5} |",
                "Workspace", "Package", "Version", "Publish", "Tests"
            ),
            format!(
                "| {:workspace_len$} | {:package_len$} | {:version_len$} | docker | cargo | npm | binary | any | {:^5} |",
                "", "", "", ""
            ),
            format!(
                "|-{:-^workspace_len$}-|-{:-^package_len$}-|-{:-^version_len$}-|-{:-^35}-|-{:-^5}-|",
                "-", "-", "-", "-", "-"
            ),
        ];
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
    common_options: &PackageRelatedOptions,
    options: &Options,
    working_directory: PathBuf,
) -> anyhow::Result<Results> {
    tracing::info!("Check directory for crates that need publishing");
    let started = Instant::now();
    let path = match working_directory.is_absolute() {
        true => working_directory.clone(),
        false => dunce::canonicalize(&working_directory)
            .with_context(|| format!("Failed to get absolute path from {working_directory:?}"))?,
    };

    let toolchain =
        get_toolchain(&working_directory).unwrap_or_else(|_| DEFAULT_TOOLCHAIN.to_string());
    let now = Utc::now().date_naive();
    let epoch = NaiveDate::parse_from_str(CUSTOM_EPOCH, "%Y-%m-%d").unwrap(); // I'm confident about this
    let timestamp = format!("{}", (now - epoch).num_days());

    let binary_store = BinaryStore::new(
        options.binary_store_storage_account.clone(),
        options.binary_store_container_name.clone(),
        options.binary_store_access_key.clone(),
    )?;
    tracing::debug!("Base directory: {:?}", path);
    // 1. Find all workspaces to investigate
    if common_options.progress {
        println!(
            "{} {}Resolving workspaces...",
            style("[1/7]").bold().dim(),
            LOOKING_GLASS
        );
    }
    let limit_dependency_kind = match options.ignore_dev_dependencies {
        true => Some(DependencyKind::Normal),
        false => None,
    };
    let crates = CrateGraph::new(
        &path,
        common_options.cargo_main_registry.clone(),
        limit_dependency_kind,
    )?;
    let mut packages: HashMap<PackageId, Result> = HashMap::new();
    let mut dep_to_id: HashMap<String, PackageId> = HashMap::new();

    // 2. For each workspace, find if one of the subcrates needs publishing
    if common_options.progress {
        println!(
            "{} {}Resolving packages...",
            style("[2/7]").bold().dim(),
            TRUCK
        );
    }

    for workspace in crates.workspaces() {
        let resolve = workspace.metadata.resolve.as_ref().unwrap();
        // Let's add the node to the deps
        for node in &resolve.nodes {
            for node_dep in &node.deps {
                dep_to_id.insert(node_dep.name.to_string(), node_dep.pkg.clone());
            }
        }
        // Let's add all package to the deps as well
        let workspace_packages = workspace.metadata.workspace_packages();
        for package in workspace_packages {
            dep_to_id.insert(package.name.to_string(), package.id.clone());
        }

        for package in workspace.metadata.workspace_packages() {
            match Result::new(
                workspace.path.to_string_lossy().into(),
                package.clone(),
                working_directory.clone(),
                options.hide_dependencies,
                &dep_to_id,
            ) {
                Ok(package) => {
                    let blacklisted = !common_options.blacklist.is_empty()
                        && common_options.blacklist.contains(&package.package);
                    let whitelisted = common_options.whitelist.is_empty()
                        || common_options.whitelist.contains(&package.package);
                    if !blacklisted && whitelisted {
                        packages.insert(package.package_id.clone().unwrap().clone(), package);
                    }
                }
                Err(e) => {
                    let error_msg = format!("Could not check package {}: {}", package.name, e);
                    if options.fail_unit_error {
                        anyhow::bail!(error_msg)
                    } else {
                        tracing::warn!("{}", error_msg);
                        continue;
                    }
                }
            }
        }
    }

    let package_keys: Vec<PackageId> = packages.keys().cloned().collect();

    // 5. Compute Runtime information
    if common_options.progress {
        println!(
            "{} {}Compute runtime information...",
            style("[3/7]").bold().dim(),
            TRUCK
        );
    }

    let mut pb: Option<ProgressBar> = None;
    if common_options.progress {
        pb = Some(ProgressBar::new(packages.len() as u64).with_style(
            ProgressStyle::with_template("{spinner} {wide_msg} {pos}/{len}")?,
        ));
    }
    let mut package_versions: HashMap<PackageId, String> = HashMap::new();
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
                    &dep_to_id,
                )
                .await?;
        }
    }

    // Check Alt Registries Settings
    // If package A needs to be published to an alt registry, all of its dependencies should be as well
    if common_options.progress {
        println!(
            "{} {}Back-feeding alt registry settings...",
            style("[4/7]").bold().dim(),
            PAPER
        );
    }
    let mut pb: Option<ProgressBar> = None;
    if common_options.progress {
        pb = Some(ProgressBar::new(packages.len() as u64).with_style(
            ProgressStyle::with_template("{spinner} {wide_msg} {pos}/{len}")?,
        ));
    }

    let mut packages_registries: HashMap<PackageId, HashSet<String>> = HashMap::new();
    let mut all_packages_registries: HashSet<String> = HashSet::new();
    for package_key in package_keys.clone() {
        if let Some(package) = packages.get(&package_key) {
            if let Some(registries) = &package.publish_detail.cargo.registries {
                packages_registries.insert(package_key.clone(), registries.clone());
                all_packages_registries.extend(registries.clone());
            }
        }
    }
    for (package_key, registries) in packages_registries {
        if let Some(ref pb) = pb {
            pb.inc(1);
        }
        if !registries.is_empty() {
            let transitive_dependencies = crates
                .dependency_graph()
                .get_transitive_dependencies(package_key.clone());
            // The package should be publish to some alt registries
            // Let's set that in all it's dependent
            // Let's update each transitive dep with the list of registries
            for dep_id in transitive_dependencies {
                if let Some(dep) = packages.get_mut(&dep_id) {
                    let mut dep_registries = dep
                        .publish_detail
                        .cargo
                        .registries
                        .clone()
                        .unwrap_or_default();
                    for registry in &registries {
                        dep_registries.insert(registry.clone());
                    }
                    dep.publish_detail.cargo.registries = Some(dep_registries);
                }
            }
        }
    }
    // Check Release status
    if common_options.progress {
        println!(
            "{} {}Checking published status...",
            style("[5/7]").bold().dim(),
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
    let cargo = Cargo::new(&all_packages_registries)?;
    let mut docker = Docker::new(None)?;
    if let (Some(docker_registry), Some(docker_username), Some(docker_password)) = (
        options.docker_registry.clone(),
        options.docker_registry_username.clone(),
        options.docker_registry_password.clone(),
    ) {
        docker.add_registry_auth(docker_registry, docker_username, docker_password)
    }
    let mut pb: Option<ProgressBar> = None;
    if common_options.progress {
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
                    .check_publishable(
                        options.skip_npm,
                        &npm,
                        options.skip_cargo,
                        &cargo,
                        options.autopublish_cargo,
                        options.skip_docker,
                        &mut docker,
                        options.skip_binary,
                        &binary_store,
                    )
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
                            tracing::warn!("{}", error_msg);
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
                        if !env_string.starts_with(&format!("refs/tags/{check_key}")) {
                            package.publish = false;
                        }
                    }
                }
            }
        }
    }

    if common_options.progress {
        println!(
            "{} {}Resolving packages dependencies...",
            style("[6/7]").bold().dim(),
            TRUCK
        );
    }
    let mut pb: Option<ProgressBar> = None;
    if common_options.progress {
        pb = Some(ProgressBar::new(packages.len() as u64).with_style(
            ProgressStyle::with_template("{spinner} {wide_msg} {pos}/{len}")?,
        ));
    }
    let publish_status: HashMap<PackageId, bool> = packages
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
                d.package_id
                    .as_ref()
                    .is_some_and(|p| package_keys.contains(p))
            });
            for dep in &mut package.dependencies {
                if let Some(package_name) = &dep.package_id {
                    if let Some(dep_p) = publish_status.get(package_name) {
                        dep.publishable = *dep_p;
                    }
                }
            }
        }
    }
    let package_keys: Vec<PackageId> = packages.keys().cloned().collect();
    tracing::debug!("Package list: {package_keys:#?}");

    if common_options.progress {
        println!(
            "{} {}Checking if packages changed...",
            style("[7/7]").bold().dim(),
            TRUCK
        );
    }
    if options.check_changed {
        if common_options.progress {
            pb = Some(ProgressBar::new(packages.len() as u64).with_style(
                ProgressStyle::with_template("{spinner} {wide_msg} {pos}/{len}")?,
            ));
        }

        // Check changed from a git pov
        let changed_package_paths =
            crates.changed_packages(&common_options.base_rev, &common_options.head_rev)?;
        tracing::info!("Changed packages: {changed_package_paths:#?}");
        // Any packages that transitively depend on changed packages are also considered "changed".
        let changed_closure = crates
            .dependency_graph()
            .reverse_closure(changed_package_paths.iter().map(AsRef::as_ref));
        tracing::info!("Changed closure: {changed_closure:#?}");

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
                    tracing::info!("Marking package as changed for publish: {:?}", package.path);
                    continue;
                }
                if changed_package_paths.contains(&package.path) {
                    tracing::info!("Detected change in {:?}", package.path);
                    package.changed = true;
                } else if changed_closure.contains(&package.path) {
                    tracing::info!("A dependency changed for {:?}", package.path);
                    package.dependencies_changed = true;
                }
            }
        }
    }
    for package_key in package_keys.clone() {
        if let Some(package) = packages.get_mut(&package_key) {
            // We retest when dependencies change because not doing so has caused us to miss
            // serious bugs, or even compilation errors until the package is changed. This
            // results in longer test times, but removing the check is not a solution.
            if package.changed || package.dependencies_changed {
                package.perform_test = true;
            }
        }
    }
    if common_options.progress {
        println!("{} Done in {}", SPARKLE, HumanDuration(started.elapsed()));
    }

    Ok(Results {
        members: packages,
        crate_graph: crates,
    })
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, process::Command};

    use git2::Repository;

    use crate::{
        PackageRelatedOptions,
        commands::check_workspace::{Options, Result as Package, check_workspace},
        utils::test::{FAKE_REGISTRY, commit_all_changes, initialize_workspace},
    };

    fn create_complex_workspace() -> PathBuf {
        let tmp = assert_fs::TempDir::new()
            .unwrap()
            .into_persistent()
            .to_path_buf();

        let repo = Repository::init(&tmp).expect("Failed to init repo");

        // Configure Git user info (required for commits)
        repo.config()
            .unwrap()
            .set_str("user.name", "Test User")
            .unwrap();
        repo.config()
            .unwrap()
            .set_str("user.email", "test@example.com")
            .unwrap();
        repo.config().unwrap().set_str("gpg.sign", "false").unwrap();

        initialize_workspace(
            &tmp,
            "workspace_a",
            vec!["crates_a", "crates_b", "crates_c"],
            vec![],
        );
        initialize_workspace(&tmp, "workspace_d", vec!["crates_e", "crates_f"], vec![]);
        initialize_workspace(&tmp, "crates_g", vec![], vec!["some_other_registries"]);

        // Setup Deps
        // workspace_d/crates_e -> workspace_a/crates_a
        Command::new("cargo")
            .arg("add")
            .arg("--offline")
            .arg("--registry")
            .arg(FAKE_REGISTRY)
            .arg("--path")
            .arg("../../../workspace_a/crates/crates_a")
            .arg("workspace_a__crates_a")
            .current_dir(tmp.join("workspace_d").join("crates").join("crates_e"))
            .output()
            .expect("Failed to add workspace_a__crates_a to workspace_d__crates_e");
        // crates_g ->  workspace_d/crates_e
        Command::new("cargo")
            .arg("add")
            .arg("--offline")
            .arg("--registry")
            .arg(FAKE_REGISTRY)
            .arg("--path")
            .arg("../workspace_d/crates/crates_e")
            .arg("workspace_d__crates_e")
            .current_dir(tmp.join("crates_g"))
            .output()
            .expect("Failed to add workspace_d__crates_e");
        // crates_g ->  workspace_a/crates_b
        Command::new("cargo")
            .arg("add")
            .arg("--offline")
            .arg("--registry")
            .arg(FAKE_REGISTRY)
            .arg("--path")
            .arg("../workspace_a/crates/crates_b")
            .arg("workspace_a__crates_b")
            .current_dir(tmp.join("crates_g"))
            .output()
            .expect("Failed to add workspace_a__crates_b");
        // Stage and commit initial crate
        commit_all_changes(&tmp, "Initial commit");
        dunce::canonicalize(tmp).unwrap()
    }

    #[tokio::test]
    async fn test_alternate_registry_back_feeding() {
        rustls::crypto::ring::default_provider()
            .install_default()
            .expect("Could not install crypto provider");
        let ws = create_complex_workspace();

        // Check workspace information
        let common_options = PackageRelatedOptions::default();
        let check_workspace_options = Options::new();
        let results = check_workspace(&common_options, &check_workspace_options, ws.clone())
            .await
            .unwrap();

        fn check_registry_condition(v: &Package, should_have_registry: bool) {
            let has_alt_reg = v
                .publish_detail
                .cargo
                .registries
                .clone()
                .unwrap_or_default()
                .contains("some_other_registries");
            assert_eq!(has_alt_reg, should_have_registry);
        }

        for v in results.members.values() {
            match v.package.as_str() {
                "workspace_a__crates_a" => {
                    check_registry_condition(v, true);
                }
                "workspace_a__crates_b" => {
                    check_registry_condition(v, true);
                }
                "workspace_a__crates_c" => {
                    check_registry_condition(v, false);
                }
                "workspace_d__crates_f" => {
                    check_registry_condition(v, false);
                }
                "crates_g" => {
                    check_registry_condition(v, true);
                }
                _ => {}
            }
        }
    }
}

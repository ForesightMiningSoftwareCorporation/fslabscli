use std::default::Default;
use std::fmt::{Display, Formatter};
use std::fs::File;
use std::hash::Hash;
use std::io::{BufReader, BufWriter};
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::Context;
use clap::Parser;
use indexmap::IndexMap;
use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize, Serializer};
use serde_with::{formats::PreferOne, serde_as, OneOrMany};
use serde_yaml::Value;
use void::Void;

use crate::commands::check_workspace::{check_workspace, Options as CheckWorkspaceOptions};
use crate::utils::{deserialize_opt_string_or_map, deserialize_opt_string_or_struct, FromMap};

const EMPTY_WORKFLOW: &str = r#"

name: CI-CD - Tests and Publishing

on:
  push:
    branches:
      - main
  pull_request:
  workflow_dispatch:
    inputs:
      publish:
        type: boolean
        required: false
        description: Trigger with publish

concurrency:
  group: ${{ github.workflow }}-${{ github.head_ref || github.run_id }}
  cancel-in-progress: true

jobs:
"#;

#[derive(Debug, Parser)]
#[command(about = "Check directory for crates that need to be published.")]
pub struct Options {
    #[arg(long)]
    output: PathBuf,
    #[arg(long)]
    template: Option<PathBuf>,
    #[arg(long, default_value_t = false)]
    no_depends_on_template_jobs: bool,
    #[arg(long, default_value_t = false)]
    no_check_changed_and_publish: bool,
    #[arg(long, default_value = "v2")]
    build_workflow_version: String,
}

#[derive(Serialize)]
pub struct GenerateResult {}

impl Display for GenerateResult {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "")
    }
}

#[derive(Serialize, Debug, Deserialize, PartialEq)]
pub struct GithubWorkflowInput {
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(rename = "type")]
    pub input_type: String,
}

#[derive(Serialize, Debug, Deserialize, PartialEq)]
pub struct GithubWorkflowSecret {
    pub description: String,
    #[serde(default)]
    pub required: bool,
}

#[derive(Serialize, Debug, Deserialize, PartialEq)]
pub struct GithubWorkflowTriggerPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branches: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inputs: Option<IndexMap<String, GithubWorkflowInput>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secrets: Option<IndexMap<String, GithubWorkflowSecret>>,
}

#[derive(Serialize, Debug, Deserialize, Eq, PartialEq, Hash, Ord, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub enum GithubWorkflowTrigger {
    PullRequest,
    Push,
    WorkflowCall,
    WorkflowDispatch,
}

#[derive(Debug, Default, Deserialize, Eq, PartialEq)]
pub struct GithubWorkflowJobSecret {
    pub inherit: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secrets: Option<IndexMap<String, String>>,
}

impl Serialize for GithubWorkflowJobSecret {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
    {
        if self.inherit {
            serializer.serialize_str("inherit")
        } else {
            match self.secrets.clone() {
                Some(secrets) => {
                    let mut map = serializer.serialize_map(Some(secrets.len()))?;
                    for (k, v) in secrets {
                        map.serialize_entry(&k, &v)?;
                    }
                    map.end()
                }
                None => serializer.serialize_none(),
            }
        }
    }
}

#[derive(Serialize, Debug, Default, Deserialize, Eq, PartialEq)]
pub struct GithubWorkflowJobEnvironment {
    pub name: String,
    pub url: Option<String>,
}

#[derive(Serialize, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct GithubWorkflowJobStrategy {
    pub matrix: IndexMap<String, Value>,
    pub fail_false: Option<bool>,
}

#[derive(Serialize, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub struct GithubWorkflowJobSteps {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(rename = "if", skip_serializing_if = "Option::is_none")]
    pub step_if: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    shell: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    run: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    uses: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    working_directory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    with: Option<IndexMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    env: Option<IndexMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    continue_on_error: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_minutes: Option<usize>,
}

#[derive(Serialize, Deserialize, Debug, Default)]
struct GithubWorkflowJobContainer {
    pub image: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<IndexMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credentials: Option<IndexMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ports: Option<Vec<usize>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volumes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<String>,
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug, Default)]
#[serde(rename_all = "kebab-case")]
struct GithubWorkflowJob {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<IndexMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uses: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub needs: Option<Vec<String>>,
    #[serde(rename = "if", skip_serializing_if = "Option::is_none")]
    pub job_if: Option<String>,
    #[serde_as(deserialize_as = "Option<OneOrMany<_, PreferOne>>")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runs_on: Option<Vec<String>>,
    #[serde(
    default,
    deserialize_with = "deserialize_opt_string_or_struct",
    skip_serializing_if = "Option::is_none"
    )]
    pub environment: Option<GithubWorkflowJobEnvironment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<IndexMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defaults: Option<GithubWorkflowDefaults>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub with: Option<IndexMap<String, Value>>,
    #[serde(
    default,
    deserialize_with = "deserialize_opt_string_or_map",
    skip_serializing_if = "Option::is_none"
    )]
    pub secrets: Option<GithubWorkflowJobSecret>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<IndexMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outputs: Option<IndexMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps: Option<Vec<GithubWorkflowJobSteps>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_minutes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strategy: Option<GithubWorkflowJobStrategy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continue_on_error: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<GithubWorkflowJobContainer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub services: Option<IndexMap<String, GithubWorkflowJobContainer>>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
struct GithubWorkflowDefaultsRun {
    pub shell: Option<String>,
    pub working_directory: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
struct GithubWorkflowDefaults {
    pub run: GithubWorkflowDefaultsRun,
}

#[derive(Serialize, Deserialize, Debug)]
struct GithubWorkflow {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_name: Option<String>,
    #[serde(rename = "on", skip_serializing_if = "Option::is_none")]
    pub triggers: Option<IndexMap<GithubWorkflowTrigger, GithubWorkflowTriggerPayload>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defaults: Option<GithubWorkflowDefaults>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<IndexMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<IndexMap<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<IndexMap<String, String>>,
    pub jobs: IndexMap<String, GithubWorkflowJob>,
}

impl FromStr for GithubWorkflowJobSecret {
    type Err = Void;

    fn from_str(_s: &str) -> Result<Self, Self::Err> {
        Ok(Self {
            inherit: true,
            secrets: None,
        })
    }
}

impl FromStr for GithubWorkflowJobEnvironment {
    type Err = Void;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self {
            name: s.to_string(),
            url: None,
        })
    }
}

impl FromMap for GithubWorkflowJobSecret {
    fn from_map(map: IndexMap<String, String>) -> Result<Self, Void>
        where
            Self: Sized,
    {
        Ok(Self {
            inherit: false,
            secrets: Some(map),
        })
    }
}

#[derive(Default, Clone)]
struct PublishJobOptions {
    /// Should the tests be run
    pub skip_test: Option<StringBool>,
    /// Skip tests when no changes were detected in any cargo workspace
    pub skip_tests_no_changes: Option<StringBool>,
    /// Should miri tests be run
    pub skip_miri_test: Option<StringBool>,
    /// Should the crate be published
    pub publish: Option<StringBool>,
    /// Should the crate be published to the private registry
    pub publish_private_registry: Option<StringBool>,
    /// Should the crate be published to the public registry
    pub publish_public_registry: Option<StringBool>,
    /// Should the docker image be built and published
    pub publish_docker: Option<StringBool>,
    /// Should the binary be built and published
    pub publish_binary: Option<StringBool>,
    /// Should the npm napi package be built and published
    pub publish_npm_napi: Option<StringBool>,
    /// Rust toolchain to install.
    /// Do not set this to moving targets like "stable".
    /// Instead, leave it empty and regularly bump the default in this file.
    pub toolchain: Option<String>,
    /// Rust toolchain to use for Miri.
    /// Do not set this to moving targets like "nightly".
    /// Instead, leave it empty and regularly bump the default in this file.
    pub miri_toolchain: Option<String>,
    /// Hard coded release channel
    pub release_channel: Option<String>,
    /// Path of additional cache to get
    pub additional_cache_path: Option<String>,
    /// Key of additional cache to get
    pub additional_cache_key: Option<String>,
    /// Script to run if additional cache miss
    pub additional_cache_miss: Option<String>,
    /// Additional script to run before the additional packages
    pub additional_script: Option<String>,
    /// Package that needs to be installed before Rust compilation can happens
    pub required_packages: Option<String>,
    /// JSON array of of Cargo workspaces
    pub workspaces: Option<String>,
    /// Working directory to run the cargo command
    pub working_directory: Option<String>,
    /// Additional arguments to pass to the cargo command
    pub additional_args: Option<String>,
    /// Custom cargo commands that will be run after login
    pub custom_cargo_commands: Option<String>,
    /// Path to docker context
    pub docker_context: Option<String>,
    /// The path to the Dockerfile to use
    pub dockerfile: Option<String>,
    /// Docker image name
    pub docker_image: Option<String>,
    /// Docker registry
    pub docker_registry: Option<String>,
    /// Matrix file to load
    pub matrix_file: Option<String>,
    /// Post Build Additional script to run after the additional packages
    pub post_build_additional_script: Option<String>,
    /// Force the publish test to be marked as non required
    pub force_nonrequired_publish_test: Option<StringBool>,
    /// Should the binary bin be signed
    pub binary_sign_build: Option<StringBool>,
    /// Should the release be reported
    pub report_release: Option<StringBool>,
}

impl PublishJobOptions {
    pub fn merge(self, other: PublishJobOptions) -> Self {
        Self {
            skip_test: self.skip_test.or(other.skip_test),
            skip_tests_no_changes: self.skip_tests_no_changes.or(other.skip_tests_no_changes),
            skip_miri_test: self.skip_miri_test.or(other.skip_miri_test),
            publish: self.publish.or(other.publish),
            publish_private_registry: self
                .publish_private_registry
                .or(other.publish_private_registry),
            publish_public_registry: self
                .publish_public_registry
                .or(other.publish_public_registry),
            publish_docker: self.publish_docker.or(other.publish_docker),
            publish_binary: self.publish_binary.or(other.publish_binary),
            publish_npm_napi: self.publish_npm_napi.or(other.publish_npm_napi),
            toolchain: self.toolchain.or(other.toolchain),
            miri_toolchain: self.miri_toolchain.or(other.miri_toolchain),
            release_channel: self.release_channel.or(other.release_channel),
            additional_cache_path: self.additional_cache_path.or(other.additional_cache_path),
            additional_cache_key: self.additional_cache_key.or(other.additional_cache_key),
            additional_cache_miss: self.additional_cache_miss.or(other.additional_cache_miss),
            additional_script: self.additional_script.or(other.additional_script),
            required_packages: self.required_packages.or(other.required_packages),
            workspaces: self.workspaces.or(other.workspaces),
            working_directory: self.working_directory.or(other.working_directory),
            additional_args: self.additional_args.or(other.additional_args),
            custom_cargo_commands: self.custom_cargo_commands.or(other.custom_cargo_commands),
            docker_context: self.docker_context.or(other.docker_context),
            dockerfile: self.dockerfile.or(other.dockerfile),
            docker_image: self.docker_image.or(other.docker_image),
            docker_registry: self.docker_registry.or(other.docker_registry),
            matrix_file: self.matrix_file.or(other.matrix_file),
            post_build_additional_script: self
                .post_build_additional_script
                .or(other.post_build_additional_script),
            force_nonrequired_publish_test: self
                .force_nonrequired_publish_test
                .or(other.force_nonrequired_publish_test),
            binary_sign_build: self.binary_sign_build.or(other.binary_sign_build),
            report_release: self.report_release.or(other.report_release),
        }
    }
}

impl From<PublishJobOptions> for IndexMap<String, Value> {
    fn from(val: PublishJobOptions) -> Self {
        let mut map: IndexMap<String, Value> = IndexMap::new();
        if let Some(skip_test) = val.skip_test {
            map.insert("skip_test".to_string(), skip_test.into());
        }
        if let Some(skip_tests_no_changes) = val.skip_tests_no_changes {
            map.insert(
                "skip_tests_no_changes".to_string(),
                skip_tests_no_changes.into(),
            );
        }
        if let Some(skip_miri_test) = val.skip_miri_test {
            map.insert("skip_miri_test".to_string(), skip_miri_test.into());
        }
        if let Some(publish) = val.publish {
            map.insert("publish".to_string(), publish.into());
        }
        if let Some(publish_private_registry) = val.publish_private_registry {
            map.insert(
                "publish_private_registry".to_string(),
                publish_private_registry.into(),
            );
        }
        if let Some(publish_public_registry) = val.publish_public_registry {
            map.insert(
                "publish_public_registry".to_string(),
                publish_public_registry.into(),
            );
        }
        if let Some(publish_docker) = val.publish_docker {
            map.insert("publish_docker".to_string(), publish_docker.into());
        }
        if let Some(publish_binary) = val.publish_binary {
            map.insert("publish_binary".to_string(), publish_binary.into());
        }
        if let Some(publish_npm_napi) = val.publish_npm_napi {
            map.insert("publish_npm_napi".to_string(), publish_npm_napi.into());
        }
        if let Some(toolchain) = val.toolchain {
            map.insert("toolchain".to_string(), toolchain.into());
        }
        if let Some(miri_toolchain) = val.miri_toolchain {
            map.insert("miri_toolchain".to_string(), miri_toolchain.into());
        }
        if let Some(release_channel) = val.release_channel {
            map.insert("release_channel".to_string(), release_channel.into());
        }
        if let Some(additional_cache_path) = val.additional_cache_path {
            map.insert(
                "additional_cache_path".to_string(),
                additional_cache_path.into(),
            );
        }
        if let Some(additional_cache_key) = val.additional_cache_key {
            map.insert(
                "additional_cache_key".to_string(),
                additional_cache_key.into(),
            );
        }
        if let Some(additional_cache_miss) = val.additional_cache_miss {
            map.insert(
                "additional_cache_miss".to_string(),
                additional_cache_miss.into(),
            );
        }
        if let Some(additional_script) = val.additional_script {
            map.insert("additional_script".to_string(), additional_script.into());
        }
        if let Some(required_packages) = val.required_packages {
            map.insert("required_packages".to_string(), required_packages.into());
        }
        if let Some(workspaces) = val.workspaces {
            map.insert("workspaces".to_string(), workspaces.into());
        }
        if let Some(working_directory) = val.working_directory {
            map.insert("working_directory".to_string(), working_directory.into());
        }
        if let Some(additional_args) = val.additional_args {
            map.insert("additional_args".to_string(), additional_args.into());
        }
        if let Some(custom_cargo_commands) = val.custom_cargo_commands {
            map.insert(
                "custom_cargo_commands".to_string(),
                custom_cargo_commands.into(),
            );
        }
        if let Some(docker_context) = val.docker_context {
            map.insert("docker_context".to_string(), docker_context.into());
        }
        if let Some(dockerfile) = val.dockerfile {
            map.insert("dockerfile".to_string(), dockerfile.into());
        }
        if let Some(docker_image) = val.docker_image {
            map.insert("docker_image".to_string(), docker_image.into());
        }
        if let Some(docker_registry) = val.docker_registry {
            map.insert("docker_registry".to_string(), docker_registry.into());
        }
        if let Some(matrix_file) = val.matrix_file {
            map.insert("matrix_file".to_string(), matrix_file.into());
        }
        if let Some(post_build_additional_script) = val.post_build_additional_script {
            map.insert(
                "post_build_additional_script".to_string(),
                post_build_additional_script.into(),
            );
        }
        if let Some(force_nonrequired_publish_test) = val.force_nonrequired_publish_test {
            map.insert(
                "force_nonrequired_publish_test".to_string(),
                force_nonrequired_publish_test.into(),
            );
        }
        if let Some(binary_sign_build) = val.binary_sign_build {
            map.insert("binary_sign_build".to_string(), binary_sign_build.into());
        }
        if let Some(report_release) = val.report_release {
            map.insert("report_release".to_string(), report_release.into());
        }
        map
    }
}

impl From<IndexMap<String, Value>> for PublishJobOptions {
    fn from(value: IndexMap<String, Value>) -> Self {
        let mut me = Self {
            ..Default::default()
        };
        for (k, v) in value {
            match k.as_str() {
                "skip_test" => me.skip_test = Some(v.into()),
                "skip_tests_no_changes" => me.skip_tests_no_changes = Some(v.into()),
                "skip_miri_test" => me.skip_miri_test = Some(v.into()),
                "publish" => me.publish = Some(v.into()),
                "publish_private_registry" => me.publish_private_registry = Some(v.into()),
                "publish_public_registry" => me.publish_public_registry = Some(v.into()),
                "publish_docker" => me.publish_docker = Some(v.into()),
                "publish_binary" => me.publish_binary = Some(v.into()),
                "publish_npm_napi" => me.publish_npm_napi = Some(v.into()),
                "toolchain" => {
                    me.toolchain = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "miri_toolchain" => {
                    me.miri_toolchain = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "release_channel" => {
                    me.release_channel = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "additional_cache_path" => {
                    me.additional_cache_path = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "additional_cache_key" => {
                    me.additional_cache_key = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "additional_cache_miss" => {
                    me.additional_cache_miss = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "additional_script" => {
                    me.additional_script = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "required_packages" => {
                    me.required_packages = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "workspaces" => {
                    me.workspaces = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "working_directory" => {
                    me.working_directory = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "additional_args" => {
                    me.additional_args = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "custom_cargo_commands" => {
                    me.custom_cargo_commands = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "docker_context" => {
                    me.docker_context = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "dockerfile" => {
                    me.dockerfile = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "docker_image" => {
                    me.docker_image = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "docker_registry" => {
                    me.docker_registry = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "matrix_file" => {
                    me.matrix_file = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "post_build_additional_script" => {
                    me.post_build_additional_script = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "force_nonrequired_publish_test" => {
                    me.force_nonrequired_publish_test = Some(v.into())
                }
                "binary_sign_build" => me.binary_sign_build = Some(v.into()),
                "report_release" => me.report_release = Some(v.into()),
                _ => {}
            }
        }
        me
    }
}

#[derive(Clone, Default)]
struct StringBool(bool);

impl From<StringBool> for Value {
    fn from(val: StringBool) -> Value {
        Value::String(match val.0 {
            true => "true".to_string(),
            false => "false".to_string(),
        })
    }
}

impl From<Value> for StringBool {
    fn from(value: Value) -> Self {
        Self(value.as_bool().unwrap_or(false))
    }
}

impl Serialize for StringBool {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
    {
        match self.0 {
            true => serializer.serialize_str("true"),
            false => serializer.serialize_str("false"),
        }
    }
}

pub async fn generate_workflow(
    options: Box<Options>,
    working_directory: PathBuf,
) -> anyhow::Result<GenerateResult> {
    // Get Base Workflow
    let mut workflow_template: GithubWorkflow = match options.template {
        Some(template) => {
            let file = File::open(template)?;
            let reader = BufReader::new(file);
            serde_yaml::from_reader(reader)
        }
        None => serde_yaml::from_str(EMPTY_WORKFLOW),
    }
        .map_err(|e| {
            log::error!("Unparseable template: {}", e);
            e
        })
        .with_context(|| "Could not parse workflow template")?;
    // Get Template jobs, we'll make the generated jobs depends on it
    let mut initial_jobs: Vec<String> = workflow_template.jobs.keys().cloned().collect();
    // If we need to test for changed and publish
    let check_job_key = "check_changed_and_publish".to_string();
    if !options.no_check_changed_and_publish {
        workflow_template.jobs.insert(
            check_job_key.clone(),
            GithubWorkflowJob {
                name: Some("Check which workspace member changed and / or needs publishing".to_string()),
                runs_on: Some(vec!["ubuntu-latest".to_string()]),
                outputs: Some(IndexMap::from([("workspace".to_string(), "${{ steps.check_workspace.outputs.workspace }}".to_string())])),
                ..Default::default()
            },
        );
        ///
        // steps:
        // - name: Install fslabscli
        // uses: ForesightMiningSoftwareCorporation/fslabscli-action@v1
        //     - name: Checkout workspace
        // uses: actions/checkout@v4
        //     - name: Check Workspace
        // id: check_workspace
        // working-directory: .
        // shell: bash
        // run: |
        // echo workspace=$(fslabscli check-workspace --json) >> $GITHUB_OUTPUT
        // initial_jobs.push(check_job_key.clone());
    }
    // Get Directory information
    let members =
        check_workspace(Box::new(CheckWorkspaceOptions::new()), working_directory).await?;
    for (member_key, member) in members.0 {
        let test_job_key = format!("test_{}", member.package);
        let publish_job_key = format!("publish_{}", member.package);
        let mut test_needs = match options.no_depends_on_template_jobs {
            false => initial_jobs.clone(),
            true => vec![],
        };
        for dependency in &member.dependencies {
            test_needs.push(format!("test_{}", dependency.package))
        }
        let mut publish_needs = match options.no_depends_on_template_jobs {
            false => initial_jobs.clone(),
            true => vec![],
        };
        for dependency in &member.dependencies {
            publish_needs.push(format!("publish_{}", dependency.package))
        }
        // add self test to publish needs
        publish_needs.push(test_job_key.clone());
        let base_if = "always() && !contains(needs.*.result, 'failure') && !contains(needs.*.result, 'cancelled')".to_string();
        let mut publish_if = format!("{} && (github.event_name == 'push' || (github.event_name == 'workflow_dispatch' && inputs.publish))", base_if);
        let mut test_if = base_if.clone();
        if !options.no_check_changed_and_publish {
            publish_if = format!(
                "{} && (fromJSON(needs.{}.outputs.workspace).{}.publish == 'true')",
                publish_if, &check_job_key, member_key
            );
            test_if = format!(
                "{} && (fromJSON(needs.{}.outputs.workspace).{}.changed == 'true')",
                test_if, &check_job_key, member_key
            );
        }
        let cargo_options: PublishJobOptions = match member.ci_args {
            Some(a) => a.into(),
            None => Default::default(),
        };
        let job_working_directory = member.path.to_string_lossy().to_string();
        let publish_with: PublishJobOptions = PublishJobOptions {
            working_directory: Some(job_working_directory.clone()),
            skip_test: Some(StringBool(true)),
            publish: Some(StringBool(member.publish)),
            publish_private_registry: Some(StringBool(
                member.publish_detail.cargo.publish
                    && !(member.publish_detail.cargo.allow_public
                    && member.publish_detail.cargo.registry.is_none()),
            )),
            publish_public_registry: Some(StringBool(
                member.publish_detail.cargo.publish
                    && (member.publish_detail.cargo.allow_public
                    && member.publish_detail.cargo.registry.is_none()),
            )),
            publish_docker: Some(StringBool(member.publish_detail.docker.publish)),
            publish_npm_napi: Some(StringBool(member.publish_detail.npm_napi.publish)),
            publish_binary: Some(StringBool(member.publish_detail.binary)),
            ..Default::default()
        }
            .merge(cargo_options.clone());
        let test_with: PublishJobOptions = PublishJobOptions {
            working_directory: Some(job_working_directory),
            publish: Some(StringBool(false)),
            ..Default::default()
        }
            .merge(cargo_options.clone());

        let test_job = GithubWorkflowJob {
            name: Some(format!("Test {}: {}", member.workspace, member.package)),
            uses: Some(
                format!("ForesightMiningSoftwareCorporation/github/.github/workflows/rust-build.yml@{}", options.build_workflow_version)
                    .to_string(),
            ),
            needs: Some(test_needs),
            job_if: Some(test_if),
            with: Some(test_with.into()),
            secrets: Some(GithubWorkflowJobSecret {
                inherit: true,
                secrets: None,
            }),
            ..Default::default()
        };
        let publish_job = GithubWorkflowJob {
            name: Some(format!("Publish {}: {}", member.workspace, member.package)),
            uses: Some(
                format!("ForesightMiningSoftwareCorporation/github/.github/workflows/rust-build.yml@{}", options.build_workflow_version)
                    .to_string(),
            ),
            needs: Some(publish_needs),
            job_if: Some(publish_if),
            with: Some(publish_with.into()),
            secrets: Some(GithubWorkflowJobSecret {
                inherit: true,
                secrets: None,
            }),
            ..Default::default()
        };
        workflow_template
            .jobs
            .insert(test_job_key.clone(), test_job);
        workflow_template.jobs.insert(publish_job_key, publish_job);
    }
    let output_file = File::create(options.output)?;
    let mut writer = BufWriter::new(output_file);
    serde_yaml::to_writer(&mut writer, &workflow_template)?;
    Ok(GenerateResult {})
}

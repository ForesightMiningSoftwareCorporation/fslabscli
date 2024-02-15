use std::fmt::{Display, Formatter};
use std::fs::File;
use std::hash::Hash;
use std::io::{BufReader, BufWriter};
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::Context;
use clap::Parser;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize, Serializer};
use serde::ser::SerializeMap;
use serde_with::{formats::PreferOne, OneOrMany, serde_as};
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
    #[arg(long, default_value_t = true)]
    depends_on_template_jobs: bool,
    #[arg(long, default_value_t = true)]
    inject_check_changed_and_publish: bool,
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
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error> where S: Serializer {
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
                None => serializer.serialize_none()
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
    #[serde(default, deserialize_with = "deserialize_opt_string_or_struct", skip_serializing_if = "Option::is_none")]
    pub environment: Option<GithubWorkflowJobEnvironment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<IndexMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defaults: Option<GithubWorkflowDefaults>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub with: Option<IndexMap<String, Value>>,
    #[serde(default, deserialize_with = "deserialize_opt_string_or_map", skip_serializing_if = "Option::is_none")]
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
    pub concurrency: Option<IndexMap<String, String>>,
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
    fn from_map(map: IndexMap<String, String>) -> Result<Self, Void> where Self: Sized {
        Ok(Self {
            inherit: false,
            secrets: Some(map),
        })
    }
}

pub async fn generate_workflow(options: Options, working_directory: PathBuf) -> anyhow::Result<GenerateResult> {
    // Get Base Workflow
    let mut workflow_template: GithubWorkflow = match options.template {
        Some(template) => {
            let file = File::open(template)?;
            let reader = BufReader::new(file);
            serde_yaml::from_reader(reader)
        }
        None => serde_yaml::from_str(EMPTY_WORKFLOW)
    }.map_err(|e| {
        log::error!("Unparseable template: {}", e);
        e
    }).with_context(|| "Could not parse workflow template")?;
    // Get Template jobs, we'll make the generated jobs depends on it
    let mut initial_jobs: Vec<String> = workflow_template.jobs.keys().cloned().collect();
    // If we need to test for changed and publish
    let check_job_key = "check_changed_and_publish".to_string();
    if options.inject_check_changed_and_publish {
        workflow_template.jobs.insert(check_job_key.clone(), GithubWorkflowJob {
            ..Default::default()
        });
        initial_jobs.push(check_job_key.clone());
    }
    // Get Directory information
    let members = check_workspace(CheckWorkspaceOptions::new(), working_directory).await?;
    for (member_key, member) in members.0 {
        let mut needs = match options.depends_on_template_jobs {
            true => initial_jobs.clone(),
            false => vec![],
        };
        for dependency in member.dependencies {
            needs.push(format!("{}", dependency.package))
        }
        let mut job_if = "always() && !contains(needs.*.result, 'failure') && !contains(needs.*.result, 'cancelled')".to_string();
        let is_publish = "github.event_name == 'push' || (github.event_name == 'workflow_dispatch' && inputs.publish)".to_string();
        if options.inject_check_changed_and_publish {
            job_if = format!("{} && ((fromJSON(needs.{}.outputs.output).{}.changed) || (fromJSON(needs.{}.outputs.output.{}.publish && {})))", job_if, &check_job_key, member_key, &check_job_key, member_key, is_publish.clone());
        }
        let mut with: IndexMap<String, Value> = IndexMap::new();
        with.insert("publish".to_string(), format!("${{ {} }})", is_publish.clone()).into());
        with.insert("working_directory".to_string(), member.path.to_string_lossy().to_string().into());
        if member.publish_detail.cargo.publish {
            if member.publish_detail.cargo.allow_public && member.publish_detail.cargo.registry.is_none() {
                with.insert("publish_public_registry".to_string(), "true".to_string().into());
            } else {
                with.insert("publish_private_registry".to_string(), "true".to_string().into());
            }
        }
        if member.publish_detail.docker.publish {
            with.insert("publish_docker".to_string(), "true".to_string().into());
        }
        if member.publish_detail.npm_napi.publish {
            with.insert("publish_npm_napi".to_string(), "true".to_string().into());
        }
        if member.publish_detail.binary {
            with.insert("publish_binary".to_string(), "true".to_string().into());
        }
        if let Some(args) = member.publish_detail.args {
            for (k, v) in args {
                with.insert(k, v);
            }
        }
        let new_job = GithubWorkflowJob {
            name: Some(format!("Test and Publish {}: {}", member.workspace, member.package)),
            uses: Some("ForesightMiningSoftwareCorporation/github/.github/workflows/rust-build.yml@v1".to_string()),
            needs: Some(needs),
            job_if: Some(job_if),
            with: Some(with),
            secrets: Some(GithubWorkflowJobSecret {
                inherit: true,
                secrets: None,
            }),
            ..Default::default()
        };
        workflow_template.jobs.insert(format!("{}", member.package), new_job);
    }
    let output_file = File::create(options.output)?;
    let mut writer = BufWriter::new(output_file);
    serde_yaml::to_writer(&mut writer, &workflow_template)?;
    Ok(GenerateResult {})
}
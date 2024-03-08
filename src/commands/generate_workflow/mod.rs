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

use itertools::Itertools;
use publish_workflow::PublishWorkflowArgs;

use crate::commands::check_workspace::{check_workspace, Options as CheckWorkspaceOptions};
use crate::commands::generate_workflow::test_workflow::TestWorkflowArgs;
use crate::utils::{deserialize_opt_string_or_map, deserialize_opt_string_or_struct, FromMap};

mod publish_workflow;
mod test_workflow;

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

const CHECK_SCRIPT: &str = r#"BASE_REF=${{ github.base_ref }}
HEAD_REF=${{ github.head_ref }}
if [ -z "$HEAD_REF" ]; then
  CHECK_CHANGED=()
else
  CHECK_CHANGED=('--check-changed' '--changed-base-ref' 'origin/${{ github.base_ref }}' '--changed-head-ref' '${{ github.head_ref }}')
  git fetch origin ${{ github.base_ref }} --depth 1
fi
echo workspace=$(fslabscli check-workspace --json --check-publish "${CHECK_CHANGED[@]}") >> $GITHUB_OUTPUT"#;

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
    #[arg(long, default_value_t = false)]
    cargo_default_publish: bool,
    #[arg(long, default_value = "standard")]
    nomad_runner_label: String,
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

#[derive(Clone, Default, Debug)]
pub struct StringBool(bool);

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
    // Get Directory information
    let members = check_workspace(
        Box::new(
            CheckWorkspaceOptions::new().with_cargo_default_publish(options.cargo_default_publish),
        ),
        working_directory,
    )
    .await?;
    if !options.no_check_changed_and_publish {
        // We need to login to any docker registry required
        let mut registries_steps: Vec<GithubWorkflowJobSteps> = members
            .0
            .iter()
            .filter(|(_, v)| v.publish_detail.docker.publish)
            .unique_by(|(_, v)| v.publish_detail.docker.repository.clone())
            .filter_map(|(_, v)| {
                if let Some(repo) = v.publish_detail.docker.repository.clone() {
                    let github_secret_key = repo.clone().replace('.', "_").to_ascii_uppercase();
                    return Some(GithubWorkflowJobSteps {
                        name: Some(format!("Login to {}", repo)),
                        uses: Some("docker/login-action@v3".to_string()),
                        with: Some(IndexMap::from([
                            ("registry".to_string(), repo.clone()),
                            (
                                "username".to_string(),
                                format!("${{{{ secrets.DOCKER_{}_USERNAME }}}}", github_secret_key),
                            ),
                            (
                                "password".to_string(),
                                format!("${{{{ secrets.DOCKER_{}_PASSWORD }}}}", github_secret_key),
                            ),
                        ])),
                        ..Default::default()
                    });
                }
                None
            })
            .collect();
        initial_jobs.push(check_job_key.clone());
        let steps = vec![
            GithubWorkflowJobSteps {
                name: Some("Generate token".to_string()),
                id: Some("generate_token".to_string()),
                uses: Some("tibdex/github-app-token@v2.1.0".to_string()),
                with: Some(IndexMap::from([
                    (
                        "app_id".to_string(),
                        "${{ secrets.FMSC_BOT_GITHUB_APP_ID }}".to_string(),
                    ),
                    (
                        "private_key".to_string(),
                        "${{ secrets.FMSC_BOT_GITHUB_APP_PRIVATE_KEY }}".to_string(),
                    ),
                ])),
                ..Default::default()
            },
            GithubWorkflowJobSteps {
                name: Some("Install FSLABScli".to_string()),
                uses: Some("ForesightMiningSoftwareCorporation/fslabscli-action@v1".to_string()),
                with: Some(IndexMap::from([(
                    "token".to_string(),
                    "${{ steps.generate_token.outputs.token }}".to_string(),
                )])),
                ..Default::default()
            },
            GithubWorkflowJobSteps {
                name: Some("Checkout repo".to_string()),
                uses: Some("actions/checkout@v4".to_string()),
                with: Some(IndexMap::from([(
                    "ref".to_string(),
                    "${{ github.head_ref }}".to_string(),
                )])),
                ..Default::default()
            },
            GithubWorkflowJobSteps {
                name: Some("Check workspace".to_string()),
                working_directory: Some(".".to_string()),
                id: Some("check_workspace".to_string()),
                shell: Some("bash".to_string()),
                run: Some(CHECK_SCRIPT.to_string()),
                ..Default::default()
            },
        ];
        registries_steps.extend(steps);
        workflow_template.jobs.insert(
            check_job_key.clone(),
            GithubWorkflowJob {
                name: Some(
                    "Check which workspace member changed and / or needs publishing".to_string(),
                ),
                runs_on: Some(vec![
                    "self-hosted".to_string(),
                    options.nomad_runner_label,
                    "${{ github.run_id }}__check_changed__${{ github.run_attempt }}".to_string(),
                ]),
                outputs: Some(IndexMap::from([(
                    "workspace".to_string(),
                    "${{ steps.check_workspace.outputs.workspace }}".to_string(),
                )])),
                steps: Some(registries_steps),
                ..Default::default()
            },
        );
    }
    let mut member_keys: Vec<String> = members.0.keys().cloned().collect();
    member_keys.sort();
    let mut actual_tests: Vec<String> = vec![];
    for member_key in member_keys {
        let Some(member) = members.0.get(&member_key) else {
            continue;
        };
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
            if dependency.publishable {
                // Can this really be?
                publish_needs.push(format!("publish_{}", dependency.package))
            }
        }
        // add self test to publish needs
        if !member.test_detail.skip.unwrap_or(false) {
            publish_needs.push(test_job_key.clone());
        }
        let base_if = "always() && !contains(needs.*.result, 'failure') && !contains(needs.*.result, 'cancelled')".to_string();
        let mut publish_if = format!("{} && (github.event_name == 'push' || (github.event_name == 'workflow_dispatch' && inputs.publish))", base_if);
        let mut test_if = base_if.clone();
        if !options.no_check_changed_and_publish {
            publish_if = format!(
                "{} && (fromJSON(needs.{}.outputs.workspace).{}.publish)",
                publish_if, &check_job_key, member_key
            );
            test_if = format!(
                "{} && (fromJSON(needs.{}.outputs.workspace).{}.changed)",
                test_if, &check_job_key, member_key,
            );
        }
        let cargo_publish_options: PublishWorkflowArgs = match member.publish_detail.args.clone() {
            Some(a) => a.into(),
            None => Default::default(),
        };
        let cargo_test_options: TestWorkflowArgs = match member.test_detail.args.clone() {
            Some(a) => a.into(),
            None => Default::default(),
        };
        let job_working_directory = member.path.to_string_lossy().to_string();
        let publish_with: PublishWorkflowArgs = PublishWorkflowArgs {
            working_directory: Some(job_working_directory.clone()),
            skip_test: Some(StringBool(member.test_detail.skip.unwrap_or(false))),
            publish: Some(StringBool(member.publish)),
            publish_private_registry: Some(StringBool(
                member.publish_detail.cargo.publish
                    && !(member.publish_detail.cargo.allow_public
                        && (member
                            .publish_detail
                            .cargo
                            .registry
                            .clone()
                            .unwrap_or(vec!["public".to_string()])
                            == vec!["public"])),
            )),
            publish_public_registry: Some(StringBool(
                member.publish_detail.cargo.publish
                    && (member.publish_detail.cargo.allow_public
                        && (member
                            .publish_detail
                            .cargo
                            .registry
                            .clone()
                            .unwrap_or(vec!["public".to_string()])
                            == vec!["public"])),
            )),
            publish_docker: Some(StringBool(member.publish_detail.docker.publish)),
            docker_image: match member.publish_detail.docker.publish {
                true => Some(member.package.clone()),
                false => None,
            },
            docker_registry: match member.publish_detail.docker.publish {
                true => member.publish_detail.docker.repository.clone(),
                false => None,
            },
            publish_npm_napi: Some(StringBool(member.publish_detail.npm_napi.publish)),
            publish_binary: Some(StringBool(member.publish_detail.binary)),
            ..Default::default()
        }
        .merge(cargo_publish_options.clone());
        let test_with: TestWorkflowArgs = TestWorkflowArgs {
            working_directory: Some(job_working_directory),
            test_publish_required: Some(StringBool(member.publish_detail.cargo.publish)),
            ..Default::default()
        }
        .merge(cargo_test_options.clone());

        let test_job = GithubWorkflowJob {
            name: Some(format!("Test {}: {}", member.workspace, member.package)),
            uses: Some(format!(
                "ForesightMiningSoftwareCorporation/github/.github/workflows/rust-test.yml@{}",
                options.build_workflow_version
            )),
            needs: Some(test_needs),
            job_if: Some(format!("${{{{ {} }}}}", test_if)),
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
                format!(
                    "ForesightMiningSoftwareCorporation/github/.github/workflows/rust-build.yml@{}",
                    options.build_workflow_version
                )
                .to_string(),
            ),
            needs: Some(publish_needs),
            job_if: Some(format!("${{{{ {} }}}}", publish_if)),
            with: Some(publish_with.into()),
            secrets: Some(GithubWorkflowJobSecret {
                inherit: true,
                secrets: None,
            }),
            ..Default::default()
        };

        if !member.test_detail.skip.unwrap_or(false) {
            workflow_template
                .jobs
                .insert(test_job_key.clone(), test_job);
            actual_tests.push(test_job_key.clone());
        }
        if member.publish {
            workflow_template.jobs.insert(publish_job_key, publish_job);
        }
    }
    // Add Tests Reporting
    workflow_template.jobs.insert("test_results".to_string(), GithubWorkflowJob {
        name: Some("Tests Results".to_string()),
        job_if: Some("always()".to_string()),
            uses: Some(
                format!(
                    "ForesightMiningSoftwareCorporation/github/.github/workflows/check_summaries.yml@{}",
                    options.build_workflow_version
                )
            ),
        with: Some(IndexMap::from([("run_type".to_string(), "checks".into())])),
        secrets: Some(GithubWorkflowJobSecret {
            inherit: true,
            secrets: None,
        }),
        needs: Some(actual_tests),

        ..Default::default()
    });
    let output_file = File::create(options.output)?;
    let mut writer = BufWriter::new(output_file);
    serde_yaml::to_writer(&mut writer, &workflow_template)?;
    Ok(GenerateResult {})
}

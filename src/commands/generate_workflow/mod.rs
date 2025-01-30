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
use serde::ser::{SerializeMap, SerializeSeq, SerializeStruct};
use serde::{Deserialize, Serialize, Serializer};
use serde_with::{formats::PreferOne, serde_as, OneOrMany};
use serde_yaml::Value;
use void::Void;

use itertools::Itertools;

use crate::commands::check_workspace::{check_workspace, Options as CheckWorkspaceOptions};
use crate::utils::{deserialize_opt_string_or_map, deserialize_opt_string_or_struct, FromMap};
use crate::PrettyPrintable;

use self::workflows::publish_docker::PublishDockerWorkflow;
use self::workflows::publish_npm_napi::PublishNpmNapiWorkflow;
use self::workflows::publish_rust_binary::PublishRustBinaryWorkflow;
use self::workflows::publish_rust_installer::PublishRustInstallerWorkflow;
use self::workflows::publish_rust_registry::PublishRustRegistryWorkflow;
use self::workflows::Workflow;

use super::check_workspace::Results as Members;

mod workflows;

const EMPTY_WORKFLOW: &str = r#"
concurrency:
  group: ${{ github.workflow }}-${{ github.head_ref || github.run_id }}
  cancel-in-progress: true

jobs:
"#;

const CHECK_SCRIPT: &str = r#"if [ -z "${HEAD_REF}" ]; then
  CHECK_CHANGED=()
else
  CHECK_CHANGED=('--check-changed' '--changed-base-ref' "origin/${BASE_REF}" '--changed-head-ref' "${HEAD_REF}")
  git fetch origin ${BASE_REF} --depth 1
fi
workspace=$(fslabscli check-workspace --json --check-publish "${CHECK_CHANGED[@]}" --binary-store-storage-account ${{ vars.BINARY_STORE_STORAGE_ACCOUNT }} --binary-store-container-name ${{ vars.BINARY_STORE_CONTAINER_NAME }} --binary-store-access-key ${{ secrets.BINARY_STORE_ACCESS_KEY }} --cargo-default-publish --cargo-registry foresight-mining-software-corporation --cargo-registry-url https://shipyard.rs/api/v1/shipyard/krates/by-name/ --cargo-registry-user-agent "shipyard ${{ secrets.CARGO_PRIVATE_REGISTRY_TOKEN }}")
if [ $? -ne 0 ]; then
  echo "Could not check workspace"
  exit 1
fi
echo workspace=${workspace} >> $GITHUB_OUTPUT"#;

#[derive(Debug, Parser)]
#[command(about = "Check directory for crates that need to be published.")]
pub struct Options {
    #[arg(long)]
    template: Option<PathBuf>,
    #[arg(long, default_value_t = false)]
    no_depends_on_template_jobs: bool,
    #[arg(long, default_value = "main")]
    build_workflow_version: String,
    #[arg(long, default_value = "2.7.0")]
    fslabscli_version: String,
    #[arg(long, default_value_t = false)]
    cargo_default_publish: bool,
    #[arg(long, default_value = "0 19 * * *")]
    nightly_cron_schedule: String,
    /// Default branch to consider for publishing and schedule release
    #[arg(long, default_value = "main")]
    default_branch: String,
}

#[derive(Serialize)]
pub struct GenerateResult {}

impl Display for GenerateResult {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "")
    }
}

impl PrettyPrintable for GenerateResult {
    fn pretty_print(&self) -> String {
        format!("{}", self)
    }
}

#[derive(Serialize, Debug, Deserialize, PartialEq, Clone)]
pub struct GithubWorkflowInput {
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(rename = "type")]
    pub input_type: String,
}

#[derive(Serialize, Debug, Deserialize, PartialEq, Clone)]
pub struct GithubWorkflowSecret {
    pub description: String,
    #[serde(default)]
    pub required: bool,
}

#[derive(Serialize, Debug, Deserialize, PartialEq, Clone)]
pub struct GithubWorkflowCron {
    pub cron: String,
}

#[derive(Debug, Deserialize, PartialEq, Clone, Default)]
pub struct GithubWorkflowTriggerPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branches: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inputs: Option<IndexMap<String, GithubWorkflowInput>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secrets: Option<IndexMap<String, GithubWorkflowSecret>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crons: Option<Vec<GithubWorkflowCron>>,
}

impl Serialize for GithubWorkflowTriggerPayload {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if let Some(crons) = self.crons.clone() {
            let mut seq = serializer.serialize_seq(Some(crons.len()))?;
            for e in crons {
                seq.serialize_element(&e)?;
            }
            seq.end()
        } else {
            let mut state = serializer.serialize_struct("GithubWorkflowTriggerPayload", 5)?;
            if let Some(branches) = &self.branches {
                state.serialize_field("branches", branches)?;
            }
            if let Some(tags) = &self.tags {
                state.serialize_field("tags", tags)?;
            }
            if let Some(paths) = &self.paths {
                state.serialize_field("paths", paths)?;
            }
            if let Some(inputs) = &self.inputs {
                state.serialize_field("inputs", inputs)?;
            }
            if let Some(secrets) = &self.secrets {
                state.serialize_field("secrets", secrets)?;
            }
            state.end()
        }
    }
}

#[derive(Serialize, Debug, Deserialize, Eq, PartialEq, Hash, Ord, PartialOrd, Clone)]
#[serde(rename_all = "snake_case")]
pub enum GithubWorkflowTrigger {
    PullRequest,
    Push,
    WorkflowCall,
    WorkflowDispatch,
    Schedule,
}

#[derive(Debug, Default, Deserialize, Eq, PartialEq, Clone)]
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

#[derive(Serialize, Debug, Default, Deserialize, Eq, PartialEq, Clone)]
pub struct GithubWorkflowJobEnvironment {
    pub name: String,
    pub url: Option<String>,
}

#[derive(Serialize, Debug, Default, Deserialize, Eq, PartialEq, Clone)]
#[serde(rename_all = "snake_case")]
pub struct GithubWorkflowJobStrategy {
    pub matrix: IndexMap<String, Value>,
    pub fail_false: Option<bool>,
}

#[derive(Serialize, Debug, Default, Deserialize, Eq, PartialEq, Clone)]
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

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
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
#[derive(Serialize, Deserialize, Debug, Default, Clone)]
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

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "kebab-case")]
struct GithubWorkflowDefaultsRun {
    pub shell: Option<String>,
    pub working_directory: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct GithubWorkflowDefaults {
    pub run: GithubWorkflowDefaultsRun,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
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

fn get_base_workflow(template: &Option<PathBuf>, name: String) -> anyhow::Result<GithubWorkflow> {
    let mut workflow: GithubWorkflow = (match template {
        Some(template) => {
            let file = File::open(template)?;
            let reader = BufReader::new(file);
            serde_yaml::from_reader(reader)
        }
        None => serde_yaml::from_str(EMPTY_WORKFLOW),
    })
    .with_context(|| "Could not parse workflow template")?;
    workflow.name = Some(name);
    Ok(workflow)
}

fn get_publish_triggers(
    default_branch: String,
) -> IndexMap<GithubWorkflowTrigger, GithubWorkflowTriggerPayload> {
    IndexMap::from([
        (
            GithubWorkflowTrigger::Push,
            GithubWorkflowTriggerPayload {
                branches: Some(vec![default_branch]),
                tags: Some(vec![
                    "*-alpha-*.*.*".to_string(),
                    "*-beta-*.*.*".to_string(),
                    "*-prod-*.*.*".to_string(),
                ]),
                ..Default::default()
            },
        ),
        (
            GithubWorkflowTrigger::WorkflowDispatch,
            GithubWorkflowTriggerPayload {
                inputs: Some(IndexMap::from([(
                    "publish".to_string(),
                    GithubWorkflowInput {
                        description: "Trigger with publish".to_string(),
                        default: None,
                        required: false,
                        input_type: "boolean".to_string(),
                    },
                )])),
                ..Default::default()
            },
        ),
    ])
}

fn get_check_workspace_job(
    members: &Members,
    required_jobs: Vec<String>,
    fslabscli_version: &str,
) -> GithubWorkflowJob {
    // For each package published to docker, we need to login to the registry in order to check if the package needs publishing
    let docker_steps: Vec<GithubWorkflowJobSteps> = members
        .0
        .iter()
        .filter(|(_, v)| v.publish_detail.docker.publish)
        .unique_by(|(_, v)| v.publish_detail.docker.repository.clone())
        .filter_map(|(_, v)| {
            if let Some(repo) = v.publish_detail.docker.repository.clone() {
                let github_secret_key = repo.clone().replace('.', "_").to_ascii_uppercase();
                return Some(GithubWorkflowJobSteps {
                    name: Some(format!("Docker Login to {}", repo)),
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
    // For each packaeg published to npm, we need to login to the npm registry
    let npm_steps: Vec<GithubWorkflowJobSteps> = members
        .0
        .iter()
        .filter(|(_, v)| v.publish_detail.npm_napi.publish)
        .unique_by(|(_, v)| v.publish_detail.npm_napi.scope.clone())
        .filter_map(|(_, v)| {
            if let Some(scope) = v.publish_detail.npm_napi.scope.clone() {
                let github_secret_key = scope.clone().replace('.', "_").to_ascii_uppercase();
                let run = format!(
                    r#"
echo "@{scope}:registry=https://npm.pkg.github.com/" >> ~/.npmrc
echo "//npm.pkg.github.com/:_authToken=${{{{ secrets.NPM_{github_secret_key}_TOKEN }}}}" >> ~/.npmrc
                    "#
                );
                return Some(GithubWorkflowJobSteps {
                    name: Some(format!("NPM Login to {}", scope)),
                    shell: Some("bash".to_string()),
                    run: Some(run.to_string()),
                    ..Default::default()
                });
            }
            None
        })
        .collect();
    let mut install_fslabscli_args = IndexMap::from([(
        "token".to_string(),
        "${{ secrets.GITHUB_TOKEN }}".to_string(),
    )]);
    install_fslabscli_args.insert("version".to_string(), fslabscli_version.to_string());
    let steps = vec![
        GithubWorkflowJobSteps {
            name: Some("Install FSLABScli".to_string()),
            uses: Some("ForesightMiningSoftwareCorporation/fslabscli-action@v1".to_string()),
            with: Some(install_fslabscli_args),
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
            env: Some(IndexMap::from([
                ("BASE_REF".to_string(), "${{ github.base_ref }}".to_string()),
                ("HEAD_REF".to_string(), "${{ github.head_ref }}".to_string()),
            ])),
            run: Some(CHECK_SCRIPT.to_string()),
            ..Default::default()
        },
    ];
    GithubWorkflowJob {
        name: Some("Check which workspace member changed and / or needs publishing".to_string()),
        runs_on: Some(vec!["ubuntu-latest".to_string()]),
        needs: Some(required_jobs),
        outputs: Some(IndexMap::from([(
            "workspace".to_string(),
            "${{ steps.check_workspace.outputs.workspace }}".to_string(),
        )])),
        steps: Some([docker_steps, npm_steps, steps].concat()),
        ..Default::default()
    }
}

pub async fn generate_workflow(
    options: Box<Options>,
    working_directory: PathBuf,
) -> anyhow::Result<GenerateResult> {
    // Get Directory information
    let check_workspace_options =
        CheckWorkspaceOptions::new().with_cargo_default_publish(options.cargo_default_publish);
    let members = check_workspace(Box::new(check_workspace_options), working_directory.clone())
        .await
        .with_context(|| "Could not get directory information")?;
    // Get workflows, useful in case where additional tools need to be run before
    let mut publish_workflow =
        get_base_workflow(&options.template, "CI - CD: Publishing".to_string())?;

    // Triggers
    let mut publish_triggers = get_publish_triggers(options.default_branch);
    // If we have binaries to publish,  nightly Publish should be done every night at 3AM
    if members
        .0
        .values()
        .any(|r| r.publish_detail.binary.publish || r.publish_detail.binary.installer.publish)
    {
        publish_triggers.insert(
            GithubWorkflowTrigger::Schedule,
            GithubWorkflowTriggerPayload {
                crons: Some(vec![GithubWorkflowCron {
                    cron: options.nightly_cron_schedule.clone(),
                }]),
                ..Default::default()
            },
        );
    }
    publish_workflow.triggers = Some(publish_triggers);

    // Create check_workspace job, and add it to both workflows
    let check_job_key = "check_changed_and_publish".to_string();
    let check_workspace_job = get_check_workspace_job(
        &members,
        publish_workflow.jobs.keys().cloned().collect(),
        &options.fslabscli_version,
    );
    publish_workflow
        .jobs
        .insert(check_job_key.clone(), check_workspace_job);

    let mut member_keys: Vec<String> = members.0.keys().cloned().collect();
    member_keys.sort();
    let base_if = "!cancelled() && !contains(needs.*.result, 'failure') && !contains(needs.*.result, 'cancelled')".to_string();

    let mut actual_publishings: Vec<String> = vec![];
    for member_key in member_keys {
        let Some(member) = members.0.get(&member_key) else {
            continue;
        };
        let working_directory = member.path.to_string_lossy().to_string();
        let dynamic_value_base = format!(
            "(fromJSON(needs.{}.outputs.workspace).{}",
            check_job_key, member_key
        );

        let mut testing_requirements: Vec<String> = vec![check_job_key.clone()];
        let mut publishing_requirements: Vec<String> = vec![check_job_key.clone()];

        for dependency in &member.dependencies {
            // Each testing job needs to depends on its'previous testing job
            if let Some(package_name) = dependency.package.clone() {
                testing_requirements.push(format!("test_{}", package_name));
                if dependency.publishable {
                    if let Some(dependency_package) = members.0.get(&package_name) {
                        if dependency_package.publish_detail.binary.publish {
                            publishing_requirements
                                .push(format!("publish_rust_binary_{}", package_name,));
                        }
                        if dependency_package.publish_detail.binary.installer.publish {
                            publishing_requirements
                                .push(format!("publish_rust_installer_{}", package_name,));
                        }
                        if dependency_package.publish_detail.cargo.publish {
                            publishing_requirements
                                .push(format!("publish_rust_registry_{}", package_name,));
                        }
                        if dependency_package.publish_detail.docker.publish {
                            publishing_requirements
                                .push(format!("publish_docker_{}", package_name,));
                        }
                        if dependency_package.publish_detail.npm_napi.publish {
                            publishing_requirements
                                .push(format!("publish_npm_napi_{}", package_name,));
                        }
                    }
                }
            }
        }
        let mut member_workflows: Vec<Box<dyn Workflow>> = vec![];
        if member.publish {
            if member.publish_detail.binary.publish {
                let windows_working_directory = match working_directory == "." {
                    //true => "".to_string(),
                    true => working_directory.clone(),
                    false => working_directory.clone(),
                };
                member_workflows.push(Box::new(PublishRustBinaryWorkflow::new(
                    member_key.clone(),
                    member.publish_detail.binary.targets.clone(),
                    member.publish_detail.additional_args.clone(),
                    member.publish_detail.binary.sign,
                    windows_working_directory.clone(),
                    &dynamic_value_base,
                )));
                if member.publish_detail.binary.installer.publish {
                    member_workflows.push(Box::new(PublishRustInstallerWorkflow::new(
                        member_key.clone(),
                        windows_working_directory.clone(),
                        member.publish_detail.binary.sign,
                        &dynamic_value_base,
                    )));
                }
            }
            if member.publish_detail.docker.publish {
                member_workflows.push(Box::new(PublishDockerWorkflow::new(
                    member_key.clone(),
                    member_key.clone(),
                    working_directory.clone(),
                    member.publish_detail.docker.context.clone(),
                    member.publish_detail.docker.dockerfile.clone(),
                    member.publish_detail.docker.repository.clone(),
                    &dynamic_value_base,
                )));
            }
            if member.publish_detail.npm_napi.publish {
                member_workflows.push(Box::new(PublishNpmNapiWorkflow::new(
                    member_key.clone(),
                    working_directory.clone(),
                    &dynamic_value_base,
                )));
            }
            if member.publish_detail.cargo.publish {
                member_workflows.push(Box::new(PublishRustRegistryWorkflow::new(
                    member_key.clone(),
                    working_directory.clone(),
                    &dynamic_value_base,
                )));
            }
        }

        let publishing_if = format!("{} && (github.event_name == 'push' || (github.event_name == 'workflow_dispatch' && inputs.publish))", base_if);

        for publishing_job in member_workflows.iter() {
            let mut needs = publishing_requirements.clone();
            if let Some(additional_keys) = publishing_job.get_additional_dependencies() {
                needs.extend(additional_keys);
            }
            let job = GithubWorkflowJob {
                name: Some(format!(
                    "{} {}: {}",
                    publishing_job.job_label(),
                    member.workspace,
                    member.package
                )),
                needs: Some(needs),
                uses: Some(
                    format!(
                        "ForesightMiningSoftwareCorporation/github/.github/workflows/{}.yml@{}",
                        publishing_job.workflow_name(),
                        options.build_workflow_version
                    )
                    .to_string(),
                ),
                job_if: Some(format!(
                    "${{{{ {} && {}.publish_detail.{}.publish) }}}}",
                    publishing_if,
                    dynamic_value_base,
                    publishing_job.publish_info_key()
                )),
                with: Some(publishing_job.get_inputs()),
                secrets: Some(GithubWorkflowJobSecret {
                    inherit: true,
                    secrets: None,
                }),
                ..Default::default()
            };
            let key = format!("{}_{}", publishing_job.job_prefix_key(), member.package);
            actual_publishings.push(key.clone());
            publish_workflow.jobs.insert(key, job);
        }
    }
    // Add Reporting
    // Publishing
    let mut publishing_reporting_needs = vec![check_job_key.clone()];
    publishing_reporting_needs.extend(actual_publishings);
    publish_workflow.jobs.insert("publishing_results".to_string(), GithubWorkflowJob {
        name: Some("Publishing Results".to_string()),
        job_if: Some("always() && !contains(needs.*.result, 'cancelled')".to_string()),
            uses: Some(
                format!(
                    "ForesightMiningSoftwareCorporation/github/.github/workflows/check_summaries.yml@{}",
                    options.build_workflow_version
                )
            ),
        with: Some(
            IndexMap::from([
                ("run_type".to_string(), "publishing".into()),
                ("check_changed_outcome".to_string(), format!("${{{{ needs.{}.result }}}}", check_job_key).into()),
                ("fslabscli_version".to_string(), options.fslabscli_version.clone().into())
            ])
        ),
        secrets: Some(GithubWorkflowJobSecret {
            inherit: true,
            secrets: None,
        }),
        needs: Some(publishing_reporting_needs),
        ..Default::default()
    });
    // If we are splitted then we actually need to create two files
    let release_file_path = working_directory.join(".github/workflows/release_publish.yml");
    let release_file = File::create(release_file_path)?;
    let mut release_writer = BufWriter::new(release_file);
    serde_yaml::to_writer(&mut release_writer, &publish_workflow)?;
    Ok(GenerateResult {})
}

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
echo workspace=$(fslabscli check-workspace --json --check-publish "${CHECK_CHANGED[@]}" --binary-store-storage-account ${{ secrets.BINARY_STORE_STORAGE_ACCOUNT }} --binary-store-container-name ${{ secrets.BINARY_STORE_CONTAINER_NAME }} --binary-store-access-key ${{ secrets.BINARY_STORE_ACCESS_KEY }} --cargo-default-publish --cargo-registry foresight-mining-software-corporation --cargo-registry-url https://shipyard.rs/api/v1/shipyard/krates/by-name/ --cargo-registry-user-agent "shipyard ${{ secrets.CARGO_PRIVATE_REGISTRY_TOKEN }}") >> $GITHUB_OUTPUT"#;

#[derive(Debug, Parser)]
#[command(about = "Check directory for crates that need to be published.")]
pub struct Options {
    #[arg(long)]
    output: PathBuf,
    #[arg(long)]
    output_release: Option<PathBuf>,
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
    #[arg(long, default_value_t = false)]
    test_publish_required_disabled: bool,
}

#[derive(Serialize)]
pub struct GenerateResult {}

impl Display for GenerateResult {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "")
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
}

#[derive(Serialize, Debug, Deserialize, Eq, PartialEq, Hash, Ord, PartialOrd, Clone)]
#[serde(rename_all = "snake_case")]
pub enum GithubWorkflowTrigger {
    PullRequest,
    Push,
    WorkflowCall,
    WorkflowDispatch,
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

pub async fn generate_workflow(
    options: Box<Options>,
    working_directory: PathBuf,
) -> anyhow::Result<GenerateResult> {
    // Get Base Workflow
    let workflow_template: GithubWorkflow = match options.template {
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
    let mut publish_workflow = workflow_template.clone();
    let mut test_workflow = workflow_template;
    let split_workflows = options.output_release.is_some();

    // Triggers
    let mut test_triggers: IndexMap<GithubWorkflowTrigger, GithubWorkflowTriggerPayload> =
        IndexMap::new();
    let mut publish_triggers: IndexMap<GithubWorkflowTrigger, GithubWorkflowTriggerPayload> =
        IndexMap::new();
    // Tests should be done on pr always
    test_triggers.insert(
        GithubWorkflowTrigger::PullRequest,
        GithubWorkflowTriggerPayload {
            branches: None,
            tags: None,
            paths: None,
            inputs: None,
            secrets: None,
        },
    );
    // Publish should be done on push to main
    publish_triggers.insert(
        GithubWorkflowTrigger::Push,
        GithubWorkflowTriggerPayload {
            branches: Some(vec!["main".to_string()]),
            tags: Some(vec![
                "*-alpha-*.*.*".to_string(),
                "*-beta-*.*.*".to_string(),
                "*-prod-*.*.*".to_string(),
            ]),
            paths: None,
            inputs: None,
            secrets: None,
        },
    );
    // Publish should be done on manual dispatch
    publish_triggers.insert(
        GithubWorkflowTrigger::WorkflowDispatch,
        GithubWorkflowTriggerPayload {
            branches: None,
            tags: None,
            paths: None,
            inputs: Some(IndexMap::from([(
                "publish".to_string(),
                GithubWorkflowInput {
                    description: "Trigger with publish".to_string(),
                    default: None,
                    required: false,
                    input_type: "boolean".to_string(),
                },
            )])),
            secrets: None,
        },
    );
    if split_workflows {
        test_workflow.name = Some("CI - CD: Tests".to_string());
        publish_workflow.name = Some("CI - CD: Publishing".to_string());
    } else {
        test_workflow.name = Some("CI - CD: Tests and Publishing".to_string());
        test_triggers.extend(publish_triggers.clone());
    }
    test_workflow.triggers = Some(test_triggers);
    publish_workflow.triggers = Some(publish_triggers);

    //
    // Get Template jobs, we'll make the generated jobs depends on it
    let mut initial_jobs: Vec<String> = test_workflow.jobs.keys().cloned().collect();
    // If we need to test for changed and publish
    let check_job_key = "check_changed_and_publish".to_string();
    // Get Directory information
    let members = check_workspace(
        Box::new(
            CheckWorkspaceOptions::new().with_cargo_default_publish(options.cargo_default_publish),
        ),
        working_directory,
    )
    .await
    .map_err(|e| {
        log::error!("Unparseable template: {}", e);
        e
    })?;
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
        registries_steps.extend(npm_steps);
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
        let check_job = GithubWorkflowJob {
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
        };
        test_workflow
            .jobs
            .insert(check_job_key.clone(), check_job.clone());
        publish_workflow
            .jobs
            .insert(check_job_key.clone(), check_job);
    }
    let mut member_keys: Vec<String> = members.0.keys().cloned().collect();
    member_keys.sort();
    let base_if = "!cancelled() && !contains(needs.*.result, 'failure') && !contains(needs.*.result, 'cancelled')".to_string();
    let mut actual_tests: Vec<String> = vec![];
    for member_key in member_keys {
        let Some(member) = members.0.get(&member_key) else {
            continue;
        };
        let test_job_key = format!("test_{}", member.package);
        let publish_job_key = format!("publish_{}", member.package);
        let mut test_needs = match options.no_depends_on_template_jobs {
            false => initial_jobs.clone(),
            true => vec![check_job_key.clone()],
        };
        for dependency in &member.dependencies {
            test_needs.push(format!("test_{}", dependency.package))
        }
        let mut publish_needs = match options.no_depends_on_template_jobs {
            false => initial_jobs.clone(),
            true => vec![check_job_key.clone()],
        };
        for dependency in &member.dependencies {
            if dependency.publishable {
                // Can this really be?
                publish_needs.push(format!("publish_{}", dependency.package))
            }
        }
        // add self test to publish needs and not split
        if !member.test_detail.skip.unwrap_or(false) && !split_workflows {
            publish_needs.push(test_job_key.clone());
        }
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
        let publish_private_registry = Some(
            match member.publish_detail.cargo.publish
                && !(member.publish_detail.cargo.allow_public
                    && (member
                        .publish_detail
                        .cargo
                        .registry
                        .clone()
                        .unwrap_or(vec!["public".to_string()])
                        == vec!["public"]))
            {
                true => format!(
                    "${{{{ fromJson(needs.{}.outputs.workspace).{}.publish_detail.cargo.publish }}}}",
                    &check_job_key, member_key
                ),
                false => "false".to_string(),
            },
        );
        let publish_public_registry = Some(
            match member.publish_detail.cargo.publish
                && (member.publish_detail.cargo.allow_public
                    && (member
                        .publish_detail
                        .cargo
                        .registry
                        .clone()
                        .unwrap_or(vec!["public".to_string()])
                        == vec!["public"]))
            {
                true => format!(
                    "${{{{ fromJson(needs.{}.outputs.workspace).{}.publish_detail.cargo.publish }}}}",
                    &check_job_key, member_key
                ),
                false => "false".to_string(),
            },
        );
        let publish_docker = Some(match member.publish_detail.docker.publish {
            true => format!(
                "${{{{ fromJson(needs.{}.outputs.workspace).{}.publish_detail.docker.publish }}}}",
                &check_job_key, member_key
            ),
            false => "false".to_string(),
        });
        let publish_npm_napi = Some(match member.publish_detail.npm_napi.publish {
            true => format!(
                "${{{{ fromJson(needs.{}.outputs.workspace).{}.publish_detail.npm_napi.publish }}}}",
                &check_job_key, member_key
            ),
            false => "false".to_string(),
        });
        let publish_binary = Some(match member.publish_detail.binary.publish {
            true => format!(
                "${{{{ fromJson(needs.{}.outputs.workspace).{}.publish_detail.binary.publish }}}}",
                &check_job_key, member_key
            ),
            false => "false".to_string(),
        });
        let publish_installer = Some(match member.publish_detail.binary.installer.publish {
            true => format!(
                "${{{{ fromJson(needs.{}.outputs.workspace).{}.publish_detail.binary.publish }}}}",
                &check_job_key, member_key
            ),
            false => "false".to_string(),
        });
        let publish_with: PublishWorkflowArgs = PublishWorkflowArgs {
            working_directory: Some(job_working_directory.clone()),
            publish: Some(StringBool(member.publish)),
            publish_private_registry,
            publish_public_registry,
            publish_docker,
            publish_npm_napi,
            publish_binary,
            docker_image: match member.publish_detail.docker.publish {
                true => Some(member.package.clone()),
                false => None,
            },
            docker_registry: match member.publish_detail.docker.publish {
                true => member.publish_detail.docker.repository.clone(),
                false => None,
            },
            binary_sign_build: match member.publish_detail.binary.publish {
                true => Some(StringBool(member.publish_detail.binary.sign)),
                false => None,
            },
            binary_application_name: match member.publish_detail.binary.publish {
                true => Some(member.publish_detail.binary.name.clone()),
                false => None,
            },
            binary_targets: match member.publish_detail.binary.publish {
                true => Some(member.publish_detail.binary.targets.clone()),
                false => None,
            },
            ..Default::default()
        }
        .merge(cargo_publish_options.clone());
        let test_with: TestWorkflowArgs = TestWorkflowArgs {
            working_directory: Some(job_working_directory.clone()),
            test_publish_required: Some(StringBool(
                member.publish_detail.cargo.publish && !options.test_publish_required_disabled,
            )),
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
            env: member.test_detail.env.clone(),
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
            env: member.publish_detail.env.clone(),
            secrets: Some(GithubWorkflowJobSecret {
                inherit: true,
                secrets: None,
            }),
            ..Default::default()
        };

        if !member.test_detail.skip.unwrap_or(false) {
            test_workflow.jobs.insert(test_job_key.clone(), test_job);
            actual_tests.push(test_job_key.clone());
        }
        if member.publish {
            let wf = match split_workflows {
                true => &mut publish_workflow,
                false => &mut test_workflow,
            };
            wf.jobs.insert(publish_job_key.clone(), publish_job);
            if member.publish_detail.binary.installer.publish {
                let mut installer_needs = match options.no_depends_on_template_jobs {
                    false => initial_jobs.clone(),
                    true => vec![check_job_key.clone()],
                };
                installer_needs.push(publish_job_key.clone());
                installer_needs.push(format!(
                    "{}_{}",
                    publish_job_key, member.publish_detail.binary.launcher.path
                ));
                // We need to add a new publish job for the installer
                wf.jobs.insert(format!("{}_installer", publish_job_key.clone()), GithubWorkflowJob {
                    name: Some(format!(
                        "Publish {}: {} installer",
                        member.workspace, member.package
                    )),
                    uses: Some(
                        format!("ForesightMiningSoftwareCorporation/github/.github/workflows/rust-build.yml@{}", options.build_workflow_version).to_string(),
                    ),
                    needs: Some(installer_needs),
                    with: Some(
                        PublishWorkflowArgs {
                            publish: Some(StringBool(true)),
                            publish_installer,
                            binary_application_name: Some(member.publish_detail.binary.name.clone()),
            working_directory: Some(job_working_directory.clone()),
            skip_test: Some(StringBool(true)),
                            ..Default::default()
                        }
                        .into(),
                    ),
                    job_if: Some(format!("${{{{ {} }}}}", publish_if)),
                    secrets: Some(GithubWorkflowJobSecret {
                        inherit: true,
                        secrets: None,
                    }),
                    ..Default::default()
                });
            };
        }
    }
    // Add Tests Reporting
    test_workflow.jobs.insert("test_results".to_string(), GithubWorkflowJob {
        name: Some("Tests Results".to_string()),
        job_if: Some("always() && !contains(needs.*.result, 'cancelled')".to_string()),
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
    // If we are splitted then we actually need to create two files
    let output_file = File::create(options.output)?;
    let mut writer = BufWriter::new(output_file);
    serde_yaml::to_writer(&mut writer, &test_workflow)?;
    if let Some(output_path) = options.output_release {
        let output_file = File::create(output_path)?;
        let mut writer = BufWriter::new(output_file);
        serde_yaml::to_writer(&mut writer, &publish_workflow)?;
    }
    Ok(GenerateResult {})
}

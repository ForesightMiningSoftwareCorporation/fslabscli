use std::fmt::{Debug, Display, Formatter, Result as FmtResult};
use std::path::PathBuf;

use clap::Parser;
use http_body_util::BodyExt;
use http_body_util::Empty;
use hyper::body::Bytes;
use hyper::{Method, Request};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client as HyperClient;
use run_types::{JobType, RunTypeOutput};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::hash::Hash;
use strum_macros::EnumString;

use self::run_types::JobResult;
use self::run_types::{
    checks::{CheckJobType, CheckRunOutput},
    publishing::{PublishingJobType, PublishingRunOutput},
    Run,
};
use self::template::{Summary, SummaryTableCell};
use crate::PrettyPrintable;

mod run_types;
mod template;

#[derive(Debug, Parser)]
#[command(about = "Generate summary of github run.")]
pub struct Options {
    #[arg(long, default_value_t, value_enum)]
    run_type: RunTypeOption,
    #[arg(long, env = "GITHUB_STEP_SUMMARY")]
    output: PathBuf,
    #[arg(long, env = "GITHUB_TOKEN")]
    github_token: Option<String>,
    #[arg(long)]
    github_event_name: Option<String>,
    #[arg(long)]
    github_issue_number: Option<u64>,
    #[arg(long)]
    github_repo: Option<String>,
    #[arg(long, default_value_t = false)]
    hide_previous_pr_comment: bool,
    #[arg(long, default_value = "https://ci.fslabs.ca")]
    mining_bot_url: String,
    #[arg(long, default_value_t, value_enum)]
    check_changed_outcome: RunOutcome,
}

#[derive(clap::ValueEnum, Clone, Default, Debug, Serialize)]
enum RunTypeOption {
    #[default]
    Checks,
    Publishing,
}

#[derive(clap::ValueEnum, EnumString, Debug, Clone, Default)]
enum RunType {
    #[default]
    Checks,
    Publishing,
}

#[derive(clap::ValueEnum, Clone, Default, Debug, Serialize, PartialEq)]
enum RunOutcome {
    #[default]
    Success,
    Failure,
    Cancelled,
    Skipped,
}
#[derive(Serialize)]
pub struct SummariesResult {}

impl Display for SummariesResult {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "")
    }
}

impl PrettyPrintable for SummariesResult {
    fn pretty_print(&self) -> String {
        format!("{}", self)
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, Copy)]
#[serde(rename_all = "kebab-case")]
pub enum CheckOutcome {
    Success,
    Failure,
    Cancelled,
    Skipped,
}

impl CheckOutcome {
    pub fn is_passing(&self) -> bool {
        !matches!(self, &Self::Failure)
    }
}

impl Display for CheckOutcome {
    fn fmt(&self, f: &mut Formatter) -> FmtResult {
        match self {
            CheckOutcome::Success => write!(f, "✅"),
            CheckOutcome::Failure => write!(f, "❌"),
            CheckOutcome::Cancelled => write!(f, "⛔"),
            CheckOutcome::Skipped => write!(f, "⏭"),
        }
    }
}

impl From<CheckOutcome> for String {
    fn from(val: CheckOutcome) -> Self {
        match val {
            CheckOutcome::Success => "✅".to_string(),
            CheckOutcome::Failure => "❌".to_string(),
            CheckOutcome::Cancelled => "⛔".to_string(),
            CheckOutcome::Skipped => "⏭".to_string(),
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct CheckOutput {
    pub outcome: CheckOutcome,
    pub required: bool,
    pub number: Option<usize>,
    pub log_url: Option<String>,
}

pub fn get_success_emoji(success: bool) -> String {
    match success {
        true => "✅".to_string(),
        false => "❌".to_string(),
    }
}

fn get_outcome_color(outcome: CheckOutcome, required: bool) -> String {
    match outcome {
        CheckOutcome::Success => "%2346B76E".to_string(),
        CheckOutcome::Failure => match required {
            true => "%23D41159".to_string(),
            false => "%23fddf68".to_string(),
        },
        CheckOutcome::Cancelled => "%236e7781".to_string(),
        CheckOutcome::Skipped => "%236e7781".to_string(),
    }
}

// TODO: This is copyied from mining-bot, it should probably be shared between the two
//
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Step {
    pub name: String,
    pub number: usize,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[non_exhaustive]
pub struct WorkflowJobInstanceRaw {
    pub id: usize,
    pub html_url: String,
    pub steps: Vec<Step>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[non_exhaustive]
pub struct WorkflowJobInstance {
    pub run_id: String,
    pub run_attempt: String,
    pub raw: WorkflowJobInstanceRaw,
}

async fn _get_workflow_info(
    client: &HyperClient<HttpsConnector<HttpConnector>, Empty<Bytes>>,
    url: String,
) -> anyhow::Result<WorkflowJobInstance> {
    let req = Request::builder()
        .method(Method::GET)
        .uri(url.clone())
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .body(Empty::default())?;

    let res = client.request(req).await?;
    if res.status().as_u16() >= 400 {
        anyhow::bail!("Something went wrong while getting npm api data");
    }

    let body = res.into_body().collect().await?.to_bytes();

    let body_str = String::from_utf8_lossy(&body);

    Ok(serde_json::from_str::<WorkflowJobInstance>(
        body_str.as_ref(),
    )?)
}

// pub async fn checks_summaries(
//     options: Box<Options>,
//     summaries_dir: PathBuf,
//     checks_map: HashMap<String, HashMap<JobType, SummaryFile<CheckOutputs>>>,
// ) -> anyhow::Result<SummariesResult> {

//     match overall_success {
//         true => Ok(SummariesResult {}),
//         false => anyhow::bail!("Required test failed"),
//     }
// }

pub async fn processing_summaries<T, O>(
    working_directory: &PathBuf,
    output: &PathBuf,
    mining_bot_url: &str,
    github_token: Option<&str>,
    github_event_name: Option<&str>,
    github_issue_number: Option<u64>,
) -> anyhow::Result<SummariesResult>
where
    T: JobType<O> + Clone + Ord + Hash + Eq + PartialEq + Debug + DeserializeOwned,
    O: RunTypeOutput + Debug + DeserializeOwned,
{
    let mut summary = Summary::new(output);
    // Load outputs
    let run = Run::<T, O>::new(working_directory)?;
    // Generate GH Summary table

    let mut overall_success = true;
    let mut overall_result = JobResult::new();

    for (package, checks) in &run.jobs {
        let mut success = true;
        let (header_row, biggest_col) = T::get_headers(checks)?;
        let mut rows: Vec<Vec<SummaryTableCell>> = vec![header_row];
        checks.iter().for_each(|(check_type, check)| {
            let colspan = check_type.get_colspan(&check.outputs, biggest_col);
            let (check_cell_name, check_success) = check_type.get_cell_name(check);
            success &= check_success;
            let mut row: Vec<SummaryTableCell> = vec![SummaryTableCell::new(check_cell_name, 1)];
            let (cells, job_result) = check_type.get_cells(check, colspan, mining_bot_url);
            overall_result.merge(&job_result);
            row.extend(cells);
            rows.push(row);
        });

        summary.add_content(
            Summary::detail(
                Summary::heading(
                    format!("{} - {}", package, get_success_emoji(success)),
                    Some(2),
                ),
                Summary::table(rows),
                !success,
            ),
            true,
        );
        overall_success &= success;
    }

    let results = [
        (overall_result.succeeded, "passed"),
        (overall_result.failed, "failed"),
        (overall_result.failed_o, "failed (non required)"),
        (overall_result.cancelled, "cancelled"),
        (overall_result.skipped, "skipped"),
    ];
    let messages: Vec<String> = results
        .iter()
        .filter_map(|(r, l)| {
            if *r > 0 {
                return Some(format!("{} {}", r, l));
            }
            None
        })
        .collect();

    let icon_svg = format!(
        "{}/svg/tests.svg?passed={}&failed={}&failed_o={}&skipped={}&cancelled={}",
        mining_bot_url,
        overall_result.succeeded,
        overall_result.failed,
        overall_result.failed_o,
        overall_result.skipped,
        overall_result.cancelled
    );
    summary.prepend_content(format!("![{}]({})", messages.join(", "), icon_svg), true);
    summary.write(true).await?;

    // GH Side effect
    if let Some(github_token) = github_token {
        T::github_side_effect(
            github_token,
            github_event_name,
            github_issue_number,
            &run.jobs,
            &summary.get_content(),
        )
        .await?;
    }

    match overall_success {
        true => Ok(SummariesResult {}),
        false => anyhow::bail!("Run failed"),
    }
}

pub async fn summaries(
    options: Box<Options>,
    working_directory: PathBuf,
) -> anyhow::Result<SummariesResult> {
    if options.check_changed_outcome.clone() != RunOutcome::Success {
        anyhow::bail!("Ci error, please check `check_workspace` job and ping devops ");
    }
    match options.run_type {
        RunTypeOption::Publishing => {
            processing_summaries::<PublishingJobType, PublishingRunOutput>(
                &working_directory,
                &options.output,
                &options.mining_bot_url,
                options.github_token.as_ref().map(String::as_ref),
                options.github_event_name.as_ref().map(String::as_ref),
                options.github_issue_number,
            )
            .await
        }
        RunTypeOption::Checks => {
            processing_summaries::<CheckJobType, CheckRunOutput>(
                &working_directory,
                &options.output,
                &options.mining_bot_url,
                options.github_token.as_ref().map(String::as_ref),
                options.github_event_name.as_ref().map(String::as_ref),
                options.github_issue_number,
            )
            .await
        }
    }
}

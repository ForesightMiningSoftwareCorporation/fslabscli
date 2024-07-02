use std::cmp::min;
use std::collections::HashMap;
use std::fmt::{Display, Formatter, Result as FmtResult};
use std::fs;
use std::path::PathBuf;

use clap::Parser;
use http_body_util::BodyExt;
use http_body_util::Empty;
use hyper::body::Bytes;
use hyper::{Method, Request};
use hyper_rustls::ConfigBuilderExt;
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::rt::TokioExecutor;
use num::integer::lcm;
use octocrab::Octocrab;
use serde::{Deserialize, Serialize};
use template::Summary;

use crate::commands::summaries::template::SummaryTableCell;
use crate::PrettyPrintable;

mod template;

static GH_MAX_COMMENT_LENGTH: usize = 65536;

#[derive(Debug, Parser)]
#[command(about = "Generate summary of github run.")]
pub struct Options {
    #[arg(long, default_value_t, value_enum)]
    run_type: RunType,
    #[arg(long, env = "GITHUB_STEP_SUMMARY")]
    output: PathBuf,
    #[arg(long)]
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
}

#[derive(clap::ValueEnum, Clone, Default, Debug, Serialize)]
enum RunType {
    #[default]
    Checks,
    Publishing,
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

#[derive(Deserialize, Serialize, Debug, Eq, Hash, PartialEq, Clone)]
#[serde(rename_all = "kebab-case")]
enum CheckType {
    Check,
    Test,
    Miri,
}

impl Display for CheckType {
    fn fmt(&self, f: &mut Formatter) -> FmtResult {
        match self {
            Self::Check => write!(f, "check"),
            Self::Test => write!(f, "test"),
            Self::Miri => write!(f, "miri"),
        }
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

#[derive(Deserialize, Serialize, Debug)]
#[serde(rename_all = "kebab-case")]
struct CheckOutputs {
    pub check: Option<CheckOutput>,
    pub clippy: Option<CheckOutput>,
    pub doc: Option<CheckOutput>,
    pub custom: Option<CheckOutput>,
    pub deny_advisories: Option<CheckOutput>,
    pub deny_bans: Option<CheckOutput>,
    pub deny_license: Option<CheckOutput>,
    pub deny_sources: Option<CheckOutput>,
    pub dependencies: Option<CheckOutput>,
    pub fmt: Option<CheckOutput>,
    pub miri: Option<CheckOutput>,
    pub publish_dryrun: Option<CheckOutput>,
    pub tests: Option<CheckOutput>,
}

#[derive(Deserialize, Serialize, Debug)]
struct CheckSummary {
    pub name: String,
    pub start_time: String,
    pub end_time: String,
    pub working_directory: String,
    #[serde(rename = "type")]
    pub check_type: CheckType,
    pub server_url: String,
    pub repository: String,
    pub run_id: String,
    pub run_attempt: String,
    pub actor: String,
    pub event_name: String,
    pub outputs: CheckOutputs,
}

#[derive(Deserialize, Serialize, Debug)]
struct PublishSummary {
    pub name: String,
    pub start_time: String,
    pub end_time: String,
    pub working_directory: String,
    pub released: bool,
}

fn get_success_emoji(success: bool) -> String {
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

pub struct CheckedOutput {
    pub check_name: String,
    pub sub_checks: Vec<(String, CheckOutput)>,
    pub check_success: bool,
    pub url: Option<String>,
}

async fn get_workflow_info(
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

pub async fn checks_summaries(
    options: Box<Options>,
    summaries_dir: PathBuf,
) -> anyhow::Result<SummariesResult> {
    // load all files as ChecksSummaries
    let mut summaries: Vec<CheckSummary> = vec![];
    // Read the directory
    let dir = fs::read_dir(summaries_dir)?;

    // Collect paths of JSON files
    let json_files: Vec<_> = dir
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().map_or(false, |ext| ext == "json"))
        .map(|entry| entry.path())
        .collect();

    // Deserialize each JSON file and collect into vector
    for file_path in json_files {
        let file_content = fs::read_to_string(&file_path)?;
        let deserialized: CheckSummary = serde_json::from_str(&file_content)?;
        summaries.push(deserialized);
    }

    // We have a list of file we need to get to a HashMap<Package, HashMap<CheckType, CheckSummary>>
    let mut checks_map: HashMap<String, HashMap<CheckType, CheckSummary>> = HashMap::new();
    for summary in summaries {
        let inner_map = checks_map.entry(summary.name.clone()).or_default();
        inner_map.insert(summary.check_type.clone(), summary);
    }

    // For each package we need to check if the checks wer a success, and for each check type, generate a report
    let mut summary = Summary::new(options.output);
    let mut overall_success = true;
    let mut failed = 0;
    let mut failed_o = 0;
    let mut skipped = 0;
    let mut cancelled = 0;
    let mut succeeded = 0;

    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(
            rustls::ClientConfig::builder()
                .with_native_roots()?
                .with_no_client_auth(),
        )
        .https_or_http()
        .enable_http1()
        .build();
    let client = HyperClient::builder(TokioExecutor::new()).build(https);
    for (package, checks) in checks_map {
        let mut success = true;

        let mut check_outputs: Vec<CheckedOutput> = vec![];
        for (check_name, check_summary) in checks {
            let mining_bot_url = format!(
                "{}/workflow?run_id={}&working_directory={}&check_type={}&run_attempt={}",
                options.mining_bot_url.clone(),
                check_summary.run_id.clone(),
                check_summary.working_directory.replace('/', "%2F"),
                check_name,
                check_summary.run_attempt.clone(),
            );
            let workflow_info = get_workflow_info(&client, mining_bot_url).await;
            let base_url: Option<String> =
                workflow_info.as_ref().map(|w| w.raw.html_url.clone()).ok();
            let steps = workflow_info.as_ref().map(|w| w.raw.steps.to_vec()).ok();
            let mut check_success = true;
            let sub_checks: Vec<(String, Option<CheckOutput>)> = vec![
                ("check".to_string(), check_summary.outputs.check.clone()),
                ("clippy".to_string(), check_summary.outputs.clippy.clone()),
                ("doc".to_string(), check_summary.outputs.doc.clone()),
                // ("custom", check_summary.outputs.custom.clone()),
                (
                    "deny_advisories".to_string(),
                    check_summary.outputs.deny_advisories.clone(),
                ),
                (
                    "deny_bans".to_string(),
                    check_summary.outputs.deny_bans.clone(),
                ),
                (
                    "deny_license".to_string(),
                    check_summary.outputs.deny_license.clone(),
                ),
                (
                    "deny_sources".to_string(),
                    check_summary.outputs.deny_sources.clone(),
                ),
                (
                    "dependencies".to_string(),
                    check_summary.outputs.dependencies.clone(),
                ),
                ("fmt".to_string(), check_summary.outputs.fmt.clone()),
                ("miri".to_string(), check_summary.outputs.miri.clone()),
                (
                    "publish_dryrun".to_string(),
                    check_summary.outputs.publish_dryrun.clone(),
                ),
                ("tests".to_string(), check_summary.outputs.tests.clone()),
            ];
            let mut checked_sub_checks: Vec<(String, CheckOutput)> = vec![];
            for (subcheck, check) in sub_checks {
                if let Some(check) = check {
                    let mut new_check = check.clone();
                    if let (Some(steps), Some(base_url)) = (steps.clone(), base_url.clone()) {
                        let step = steps.iter().find(|c| c.name == subcheck.replace('_', "-"));
                        if let Some(step) = step {
                            new_check.number = Some(step.number);
                            new_check.log_url =
                                Some(format!("{}#step:{}:1", base_url, step.number));
                        }
                    }
                    checked_sub_checks.push((subcheck, new_check));
                    if check.required {
                        check_success &= check.outcome.is_passing();
                    }
                }
            }
            if checked_sub_checks.is_empty() {
                continue;
            }
            // order sub check by number
            checked_sub_checks.sort_by_key(|(n, o)| (o.number, n.clone()));
            check_outputs.push(CheckedOutput {
                check_name: check_name.to_string(),
                sub_checks: checked_sub_checks,
                check_success,
                url: base_url,
            });
            success &= check_success;
        }
        // #1 Find the lcm between the result to display a nice table
        let mut lcm_result: usize = 1;
        for checked in check_outputs.iter() {
            lcm_result = lcm(lcm_result, checked.sub_checks.len());
        }
        let header_row: Vec<SummaryTableCell> = vec![
            SummaryTableCell::new_header("Category".to_string(), 1),
            SummaryTableCell::new_header("Checks".to_string(), lcm_result),
        ];
        let mut rows: Vec<Vec<SummaryTableCell>> = vec![header_row];
        check_outputs.sort_by_key(|c| c.check_name.clone());
        for checked in check_outputs.iter() {
            let colspan = lcm_result / (checked.sub_checks.len());
            let check_cell_name = format!(
                "{} {}",
                get_success_emoji(checked.check_success),
                if let Some(url) = checked.url.clone() {
                    summary.link(checked.check_name.to_string(), url)
                } else {
                    checked.check_name.to_string()
                }
            );
            let mut row: Vec<SummaryTableCell> = vec![SummaryTableCell::new(check_cell_name, 1)];
            let mut imgs: Vec<String> = vec![];
            for (subcheck_name, subcheck) in checked.sub_checks.iter() {
                match subcheck.outcome {
                    CheckOutcome::Success => succeeded += 1,
                    CheckOutcome::Failure => match subcheck.required {
                        true => failed += 1,
                        false => failed_o += 1,
                    },
                    CheckOutcome::Cancelled => cancelled += 1,
                    CheckOutcome::Skipped => skipped += 1,
                }
                let subcheck_image = summary.image(
                    format!(
                        "{}/svg/rectangle.svg?fill={}&text={}&colspan={}",
                        options.mining_bot_url,
                        get_outcome_color(subcheck.outcome, subcheck.required),
                        subcheck_name,
                        colspan,
                    ),
                    format!("{}", subcheck.outcome),
                    subcheck_name.to_string(),
                    None,
                    None,
                );
                let subcheck_cell = match subcheck.log_url.clone() {
                    Some(u) => summary.link(subcheck_image, u),
                    None => subcheck_image,
                };
                imgs.push(subcheck_cell);
            }
            row.push(SummaryTableCell::new(imgs.join(""), 1));
            rows.push(row);
        }

        summary.add_content(
            summary.detail(
                summary.heading(
                    format!("{} - {}", package, get_success_emoji(success)),
                    Some(2),
                ),
                summary.table(rows),
                !success,
            ),
            true,
        );
        overall_success &= success;
    }

    let mut messages: Vec<String> = vec![];
    if succeeded > 0 {
        messages.push(format!("{} passed", succeeded))
    }
    if failed > 0 {
        messages.push(format!("{} failed", failed))
    }
    if failed_o > 0 {
        messages.push(format!("{} failed (non required)", failed_o))
    }
    if cancelled > 0 {
        messages.push(format!("{} cancelled", cancelled))
    }
    if skipped > 0 {
        messages.push(format!("{} skipped", skipped))
    }

    let icon_svg = format!(
        "{}/svg/tests.svg?passed={}&failed={}&failed_o={}&skipped={}&cancelled={}",
        options.mining_bot_url, succeeded, failed, failed_o, skipped, cancelled
    );
    summary.prepend_content(format!("![{}]({})", messages.join(", "), icon_svg), true);
    summary.write(true).await?;
    if let (
        Some(github_token),
        Some(github_event_name),
        Some(github_issue_number),
        Some(github_repo),
    ) = (
        options.github_token,
        options.github_event_name,
        options.github_issue_number,
        options.github_repo,
    ) {
        if github_event_name == "pull_request" || github_event_name == "pull_request_target" {
            // We have a github token we should try to update the pr
            let octocrab = Octocrab::builder().personal_token(github_token).build()?;
            if let Some((owner, repo)) = github_repo.split_once('/') {
                let issues_client = octocrab.issues(owner, repo);
                let output = summary.get_content();
                if options.hide_previous_pr_comment {
                    // Hide previsous
                    let user = octocrab
                        .current()
                        .user()
                        .await
                        .map(|u| u.login)
                        .unwrap_or_else(|_| "fmsc-bot[bot]".to_string());
                    if let Ok(existing_comments) = issues_client
                        .list_comments(github_issue_number)
                        .send()
                        .await
                        .map_err(|e| {
                            println!("Could not list comments: {:?}", e);
                            e
                        })
                    {
                        for existing_comment in existing_comments {
                            if existing_comment.user.login != user {
                                continue;
                            }
                            // Delete all of our comments? Maybe we nmeed to be more clever
                            let _ = issues_client
                                .delete_comment(existing_comment.id)
                                .await
                                .map_err(|e| {
                                    println!("Could not delete comment: {:?}", e);
                                    e
                                });
                        }
                    }
                }
                let comments = split_comments(output);
                for comment in comments {
                    let _ = issues_client
                        .create_comment(github_issue_number, comment)
                        .await
                        .map_err(|e| {
                            println!("Could not create comment: {:?}", e);
                            e
                        });
                }
            }
        }
    }

    match overall_success {
        true => Ok(SummariesResult {}),
        false => anyhow::bail!("Required test failed"),
    }
}

fn split_comments(comment: String) -> Vec<String> {
    let sep_start = "Continued from previous comment.\n";
    let sep_end =
        "\n**Warning**: Output length greater than max comment size. Continued in next comment.";
    if comment.len() < GH_MAX_COMMENT_LENGTH {
        return vec![comment];
    }
    let max_with_seps: usize = GH_MAX_COMMENT_LENGTH - sep_start.len() - sep_end.len();
    let mut comments: Vec<String> = vec![];
    let num_comments: usize = (comment.len() as f64 / max_with_seps as f64).ceil() as usize;
    for i in 0..num_comments {
        let up_to = min(comment.len(), (i + 1) * max_with_seps);
        let portion = &comment[i * max_with_seps..up_to];
        let mut portion_with_sep = portion.to_string();
        if i < num_comments - 1 {
            portion_with_sep.push_str(sep_end);
        }
        if i > 0 {
            portion_with_sep = format!("{}{}", sep_start, portion_with_sep);
        }
        comments.push(portion_with_sep)
    }
    comments
}

pub async fn publishing_summaries(
    _options: Box<Options>,
    _summaries_directory: PathBuf,
) -> anyhow::Result<SummariesResult> {
    Ok(SummariesResult {})
}

pub async fn summaries(
    options: Box<Options>,
    working_directory: PathBuf,
) -> anyhow::Result<SummariesResult> {
    match options.run_type.clone() {
        RunType::Checks => checks_summaries(options, working_directory).await,
        RunType::Publishing => publishing_summaries(options, working_directory).await,
    }
}

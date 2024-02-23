use std::collections::HashMap;
use std::fmt::{Display, Formatter, Result as FmtResult};
use std::fs;
use std::path::PathBuf;

use clap::Parser;
use http_body_util::BodyExt;
use http_body_util::Empty;
use hyper::{Method, Request};
use hyper::body::Bytes;
use hyper_rustls::ConfigBuilderExt;
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use num::integer::lcm;
use serde::{Deserialize, Serialize};

use template::Summary;

use crate::commands::summaries::template::SummaryTableCell;

mod template;

#[derive(Debug, Parser)]
#[command(about = "Generate summary of github run.")]
pub struct Options {
    #[arg(long, default_value_t, value_enum)]
    run_type: RunType,
    #[arg(long, env = "GITHUB_STEP_SUMMARY")]
    output: PathBuf,
    #[arg(long, default_value_t = false)]
    compute_links: bool,
    #[arg(long, default_value = "https://gh.dc1.foresightmining.com")]
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

#[derive(Deserialize, Serialize, Debug, Eq, Hash, PartialEq, Clone)]
#[serde(rename_all = "kebab-case")]
enum CheckType {
    Check,
    Test,
    Miri,
}

impl CheckType {
    pub fn pretty_print(&self) -> String {
        match self {
            Self::Check => "Check".to_string(),
            Self::Test => "Test".to_string(),
            Self::Miri => "Miri".to_string(),
        }
    }
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
enum CheckOutcome {
    Success,
    Failure,
    Cancelled,
    Skipped,
}

impl CheckOutcome {
    pub fn is_passing(&self) -> bool {
        match &self {
            &Self::Failure => false,
            _ => true,
        }
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

impl Into<String> for CheckOutcome {
    fn into(self) -> String {
        match self {
            CheckOutcome::Success => "✅".to_string(),
            CheckOutcome::Failure => "❌".to_string(),
            CheckOutcome::Cancelled => "⛔".to_string(),
            CheckOutcome::Skipped => "⏭".to_string(),
            _ => "❔".to_string(),
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "kebab-case")]
struct CheckOutput {
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

fn get_required_emoji(required: bool) -> String {
    match required {
        true => "✅".to_string(),
        false => "-".to_string(),
    }
}

fn get_success_emoji(success: bool) -> String {
    match success {
        true => "✅".to_string(),
        false => "❌".to_string(),
    }
}

fn get_success_color(success: bool) -> String {
    match success {
        true => "green".to_string(),
        false => "red".to_string(),
    }
}

fn get_outcome_color(outcome: CheckOutcome) -> String {
    match outcome {
        CheckOutcome::Success => "green".to_string(),
        CheckOutcome::Failure => "red".to_string(),
        CheckOutcome::Cancelled => "grey".to_string(),
        CheckOutcome::Skipped => "grey".to_string(),
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

async fn get_workflow_info(
    client: &HyperClient<HttpsConnector<HttpConnector>, Empty<Bytes>>,
    url: String,
) -> anyhow::Result<WorkflowJobInstance> {
    let req = Request::builder()
        .method(Method::GET)
        .uri(url)
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
        let mut inner_map = checks_map
            .entry(summary.name.clone())
            .or_insert_with(HashMap::new);
        inner_map.insert(summary.check_type.clone(), summary);
    }

    // For each package we need to check if the checks wer a success, and for each check type, generate a report
    let mut summary = Summary::new(options.output);
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

        let mut check_outputs: Vec<(String, Vec<(&str, CheckOutput)>, bool, String)> = vec![];
        for (check_name, check_summary) in checks {
            let mining_bot_url = format!(
                "{}/workflow?run_id={}&working_directory={}&check_type={}&run_attempt={}",
                options.mining_bot_url.clone(),
                check_summary.run_id.clone(),
                check_summary.working_directory.replace("/", "%2F"),
                check_name,
                check_summary.run_attempt.clone(),
            );
            let Ok(workflow_info) = get_workflow_info(&client, mining_bot_url).await else { continue; };
            let base_url = workflow_info.raw.html_url.clone();
            let steps: Vec<Step> = workflow_info.raw.steps.iter().cloned().collect();
            let mut check_success = true;
            let sub_checks: Vec<(&str, Option<CheckOutput>)> = vec![
                ("check", check_summary.outputs.check.clone()),
                ("clippy", check_summary.outputs.clippy.clone()),
                ("doc", check_summary.outputs.doc.clone()),
                // ("custom", check_summary.outputs.custom.clone()),
                (
                    "deny_advisories",
                    check_summary.outputs.deny_advisories.clone(),
                ),
                ("deny_bans", check_summary.outputs.deny_bans.clone()),
                ("deny_license", check_summary.outputs.deny_license.clone()),
                ("deny_sources", check_summary.outputs.deny_sources.clone()),
                ("dependencies", check_summary.outputs.dependencies.clone()),
                ("fmt", check_summary.outputs.fmt.clone()),
                ("miri", check_summary.outputs.miri.clone()),
                (
                    "publish_dryrun",
                    check_summary.outputs.publish_dryrun.clone(),
                ),
                ("tests", check_summary.outputs.tests.clone()),
            ];
            let mut checked_sub_checks: Vec<(&str, CheckOutput)> = vec![];
            for (subcheck, check) in sub_checks {
                if let Some(check) = check {
                    let step = steps.iter().find(|c| c.name == subcheck.replace("_", "-"));
                    let mut new_check = check.clone();
                    if let Some(step) = step {
                        new_check.number = Some(step.number);
                        new_check.log_url = Some(format!("{}#step:{}:1", base_url, step.number));
                        checked_sub_checks.push((subcheck, new_check));
                        if check.required {
                            check_success &= check.outcome.is_passing();
                        }
                    }
                }
            }
            // order sub check by number
            checked_sub_checks.sort_by_key(|(_, o)| o.number.unwrap());
            check_outputs.push((check_name.to_string(), checked_sub_checks, check_success, base_url));
            success &= check_success;
        }
        // #1 Find the lcm between the result to display a nice table
        let mut lcm_result: usize = 1;
        for (_, inner_vec, _, _) in check_outputs.iter() {
            let inner_vec_len = inner_vec.len();
            lcm_result = lcm(lcm_result, inner_vec_len);
        }
        let header_row: Vec<SummaryTableCell> = vec![
            SummaryTableCell::new_header("Category".to_string(), 1),
            SummaryTableCell::new_header("Checks".to_string(), lcm_result),
        ];
        let mut rows: Vec<Vec<SummaryTableCell>> = vec![header_row];
        for (check_name, check, check_success, check_url) in check_outputs.iter() {
            let colspan = lcm_result / (check.len());
            let check_cell_name = format!("{} {}",
                                          get_success_emoji(*check_success),
                                          summary.link(check_name.to_string(), check_url.clone())
            );
            let mut row: Vec<SummaryTableCell> = vec![SummaryTableCell::new(check_cell_name, 1)];
            for (subcheck_name, subcheck) in check {
                let subcheck_cell = format!("{} {}", subcheck.outcome, match subcheck.log_url.clone() {
                    Some(u) => summary.link(subcheck_name.to_string(), u),
                    None => subcheck_name.to_string(),
                });
                row.push(SummaryTableCell::new(subcheck_cell, colspan));
            }
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
        )
    }
    summary.write(true).await;

    //    println!("{:?}", checks_map);
    Ok(SummariesResult {})
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

use clap::Parser;
use humantime::format_duration;
use hyper_rustls::ConfigBuilderExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::{Display, Formatter, Result as FmtResult};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;
mod template;
use http_body_util::BodyExt;
use http_body_util::Empty;
use hyper::body::Bytes;
use hyper::{Method, Request};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::rt::TokioExecutor;
use template::Summary;

use crate::commands::summaries::template::SummaryTableRow;

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

// TODO: This is copyied from mining-bot, it should probably be shared between the two
//
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Step {
    pub name: String,
    pub number: i64,
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

        let mut check_outputs: Vec<String> = vec![];
        for (check_name, check_summary) in checks {
            let mining_bot_url = format!(
                "{}/workflow?run_id={}&working_directory={}&check_type={}&run_attempt={}",
                options.mining_bot_url.clone(),
                check_summary.run_id.clone(),
                check_summary.working_directory.replace("/", "%2F"),
                check_name,
                check_summary.run_attempt.clone(),
            );
            let workflow_info = get_workflow_info(&client, mining_bot_url).await;
            let mut base_url: Option<String> = None;
            let mut job_id: Option<String> = None;
            let mut steps: Vec<Step> = vec![];
            if let Ok(i) = workflow_info {
                job_id = Some(format!("{}", i.raw.id));
                base_url = Some(i.raw.html_url.clone());
                steps = i.raw.steps.iter().cloned().collect();
            }
            let mut check_success = true;
            let mut rows: Vec<Vec<SummaryTableRow>> = vec![vec![
                SummaryTableRow::new_header("Required".to_string()),
                SummaryTableRow::new_header("Step".to_string()),
                SummaryTableRow::new_header("Result".to_string()),
                SummaryTableRow::new_header("Details".to_string()),
            ]];
            let sub_checks: Vec<(&str, Option<CheckOutput>)> = vec![
                ("check", check_summary.outputs.check.clone()),
                ("clippy", check_summary.outputs.clippy.clone()),
                ("doc", check_summary.outputs.doc.clone()),
                ("custom", check_summary.outputs.custom.clone()),
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
            for (subcheck, check) in sub_checks {
                if let Some(check) = check {
                    //let log_url = match workflow_info {
                    //   Ok(_) => "".to_string(),
                    //    Err(_) => "".to_string(),
                    //};
                    let step = steps.iter().find(|c| c.name == subcheck.replace("_", "-"));
                    let log_url = match (base_url.clone(), step) {
                        (Some(u), Some(c)) => {
                            summary.link("logs".to_string(), format!("{}#step:{}:1", u, c.number))
                        }
                        _ => "".to_string(),
                    };
                    rows.push(vec![
                        SummaryTableRow::new(get_required_emoji(check.required)),
                        SummaryTableRow::new(subcheck.to_string()),
                        SummaryTableRow::new(check.outcome.into()),
                        SummaryTableRow::new(log_url),
                    ]);
                    if check.required {
                        check_success &= check.outcome.is_passing();
                    }
                }
            }
            let duration = match (
                check_summary.start_time.parse::<i64>(),
                check_summary.end_time.parse::<i64>(),
            ) {
                (Ok(start_time), Ok(end_time)) => {
                    format_duration(Duration::from_secs((end_time - start_time) as u64) / 1000)
                        .to_string()
                }
                _ => "-".to_string(),
            };
            let heading = summary.heading(
                format!("{} - {}", check_name, get_success_emoji(check_success)),
                Some(3),
            );
            let run_link = base_url.unwrap_or(format!(
                "{}/{}/actions/runs/{}",
                check_summary.server_url, check_summary.repository, check_summary.run_id
            ));
            let run_link_text = job_id.unwrap_or("".to_string());
            check_outputs.push(summary.detail(
                heading,
                format!(
                    "{}\n{}\n{}",
                    summary.table(rows),
                    summary.p(format!(
                        "Run: {}",
                        summary.link(
                            format!("{}/{}", check_summary.run_id.clone(), run_link_text),
                            run_link
                        )
                    )),
                    summary.p(format!("Duration: {}", duration)),
                ),
                !check_success,
            ));
            success &= check_success;
        }
        summary.add_content(
            summary.detail(
                summary.heading(
                    format!("{} - {}", package, get_success_emoji(success)),
                    Some(2),
                ),
                check_outputs.join(""),
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

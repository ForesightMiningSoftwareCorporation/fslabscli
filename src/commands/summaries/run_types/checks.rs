use std::{
    cmp::min,
    fmt::{Display, Formatter, Result as FmtResult},
};

use indexmap::IndexMap;
use num::integer::lcm;
use octocrab::Octocrab;
use serde::{Deserialize, Serialize};

use crate::commands::summaries::{
    get_outcome_color,
    run_types::JobResult,
    template::{Summary, SummaryTableCell},
    CheckOutcome, CheckOutput,
};

use super::{Job, JobType, RunTypeOutput};

static GH_MAX_COMMENT_LENGTH: usize = 65536;

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

#[derive(Deserialize, Serialize, Debug)]
#[serde(rename_all = "kebab-case")]
pub struct CheckRunOutput {
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

impl CheckRunOutput {
    fn as_vec(&self) -> Vec<(&str, &Option<CheckOutput>)> {
        vec![
            ("check", &self.check),
            ("clippy", &self.clippy),
            ("doc", &self.doc),
            ("custom", &self.custom),
            ("deny_advisories", &self.deny_advisories),
            ("deny_bans", &self.deny_bans),
            ("deny_license", &self.deny_license),
            ("deny_sources", &self.deny_sources),
            ("dependencies", &self.dependencies),
            ("fmt", &self.fmt),
            ("miri", &self.miri),
            ("publish_dryrun", &self.publish_dryrun),
            ("tests", &self.tests),
        ]
    }
}

impl RunTypeOutput for CheckRunOutput {}

#[derive(Deserialize, Serialize, Debug, Eq, Hash, PartialEq, Clone, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub enum CheckJobType {
    Check,
    Test,
    Miri,
}

impl Display for CheckJobType {
    fn fmt(&self, f: &mut Formatter) -> FmtResult {
        match self {
            Self::Check => write!(f, "check"),
            Self::Test => write!(f, "test"),
            Self::Miri => write!(f, "miri"),
        }
    }
}

impl JobType<CheckRunOutput> for CheckJobType {
    fn get_headers(
        jobs: &IndexMap<Self, super::Job<Self, CheckRunOutput>>,
    ) -> anyhow::Result<(Vec<SummaryTableCell>, usize)> {
        let mut lcm_result: usize = 1;
        for (_, job) in jobs.iter() {
            let num_check = job
                .outputs
                .as_vec()
                .iter()
                .filter(|(_, o)| o.is_some())
                .count();
            lcm_result = lcm(lcm_result, num_check);
        }
        Ok((
            vec![
                SummaryTableCell::new_header("Category".to_string(), 1),
                SummaryTableCell::new_header("Checks".to_string(), lcm_result),
            ],
            lcm_result,
        ))
    }
    fn get_colspan(&self, outputs: &CheckRunOutput, max_colspan: usize) -> usize {
        let num_check = outputs.as_vec().iter().filter(|(_, o)| o.is_some()).count();
        max_colspan / num_check
    }
    fn get_job_success(&self, job: &Job<Self, CheckRunOutput>) -> bool
    where
        Self: Sized,
    {
        job.outputs
            .as_vec()
            .iter()
            .filter(|(_, c)| c.is_some())
            .all(|(_, c)| {
                c.as_ref()
                    .is_some_and(|c| c.required && c.outcome.is_passing())
            })
    }

    fn get_cells(
        &self,
        job: &Job<Self, CheckRunOutput>,
        colspan: usize,
        mining_bot_url: &str,
    ) -> (Vec<SummaryTableCell>, JobResult) {
        let mut job_result = JobResult::new();
        let imgs: Vec<String> = job
            .outputs
            .as_vec()
            .iter()
            .filter_map(|(t, c)| c.as_ref().map(|c| (t, c)))
            .map(|(n, c)| {
                match c.outcome {
                    CheckOutcome::Success => {
                        job_result.succeeded += 1;
                    }
                    CheckOutcome::Failure => match c.required {
                        true => {
                            job_result.failed += 1;
                        }
                        false => {
                            job_result.failed_o += 1;
                        }
                    },
                    CheckOutcome::Cancelled => {
                        job_result.cancelled += 1;
                    }
                    CheckOutcome::Skipped => {
                        job_result.skipped += 1;
                    }
                };

                let subcheck_image = Summary::image(
                    format!(
                        "{}/svg/rectangle.svg?fill={}&text={}&colspan={}",
                        mining_bot_url, //TODO find a way to share it
                        get_outcome_color(c.outcome, c.required),
                        n,
                        colspan,
                    ),
                    format!("{}", c.outcome),
                    n.to_string(),
                    None,
                    None,
                );
                match &c.log_url {
                    Some(u) => Summary::link(subcheck_image, u.to_string()),
                    None => subcheck_image,
                }
            })
            .collect();
        (vec![SummaryTableCell::new(imgs.join(""), 1)], job_result)
    }

    async fn github_side_effect(
        token: &str,
        event_name: Option<&str>,
        issue_number: Option<u64>,
        runs: &IndexMap<String, IndexMap<Self, Job<Self, CheckRunOutput>>>,
        summary: &str,
    ) -> anyhow::Result<()> {
        let Some(event_name) = event_name else {
            return Ok(());
        };
        let Some(issue_number) = issue_number else {
            return Ok(());
        };
        // Let's get the repo from the job
        let Some(repository) = runs
            .first()
            .and_then(|(_, j)| j.first().map(|(_, c)| &c.repository))
        else {
            return Ok(());
        };
        if event_name == "pull_request" || event_name == "pull_request_target" {
            // We have a github token we should try to update the pr
            let octocrab = Octocrab::builder()
                .personal_token(token.to_string())
                .build()?;
            if let Some((owner, repo)) = repository.split_once('/') {
                let issues_client = octocrab.issues(owner, repo);
                // Hide previsous
                let user = octocrab
                    .current()
                    .user()
                    .await
                    .map(|u| u.login)
                    .unwrap_or_else(|_| "fmsc-bot[bot]".to_string());
                if let Ok(existing_comments) = issues_client
                    .list_comments(issue_number)
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
                let comments = split_comments(summary.to_string());
                for comment in comments {
                    let _ = issues_client
                        .create_comment(issue_number, comment)
                        .await
                        .map_err(|e| {
                            println!("Could not create comment: {:?}", e);
                            e
                        });
                }
            }
        }
        Ok(())
    }
}

use std::fmt::{Display, Formatter, Result as FmtResult};

use indexmap::IndexMap;
use num::integer::lcm;
use serde::{Deserialize, Serialize};

use crate::commands::summaries::{
    get_outcome_color,
    run_types::JobResult,
    template::{Summary, SummaryTableCell},
    CheckOutcome, CheckOutput,
};

use super::{Job, JobType, RunTypeOutput};

pub struct CheckedOutput {
    pub check_name: String,
    pub sub_checks: Vec<(String, CheckOutput)>,
    pub check_success: bool,
    pub url: Option<String>,
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
    ) -> (Vec<SummaryTableCell>, JobResult) {
        let mut job_result = JobResult::new();
        let imgs: Vec<String> = job
            .outputs
            .as_vec()
            .iter()
            .filter_map(|(t, c)| match c {
                Some(c) => Some((t, c)),
                None => None,
            })
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
                        "".to_string(), //options.mining_bot_url, //TODO find a way to share it
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
}

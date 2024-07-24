use humanize_duration::prelude::DurationExt;
use humanize_duration::Truncate;
use std::fmt::{Display, Formatter, Result as FmtResult};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::commands::summaries::{
    get_success_emoji, run_types::JobResult, template::SummaryTableCell,
};

use super::{Job, JobType, RunTypeOutput};

#[derive(Deserialize, Serialize, Debug)]
#[serde(rename_all = "kebab-case")]
pub struct PublishingRunOutput {
    pub released: Option<bool>,
    pub version: Option<String>,
}

impl RunTypeOutput for PublishingRunOutput {}

#[derive(Deserialize, Serialize, Debug, Eq, Hash, PartialEq, Clone, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub enum PublishingJobType {
    #[serde(rename = "docker-publish")]
    Docker,
    #[serde(rename = "npm-napi-publish")]
    NpmNapi,
    #[serde(rename = "rust-binary-publish")]
    RustBinary,
    #[serde(rename = "rust-installer-publish")]
    RustInstaller,
    #[serde(rename = "rust-registry-publish")]
    RustRegistry,
}

impl Display for PublishingJobType {
    fn fmt(&self, f: &mut Formatter) -> FmtResult {
        match self {
            Self::Docker => write!(f, "docker-publish"),
            Self::NpmNapi => write!(f, "npm-napi-publish"),
            Self::RustBinary => write!(f, "rust-binary-publish"),
            Self::RustInstaller => write!(f, "rust-installer-publish"),
            Self::RustRegistry => write!(f, "rust-registry-publish"),
        }
    }
}

impl JobType<PublishingRunOutput> for PublishingJobType {
    fn get_headers(
        _jobs: &IndexMap<Self, Job<Self, PublishingRunOutput>>,
    ) -> anyhow::Result<(Vec<SummaryTableCell>, usize)> {
        Ok((
            vec![
                SummaryTableCell::new_header("Release type".to_string(), 1),
                SummaryTableCell::new_header("Version".to_string(), 1),
                SummaryTableCell::new_header("Duration".to_string(), 1),
                SummaryTableCell::new_header("Published".to_string(), 1),
            ],
            1,
        ))
    }
    fn get_job_success(&self, job: &Job<Self, PublishingRunOutput>) -> bool
    where
        Self: Sized,
    {
        job.outputs.released.is_some_and(|v| v)
    }
    fn get_cells(
        &self,
        job: &Job<Self, PublishingRunOutput>,
        _colspan: usize,
    ) -> (Vec<SummaryTableCell>, JobResult) {
        let mut job_result = JobResult::new();
        match job.outputs.released {
            Some(r) => match r {
                true => {
                    job_result.succeeded += 1;
                }
                false => {
                    job_result.failed += 1;
                }
            },
            None => {
                job_result.skipped += 1;
            }
        };
        let duration = match (job.start_time, job.end_time) {
            (Some(start_time), Some(end_time)) => {
                let duration = end_time - start_time;
                duration.human(Truncate::Nano).to_string()
            }
            _ => "".to_string(),
        };
        (
            vec![
                SummaryTableCell::new(
                    job.outputs
                        .version
                        .clone()
                        .unwrap_or_else(|| "-".to_string()),
                    1,
                ),
                SummaryTableCell::new(duration, 1),
                SummaryTableCell::new(get_success_emoji(job.outputs.released.unwrap_or(false)), 1),
            ],
            job_result,
        )
    }

    async fn github_side_effect(
        _token: &str,
        _event_name: Option<&str>,
        _issue_number: Option<u64>,
        _runs: &IndexMap<String, IndexMap<Self, Job<Self, PublishingRunOutput>>>,
        _summary: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

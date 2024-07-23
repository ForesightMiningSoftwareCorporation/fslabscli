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
    DockerPublish,
    NpmNapiPublish,
    RustBinaryPublish,
    RustInstallerPublish,
    RustRegistryPublish,
}

impl Display for PublishingJobType {
    fn fmt(&self, f: &mut Formatter) -> FmtResult {
        match self {
            Self::DockerPublish => write!(f, "docker-publish"),
            Self::NpmNapiPublish => write!(f, "npm-napi-publish"),
            Self::RustBinaryPublish => write!(f, "rust-binary-publish"),
            Self::RustInstallerPublish => write!(f, "rust-installer-publish"),
            Self::RustRegistryPublish => write!(f, "rust-registry-publish"),
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
        (
            vec![
                SummaryTableCell::new(
                    job.outputs
                        .version
                        .clone()
                        .unwrap_or_else(|| "-".to_string()),
                    1,
                ),
                SummaryTableCell::new(get_success_emoji(job.outputs.released.unwrap_or(false)), 1),
            ],
            job_result,
        )
    }
}

use humanize_duration::prelude::DurationExt;
use humanize_duration::Truncate;
use octocrab::{models::RunId, params::actions::ArchiveFormat, Octocrab};
use std::{
    fmt::{Display, Formatter, Result as FmtResult},
    io::{Cursor, Read},
};
use zip::ZipArchive;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::commands::summaries::{
    get_success_emoji, run_types::JobResult, template::SummaryTableCell,
};

use super::{Job, JobType, RunTypeOutput};

#[derive(Deserialize, Serialize, Debug)]
#[serde(rename_all = "snake_case")]
pub struct PublishingRunOutput {
    pub released: Option<bool>,
    pub version: Option<String>,
    pub release_channel: Option<String>,
    pub sha: Option<String>,
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
        _mining_bot_url: &str,
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
        token: &str,
        _event_name: Option<&str>,
        _issue_number: Option<u64>,
        runs: &IndexMap<String, IndexMap<Self, Job<Self, PublishingRunOutput>>>,
        _summary: &str,
    ) -> anyhow::Result<()> {
        // We have a github token we should try to update the pr
        let octocrab = Octocrab::builder()
            .personal_token(token.to_string())
            .build()?;
        for (_, jobs) in runs {
            let Some(repository_id) = jobs.first().map(|(_, c)| &c.repository) else {
                continue;
            };
            let Some(Some(sha)) = jobs.first().map(|(_, c)| &c.outputs.sha) else {
                continue;
            };

            // Run artifact
            let run_id: RunId = match jobs.first().map(|(_, c)| &c.run_id) {
                Some(run_id) => match run_id.parse::<u64>() {
                    Ok(i) => i.into(),
                    Err(_) => {
                        continue;
                    }
                },
                None => {
                    continue;
                }
            };
            let split: Vec<String> = repository_id.split('/').map(|s| s.to_string()).collect();
            let owner: String;
            let repository: String;
            if split.len() == 2 {
                owner = split[0].clone();
                repository = split[1].clone();
            } else {
                continue;
            }
            let Some(package_name) = jobs.first().map(|(_, c)| &c.name) else {
                continue;
            };
            let mut release_status: IndexMap<PublishingJobType, Option<&String>> =
                IndexMap::from([
                    (
                        PublishingJobType::Docker,
                        get_version(jobs.get(&PublishingJobType::Docker)),
                    ),
                    (
                        PublishingJobType::NpmNapi,
                        get_version(jobs.get(&PublishingJobType::NpmNapi)),
                    ),
                    (
                        PublishingJobType::RustBinary,
                        get_version(jobs.get(&PublishingJobType::RustBinary)),
                    ),
                    (
                        PublishingJobType::RustInstaller,
                        get_version(jobs.get(&PublishingJobType::RustInstaller)),
                    ),
                    (
                        PublishingJobType::RustRegistry,
                        get_version(jobs.get(&PublishingJobType::RustRegistry)),
                    ),
                ]);
            release_status.retain(|_, r| r.is_some());
            let released = !release_status.is_empty();
            if released {
                // Compute tag, should be the binary version or the version
                let Some(version) = (match release_status.get(&PublishingJobType::RustBinary) {
                    Some(Some(version)) => Some((*version).clone()),
                    _ => release_status
                        .first()
                        .map(|(_, v)| v)
                        .and_then(|i| *i)
                        .cloned(),
                }) else {
                    continue;
                };
                // If it' s a binary release, we should use the release_channel as part of the tag, otherwise, no need
                let mut release_channel: Option<String> = None;
                if let Some(binary_job) = jobs.get(&PublishingJobType::RustBinary) {
                    if binary_job.outputs.released.is_some_and(|r| r) {
                        release_channel = binary_job.outputs.release_channel.clone();
                    }
                }
                if release_channel.is_none() {
                    if let Some(installer_job) = jobs.get(&PublishingJobType::RustInstaller) {
                        if installer_job.outputs.released.is_some_and(|r| r) {
                            release_channel = installer_job.outputs.release_channel.clone();
                        }
                    }
                }
                let tag_parts: Vec<String> =
                    vec![Some(package_name.clone()), release_channel, Some(version)]
                        .into_iter()
                        .flatten()
                        .collect();
                let tag_name = tag_parts.join("-");

                let repo = octocrab.repos(&owner, &repository);
                let releases = repo.releases();
                // Let' s check if the release exists
                let release = match releases.get_by_tag(&tag_name).await {
                    Ok(r) => {
                        // release exists
                        r
                    }
                    Err(e) => match e {
                        octocrab::Error::GitHub { source, .. } => match source.status_code {
                            http::StatusCode::NOT_FOUND => {
                                // Need to create it
                                // 1. Let' s create the release note, by using github default for now
                                let release_note = match releases
                                    .generate_release_notes(&tag_name)
                                    .target_commitish(sha)
                                    .send()
                                    .await
                                {
                                    Ok(r) => r,
                                    Err(_) => {
                                        continue;
                                    }
                                };
                                // 2. Let' s create the release.
                                match releases
                                    .create(&tag_name)
                                    .target_commitish(sha)
                                    .name(&release_note.name)
                                    .body(&release_note.body)
                                    .draft(true)
                                    .send()
                                    .await
                                {
                                    Ok(r) => r,
                                    Err(_) => {
                                        continue;
                                    }
                                }
                            }
                            _ => {
                                continue;
                            }
                        },
                        _ => {
                            // Probably permission denied
                            continue;
                        }
                    },
                };
                // We now have the release, we need to upload the assets to it
                let actions = octocrab.actions();
                if let Ok(artifacts) = actions
                    .list_workflow_run_artifacts(&owner, &repository, run_id)
                    .send()
                    .await
                {
                    if let Some(page) = artifacts.value {
                        for artifact in page {
                            if artifact
                                .name
                                .starts_with(&format!("release-binaries-signed-{}", package_name))
                                || artifact.name.starts_with(&format!(
                                    "release-installer-signed-{}",
                                    package_name
                                ))
                            {
                                // Download zip
                                if let Ok(data) = actions
                                    .download_artifact(
                                        &owner,
                                        &repository,
                                        artifact.id,
                                        ArchiveFormat::Zip,
                                    )
                                    .await
                                {
                                    let mut buf = Cursor::new(data);
                                    // Extract zip and upload
                                    if let Ok(mut archive) = ZipArchive::new(&mut buf) {
                                        for i in 0..archive.len() {
                                            if let Ok(mut file) = archive.by_index(i) {
                                                if let Some(outpath) = file.enclosed_name() {
                                                    if file.is_dir() {
                                                        continue;
                                                    }
                                                    if let Some(file_name) = outpath.file_name() {
                                                        if let Some(file_name) = file_name.to_str()
                                                        {
                                                            let mut data: Vec<u8> = vec![];
                                                            if file.read_to_end(&mut data).is_ok() {
                                                                let _ = releases
                                                                    .upload_asset(
                                                                        release.id.into_inner(),
                                                                        file_name,
                                                                        data.into(),
                                                                    )
                                                                    .send()
                                                                    .await;
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                // Mark as non draft and latest
                let _ = releases
                    .update(release.id.into_inner())
                    .draft(false)
                    .make_latest(octocrab::repos::releases::MakeLatest::True)
                    .send()
                    .await;
            }
        }
        Ok(())
    }
}

fn get_version(job: Option<&Job<PublishingJobType, PublishingRunOutput>>) -> Option<&String> {
    job.and_then(|j| {
        j.outputs
            .released
            .and_then(|r| r.then_some(j.outputs.version.as_ref()))
            .flatten()
    })
}

use anyhow;
use indexmap::IndexMap;
use serde::{de::DeserializeOwned, Deserialize};
use std::fmt::Display;
use std::fs;
use std::hash::Hash;
use std::path::PathBuf;

use crate::commands::summaries::get_success_emoji;

use super::template::SummaryTableCell;

pub mod checks;
pub mod publishing;

pub struct JobResult {
    pub failed: usize,
    pub failed_o: usize,
    pub skipped: usize,
    pub cancelled: usize,
    pub succeeded: usize,
}

impl JobResult {
    pub fn new() -> Self {
        Self {
            failed: 0,
            failed_o: 0,
            skipped: 0,
            cancelled: 0,
            succeeded: 0,
        }
    }

    pub fn merge(&mut self, other: &Self) {
        self.failed += other.failed;
        self.failed_o += other.failed_o;
        self.skipped += other.skipped;
        self.cancelled += other.cancelled;
        self.succeeded += other.succeeded;
    }
}
pub trait JobType<T>
where
    Self: Sized + Display,
    T: RunTypeOutput,
{
    fn get_headers(
        runs: &IndexMap<Self, Job<Self, T>>,
    ) -> anyhow::Result<(Vec<SummaryTableCell>, usize)>;
    fn get_colspan(&self, _outputs: &T, _max_colspan: usize) -> usize {
        1
    }
    fn get_cell_name(&self, job: &Job<Self, T>) -> (String, bool) {
        let success = self.get_job_success(job);
        (
            format!("{} - {}", get_success_emoji(success), self),
            success,
        )
    }
    fn get_job_success(&self, job: &Job<Self, T>) -> bool;
    fn get_cells(&self, job: &Job<Self, T>, colspan: usize) -> (Vec<SummaryTableCell>, JobResult);
    async fn github_side_effect(
        token: &str,
        event_name: Option<&str>,
        issue_number: Option<u64>,
        runs: &IndexMap<String, IndexMap<Self, Job<Self, T>>>,
        summary: &str,
    ) -> anyhow::Result<()>;
}
pub trait RunTypeOutput {}

#[derive(Deserialize, Debug)]
pub struct Job<T: JobType<O>, O: RunTypeOutput> {
    pub name: String,
    pub start_time: String,
    pub end_time: String,
    pub working_directory: String,
    #[serde(rename = "type")]
    pub job_type: T,
    pub server_url: String,
    pub repository: String,
    pub run_id: String,
    pub run_attempt: String,
    pub actor: String,
    pub event_name: String,
    pub outputs: O,
}

pub struct Run<T: JobType<O>, O: RunTypeOutput> {
    pub jobs: IndexMap<String, IndexMap<T, Job<T, O>>>,
}

impl<T, O> Run<T, O>
where
    T: JobType<O> + Clone + Ord + Hash + Eq + PartialEq + DeserializeOwned,
    O: RunTypeOutput + DeserializeOwned,
{
    pub fn new(working_directory: &PathBuf) -> anyhow::Result<Self> {
        let mut summaries: Vec<Job<T, O>> = vec![];
        // Read the directory
        let dir = fs::read_dir(working_directory)?;

        // Collect paths of JSON files
        let json_files: Vec<_> = dir
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().extension().map_or(false, |ext| ext == "json"))
            .map(|entry| entry.path())
            .collect();

        // Deserialize each JSON file and collect into vector
        for file_path in json_files {
            let file_content = fs::read_to_string(&file_path)?;
            summaries.push(serde_json::from_str::<Job<T, O>>(&file_content)?);
        }

        // We have a list of file we need to get to a HashMap<Package, HashMap<CheckType, Summary>>
        // load all files as ChecksSummaries
        let mut jobs = IndexMap::<String, IndexMap<T, Job<T, O>>>::new();
        for job in summaries {
            let inner_map = jobs.entry(job.name.clone()).or_default();
            inner_map.insert(job.job_type.clone(), job);
        }
        // Sort the sub keys
        let _ = jobs.iter_mut().map(|(_, checks)| {
            checks.sort_keys();
        });
        // Sort the main keys
        jobs.sort_keys();
        // Sort the keys
        Ok(Run { jobs })
    }
}

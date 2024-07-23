use anyhow;
use serde::{de::DeserializeOwned, Deserialize};
use std::collections::HashMap;
use std::fs;
use std::hash::Hash;
use std::path::PathBuf;

use super::template::SummaryTableCell;

pub mod checks;
pub mod publishing;

pub trait JobType<T: RunTypeOutput> {
    fn get_headers(runs: HashMap<Self, Job<Self, T>>) -> anyhow::Result<Vec<SummaryTableCell>>;
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
    pub jobs: HashMap<String, HashMap<T, Job<T, O>>>,
}

impl<T, O> Run<T, O>
where
    T: JobType<O> + Clone + Hash + Eq + PartialEq + DeserializeOwned,
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
        let mut jobs = HashMap::<String, HashMap<T, Job<T, O>>>::new();
        for job in summaries {
            let inner_map = jobs.entry(job.name.clone()).or_default();
            inner_map.insert(job.job_type.clone(), job);
        }
        Ok(Run { jobs })
    }

    pub fn get_headers(&self, package: &str) -> Option<Vec<SummaryTableCell>> {
        self.jobs.get(package).map(|runs| T::get_headers(runs))
    }
}

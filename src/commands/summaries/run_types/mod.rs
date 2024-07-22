use anyhow;
use serde::{de::DeserializeOwned, Deserialize};
use std::collections::HashMap;
use std::fs;
use std::hash::Hash;
use std::path::PathBuf;

pub mod checks;
pub mod publishing;

pub trait JobType {}
pub trait RunTypeOutput {}

#[derive(Deserialize, Debug)]
pub struct Run<T: JobType, O: RunTypeOutput> {
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

impl<T, O> Run<T, O>
where
    T: JobType + Clone + Hash + Eq + PartialEq,
    O: RunTypeOutput,
{
    pub fn load(working_directory: &PathBuf) -> anyhow::Result<HashMap<String, HashMap<T, Self>>>
    where
        Self: Sized + DeserializeOwned,
    {
        let mut summaries: Vec<Self> = vec![];
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
            summaries.push(serde_json::from_str::<Self>(&file_content)?);
        }

        // We have a list of file we need to get to a HashMap<Package, HashMap<CheckType, Summary>>
        // load all files as ChecksSummaries
        let mut checks_map = HashMap::<String, HashMap<T, Self>>::new();
        for summary in summaries {
            let inner_map = checks_map.entry(summary.name.clone()).or_default();
            inner_map.insert(summary.job_type.clone(), summary);
        }
        Ok(checks_map)
    }
}

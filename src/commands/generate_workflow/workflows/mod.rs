use indexmap::IndexMap;
use serde_yaml::Value;
pub mod publish_docker;
pub mod publish_npm_napi;
pub mod publish_rust_binary;
pub mod publish_rust_installer;
pub mod publish_rust_registry;

pub trait Workflow {
    fn job_prefix_key(&self) -> String;
    fn job_label(&self) -> String;
    fn workflow_name(&self) -> String;
    fn publish_info_key(&self) -> String;
    fn get_inputs(&self) -> IndexMap<String, Value>;
    fn get_additional_dependencies(&self) -> Option<Vec<String>>;
}

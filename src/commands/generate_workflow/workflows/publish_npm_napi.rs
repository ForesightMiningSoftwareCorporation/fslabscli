use indexmap::IndexMap;
use serde_yaml::Value;

use super::Workflow;

#[derive(Default, Clone)]
pub struct PublishNpmNapiWorkflowOutputs {
    /// Was the binary released
    pub _released: bool,
}

#[derive(Default, Clone)]
pub struct PublishNpmNapiWorkflowInputs {
    /// Working directory to run the cargo command
    pub working_directory: String,
}

impl From<&PublishNpmNapiWorkflowInputs> for IndexMap<String, Value> {
    fn from(val: &PublishNpmNapiWorkflowInputs) -> Self {
        let mut map: IndexMap<String, Value> = IndexMap::new();
        map.insert(
            "working_directory".to_string(),
            val.working_directory.clone().into(),
        );
        map
    }
}

pub struct PublishNpmNapiWorkflow {
    pub inputs: PublishNpmNapiWorkflowInputs,
    pub _outputs: Option<PublishNpmNapiWorkflowOutputs>,
}

impl PublishNpmNapiWorkflow {
    pub fn new(working_directory: String, _dynamic_value_base: &str) -> Self {
        Self {
            inputs: PublishNpmNapiWorkflowInputs { working_directory },
            _outputs: None,
        }
    }
}

impl Workflow for PublishNpmNapiWorkflow {
    fn job_prefix_key(&self) -> String {
        "publish_npm_napi".to_string()
    }

    fn job_label(&self) -> String {
        "Publish Npm Napi".to_string()
    }
    fn workflow_name(&self) -> String {
        "npm-napi-release".to_string()
    }
    fn publish_info_key(&self) -> String {
        "npm_napi".to_string()
    }
    fn get_inputs(&self) -> IndexMap<String, Value> {
        (&self.inputs).into()
    }
    fn get_additional_dependencies(&self) -> Option<Vec<String>> {
        None
    }
}

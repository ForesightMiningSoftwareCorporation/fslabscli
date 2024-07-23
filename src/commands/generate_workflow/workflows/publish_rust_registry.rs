use indexmap::IndexMap;
use serde_yaml::Value;

use super::Workflow;

#[derive(Default, Clone)]
pub struct PublishRustRegistryWorkflowOutputs {
    /// Was the binary released
    pub _released: bool,
}

#[derive(Default, Clone)]
pub struct PublishRustRegistryWorkflowInputs {
    /// Package name
    pub package: String,
    /// Package version
    pub version: String,
    /// Working directory to run the cargo command
    pub working_directory: String,
    /// Which toolchain to use
    pub toolchain: String,
    /// Additional args to pass to the cargo command
    pub additional_args: String,
    /// Additional script to pass to the cargo command
    pub additional_script: String,
    /// Additional args to pass to the cargo command
    pub custom_cargo_commands: String,
    /// Public release
    pub public_release: String,
    pub ci_runner: String,
}

impl From<&PublishRustRegistryWorkflowInputs> for IndexMap<String, Value> {
    fn from(val: &PublishRustRegistryWorkflowInputs) -> Self {
        let mut map: IndexMap<String, Value> = IndexMap::new();
        map.insert("package".to_string(), val.package.clone().into());
        map.insert("version".to_string(), val.version.clone().into());
        map.insert(
            "working_directory".to_string(),
            val.working_directory.clone().into(),
        );
        map.insert("toolchain".to_string(), val.toolchain.clone().into());
        map.insert(
            "public_release".to_string(),
            val.public_release.clone().into(),
        );
        map.insert(
            "additional_args".to_string(),
            val.additional_args.clone().into(),
        );
        map.insert(
            "additional_script".to_string(),
            val.additional_script.clone().into(),
        );
        map.insert(
            "custom_cargo_commands".to_string(),
            val.custom_cargo_commands.clone().into(),
        );
        map.insert("ci_runner".to_string(), val.ci_runner.clone().into());
        map
    }
}

pub struct PublishRustRegistryWorkflow {
    pub inputs: PublishRustRegistryWorkflowInputs,
    pub _outputs: Option<PublishRustRegistryWorkflowOutputs>,
}

impl PublishRustRegistryWorkflow {
    pub fn new(package: String, working_directory: String, dynamic_value_base: &str) -> Self {
        Self {
            inputs: PublishRustRegistryWorkflowInputs {
                package,
                working_directory,
                version: format!(
                    "${{{{ {}.{}) }}}}",
                    dynamic_value_base, "publish_detail.version"
                ),
                toolchain: format!("${{{{ {}.{}) }}}}", dynamic_value_base, "toolchain"),
                additional_args: format!(
                    "${{{{ {}.{}) }}}}",
                    dynamic_value_base, "publish.args.additional_args"
                ),
                additional_script: format!(
                    "${{{{ {}.{}) }}}}",
                    dynamic_value_base, "publish.args.additional_script"
                ),
                custom_cargo_commands: format!(
                    "${{{{ {}.{}) }}}}",
                    dynamic_value_base, "publish.args.custom_cargo_commands"
                ),
                public_release: format!(
                    "${{{{ {}).cargo.allow_public && 'true' || 'false' }}}}",
                    dynamic_value_base
                ),
                ci_runner: format!(
                    "${{{{ {}.{}) }}}}",
                    dynamic_value_base, "publish_detail.ci_runner"
                ),
            },

            _outputs: None,
        }
    }
}

impl Workflow for PublishRustRegistryWorkflow {
    fn job_prefix_key(&self) -> String {
        "publish_rust_registry".to_string()
    }

    fn job_label(&self) -> String {
        "Publish rust registry".to_string()
    }
    fn workflow_name(&self) -> String {
        "rust_registry_publish".to_string()
    }
    fn publish_info_key(&self) -> String {
        "cargo".to_string()
    }
    fn get_inputs(&self) -> IndexMap<String, Value> {
        (&self.inputs).into()
    }
    fn get_additional_dependencies(&self) -> Option<Vec<String>> {
        None
    }
}

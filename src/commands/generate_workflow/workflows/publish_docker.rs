use indexmap::IndexMap;
use serde_yaml::Value;

use super::Workflow;

#[derive(Default, Clone)]
pub struct PublishDockerWorkflowOutputs {
    /// Was the binary released
    pub _released: bool,
}

#[derive(Default, Clone)]
pub struct PublishDockerWorkflowInputs {
    /// Package name
    pub package: String,
    /// Package version
    pub version: String,
    /// Docker image
    pub image: String,
    /// Docker Context image
    pub context: Option<String>,
    /// Dockerfile path
    pub dockerfile: Option<String>,
    /// Docker registry
    pub registry: Option<String>,
    /// Which toolchain to use
    pub toolchain: String,
    /// Working directory to run the cargo command
    pub working_directory: String,
}

impl From<&PublishDockerWorkflowInputs> for IndexMap<String, Value> {
    fn from(val: &PublishDockerWorkflowInputs) -> Self {
        let mut map: IndexMap<String, Value> = IndexMap::new();
        map.insert("package".to_string(), val.package.clone().into());
        map.insert("version".to_string(), val.version.clone().into());
        map.insert("image".to_string(), val.image.clone().into());
        map.insert("toolchain".to_string(), val.toolchain.clone().into());
        if let Some(context) = &val.context {
            map.insert("context".to_string(), context.clone().into());
        }
        if let Some(dockerfile) = &val.dockerfile {
            map.insert("dockerfile".to_string(), dockerfile.clone().into());
        }
        if let Some(registry) = &val.registry {
            map.insert("registry".to_string(), registry.clone().into());
        }
        map.insert(
            "working_directory".to_string(),
            val.working_directory.clone().into(),
        );
        map
    }
}

pub struct PublishDockerWorkflow {
    pub inputs: PublishDockerWorkflowInputs,
    pub _outputs: Option<PublishDockerWorkflowOutputs>,
}

impl PublishDockerWorkflow {
    pub fn new(
        package: String,
        image: String,
        working_directory: String,
        context: Option<String>,
        dockerfile: Option<String>,
        registry: Option<String>,
        dynamic_value_base: &str,
    ) -> Self {
        Self {
            inputs: PublishDockerWorkflowInputs {
                package,
                image,
                working_directory,
                context,
                dockerfile,
                registry,
                version: format!("${{{{ {}.{}) }}}}", dynamic_value_base, "version"),
                toolchain: format!("${{{{ {}.{}) }}}}", dynamic_value_base, "toolchain"),
            },
            _outputs: None,
        }
    }
}

impl Workflow for PublishDockerWorkflow {
    fn job_prefix_key(&self) -> String {
        "publish_docker".to_string()
    }

    fn job_label(&self) -> String {
        "Publish Docker".to_string()
    }
    fn workflow_name(&self) -> String {
        "docker_publish".to_string()
    }
    fn publish_info_key(&self) -> String {
        "docker".to_string()
    }
    fn get_inputs(&self) -> IndexMap<String, Value> {
        (&self.inputs).into()
    }
    fn get_additional_dependencies(&self) -> Option<Vec<String>> {
        None
    }
}

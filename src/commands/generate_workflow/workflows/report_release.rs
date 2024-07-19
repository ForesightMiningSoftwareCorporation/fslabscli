use indexmap::IndexMap;
use serde_yaml::Value;

use super::Workflow;

#[derive(Default, Clone)]
pub struct ReportReleaseWorkflowOutputs {
    /// Was the binary released
    pub _released: bool,
}

#[derive(Default, Clone)]
pub struct ReportReleaseWorkflowInputs {
    /// Working directory to run the cargo command
    pub working_directory: String,
    /// Was the package released as a crate
    pub registry_release: Option<String>,
    /// Was the package released as a binary
    pub binary_release: Option<String>,
    /// Was the package released as an installer
    pub installer_release: Option<String>,
    /// Was the package released as a docker image
    pub docker_release: Option<String>,
    /// Was the package released as napi
    pub npm_napi_release: Option<String>,
}

impl From<&ReportReleaseWorkflowInputs> for IndexMap<String, Value> {
    fn from(val: &ReportReleaseWorkflowInputs) -> Self {
        let mut map: IndexMap<String, Value> = IndexMap::new();
        map.insert(
            "working_directory".to_string(),
            val.working_directory.clone().into(),
        );
        if let Some(registry_release) = &val.registry_release {
            map.insert(
                "registry_release".to_string(),
                registry_release.clone().into(),
            );
        }
        if let Some(binary_release) = &val.binary_release {
            map.insert("binary_release".to_string(), binary_release.clone().into());
        }
        if let Some(installer_release) = &val.installer_release {
            map.insert(
                "installer_release".to_string(),
                installer_release.clone().into(),
            );
        }
        if let Some(docker_release) = &val.docker_release {
            map.insert("docker_release".to_string(), docker_release.clone().into());
        }
        if let Some(npm_napi_release) = &val.npm_napi_release {
            map.insert(
                "npm_napi_release".to_string(),
                npm_napi_release.clone().into(),
            );
        }
        map
    }
}

pub struct ReportReleaseWorkflow {
    pub inputs: ReportReleaseWorkflowInputs,
    pub _outputs: Option<ReportReleaseWorkflowOutputs>,
}

impl ReportReleaseWorkflow {
    pub fn new(
        package: &str,
        working_directory: &str,
        registry: bool,
        binary: bool,
        installer: bool,
        docker: bool,
        npm_napi: bool,
        _dynamic_value_base: &str,
    ) -> Self {
        Self {
            inputs: ReportReleaseWorkflowInputs {
                working_directory: working_directory.to_string(),
                registry_release: match registry {
                    true => Some(format!("${{ needs.publish_rust_registry_{}.outputs.released == 'true' && 'true' || 'false' }}", package)),
                    false => None,
                },
                binary_release: match binary {
                    true => Some(format!("${{ needs.publish_rust_binary_{}.outputs.released == 'true' && 'true' || 'false' }}", package)),
                    false => None,
                },
                installer_release: match installer {
                    true => Some(format!("${{ needs.publish_rust_installer_{}.outputs.released == 'true' && 'true' || 'false' }}", package)),
                    false => None,
                },
                docker_release: match docker {
                    true => Some(format!("${{ needs.publish_docker_{}.outputs.released == 'true' && 'true' || 'false' }}", package)),
                    false => None,
                },
                npm_napi_release: match npm_napi {
                    true => Some(format!("${{ needs.publish_npm_napi_{}.outputs.released == 'true' && 'true' || 'false' }}", package)),
                    false => None,
                },
            },

            _outputs: None,
        }
    }
}

impl Workflow for ReportReleaseWorkflow {
    fn job_prefix_key(&self) -> String {
        "report_release".to_string()
    }

    fn job_label(&self) -> String {
        "Report release".to_string()
    }
    fn workflow_name(&self) -> String {
        "report_release".to_string()
    }
    fn publish_info_key(&self) -> String {
        "na".to_string()
    }
    fn get_inputs(&self) -> IndexMap<String, Value> {
        (&self.inputs).into()
    }
    fn get_additional_dependencies(&self) -> Option<Vec<String>> {
        None
    }
}

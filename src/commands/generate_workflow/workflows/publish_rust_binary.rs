use indexmap::IndexMap;
use serde_yaml::Value;

use super::Workflow;

#[derive(Default, Clone)]
pub struct PublishRustBinaryWorkflowOutputs {
    /// Was the binary released
    pub released: bool,
}

#[derive(Default, Clone)]
pub struct PublishRustBinaryWorkflowInputs {
    /// Package name
    pub package: String,
    /// Package version
    pub version: String,
    /// Which toolchain to use
    pub toolchain: String,
    pub launcher_app_name: String,
    pub launcher_fallback_app_name: String,
    /// Which release_channel
    pub release_channel: String,
    /// Binaries targets
    pub targets: Option<Vec<String>>,
    /// Additional args to pass to the cargo command
    pub additional_args: Option<String>,
    /// Working directory to run the cargo command
    pub working_directory: String, // ''
    /// Should the binary bin be signed
    pub sign_build: Option<bool>,
    /// Used to configure the target runner and extension
    pub targets_config: Option<String>, // '{"x86_64-pc-windows-msvc":{"extension":".exe","runner":"windows-latest-16-cores-custom"},"x86_64-unknown-linux-gnu":{"extension":"","runner":"ubuntu-latest-16-cores"}}'
}

impl From<&PublishRustBinaryWorkflowInputs> for IndexMap<String, Value> {
    fn from(val: &PublishRustBinaryWorkflowInputs) -> Self {
        let mut map: IndexMap<String, Value> = IndexMap::new();
        map.insert("package".to_string(), val.package.clone().into());
        map.insert("version".to_string(), val.version.clone().into());
        map.insert("toolchain".to_string(), val.toolchain.clone().into());
        map.insert(
            "release_channel".to_string(),
            val.release_channel.clone().into(),
        );
        map.insert(
            "working_directory".to_string(),
            val.working_directory.clone().into(),
        );
        map.insert(
            "launcher_app_name".to_string(),
            val.launcher_app_name.clone().into(),
        );
        map.insert(
            "launcher_fallback_app_name".to_string(),
            val.launcher_fallback_app_name.clone().into(),
        );

        if let Some(targets) = &val.targets {
            map.insert(
                "targets".to_string(),
                format!("[\"{}\"]", targets.clone().join("\",\"")).into(),
            );
        }
        if let Some(additional_args) = &val.additional_args {
            map.insert(
                "additional_args".to_string(),
                additional_args.clone().into(),
            );
        }
        if let Some(sign_build) = &val.sign_build {
            map.insert("sign_build".to_string(), sign_build.clone().into());
        }
        if let Some(targets_config) = &val.targets_config {
            map.insert("targets_config".to_string(), targets_config.clone().into());
        }
        map
    }
}

pub struct PublishRustBinaryWorkflow {
    pub inputs: PublishRustBinaryWorkflowInputs,
    pub outputs: Option<PublishRustBinaryWorkflowOutputs>,
}

impl PublishRustBinaryWorkflow {
    pub fn new(
        package: String,
        version: String,
        toolchain: String,
        release_channel: String,
        targets: Vec<String>,
        additional_args: Option<String>,
        working_directory: String,
        sign_build: bool,
        launcher_app_name: String,
        launcher_fallback_app_name: String,
    ) -> Self {
        Self {
            inputs: PublishRustBinaryWorkflowInputs {
                package,
                version,
                toolchain,
                release_channel,
                targets: Some(targets),
                additional_args,
                working_directory,
                sign_build: Some(sign_build),
                targets_config: None,
                launcher_app_name,
                launcher_fallback_app_name,
            },
            outputs: None,
        }
    }
}

impl Workflow for PublishRustBinaryWorkflow {
    fn job_prefix_key(&self) -> String {
        "publish_rust_binary".to_string()
    }

    fn job_label(&self) -> String {
        "Publish Rust binary".to_string()
    }
    fn workflow_name(&self) -> String {
        "rust_binary_publish".to_string()
    }
    fn publish_info_key(&self) -> String {
        "binary".to_string()
    }
    fn get_inputs(&self) -> IndexMap<String, Value> {
        (&self.inputs).into()
    }
}

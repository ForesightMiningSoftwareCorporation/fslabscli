use indexmap::IndexMap;
use serde_yaml::Value;

use super::Workflow;

#[derive(Default, Clone)]
pub struct PublishRustInstallerWorkflowOutputs {
    /// Was the binary released
    pub _released: bool,
}

#[derive(Default, Clone)]
pub struct PublishRustInstallerWorkflowInputs {
    /// Package name
    pub package: String,
    /// Package version
    pub version: String,
    /// Which toolchain to use
    pub toolchain: String,
    pub application_name: String,
    pub application_fallback_name: String,
    /// Name of the blob to download to get access to the launcher
    pub launcher_blob_dir: String,
    /// Name of the blob to download to get access to the launcher
    pub launcher_name: String,
    /// Name of the blob to download to get access to the application
    pub package_blob_dir: String,
    /// Name of the blob to download to get access to the application
    pub package_name: String,
    /// Name of the blob to upload the installer to
    pub installer_blob_dir: String,
    /// Name of the installer to
    pub installer_name: String,
    /// Name of the signed installer to
    pub installer_signed_name: String,
    /// Which release_channel
    pub release_channel: String,
    /// Working directory to run the cargo command
    pub working_directory: String, // ''
    /// Should the binary bin be signed
    pub sign_build: Option<bool>,
    pub upgrade_code: String,
    pub guid_prefix: String,
    pub sas_expiry: String,
    pub sub_apps_download_script: String,
}

impl From<&PublishRustInstallerWorkflowInputs> for IndexMap<String, Value> {
    fn from(val: &PublishRustInstallerWorkflowInputs) -> Self {
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
            "application_name".to_string(),
            val.application_name.clone().into(),
        );
        map.insert(
            "application_fallback_name".to_string(),
            val.application_fallback_name.clone().into(),
        );
        map.insert(
            "launcher_blob_dir".to_string(),
            val.launcher_blob_dir.clone().into(),
        );
        map.insert(
            "launcher_name".to_string(),
            val.launcher_name.clone().into(),
        );
        map.insert(
            "package_blob_dir".to_string(),
            val.package_blob_dir.clone().into(),
        );

        map.insert("package_name".to_string(), val.package_name.clone().into());
        map.insert(
            "installer_blob_dir".to_string(),
            val.installer_blob_dir.clone().into(),
        );
        map.insert(
            "installer_name".to_string(),
            val.installer_name.clone().into(),
        );
        map.insert(
            "installer_signed_name".to_string(),
            val.installer_signed_name.clone().into(),
        );
        map.insert("upgrade_code".to_string(), val.upgrade_code.clone().into());

        map.insert("guid_prefix".to_string(), val.guid_prefix.clone().into());
        map.insert("sas_expiry".to_string(), val.sas_expiry.clone().into());
        map.insert(
            "sub_apps_download_script".to_string(),
            val.sub_apps_download_script.clone().into(),
        );

        if let Some(sign_build) = &val.sign_build {
            map.insert("sign_build".to_string(), (*sign_build).into());
        }
        map
    }
}

pub struct PublishRustInstallerWorkflow {
    pub inputs: PublishRustInstallerWorkflowInputs,
    pub _outputs: Option<PublishRustInstallerWorkflowOutputs>,
}

impl PublishRustInstallerWorkflow {
    pub fn new(
        package: String,
        working_directory: String,
        sign_build: bool,
        dynamic_value_base: &str,
    ) -> Self {
        Self {
            inputs: PublishRustInstallerWorkflowInputs {
                package,
                sign_build: Some(sign_build),
                working_directory,
                version: format!(
                    "${{{{ {}.{}) }}}}",
                    dynamic_value_base, "publish_detail.binary.rc_version"
                ),
                toolchain: format!("${{{{ {}.{}) }}}}", dynamic_value_base, "toolchain"),
                release_channel: format!(
                    "${{{{ {}.{}) }}}}",
                    dynamic_value_base, "publish_detail.release_channel"
                ),
                application_name: format!(
                    "${{{{ {}.{}) }}}}",
                    dynamic_value_base, "publish_detail.binary.name"
                ),
                application_fallback_name: format!(
                    "${{{{ {}.{}) }}}}",
                    dynamic_value_base, "publish_detail.binary.fallback_name"
                ),
                launcher_blob_dir: format!(
                    "${{{{ {}.{}) }}}}",
                    dynamic_value_base, "publish_detail.binary.installer.launcher_blob_dir"
                ),
                launcher_name: format!(
                    "${{{{ {}.{}) }}}}",
                    dynamic_value_base, "publish_detail.binary.installer.launcher_name"
                ),
                package_blob_dir: format!(
                    "${{{{ {}.{}) }}}}",
                    dynamic_value_base, "publish_detail.binary.installer.package_blob_dir"
                ),
                package_name: format!(
                    "${{{{ {}.{}) }}}}",
                    dynamic_value_base, "publish_detail.binary.installer.package_name"
                ),
                installer_blob_dir: format!(
                    "${{{{ {}.{}) }}}}",
                    dynamic_value_base, "publish_detail.binary.installer.installer_blob_dir"
                ),
                installer_name: format!(
                    "${{{{ {}.{}) }}}}",
                    dynamic_value_base, "publish_detail.binary.installer.installer_name"
                ),
                installer_signed_name: format!(
                    "${{{{ {}.{}) }}}}",
                    dynamic_value_base, "publish_detail.binary.installer.installer_signed_name"
                ),
                upgrade_code: format!(
                    "${{{{ {}.{}) }}}}",
                    dynamic_value_base, "publish_detail.binary.installer.upgrade_code"
                ),
                guid_prefix: format!(
                    "${{{{ {}.{}) }}}}",
                    dynamic_value_base, "publish_detail.binary.installer.guid_prefix"
                ),
                sas_expiry: format!(
                    "${{{{ {}.{}) }}}}",
                    dynamic_value_base, "publish_detail.binary.installer.sas_expiry"
                ),
                sub_apps_download_script: format!(
                    "${{{{ {}.{}) }}}}",
                    dynamic_value_base, "publish_detail.binary.installer.sub_apps_download_script"
                ),
            },
            _outputs: None,
        }
    }
}

impl Workflow for PublishRustInstallerWorkflow {
    fn job_prefix_key(&self) -> String {
        "publish_rust_installer".to_string()
    }

    fn job_label(&self) -> String {
        "Publish Rust installer".to_string()
    }
    fn workflow_name(&self) -> String {
        "rust_installer_publish".to_string()
    }
    fn publish_info_key(&self) -> String {
        "binary.installer".to_string()
    }
    fn get_inputs(&self) -> IndexMap<String, Value> {
        (&self.inputs).into()
    }
    fn get_additional_dependencies(&self) -> Option<Vec<String>> {
        Some(vec![
            format!("publish_rust_binary_{}", self.inputs.package),
            format!("publish_rust_binary_{}_launcher", self.inputs.package),
        ])
    }
}

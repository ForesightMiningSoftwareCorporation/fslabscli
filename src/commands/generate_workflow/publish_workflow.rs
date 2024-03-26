use indexmap::IndexMap;
use serde_yaml::Value;

use crate::commands::generate_workflow::StringBool;

#[derive(Default, Clone)]
pub struct PublishWorkflowArgs {
    /// Should the tests be run
    pub skip_test: Option<StringBool>,
    /// Skip tests when no changes were detected in any cargo workspace
    pub skip_tests_no_changes: Option<StringBool>,
    /// Should miri tests be run
    pub skip_miri_test: Option<StringBool>,
    /// Should the crate be published
    pub publish: Option<StringBool>,
    /// Should the crate be published to the private registry
    pub publish_private_registry: Option<StringBool>,
    /// Should the crate be published to the public registry
    pub publish_public_registry: Option<StringBool>,
    /// Should the docker image be built and published
    pub publish_docker: Option<StringBool>,
    /// Should the binary be built and published
    pub publish_binary: Option<StringBool>,
    /// Should the npm napi package be built and published
    pub publish_npm_napi: Option<StringBool>,
    /// Should an installer be built and published
    pub publish_installer: Option<StringBool>,
    /// Rust toolchain to install.
    /// Do not set this to moving targets like "stable".
    /// Instead, leave it empty and regularly bump the default in this file.
    pub toolchain: Option<String>,
    /// Rust toolchain to use for Miri.
    /// Do not set this to moving targets like "nightly".
    /// Instead, leave it empty and regularly bump the default in this file.
    pub miri_toolchain: Option<String>,
    /// Hard coded release channel
    pub release_channel: Option<String>,
    /// Path of additional cache to get
    pub additional_cache_path: Option<String>,
    /// Key of additional cache to get
    pub additional_cache_key: Option<String>,
    /// Script to run if additional cache miss
    pub additional_cache_miss: Option<String>,
    /// Additional script to run before the additional packages
    pub additional_script: Option<String>,
    /// Package that needs to be installed before Rust compilation can happens
    pub required_packages: Option<String>,
    /// Working directory to run the cargo command
    pub working_directory: Option<String>,
    /// Additional arguments to pass to the cargo command
    pub additional_args: Option<String>,
    /// Custom cargo commands that will be run after login
    pub custom_cargo_commands: Option<String>,
    /// Path to docker context
    pub docker_context: Option<String>,
    /// The path to the Dockerfile to use
    pub dockerfile: Option<String>,
    /// Docker image name
    pub docker_image: Option<String>,
    /// Docker registry
    pub docker_registry: Option<String>,
    /// Force the publish test to be marked as non required
    pub force_nonrequired_publish_test: Option<StringBool>,
    /// Should the binary bin be signed
    pub binary_sign_build: Option<StringBool>,
    /// Binaries targets
    pub binary_targets: Option<Vec<String>>,
    /// Name of the binary aplication
    pub binary_application_name: Option<String>,
    /// Should the release be reported
    pub report_release: Option<StringBool>,
}

impl PublishWorkflowArgs {
    pub fn merge(self, other: PublishWorkflowArgs) -> Self {
        Self {
            skip_test: self.skip_test.or(other.skip_test),
            skip_tests_no_changes: self.skip_tests_no_changes.or(other.skip_tests_no_changes),
            skip_miri_test: self.skip_miri_test.or(other.skip_miri_test),
            publish: self.publish.or(other.publish),
            publish_private_registry: self
                .publish_private_registry
                .or(other.publish_private_registry),
            publish_public_registry: self
                .publish_public_registry
                .or(other.publish_public_registry),
            publish_docker: self.publish_docker.or(other.publish_docker),
            publish_binary: self.publish_binary.or(other.publish_binary),
            publish_npm_napi: self.publish_npm_napi.or(other.publish_npm_napi),
            publish_installer: self.publish_installer.or(other.publish_installer),
            toolchain: self.toolchain.or(other.toolchain),
            miri_toolchain: self.miri_toolchain.or(other.miri_toolchain),
            release_channel: self.release_channel.or(other.release_channel),
            additional_cache_path: self.additional_cache_path.or(other.additional_cache_path),
            additional_cache_key: self.additional_cache_key.or(other.additional_cache_key),
            additional_cache_miss: self.additional_cache_miss.or(other.additional_cache_miss),
            additional_script: self.additional_script.or(other.additional_script),
            required_packages: self.required_packages.or(other.required_packages),
            working_directory: self.working_directory.or(other.working_directory),
            additional_args: self.additional_args.or(other.additional_args),
            custom_cargo_commands: self.custom_cargo_commands.or(other.custom_cargo_commands),
            docker_context: self.docker_context.or(other.docker_context),
            dockerfile: self.dockerfile.or(other.dockerfile),
            docker_image: self.docker_image.or(other.docker_image),
            docker_registry: self.docker_registry.or(other.docker_registry),
            force_nonrequired_publish_test: self
                .force_nonrequired_publish_test
                .or(other.force_nonrequired_publish_test),
            binary_sign_build: self.binary_sign_build.or(other.binary_sign_build),
            binary_targets: self.binary_targets.or(other.binary_targets),
            binary_application_name: self
                .binary_application_name
                .or(other.binary_application_name),
            report_release: self.report_release.or(other.report_release),
        }
    }
}

impl From<IndexMap<String, Value>> for PublishWorkflowArgs {
    fn from(value: IndexMap<String, Value>) -> Self {
        let mut me = Self {
            ..Default::default()
        };
        for (k, v) in value {
            match k.as_str() {
                "skip_test" => me.skip_test = Some(v.into()),
                "skip_tests_no_changes" => me.skip_tests_no_changes = Some(v.into()),
                "skip_miri_test" => me.skip_miri_test = Some(v.into()),
                "publish" => me.publish = Some(v.into()),
                "publish_private_registry" => me.publish_private_registry = Some(v.into()),
                "publish_public_registry" => me.publish_public_registry = Some(v.into()),
                "publish_docker" => me.publish_docker = Some(v.into()),
                "publish_binary" => me.publish_binary = Some(v.into()),
                "publish_npm_napi" => me.publish_npm_napi = Some(v.into()),
                "publish_installer" => me.publish_installer = Some(v.into()),
                "toolchain" => {
                    me.toolchain = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "miri_toolchain" => {
                    me.miri_toolchain = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "release_channel" => {
                    me.release_channel = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "additional_cache_path" => {
                    me.additional_cache_path = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "additional_cache_key" => {
                    me.additional_cache_key = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "additional_cache_miss" => {
                    me.additional_cache_miss = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "additional_script" => {
                    me.additional_script = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "required_packages" => {
                    me.required_packages = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "working_directory" => {
                    me.working_directory = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "additional_args" => {
                    me.additional_args = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "custom_cargo_commands" => {
                    me.custom_cargo_commands = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "docker_context" => {
                    me.docker_context = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "dockerfile" => {
                    me.dockerfile = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "docker_image" => {
                    me.docker_image = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "docker_registry" => {
                    me.docker_registry = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "force_nonrequired_publish_test" => {
                    me.force_nonrequired_publish_test = Some(v.into())
                }
                "binary_sign_build" => me.binary_sign_build = Some(v.into()),
                "binary_application_name" => {
                    me.binary_application_name = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "binary_targets" => {
                    me.binary_targets = match v {
                        Value::String(s) => serde_json::from_str(&s).ok().into(),
                        _ => None,
                    }
                }
                "report_release" => me.report_release = Some(v.into()),
                _ => {}
            }
        }
        me
    }
}

impl From<PublishWorkflowArgs> for IndexMap<String, Value> {
    fn from(val: PublishWorkflowArgs) -> Self {
        let mut map: IndexMap<String, Value> = IndexMap::new();
        if let Some(skip_test) = val.skip_test {
            map.insert("skip_test".to_string(), skip_test.into());
        }
        if let Some(skip_tests_no_changes) = val.skip_tests_no_changes {
            map.insert(
                "skip_tests_no_changes".to_string(),
                skip_tests_no_changes.into(),
            );
        }
        if let Some(skip_miri_test) = val.skip_miri_test {
            map.insert("skip_miri_test".to_string(), skip_miri_test.into());
        }
        if let Some(publish) = val.publish {
            map.insert("publish".to_string(), publish.into());
        }
        if let Some(publish_private_registry) = val.publish_private_registry {
            map.insert(
                "publish_private_registry".to_string(),
                publish_private_registry.into(),
            );
        }
        if let Some(publish_public_registry) = val.publish_public_registry {
            map.insert(
                "publish_public_registry".to_string(),
                publish_public_registry.into(),
            );
        }
        if let Some(publish_docker) = val.publish_docker {
            map.insert("publish_docker".to_string(), publish_docker.into());
        }
        if let Some(publish_binary) = val.publish_binary {
            map.insert("publish_binary".to_string(), publish_binary.into());
        }
        if let Some(publish_npm_napi) = val.publish_npm_napi {
            map.insert("publish_npm_napi".to_string(), publish_npm_napi.into());
        }
        if let Some(publish_installer) = val.publish_installer {
            map.insert("publish_installer".to_string(), publish_installer.into());
        }
        if let Some(toolchain) = val.toolchain {
            map.insert("toolchain".to_string(), toolchain.into());
        }
        if let Some(miri_toolchain) = val.miri_toolchain {
            map.insert("miri_toolchain".to_string(), miri_toolchain.into());
        }
        if let Some(release_channel) = val.release_channel {
            map.insert("release_channel".to_string(), release_channel.into());
        }
        if let Some(additional_cache_path) = val.additional_cache_path {
            map.insert(
                "additional_cache_path".to_string(),
                additional_cache_path.into(),
            );
        }
        if let Some(additional_cache_key) = val.additional_cache_key {
            map.insert(
                "additional_cache_key".to_string(),
                additional_cache_key.into(),
            );
        }
        if let Some(additional_cache_miss) = val.additional_cache_miss {
            map.insert(
                "additional_cache_miss".to_string(),
                additional_cache_miss.into(),
            );
        }
        if let Some(additional_script) = val.additional_script {
            map.insert("additional_script".to_string(), additional_script.into());
        }
        if let Some(required_packages) = val.required_packages {
            map.insert("required_packages".to_string(), required_packages.into());
        }
        if let Some(working_directory) = val.working_directory {
            map.insert("working_directory".to_string(), working_directory.into());
        }
        if let Some(additional_args) = val.additional_args {
            map.insert("additional_args".to_string(), additional_args.into());
        }
        if let Some(custom_cargo_commands) = val.custom_cargo_commands {
            map.insert(
                "custom_cargo_commands".to_string(),
                custom_cargo_commands.into(),
            );
        }
        if let Some(docker_context) = val.docker_context {
            map.insert("docker_context".to_string(), docker_context.into());
        }
        if let Some(dockerfile) = val.dockerfile {
            map.insert("dockerfile".to_string(), dockerfile.into());
        }
        if let Some(docker_image) = val.docker_image {
            map.insert("docker_image".to_string(), docker_image.into());
        }
        if let Some(docker_registry) = val.docker_registry {
            map.insert("docker_registry".to_string(), docker_registry.into());
        }
        if let Some(force_nonrequired_publish_test) = val.force_nonrequired_publish_test {
            map.insert(
                "force_nonrequired_publish_test".to_string(),
                force_nonrequired_publish_test.into(),
            );
        }
        if let Some(binary_sign_build) = val.binary_sign_build {
            map.insert("binary_sign_build".to_string(), binary_sign_build.into());
        }
        if let Some(binary_application_name) = val.binary_application_name {
            map.insert(
                "binary_application_name".to_string(),
                binary_application_name.into(),
            );
        }
        if let Some(binary_targets) = val.binary_targets {
            map.insert(
                "binary_targets".to_string(),
                format!("[\"{}\"]", binary_targets.join("\",\"")).into(),
            );
        }
        if let Some(report_release) = val.report_release {
            map.insert("report_release".to_string(), report_release.into());
        }
        map
    }
}

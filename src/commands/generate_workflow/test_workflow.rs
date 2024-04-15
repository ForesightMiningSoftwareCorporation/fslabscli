use indexmap::IndexMap;
use serde_yaml::Value;

use crate::commands::generate_workflow::StringBool;

#[derive(Default, Clone, Debug)]
pub struct TestWorkflowArgs {
    /// Package that needs to be installed before Rust compilation can happens
    pub required_packages: Option<String>,
    /// Rust toolchain to install. Do not set this to moving targets like "stable", instead leave it empty and regularly bump the default in this file.
    pub toolchain: Option<String>,
    /// Rust toolchain to install. Do not set this to moving targets like "nightly", instead leave it empty and regularly bump the default in this file.
    pub nightly_toolchain: Option<String>,
    /// Additional arguments to pass to the cargo command
    pub additional_args: Option<String>,
    /// Path of additional cache to get
    pub additional_cache_path: Option<String>,
    /// Key of additional cache to get
    pub additional_cache_key: Option<String>,
    /// Script to run if additional cache miss
    pub additional_cache_miss: Option<String>,
    /// Additional script to run before the additional packages
    pub additional_script: Option<String>,
    /// Subdirectory to treat as repo root
    pub working_directory: Option<String>,
    /// Custom cargo commands that will be run after login
    pub custom_cargo_commands: Option<String>,
    /// Should all the test ran or fail early
    pub fail_fast: Option<StringBool>,
    /// Should we skip miri test (useful when tests are incompatible)
    pub skip_miri_test: Option<StringBool>,
    /// Should the publish dry-run test be marked as required
    pub test_publish_required: Option<StringBool>,
    /// Should a postgres service be started and feeded through env variable
    pub service_database: Option<StringBool>,
}

impl TestWorkflowArgs {
    pub fn merge(self, other: TestWorkflowArgs) -> Self {
        Self {
            required_packages: self.required_packages.or(other.required_packages),
            toolchain: self.toolchain.or(other.toolchain),
            nightly_toolchain: self.nightly_toolchain.or(other.nightly_toolchain),
            additional_args: self.additional_args.or(other.additional_args),
            additional_cache_path: self.additional_cache_path.or(other.additional_cache_path),
            additional_cache_key: self.additional_cache_key.or(other.additional_cache_key),
            additional_cache_miss: self.additional_cache_miss.or(other.additional_cache_miss),
            additional_script: self.additional_script.or(other.additional_script),
            working_directory: self.working_directory.or(other.working_directory),
            custom_cargo_commands: self.custom_cargo_commands.or(other.custom_cargo_commands),
            fail_fast: self.fail_fast.or(other.fail_fast),
            skip_miri_test: self.skip_miri_test.or(other.skip_miri_test),
            test_publish_required: self.test_publish_required.or(other.test_publish_required),
            service_database: self.service_database.or(other.service_database),
        }
    }
}

impl From<TestWorkflowArgs> for IndexMap<String, Value> {
    fn from(val: TestWorkflowArgs) -> Self {
        let mut map: IndexMap<String, Value> = IndexMap::new();
        if let Some(required_packages) = val.required_packages {
            map.insert("required_packages".to_string(), required_packages.into());
        }
        if let Some(toolchain) = val.toolchain {
            map.insert("toolchain".to_string(), toolchain.into());
        }
        if let Some(nightly_toolchain) = val.nightly_toolchain {
            map.insert("nightly_toolchain".to_string(), nightly_toolchain.into());
        }
        if let Some(additional_args) = val.additional_args {
            map.insert("additional_args".to_string(), additional_args.into());
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
        if let Some(working_directory) = val.working_directory {
            map.insert("working_directory".to_string(), working_directory.into());
        }
        if let Some(custom_cargo_commands) = val.custom_cargo_commands {
            map.insert(
                "custom_cargo_commands".to_string(),
                custom_cargo_commands.into(),
            );
        }
        if let Some(fail_fast) = val.fail_fast {
            map.insert("fail_fast".to_string(), fail_fast.into());
        }
        if let Some(skip_miri_test) = val.skip_miri_test {
            map.insert("skip_miri_test".to_string(), skip_miri_test.into());
        }
        if let Some(test_publish_required) = val.test_publish_required {
            map.insert(
                "test_publish_required".to_string(),
                test_publish_required.into(),
            );
        }
        if let Some(service_database) = val.service_database {
            map.insert("service_database".to_string(), service_database.into());
        }
        map
    }
}

impl From<IndexMap<String, Value>> for TestWorkflowArgs {
    fn from(value: IndexMap<String, Value>) -> Self {
        let mut me = Self {
            ..Default::default()
        };
        for (k, v) in value {
            match k.as_str() {
                "required_packages" => {
                    me.required_packages = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "toolchain" => {
                    me.toolchain = match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    }
                }
                "nightly_toolchain" => {
                    me.nightly_toolchain = match v {
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
                "working_directory" => {
                    me.working_directory = match v {
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
                "fail_fast" => me.fail_fast = Some(v.into()),
                "skip_miri_test" => me.skip_miri_test = Some(v.into()),
                "test_publish_required" => me.test_publish_required = Some(v.into()),
                "service_database" => me.service_database = Some(v.into()),
                _ => {}
            };
        }
        me
    }
}

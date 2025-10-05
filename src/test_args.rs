use std::collections::HashMap;

/// Options provided by the `package.metadata.fslabs.test.args` object in
/// a Cargo.toml.
///
/// Tests are run for one package at a time, and these options configure all
/// extra test fixture behavior beyond what is provided by cargo.
#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct TestArgs {
    /// Script that runs before services are started.
    pub pre_service_script: Option<String>,
    /// Required services that are known by `fslabscli`.
    #[serde(default)]
    pub services: KnownServices,
    /// Required services defined by user-provided commands.
    ///
    /// A service command will be spawned in a child process and kept alive for
    /// the duration of tests.
    #[serde(default)]
    pub custom_services: HashMap<ServiceName, ServiceCommand>,
    // TODO: remove and replace users with `pre_test_script`
    //
    /// TODO: what is this for?
    pub additional_cache_miss: Option<String>,
    /// A script that runs after services are started and
    /// before the test command is run.
    pub pre_test_script: Option<String>,
    /// The command used to run tests for a package.
    ///
    /// If no value is provided, the default command is "cargo test --all-targets".
    ///
    /// For testing with `wasm-bindgen-test`, you could use `test_command = "wasm-pack test"`.
    pub test_command: Option<String>,
    /// Arguments appended to the test command.
    #[serde(default)]
    pub additional_args: String,
}

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct KnownServices {
    pub azurite: bool,
    pub minio: bool,
    pub postgres: bool,
}

pub type ServiceName = String;
pub type ServiceCommand = String;

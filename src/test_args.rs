/// Options provided by the `package.metadata.fslabs.test.args` object in
/// a Cargo.toml.
///
/// Tests are run for one package at a time, and these options configure all
/// extra test fixture behavior beyond what is provided by cargo.
#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct TestArgs {
    /// Enable an Azurite service accessible during the test.
    #[serde(default)]
    pub service_azurite: bool,
    /// Enable a Postgres service accessible during the test.
    #[serde(default)]
    pub service_database: bool,
    /// Enable a Minio (S3) service accessible during the test.
    #[serde(default)]
    pub service_minio: bool,
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

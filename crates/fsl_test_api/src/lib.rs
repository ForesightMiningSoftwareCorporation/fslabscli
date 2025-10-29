use envconfig::Envconfig;
use url::Url;

/// Environment variables created by the test runner for use in tests.
#[derive(Envconfig)]
pub struct FslTestEnv {
    /// The URL used for connecting to the Postgres service.
    ///
    /// The `sqlx` crate makes use of this and knows how to load the environment
    /// variable.
    #[envconfig(from = "DATABASE_URL")]
    pub database_url: Option<Url>,

    /// The port used to connect to the Azurite BLOB service.
    #[envconfig(from = "AZURITE_BLOB_PORT")]
    pub azurite_blob_port: Option<u16>,

    /// The URL used to access the S3 service.
    #[envconfig(from = "S3_ENDPOINT")]
    pub s3_endpoint: Option<Url>,
    #[envconfig(from = "S3_ACCESS_KEY")]
    pub s3_access_key: Option<String>,
    #[envconfig(from = "S3_SECRET_ACCESS_KEY")]
    pub s3_secret_access_key: Option<String>,
}

impl FslTestEnv {
    /// Load from environment variables.
    pub fn get() -> Result<FslTestEnv, envconfig::Error> {
        FslTestEnv::init_from_env()
    }
}

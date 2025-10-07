use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
#[serde(rename_all = "snake_case")]
pub struct PackageMetadataFslabsCiPublishS3 {
    #[serde(default)]
    pub publish: bool,
    pub build_command: String,
    #[serde(default)]
    pub bucket_name: Option<String>,
    #[serde(default)]
    pub bucket_region: Option<String>,
    #[serde(default)]
    pub bucket_prefix: Option<String>,
    #[serde(default)]
    pub output_dir: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

impl PackageMetadataFslabsCiPublishS3 {
    pub async fn check(&mut self) -> anyhow::Result<()> {
        if !self.publish {
            return Ok(());
        }
        let publish = !self.build_command.is_empty()
            && self.bucket_name.is_some()
            && self.bucket_region.is_some();
        self.publish = publish;
        Ok(())
    }
}

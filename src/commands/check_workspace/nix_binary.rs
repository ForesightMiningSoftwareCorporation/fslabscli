use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
#[serde(rename_all = "snake_case")]
pub struct PackageMetadataFslabsCiPublishNixBinary {
    #[serde(default)]
    pub publish: bool,
    #[serde(default)]
    pub error: Option<String>,
}

impl PackageMetadataFslabsCiPublishNixBinary {
    pub async fn check(&mut self) -> anyhow::Result<()> {
        // if !self.publish {
        //     return Ok(());
        // }
        // // let mut publish = true;
        // self.publish = publish;
        Ok(())
    }
}

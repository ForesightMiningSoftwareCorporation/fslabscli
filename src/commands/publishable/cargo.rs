use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct PackageMetadataFslabsCiPublishCargo {
    pub publish: bool,
    pub registry: Option<Vec<String>>,
    pub allow_public: bool,
}

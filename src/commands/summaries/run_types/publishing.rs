use std::fmt::{Display, Formatter, Result as FmtResult};

use serde::{Deserialize, Serialize};

use crate::commands::summaries::CheckOutput;

use super::{JobType, RunTypeOutput};

#[derive(Deserialize, Serialize, Debug)]
#[serde(rename_all = "kebab-case")]
pub struct PublishingRunOutput {
    pub released: Option<bool>,
}

impl RunTypeOutput for PublishingRunOutput {}

#[derive(Deserialize, Serialize, Debug, Eq, Hash, PartialEq, Clone)]
#[serde(rename_all = "kebab-case")]
pub enum PublishingJobType {
    DockerPublish,
    NpmNapiPublish,
    RustBinaryPublish,
    RustInstallerPublish,
    RustRegistryPublish,
}

impl Display for PublishingJobType {
    fn fmt(&self, f: &mut Formatter) -> FmtResult {
        match self {
            Self::DockerPublish => write!(f, "docker-publish"),
            Self::NpmNapiPublish => write!(f, "npm-napi-publish"),
            Self::RustBinaryPublish => write!(f, "rust-binary-publish"),
            Self::RustInstallerPublish => write!(f, "rust-installer-publish"),
            Self::RustRegistryPublish => write!(f, "rust-registry-publish"),
        }
    }
}

impl JobType for PublishingJobType {}

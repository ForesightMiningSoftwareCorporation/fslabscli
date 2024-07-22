use std::fmt::{Display, Formatter, Result as FmtResult};

use serde::{Deserialize, Serialize};

use crate::commands::summaries::CheckOutput;

use super::{JobType, RunTypeOutput};

#[derive(Deserialize, Serialize, Debug)]
#[serde(rename_all = "kebab-case")]
pub struct CheckRunOutput {
    pub check: Option<CheckOutput>,
    pub clippy: Option<CheckOutput>,
    pub doc: Option<CheckOutput>,
    pub custom: Option<CheckOutput>,
    pub deny_advisories: Option<CheckOutput>,
    pub deny_bans: Option<CheckOutput>,
    pub deny_license: Option<CheckOutput>,
    pub deny_sources: Option<CheckOutput>,
    pub dependencies: Option<CheckOutput>,
    pub fmt: Option<CheckOutput>,
    pub miri: Option<CheckOutput>,
    pub publish_dryrun: Option<CheckOutput>,
    pub tests: Option<CheckOutput>,
}

impl RunTypeOutput for CheckRunOutput {}

#[derive(Deserialize, Serialize, Debug, Eq, Hash, PartialEq, Clone)]
#[serde(rename_all = "kebab-case")]
pub enum CheckJobType {
    Check,
    Test,
    Miri,
}

impl Display for CheckJobType {
    fn fmt(&self, f: &mut Formatter) -> FmtResult {
        match self {
            Self::Check => write!(f, "check"),
            Self::Test => write!(f, "test"),
            Self::Miri => write!(f, "miri"),
        }
    }
}

impl JobType for CheckJobType {}

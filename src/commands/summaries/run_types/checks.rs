use std::{
    collections::HashMap,
    fmt::{Display, Formatter, Result as FmtResult},
};

use num::integer::lcm;
use serde::{Deserialize, Serialize};

use crate::commands::summaries::{template::SummaryTableCell, CheckOutput};

use super::{JobType, RunTypeOutput};

pub struct CheckedOutput {
    pub check_name: String,
    pub sub_checks: Vec<(String, CheckOutput)>,
    pub check_success: bool,
    pub url: Option<String>,
}

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

impl CheckRunOutput {
    fn as_vec(&self) -> Vec<Option<CheckOutput>> {
        vec![
            self.check,
            self.clippy,
            self.doc,
            self.custom,
            self.deny_advisories,
            self.deny_bans,
            self.deny_license,
            self.deny_sources,
            self.dependencies,
            self.fmt,
            self.miri,
            self.publish_dryrun,
            self.tests,
        ]
    }
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

impl JobType<CheckRunOutput> for CheckJobType {
    fn get_headers(
        runs: HashMap<Self, super::Job<Self, CheckRunOutput>>,
    ) -> anyhow::Result<Vec<SummaryTableCell>> {
        let mut lcm_result: usize = 1;
        for (_, checks) in runs.iter() {
            let num_check = checks.as_vec().iter().filter(|o| o.is_some()).count();
            lcm_result = lcm(lcm_result, num_check);
        }

        Ok(vec![
            SummaryTableCell::new_header("Category".to_string(), 1),
            SummaryTableCell::new_header("Checks".to_string(), lcm_result),
        ])
    }
}

use crate::PrettyPrintable;
use crate::utils::github::{InstallationRetrievalMode, generate_github_app_token};
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::fmt::{Display, Formatter};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(about = "Generate a github token for an app")]
pub struct Options {
    #[arg(long, env = "GITHUB_APP_ID")]
    github_app_id: u64,
    #[arg(long)]
    private_key_path: PathBuf,
    #[arg(long, default_value_t, value_enum)]
    installation_retrieval_mode: InstallationRetrievalMode,
    #[arg(long)]
    installation_retrieval_payload: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct TokenResult {
    token: String,
}

impl Display for TokenResult {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.token)
    }
}

impl PrettyPrintable for TokenResult {
    fn pretty_print(&self) -> String {
        self.token.clone()
    }
}

pub async fn github_app_token(
    options: Box<Options>,
    _working_directory: PathBuf,
) -> anyhow::Result<TokenResult> {
    Ok(TokenResult {
        token: generate_github_app_token(
            options.github_app_id,
            options.private_key_path,
            options.installation_retrieval_mode,
            options.installation_retrieval_payload,
        )
        .await?,
    })
}

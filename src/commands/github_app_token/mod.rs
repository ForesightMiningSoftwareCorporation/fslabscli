use crate::PrettyPrintable;
use clap::Parser;
use octocrab::models::{InstallationId, InstallationToken};
use octocrab::params::apps::CreateInstallationAccessToken;
use octocrab::Octocrab;
use serde::{Deserialize, Serialize};
use std::fmt::{Display, Formatter};
use std::fs::{self};
use std::path::PathBuf;
use url::Url;

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

#[derive(clap::ValueEnum, Clone, Default, Debug, Serialize)]
enum InstallationRetrievalMode {
    #[default]
    Id,
    Organization,
    Repository,
}

#[derive(Serialize, Deserialize)]
pub struct TokenResult {
    token: String,
}

#[derive(Serialize, Deserialize)]
pub struct CreateAccessToken {}

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
    let private_key = fs::read_to_string(options.private_key_path)?;
    let key = jsonwebtoken::EncodingKey::from_rsa_pem(private_key.as_bytes())?;

    // We have a github token we should try to update the pr
    let octocrab = Octocrab::builder()
        .app(options.github_app_id.into(), key)
        .build()?;
    let mut create_access_token = CreateInstallationAccessToken::default();
    let token_url: String = match options.installation_retrieval_payload {
        Some(payload) => match options.installation_retrieval_mode {
            InstallationRetrievalMode::Id => {
                let cand = InstallationId(payload.parse::<u64>()?);
                let installations = octocrab.apps().installations().send().await?;
                let mut url: Option<String> = None;
                for installation in installations {
                    if installation.id == cand {
                        url = installation.access_tokens_url;
                        break;
                    }
                }
                url
            }
            InstallationRetrievalMode::Organization => {
                let installation = octocrab.apps().get_org_installation(payload).await?;
                installation.access_tokens_url
            }
            InstallationRetrievalMode::Repository => {
                let (owner, repo) = payload.split_once(':').ok_or_else(|| {
                    anyhow::anyhow!("Repo is not in format owner/repo: {}", payload)
                })?;
                create_access_token.repositories.push(repo.to_string());
                let installation = octocrab
                    .apps()
                    .get_repository_installation(owner, repo)
                    .await?;
                installation.access_tokens_url
            }
        },
        None => {
            let installations = octocrab.apps().installations().send().await?;
            let installation = installations
                .items
                .first()
                .ok_or_else(|| anyhow::anyhow!("Could not find an installation for app"))?;
            installation.clone().access_tokens_url
        }
    }
    .ok_or_else(|| anyhow::anyhow!("Could not get url"))?;
    let url = Url::parse(&token_url)?;
    let access: InstallationToken = octocrab
        .post(url.path(), Some(&create_access_token))
        .await?;
    Ok(TokenResult {
        token: access.token,
    })
}

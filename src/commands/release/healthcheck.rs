//! Standing verification that the published release surface still resolves,
//! credential-less, exactly as a client would: fetch channels.json,
//! deserialize it against the contract types (stronger than schema
//! validation - these ARE the schema), follow every channel/target pointer
//! to its manifest, confirm the index lists the version, HEAD every artifact
//! and detached signature, and download + digest-verify the artifacts
//! pointers actually expose (pointed versions only; full-history sweeps
//! would move hundreds of megabytes per run).
//!
//! A missing channels.json is "nothing promoted yet", which is healthy. An
//! index gap is reported as repairable. Problems make the command exit
//! nonzero with the report; the workflow maintains the single tracking
//! issue.

use std::collections::BTreeSet;
use std::fmt::{Display, Formatter};

use anyhow::anyhow;
use clap::Parser;
use serde::Serialize;

use super::store::sha256_hex;
use super::types::{ArtifactSignature, Channels, Index, Manifest, SCHEMA_VERSION};
use crate::PrettyPrintable;

#[derive(Debug, Parser, Clone)]
#[command(about = "Verify the published release surface end to end")]
pub struct Options {
    /// Applications to check.
    #[arg(long, value_delimiter = ',', default_value = "spatial_engine")]
    pub apps: Vec<String>,
    #[arg(
        long,
        env = "RELEASE_PUBLIC_BASE_URL",
        default_value = "https://api.s3.fsl.dev"
    )]
    pub base_url: String,
    #[arg(long, default_value = "fsl-releases-channels")]
    pub channels_bucket: String,
    #[arg(long, default_value = "fsl-releases")]
    pub prod_bucket: String,
    /// HEAD artifacts but skip downloading them (fast mode).
    #[arg(long, default_value_t = false)]
    pub skip_digest_verification: bool,
}

#[derive(Debug, Serialize, Clone)]
pub struct HealthcheckResult {
    /// "app version: N artifact(s)" lines for everything checked.
    pub checked: Vec<String>,
    pub problems: Vec<String>,
}

impl Display for HealthcheckResult {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        for c in &self.checked {
            writeln!(f, "checked {c}")?;
        }
        for p in &self.problems {
            writeln!(f, "PROBLEM: {p}")?;
        }
        write!(
            f,
            "{}",
            if self.problems.is_empty() {
                "release surface healthy".to_string()
            } else {
                format!("{} problem(s) found", self.problems.len())
            }
        )
    }
}

impl PrettyPrintable for HealthcheckResult {
    fn pretty_print(&self) -> String {
        self.to_string()
    }
}

use super::http::{Client, client as https_client, head_present};

/// GET collapsing every failure to `None`: a non-2xx status, an unreachable
/// host, or an unreadable body are all the same answer a client would
/// experience.
async fn get_bytes(client: &Client, url: &str) -> Option<Vec<u8>> {
    super::http::get_bytes(client, url).await.ok()
}

async fn head_ok(client: &Client, url: &str) -> bool {
    head_present(client, url).await
}

pub async fn run(options: &Options) -> anyhow::Result<HealthcheckResult> {
    let client = https_client()?;
    let base = options.base_url.trim_end_matches('/');
    let mut checked = Vec::new();
    let mut problems = Vec::new();

    for app in &options.apps {
        let channels_url = format!("{base}/{}/{app}/channels.json", options.channels_bucket);
        let Some(channels_bytes) = get_bytes(&client, &channels_url).await else {
            // Healthy: nothing has been promoted for this app yet.
            checked.push(format!("{app}: no channels.json yet; nothing promoted"));
            continue;
        };
        let channels: Channels = match serde_json::from_slice(&channels_bytes) {
            Ok(channels) => channels,
            Err(e) => {
                problems.push(format!(
                    "{app} channels.json does not deserialize against the contract: {e}"
                ));
                continue;
            }
        };
        if channels.schema_version != SCHEMA_VERSION {
            problems.push(format!(
                "{app} channels.json has schema_version {}, expected {SCHEMA_VERSION}",
                channels.schema_version
            ));
        }

        let index_url = format!("{base}/{}/{app}/index.json", options.prod_bucket);
        let index: Option<Index> = match get_bytes(&client, &index_url).await {
            None => {
                problems.push(format!(
                    "{app}: channels exist but index.json is missing at {index_url}"
                ));
                None
            }
            Some(bytes) => match serde_json::from_slice::<Index>(&bytes) {
                Ok(index) => {
                    if index.schema_version != SCHEMA_VERSION {
                        problems.push(format!(
                            "{app} index.json has schema_version {}, expected {SCHEMA_VERSION}",
                            index.schema_version
                        ));
                    }
                    Some(index)
                }
                Err(e) => {
                    problems.push(format!(
                        "{app} index.json does not deserialize against the contract: {e}"
                    ));
                    None
                }
            },
        };

        // Every distinct pointed version, once.
        let versions: BTreeSet<&String> = channels
            .channels
            .latest
            .values()
            .chain(channels.channels.stable.values())
            .collect();
        if versions.is_empty() {
            checked.push(format!("{app}: channels.json exists but holds no pointers"));
        }

        for version in versions {
            let manifest_url = format!(
                "{base}/{}/{app}/{version}/manifest.json",
                options.prod_bucket
            );
            let Some(manifest_bytes) = get_bytes(&client, &manifest_url).await else {
                problems.push(format!(
                    "{app}: a channel points at {version} but {manifest_url} is missing"
                ));
                continue;
            };
            let manifest: Manifest = match serde_json::from_slice(&manifest_bytes) {
                Ok(manifest) => manifest,
                Err(e) => {
                    problems.push(format!(
                        "{app} {version} manifest does not deserialize against the contract: {e}"
                    ));
                    continue;
                }
            };
            if manifest.schema_version != SCHEMA_VERSION {
                problems.push(format!(
                    "{app} {version} manifest has schema_version {}, expected {SCHEMA_VERSION}",
                    manifest.schema_version
                ));
            }

            if let Some(index) = &index
                && !index.versions.iter().any(|e| e.version == *version)
            {
                problems.push(format!(
                    "{app}: pointed version {version} is absent from index.json (repairable: re-add the entry)"
                ));
            }

            for artifact in &manifest.artifacts {
                if !head_ok(&client, &artifact.url).await {
                    problems.push(format!(
                        "{app} {version}: artifact missing: {}",
                        artifact.url
                    ));
                    continue;
                }
                if !options.skip_digest_verification {
                    // Pointed versions are what clients download; verify the bytes.
                    let Some(bytes) = get_bytes(&client, &artifact.url).await else {
                        problems.push(format!("{app} {version}: cannot download {}", artifact.url));
                        continue;
                    };
                    let got = sha256_hex(&bytes);
                    if got != artifact.sha256 {
                        problems.push(format!(
                            "{app} {version}: digest mismatch for {}: manifest {}, object {got}",
                            artifact.filename, artifact.sha256
                        ));
                    }
                }
                if let ArtifactSignature::OpenpgpDetached { url: sig_url, .. } = &artifact.signature
                    && !head_ok(&client, sig_url).await
                {
                    problems.push(format!(
                        "{app} {version}: detached signature missing: {sig_url}"
                    ));
                }
            }
            checked.push(format!(
                "{app} {version}: {} artifact(s)",
                manifest.artifacts.len()
            ));
        }
    }

    let result = HealthcheckResult { checked, problems };
    if result.problems.is_empty() {
        Ok(result)
    } else {
        // Nonzero exit with the FULL report, so the workflow's tracking
        // issue carries every finding, never just the first.
        Err(anyhow!("{result}"))
    }
}

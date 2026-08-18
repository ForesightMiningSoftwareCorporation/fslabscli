//! Best-effort recorder: registers a published release and its artifacts in
//! fsl_sling BY REFERENCE, for the human-facing portal. The bucket is
//! authoritative for bytes, digests and channel pointers; fsl_sling is never
//! in a client's download path, and this command never touches fsl_sling's
//! channel routes (its alpha/beta/prod set-membership model cannot express a
//! per-target pointer and must not appear to).
//!
//! The workflow runs this under continue-on-error: a recorder outage must
//! not fail a publication that already succeeded.
//!
//! Flow: mint a client-credentials token, find or create the release record
//! by version, POST each manifest artifact to
//! `/releases/{id}/assets/by-reference` (filename, bucket key as file_path,
//! size, sha256), then PUT `{"is_draft": false}` so the portal lists it.
//! Every route nests under `/api`, which the pre-fslabscli implementation
//! missed; the path is normalised here.

use std::fmt::{Display, Formatter};
use std::path::PathBuf;

use anyhow::{Context, bail};
use base64::Engine as _;
use clap::Parser;
use hyper::Method;
use serde::Serialize;

use super::types::Manifest;
use crate::PrettyPrintable;

#[derive(Debug, Parser, Clone)]
#[command(about = "Record a published release in fsl_sling by reference (best effort)")]
pub struct Options {
    /// Path to the published manifest.json.
    #[arg(long)]
    pub manifest: PathBuf,
    /// OAuth2 issuer, e.g. https://auth.fslabs.ca
    #[arg(long, env = "FSL_RELEASES_ISSUER")]
    pub issuer: String,
    #[arg(long, env = "FSL_RELEASES_CLIENT_ID")]
    pub client_id: String,
    #[arg(long, env = "FSL_RELEASES_CLIENT_SECRET", hide_env_values = true)]
    pub client_secret: String,
    /// Service origin; /api is appended when absent.
    #[arg(long, env = "FSL_RELEASES_API_URL")]
    pub api_url: String,
    /// The fsl_sling app UUID this release belongs to.
    #[arg(long, env = "FSL_RELEASES_APP_ID")]
    pub app_id: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct RecordResult {
    pub release_id: String,
    pub recorded_assets: Vec<String>,
    pub draft_cleared: bool,
}

impl Display for RecordResult {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "recorded release {} with {} asset(s){}",
            self.release_id,
            self.recorded_assets.len(),
            if self.draft_cleared {
                ""
            } else {
                " (draft flag NOT cleared)"
            }
        )
    }
}

impl PrettyPrintable for RecordResult {
    fn pretty_print(&self) -> String {
        self.to_string()
    }
}

/// Normalise the service origin into the API base: strip trailing slashes
/// and append `/api` unless it is already there. fsl_sling nests every route
/// under `/api`, which the pre-fslabscli implementation missed.
fn normalise_api_url(origin: &str) -> String {
    let trimmed = origin.trim_end_matches('/');
    if trimmed.ends_with("/api") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/api")
    }
}

/// The bucket key fsl_sling records as `file_path`: the artifact URL with
/// the scheme and host stripped (the path after the host, no leading
/// slash). A string that is not an http(s) URL passes through unchanged.
fn file_path_from_url(url: &str) -> String {
    for scheme in ["https://", "http://"] {
        if let Some(rest) = url.strip_prefix(scheme) {
            return match rest.split_once('/') {
                Some((_host, path)) => path.to_string(),
                None => url.to_string(),
            };
        }
    }
    url.to_string()
}

use super::http::{client as https_client, request};

/// jq's `.id` for either a UUID string or a numeric id.
fn extract_id(body: &[u8]) -> Option<String> {
    match serde_json::from_slice::<serde_json::Value>(body)
        .ok()?
        .get("id")?
    {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

pub async fn run(options: &Options) -> anyhow::Result<RecordResult> {
    let manifest: Manifest = serde_json::from_slice(
        &std::fs::read(&options.manifest)
            .with_context(|| format!("no manifest at '{}'", options.manifest.display()))?,
    )
    .with_context(|| format!("unparseable manifest {}", options.manifest.display()))?;
    let version = &manifest.version;

    let api = normalise_api_url(&options.api_url);
    let client = https_client()?;

    // Mint a client-credentials token.
    let basic = base64::engine::general_purpose::STANDARD
        .encode(format!("{}:{}", options.client_id, options.client_secret));
    let (status, body) = request(
        &client,
        Method::POST,
        &format!("{}/oauth2/token", options.issuer.trim_end_matches('/')),
        &[
            ("Authorization", format!("Basic {basic}")),
            (
                "Content-Type",
                "application/x-www-form-urlencoded".to_string(),
            ),
        ],
        b"grant_type=client_credentials".to_vec(),
    )
    .await?;
    if !status.is_success() {
        bail!("could not mint a token (HTTP {status})");
    }
    let token = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| {
            v.get("access_token")
                .and_then(|t| t.as_str().map(str::to_string))
        })
        .filter(|t| !t.is_empty())
        .context("could not mint a token (no access_token in the response)")?;
    let auth = ("Authorization", format!("Bearer {token}"));

    // Find the release record by version, or create it.
    let existing = request(
        &client,
        Method::GET,
        &format!(
            "{api}/apps/{}/releases/by-version/{version}",
            options.app_id
        ),
        std::slice::from_ref(&auth),
        Vec::new(),
    )
    .await
    .ok()
    .filter(|(status, _)| status.is_success())
    .and_then(|(_, body)| extract_id(&body));

    let release_id = match existing {
        Some(id) => {
            tracing::info!("release record {id} already exists for {version}");
            id
        }
        None => {
            let (status, body) = request(
                &client,
                Method::POST,
                &format!("{api}/apps/{}/releases", options.app_id),
                &[
                    auth.clone(),
                    ("Content-Type", "application/json".to_string()),
                ],
                serde_json::to_vec(&serde_json::json!({
                    "version": version,
                    "release_notes": null,
                    "documentation_url": null,
                }))?,
            )
            .await?;
            if !status.is_success() {
                bail!("could not create the release record for {version} (HTTP {status})");
            }
            let id = extract_id(&body).context("no release id in the creation response")?;
            tracing::info!("created release record {id} for {version}");
            id
        }
    };

    // Record every artifact by reference; a single failure is logged and
    // skipped, because a partially-recorded portal entry beats none.
    let mut recorded_assets = Vec::new();
    for artifact in &manifest.artifacts {
        let body = serde_json::to_vec(&serde_json::json!({
            "filename": artifact.filename,
            "file_path": file_path_from_url(&artifact.url),
            "file_size": artifact.size_bytes,
            "checksum": artifact.sha256,
            "content_type": "application/octet-stream",
        }))?;
        let outcome = request(
            &client,
            Method::POST,
            &format!("{api}/releases/{release_id}/assets/by-reference"),
            &[
                auth.clone(),
                ("Content-Type", "application/json".to_string()),
            ],
            body,
        )
        .await;
        match outcome {
            Ok((status, _)) if status.is_success() => {
                recorded_assets.push(artifact.filename.clone());
            }
            Ok((status, _)) => tracing::warn!(
                "failed to record artifact {} (HTTP {status}), continuing",
                artifact.filename
            ),
            Err(e) => tracing::warn!(
                "failed to record artifact {} ({e:#}), continuing",
                artifact.filename
            ),
        }
    }

    // Visible in the portal; CreateReleaseRequest defaults is_draft = true
    // and a draft cannot be listed as released.
    let (status, _) = request(
        &client,
        Method::PUT,
        &format!("{api}/releases/{release_id}"),
        &[auth, ("Content-Type", "application/json".to_string())],
        serde_json::to_vec(&serde_json::json!({"is_draft": false}))?,
    )
    .await?;
    if !status.is_success() {
        bail!("could not clear the draft flag on release {release_id} (HTTP {status})");
    }

    Ok(RecordResult {
        release_id,
        recorded_assets,
        draft_cleared: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_url_gains_the_api_prefix_the_old_workflow_missed() {
        assert_eq!(
            normalise_api_url("https://releases.fslabs.ca"),
            "https://releases.fslabs.ca/api"
        );
        assert_eq!(
            normalise_api_url("https://releases.fslabs.ca/"),
            "https://releases.fslabs.ca/api"
        );
    }

    #[test]
    fn api_url_already_ending_in_api_is_untouched() {
        assert_eq!(
            normalise_api_url("https://releases.fslabs.ca/api"),
            "https://releases.fslabs.ca/api"
        );
        assert_eq!(
            normalise_api_url("https://releases.fslabs.ca/api/"),
            "https://releases.fslabs.ca/api"
        );
    }

    #[test]
    fn file_path_strips_scheme_and_host_only() {
        assert_eq!(
            file_path_from_url("https://api.s3.fsl.dev/fsl-releases/spatial_engine/1.0.0/a.deb"),
            "fsl-releases/spatial_engine/1.0.0/a.deb"
        );
        assert_eq!(file_path_from_url("http://host/x/y"), "x/y");
    }

    #[test]
    fn file_path_passes_non_urls_through_unchanged() {
        assert_eq!(
            file_path_from_url("fsl-releases/a.deb"),
            "fsl-releases/a.deb"
        );
        // No path after the host: nothing to strip.
        assert_eq!(file_path_from_url("https://host"), "https://host");
    }

    #[test]
    fn id_extraction_accepts_string_and_number() {
        assert_eq!(
            extract_id(br#"{"id": "abc-123"}"#),
            Some("abc-123".to_string())
        );
        assert_eq!(extract_id(br#"{"id": 42}"#), Some("42".to_string()));
        assert_eq!(extract_id(br#"{"id": null}"#), None);
        assert_eq!(extract_id(br#"{}"#), None);
        assert_eq!(extract_id(b"not json"), None);
    }
}

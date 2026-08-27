//! Standing verification that the published release surface still resolves,
//! credential-less, exactly as a client would: fetch channels.json,
//! deserialize it against the contract types (stronger than schema
//! validation - these ARE the schema), follow every channel/target pointer
//! to its manifest, confirm the index lists the version, and download +
//! digest-verify the artifacts pointers actually expose (pointed versions
//! only; full-history sweeps would move hundreds of megabytes per run).
//! Detached signatures are HEADed for presence; in `--skip-digest-verification`
//! mode artifacts are HEADed too instead of downloaded.
//!
//! A missing channels.json is "nothing promoted yet", which is healthy, but
//! ONLY on a definite 404: anything else means we could not find out, and
//! reporting that as healthy would let the workflow close its own tracking
//! issue during an outage. An index gap is reported as repairable. Problems
//! make the command exit nonzero with the report.

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

/// `Ok(None)` means the object genuinely is not there (404). An `Err` means we
/// could not find out: a transport failure, DNS, TLS, 5xx.
///
/// The distinction is the difference between "nothing promoted yet, healthy"
/// and "the store is down". Collapsing both to `None` made a full store outage
/// report every app as checked-and-healthy, and the workflow then CLOSED the
/// tracking issue with "Healthy again".
async fn get_bytes(client: &Client, url: &str) -> anyhow::Result<Option<Vec<u8>>> {
    match super::http::get_bytes(client, url).await {
        Ok(bytes) => Ok(Some(bytes)),
        // ONLY 404. 403 is deliberately not treated as absent: this check is
        // credential-less against an anonymously readable bucket, so a 403 means
        // the read policy is broken, not that the object is missing. Calling
        // that "nothing promoted yet" would report a whole store as healthy and
        // let the workflow close its own tracking issue. The two mistakes are
        // not equally cheap, so anything other than a definite 404 is a problem.
        Err(e) => match e.downcast_ref::<super::http::HttpStatus>() {
            Some(status) if status.status == hyper::StatusCode::NOT_FOUND => Ok(None),
            _ => Err(e),
        },
    }
}

pub async fn run(options: &Options) -> anyhow::Result<HealthcheckResult> {
    let client = https_client()?;
    let base = options.base_url.trim_end_matches('/');
    let mut checked = Vec::new();
    let mut problems = Vec::new();

    for app in &options.apps {
        let channels_url = format!("{base}/{}/{app}/channels.json", options.channels_bucket);
        let channels_bytes = match get_bytes(&client, &channels_url).await {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                // Healthy: nothing has been promoted for this app yet.
                checked.push(format!("{app}: no channels.json yet; nothing promoted"));
                continue;
            }
            Err(e) => {
                // Could not determine. Never report this as healthy.
                problems.push(format!("{app}: cannot reach {channels_url}: {e:#}"));
                continue;
            }
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
            Ok(None) => {
                problems.push(format!(
                    "{app}: channels exist but index.json is missing at {index_url}"
                ));
                None
            }
            Err(e) => {
                problems.push(format!("{app}: cannot reach {index_url}: {e:#}"));
                None
            }
            Ok(Some(bytes)) => match serde_json::from_slice::<Index>(&bytes) {
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
            let manifest_bytes = match get_bytes(&client, &manifest_url).await {
                Ok(Some(bytes)) => bytes,
                Ok(None) => {
                    problems.push(format!(
                        "{app}: a channel points at {version} but {manifest_url} is missing"
                    ));
                    continue;
                }
                Err(e) => {
                    problems.push(format!("{app}: cannot reach {manifest_url}: {e:#}"));
                    continue;
                }
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
                if options.skip_digest_verification {
                    // Fast mode: presence only.
                    if !head_present(&client, &artifact.url).await {
                        problems.push(format!(
                            "{app} {version}: artifact missing: {}",
                            artifact.url
                        ));
                        continue;
                    }
                } else {
                    // The GET proves presence by itself, so no HEAD first: it was
                    // a second round trip against the identical URL for every
                    // artifact of every pointed version.
                    let bytes = match get_bytes(&client, &artifact.url).await {
                        Ok(Some(bytes)) => bytes,
                        Ok(None) => {
                            problems.push(format!(
                                "{app} {version}: artifact missing: {}",
                                artifact.url
                            ));
                            continue;
                        }
                        Err(e) => {
                            // Distinct from missing, and the reason matters: this
                            // is the branch that tells an outage from a broken
                            // publication.
                            problems.push(format!(
                                "{app} {version}: cannot reach {}: {e:#}",
                                artifact.url
                            ));
                            continue;
                        }
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
                    && !head_present(&client, sig_url).await
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

#[cfg(test)]
mod tests {
    use super::super::http::HttpStatus;

    /// The classification `get_bytes` performs, exercised on the error type
    /// rather than through the network. A 404 is "absent, healthy"; everything
    /// else must stay an error, because reporting an outage as healthy lets the
    /// workflow close its own tracking issue while no client can resolve.
    fn classifies_as_absent(status: hyper::StatusCode) -> bool {
        let err: anyhow::Error = HttpStatus {
            url: "https://x/y.json".into(),
            status,
        }
        .into();
        matches!(
            err.downcast_ref::<HttpStatus>(),
            Some(s) if s.status == hyper::StatusCode::NOT_FOUND
        )
    }

    #[test]
    fn only_404_means_absent() {
        assert!(classifies_as_absent(hyper::StatusCode::NOT_FOUND));
        // 403 in particular: this check is credential-less against an
        // anonymously readable bucket, so a 403 is a broken read policy, not a
        // missing object. Treating it as absent reported a whole store outage
        // as "nothing promoted yet; healthy".
        for status in [
            hyper::StatusCode::FORBIDDEN,
            hyper::StatusCode::UNAUTHORIZED,
            hyper::StatusCode::INTERNAL_SERVER_ERROR,
            hyper::StatusCode::SERVICE_UNAVAILABLE,
            hyper::StatusCode::BAD_GATEWAY,
        ] {
            assert!(!classifies_as_absent(status), "{status} read as absent");
        }
    }

    #[test]
    fn a_transport_error_is_never_absent() {
        // No HttpStatus in the chain at all: DNS, TLS, connection reset.
        let err = anyhow::anyhow!("connection reset by peer");
        assert!(err.downcast_ref::<HttpStatus>().is_none());
    }
}

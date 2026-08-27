//! Probe whether the deployed object store supports S3 conditional writes.
//!
//! Conditional writes are the only mechanism that makes a published object
//! immutable: object lock plus versioning stops deletion, but a plain re-PUT
//! still succeeds and becomes the new current version. Run this once against
//! the real endpoint with the publisher credential before `release publish`
//! may be trusted, and paste the output into the PR that enables
//! publication. Any FAIL means publication must not be enabled until the
//! store is upgraded.

use std::fmt::{Display, Formatter};

use clap::Parser;
use serde::Serialize;

use super::store::{ReleaseStore, sha256_hex};
use crate::PrettyPrintable;

#[derive(Debug, Parser, Clone)]
#[command(about = "Probe the object store for conditional-write support (go/no-go gate)")]
pub struct Options {
    /// Bucket to probe (use the preview bucket, never production).
    #[arg(long)]
    pub bucket: String,
    /// Key prefix for the probe objects (they are left behind; the preview
    /// bucket's lifecycle expiry cleans them).
    #[arg(long, default_value = "_probe")]
    pub prefix: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct ProbeCase {
    pub description: String,
    pub passed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct ProbeStoreResult {
    pub bucket: String,
    pub cases: Vec<ProbeCase>,
    pub passed: bool,
}

impl Display for ProbeStoreResult {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "conditional-write probe against bucket {}", self.bucket)?;
        for case in &self.cases {
            writeln!(
                f,
                "{}: {}{}",
                if case.passed { "PASS" } else { "FAIL" },
                case.description,
                case.detail
                    .as_deref()
                    .map(|d| format!(" ({d})"))
                    .unwrap_or_default()
            )?;
        }
        write!(
            f,
            "{}",
            if self.passed {
                "conditional writes supported; publication may rely on immutability"
            } else {
                "CONDITIONAL WRITES UNSUPPORTED; do not enable publication against this store"
            }
        )
    }
}

impl PrettyPrintable for ProbeStoreResult {
    fn pretty_print(&self) -> String {
        self.to_string()
    }
}

pub async fn run(options: &Options) -> anyhow::Result<ProbeStoreResult> {
    let store = ReleaseStore::from_env(&options.bucket)?;
    // A per-invocation key: the probe must always exercise the fresh-key case.
    let nonce = sha256_hex(
        format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        )
        .as_bytes(),
    );
    let key = format!("{}/{}", options.prefix, &nonce[..16]);
    let body_one = b"probe-body-1".to_vec();
    let body_two = b"probe-body-2".to_vec();

    let mut cases = Vec::new();
    let mut case = |description: &str, passed: bool, detail: Option<String>| {
        cases.push(ProbeCase {
            description: description.to_string(),
            passed,
            detail,
        });
    };

    let first = store.put_immutable(&key, body_one.clone()).await;
    case(
        "If-None-Match:* on a fresh key succeeds",
        first.is_ok(),
        first.err().map(|e| format!("{e:#}")),
    );

    let second = store.put_immutable(&key, body_two.clone()).await;
    let refused = second
        .as_ref()
        .err()
        .and_then(|e| e.downcast_ref::<super::store::AlreadyExists>())
        .is_some();
    case(
        "If-None-Match:* on an existing key is refused",
        refused,
        (!refused).then(|| {
            second
                .err()
                .map(|e| format!("{e:#}"))
                .unwrap_or_else(|| "write succeeded".into())
        }),
    );

    // If-Match is exercised through the CAS path on a second, JSON-seeded
    // key: create via If-None-Match, then update via If-Match.
    let cas_key = format!("{key}-cas");
    store
        .put_immutable(
            &cas_key,
            serde_json::to_vec(&serde_json::json!({"probe": "seed"}))?,
        )
        .await?;
    let cas: anyhow::Result<serde_json::Value> = store
        .cas_update(&cas_key, None, |_current: serde_json::Value| {
            Ok(serde_json::json!({"probe": "updated"}))
        })
        .await;
    let cas_ok = cas.is_ok();
    case(
        "If-Match with the current ETag succeeds (CAS update)",
        cas_ok,
        cas.err().map(|e| format!("{e:#}")),
    );

    let readback = store.read(&key).await?;
    case(
        "refused overwrite left the original bytes",
        readback == body_one,
        None,
    );

    let passed = cases.iter().all(|c| c.passed);
    Ok(ProbeStoreResult {
        bucket: options.bucket.clone(),
        cases,
        passed,
    })
}

//! Move a channel pointer in channels.json. This command is the ONLY writer
//! of channel state, runs with the promotion credential (which cannot write
//! artifacts; the publication credential cannot write channels.json), and is
//! also the rollback procedure: a backward move with `--allow-backward` and
//! a `--reason`.
//!
//! Validation before any write, each with a distinct message: the manifest
//! must exist in the PRODUCTION bucket (a preview can never be promoted), it
//! must list an artifact for every selected target, and each selected
//! artifact is re-read and digest-compared, so promotion re-verifies rather
//! than trusting a record. The write is a compare-and-swap whose transform
//! AND backward gate re-run against the freshly-read document on every
//! retry: a promotion that loses a race can never apply a decision made
//! against a stale pointer.

use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};

use anyhow::{Context, bail};
use clap::Parser;
use serde::Serialize;

use super::store::{ReleaseStore, semver_gt};
use super::types::{
    Channel, ChannelPointers, Channels, Manifest, PROVENANCE_CAP, ProvenanceEntry, SCHEMA_VERSION,
};
use crate::PrettyPrintable;

#[derive(Debug, Parser, Clone)]
#[command(
    about = "Move a channel pointer in channels.json (the only writer)",
    disable_version_flag = true
)]
pub struct Options {
    #[arg(long)]
    pub app: String,
    /// Published PRODUCTION version to point at.
    #[arg(long)]
    pub version: String,
    #[arg(long, value_enum)]
    pub channel: Channel,
    /// Comma-separated target triples whose pointers move; unselected
    /// targets are untouched.
    #[arg(long, value_delimiter = ',')]
    pub targets: Vec<String>,
    /// Acknowledge moving a pointer to an OLDER version (rollback).
    #[arg(long, default_value_t = false)]
    pub allow_backward: bool,
    /// Required when --allow-backward is set.
    #[arg(long)]
    pub reason: Option<String>,
    #[arg(long, default_value = "")]
    pub moved_by: String,
    #[arg(long, default_value = "")]
    pub run_url: String,
    #[arg(long, default_value = "fsl-releases-channels")]
    pub channels_bucket: String,
    #[arg(long, default_value = "fsl-releases")]
    pub releases_bucket: String,
    #[arg(
        long,
        env = "RELEASE_PUBLIC_BASE_URL",
        default_value = "https://api.s3.fsl.dev"
    )]
    pub base_url: String,
    /// Skip the per-artifact digest re-verification (tests only).
    #[arg(long, default_value_t = false)]
    pub skip_digest_verification: bool,
}

#[derive(Debug, Serialize, Clone)]
pub struct PromoteResult {
    pub channel: Channel,
    pub version: String,
    pub targets: Vec<String>,
    pub backward: bool,
    /// The document as written.
    pub channels: Channels,
}

impl Display for PromoteResult {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "moved {} to {} for {}{}",
            self.channel,
            self.version,
            self.targets.join(", "),
            if self.backward { " (BACKWARD)" } else { "" }
        )
    }
}

impl PrettyPrintable for PromoteResult {
    fn pretty_print(&self) -> String {
        self.to_string()
    }
}

/// Apply one pointer move to a channels document: the backward gate and the
/// provenance append. Pure, so the gate is unit-tested and the CAS loop
/// re-runs it on every retry. Errors with a "backward move refused" message
/// when a selected target currently points at a newer version and
/// `allow_backward` is false.
pub fn apply_move(
    mut doc: Channels,
    options: &Options,
    now: &str,
) -> anyhow::Result<(Channels, bool)> {
    let pointers = doc.channels.get(options.channel);
    let mut backward_targets = Vec::new();
    for target in &options.targets {
        if let Some(current) = pointers.get(target)
            && semver_gt(current, &options.version)?
        {
            backward_targets.push((target.clone(), current.clone()));
        }
    }
    if !backward_targets.is_empty() && !options.allow_backward {
        let refusals: Vec<String> = backward_targets
            .iter()
            .map(|(target, current)| {
                format!(
                    "backward move refused: {}/{target} currently points at {current}, \
                     which is newer than {}; re-run with --allow-backward and a --reason \
                     if this is an intentional rollback",
                    options.channel, options.version
                )
            })
            .collect();
        bail!("{}", refusals.join("\n"));
    }
    let backward = !backward_targets.is_empty();

    let from: BTreeMap<String, Option<String>> = options
        .targets
        .iter()
        .map(|target| (target.clone(), pointers.get(target).cloned()))
        .collect();
    let pointers = doc.channels.get_mut(options.channel);
    for target in &options.targets {
        pointers.insert(target.clone(), options.version.clone());
    }
    doc.updated_at = now.to_string();
    doc.provenance.insert(
        0,
        ProvenanceEntry {
            channel: options.channel,
            targets: options.targets.clone(),
            from,
            to: options.version.clone(),
            backward,
            reason: options.reason.clone(),
            moved_at: now.to_string(),
            moved_by: options.moved_by.clone(),
            run: options.run_url.clone(),
        },
    );
    doc.provenance.truncate(PROVENANCE_CAP);
    Ok((doc, backward))
}

pub async fn run(options: &Options) -> anyhow::Result<PromoteResult> {
    // Validation precedes ALL network I/O.
    if options.allow_backward && options.reason.as_deref().is_none_or(str::is_empty) {
        bail!("--allow-backward requires --reason");
    }
    if options.targets.is_empty() {
        bail!("at least one target must be selected");
    }

    // The manifest must exist in the PRODUCTION bucket; previews never do.
    let releases = ReleaseStore::from_env(&options.releases_bucket)?;
    let manifest_key = format!("{}/{}/manifest.json", options.app, options.version);
    if !releases.exists(&manifest_key).await? {
        bail!(
            "no production manifest for {} {}: only published production versions can be promoted (previews never)",
            options.app,
            options.version
        );
    }
    let manifest: Manifest = serde_json::from_slice(&releases.read(&manifest_key).await?)
        .with_context(|| {
            format!(
                "s3://{}/{manifest_key} is not a valid manifest",
                options.releases_bucket
            )
        })?;
    for target in &options.targets {
        if !manifest.artifacts.iter().any(|a| a.target == *target) {
            bail!(
                "manifest for {} {} lists no artifact for target {target}",
                options.app,
                options.version
            );
        }
    }

    // Re-read and digest-compare every selected artifact: promotion
    // re-verifies rather than trusting a record.
    if !options.skip_digest_verification {
        for artifact in manifest
            .artifacts
            .iter()
            .filter(|a| options.targets.contains(&a.target))
        {
            let key = releases.key_from_url(&artifact.url)?;
            releases
                .verify_key(key, &artifact.sha256)
                .await
                .with_context(|| {
                    format!(
                        "artifact {key} no longer matches its recorded digest; refusing to promote"
                    )
                })?;
        }
    }

    let channels_store = ReleaseStore::from_env(&options.channels_bucket)?;
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let initial = Channels {
        schema_version: SCHEMA_VERSION,
        app: options.app.clone(),
        updated_at: now.clone(),
        manifest_base: format!("{}/{}", options.base_url, options.releases_bucket),
        channels: ChannelPointers::default(),
        provenance: Vec::new(),
    };
    let backward = std::cell::Cell::new(false);
    let doc = channels_store
        .cas_update(
            &format!("{}/channels.json", options.app),
            Some(initial),
            |doc: Channels| {
                // Re-applied to the FRESHLY READ document on every retry, so
                // the backward gate can never act on a stale pointer. Its
                // refusal propagates out of the CAS loop.
                let (next, moved_backward) = apply_move(doc, options, &now)?;
                backward.set(moved_backward);
                Ok(next)
            },
        )
        .await?;

    Ok(PromoteResult {
        channel: options.channel,
        version: options.version.clone(),
        targets: options.targets.clone(),
        backward: backward.get(),
        channels: doc,
    })
}

#[cfg(test)]
mod tests {
    use super::super::types::{TARGET_LINUX, TARGET_MACOS, TARGET_WINDOWS};
    use super::*;

    fn opts(version: &str, channel: Channel, targets: &[&str]) -> Options {
        Options {
            app: "app".into(),
            version: version.into(),
            channel,
            targets: targets.iter().map(|t| t.to_string()).collect(),
            allow_backward: false,
            reason: None,
            moved_by: "tester".into(),
            run_url: "http://run".into(),
            channels_bucket: "channels".into(),
            releases_bucket: "releases".into(),
            base_url: "https://api.s3.fsl.dev".into(),
            skip_digest_verification: true,
        }
    }

    fn doc() -> Channels {
        let mut latest = BTreeMap::new();
        latest.insert(TARGET_WINDOWS.to_string(), "1.1.0".to_string());
        latest.insert(TARGET_MACOS.to_string(), "1.0.0".to_string());
        Channels {
            schema_version: SCHEMA_VERSION,
            app: "app".into(),
            updated_at: "t0".into(),
            manifest_base: "https://api.s3.fsl.dev/releases".into(),
            channels: ChannelPointers {
                latest,
                stable: BTreeMap::new(),
            },
            provenance: Vec::new(),
        }
    }

    #[test]
    fn forward_move_touches_only_selected_targets() {
        let options = opts("1.2.0", Channel::Latest, &[TARGET_WINDOWS]);
        let (next, backward) = apply_move(doc(), &options, "t1").unwrap();
        assert!(!backward);
        assert_eq!(next.channels.latest.get(TARGET_WINDOWS).unwrap(), "1.2.0");
        assert_eq!(
            next.channels.latest.get(TARGET_MACOS).unwrap(),
            "1.0.0",
            "unselected target moved"
        );
        assert!(next.channels.stable.is_empty(), "other channel touched");
        assert_eq!(next.updated_at, "t1");
        let entry = &next.provenance[0];
        assert_eq!(entry.to, "1.2.0");
        assert!(!entry.backward);
        assert_eq!(entry.moved_at, "t1");
        assert_eq!(entry.moved_by, "tester");
        assert_eq!(entry.run, "http://run");
    }

    #[test]
    fn stable_move_leaves_latest_untouched() {
        let options = opts("1.0.0", Channel::Stable, &[TARGET_WINDOWS]);
        let (next, backward) = apply_move(doc(), &options, "t1").unwrap();
        assert!(!backward);
        assert_eq!(next.channels.stable.get(TARGET_WINDOWS).unwrap(), "1.0.0");
        assert_eq!(next.channels.latest.get(TARGET_WINDOWS).unwrap(), "1.1.0");
    }

    #[test]
    fn backward_move_is_refused_without_the_acknowledgement() {
        let options = opts("1.0.0", Channel::Latest, &[TARGET_WINDOWS]);
        let err = apply_move(doc(), &options, "t1").unwrap_err().to_string();
        assert!(err.contains("backward move refused"), "{err}");
        assert!(err.contains(&format!("latest/{TARGET_WINDOWS}")), "{err}");
        assert!(err.contains("1.1.0"), "{err}");
        assert!(err.contains("--allow-backward"), "{err}");
        assert!(err.contains("--reason"), "{err}");
    }

    #[test]
    fn one_backward_target_refuses_the_whole_move() {
        // WIN is at 1.1.0 (backward), MAC at 1.0.0 (forward): still refused.
        let options = opts("1.0.5", Channel::Latest, &[TARGET_WINDOWS, TARGET_MACOS]);
        let err = apply_move(doc(), &options, "t1").unwrap_err().to_string();
        assert!(err.contains("backward move refused"), "{err}");
    }

    #[test]
    fn acknowledged_backward_move_records_backward_and_reason() {
        let mut options = opts("1.0.0", Channel::Latest, &[TARGET_WINDOWS]);
        options.allow_backward = true;
        options.reason = Some("1.1.0 crashes on start".into());
        let (next, backward) = apply_move(doc(), &options, "t1").unwrap();
        assert!(backward);
        assert_eq!(next.channels.latest.get(TARGET_WINDOWS).unwrap(), "1.0.0");
        let entry = &next.provenance[0];
        assert!(entry.backward);
        assert_eq!(entry.reason.as_deref(), Some("1.1.0 crashes on start"));
    }

    #[test]
    fn same_version_is_not_a_backward_move() {
        let options = opts("1.1.0", Channel::Latest, &[TARGET_WINDOWS]);
        let (next, backward) = apply_move(doc(), &options, "t1").unwrap();
        assert!(!backward);
        assert_eq!(next.channels.latest.get(TARGET_WINDOWS).unwrap(), "1.1.0");
    }

    #[test]
    fn from_records_previous_pointers_including_absent_ones() {
        let options = opts("1.2.0", Channel::Latest, &[TARGET_WINDOWS, TARGET_LINUX]);
        let (next, _) = apply_move(doc(), &options, "t1").unwrap();
        let from = &next.provenance[0].from;
        assert_eq!(from.get(TARGET_WINDOWS).unwrap().as_deref(), Some("1.1.0"));
        assert_eq!(from.get(TARGET_LINUX).unwrap(), &None);
        assert_eq!(next.channels.latest.get(TARGET_LINUX).unwrap(), "1.2.0");
    }

    #[test]
    fn provenance_is_capped() {
        let mut current = doc();
        for i in 0..(PROVENANCE_CAP + 10) {
            let options = opts(
                &format!("1.1.{}", i + 1),
                Channel::Latest,
                &[TARGET_WINDOWS],
            );
            (current, _) = apply_move(current, &options, "t1").unwrap();
        }
        assert_eq!(current.provenance.len(), PROVENANCE_CAP);
        // Newest first.
        assert_eq!(
            current.provenance[0].to,
            format!("1.1.{}", PROVENANCE_CAP + 10)
        );
    }

    #[tokio::test]
    async fn allow_backward_without_reason_fails_before_any_io() {
        for reason in [None, Some(String::new())] {
            let mut options = opts("1.0.0", Channel::Latest, &[TARGET_WINDOWS]);
            options.allow_backward = true;
            options.reason = reason;
            let err = run(&options).await.unwrap_err().to_string();
            assert!(err.contains("--allow-backward requires --reason"), "{err}");
        }
    }

    #[tokio::test]
    async fn empty_target_selection_fails_before_any_io() {
        let options = opts("1.0.0", Channel::Latest, &[]);
        let err = run(&options).await.unwrap_err().to_string();
        assert!(err.contains("at least one target"), "{err}");
    }
}

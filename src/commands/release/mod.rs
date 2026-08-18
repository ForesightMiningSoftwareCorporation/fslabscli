//! Application-release pipeline: the logic behind fsl_libs'
//! `bundle_and_sign.yaml`, `promote_release.yaml` and
//! `release_healthcheck.yaml`, which are thin invocations of these
//! subcommands. The contract (manifest/index/channels shapes, the
//! library-vs-application tag boundary, storage semantics) lives HERE, in one
//! versioned place, and the workflows pin a released fslabscli binary.
//!
//! Storage rules in brief: production objects are immutable via conditional
//! writes (`probe-store` is the go/no-go gate against a deployed MinIO); the
//! manifest is written last and its existence is the atomic commit point;
//! monotonicity is derived from committed manifests, never index.json;
//! channel pointers move only through `promote`, whose backward-move gate
//! re-runs inside the compare-and-swap retry loop.

use std::fmt::{Display, Formatter};
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use serde::Serialize;

use crate::PrettyPrintable;

pub mod bundle_linux;
pub mod classify;
pub mod cleanup_drafts;
pub mod healthcheck;
pub(crate) mod http;
pub(crate) mod keyring;
pub mod probe_store;
pub mod promote;
pub mod publish;
pub mod record;
pub mod resolve;
pub mod sign_linux;
pub mod store;
pub mod types;
pub mod verify_production;

#[derive(Debug, Parser)]
#[command(about = "Application release pipeline: classify, publish, promote, resolve, verify")]
pub struct Options {
    #[command(subcommand)]
    command: ReleaseCommands,
}

#[derive(Debug, Subcommand)]
enum ReleaseCommands {
    /// Classify a published GitHub Release: library skip or validated app release
    Classify(Box<classify::Options>),
    /// Verify a release commit is eligible for production publication
    VerifyProduction(Box<verify_production::Options>),
    /// Probe the object store for conditional-write support (go/no-go gate)
    ProbeStore(Box<probe_store::Options>),
    /// Atomically publish signed artifacts: digests, manifest-last, index CAS
    Publish(Box<publish::Options>),
    /// Move a channel pointer in channels.json (the only writer)
    Promote(Box<promote::Options>),
    /// Resolve a download exactly as an installed client would
    Resolve(Box<resolve::Options>),
    /// Record a published release in fsl_sling by reference (best effort)
    Record(Box<record::Options>),
    /// Verify the published release surface end to end
    Healthcheck(Box<healthcheck::Options>),
    /// Delete asset-free draft releases left by the legacy tag trigger
    CleanupDrafts(Box<cleanup_drafts::Options>),
    /// Build the Linux .deb and AppImage in the pinned floor container
    BundleLinux(Box<bundle_linux::Options>),
    /// Detach-sign Linux artifacts with the org OpenPGP key
    SignLinux(Box<sign_linux::Options>),
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum ReleaseResult {
    Classify(classify::ClassifyResult),
    VerifyProduction(verify_production::VerifyProductionResult),
    ProbeStore(probe_store::ProbeStoreResult),
    Publish(publish::PublishResult),
    Promote(promote::PromoteResult),
    Resolve(resolve::ResolveResult),
    Record(record::RecordResult),
    Healthcheck(healthcheck::HealthcheckResult),
    CleanupDrafts(cleanup_drafts::CleanupDraftsResult),
    BundleLinux(bundle_linux::BundleLinuxResult),
    SignLinux(sign_linux::SignLinuxResult),
}

impl Display for ReleaseResult {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ReleaseResult::Classify(r) => r.fmt(f),
            ReleaseResult::VerifyProduction(r) => r.fmt(f),
            ReleaseResult::ProbeStore(r) => r.fmt(f),
            ReleaseResult::Publish(r) => r.fmt(f),
            ReleaseResult::Promote(r) => r.fmt(f),
            ReleaseResult::Resolve(r) => r.fmt(f),
            ReleaseResult::Record(r) => r.fmt(f),
            ReleaseResult::Healthcheck(r) => r.fmt(f),
            ReleaseResult::CleanupDrafts(r) => r.fmt(f),
            ReleaseResult::BundleLinux(r) => r.fmt(f),
            ReleaseResult::SignLinux(r) => r.fmt(f),
        }
    }
}

impl PrettyPrintable for ReleaseResult {
    fn pretty_print(&self) -> String {
        match self {
            ReleaseResult::Classify(r) => r.pretty_print(),
            ReleaseResult::VerifyProduction(r) => r.pretty_print(),
            ReleaseResult::ProbeStore(r) => r.pretty_print(),
            ReleaseResult::Publish(r) => r.pretty_print(),
            ReleaseResult::Promote(r) => r.pretty_print(),
            ReleaseResult::Resolve(r) => r.pretty_print(),
            ReleaseResult::Record(r) => r.pretty_print(),
            ReleaseResult::Healthcheck(r) => r.pretty_print(),
            ReleaseResult::CleanupDrafts(r) => r.pretty_print(),
            ReleaseResult::BundleLinux(r) => r.pretty_print(),
            ReleaseResult::SignLinux(r) => r.pretty_print(),
        }
    }
}

pub async fn release(
    options: Box<Options>,
    working_directory: PathBuf,
    repo_root: PathBuf,
) -> anyhow::Result<ReleaseResult> {
    match options.command {
        ReleaseCommands::Classify(o) => classify::run(&o, repo_root)
            .await
            .map(ReleaseResult::Classify),
        ReleaseCommands::VerifyProduction(o) => verify_production::run(&o, repo_root)
            .await
            .map(ReleaseResult::VerifyProduction),
        ReleaseCommands::ProbeStore(o) => probe_store::run(&o).await.map(ReleaseResult::ProbeStore),
        ReleaseCommands::Publish(o) => publish::run(&o).await.map(ReleaseResult::Publish),
        ReleaseCommands::Promote(o) => promote::run(&o).await.map(ReleaseResult::Promote),
        ReleaseCommands::Resolve(o) => resolve::run(&o).await.map(ReleaseResult::Resolve),
        ReleaseCommands::Record(o) => record::run(&o).await.map(ReleaseResult::Record),
        ReleaseCommands::Healthcheck(o) => {
            healthcheck::run(&o).await.map(ReleaseResult::Healthcheck)
        }
        ReleaseCommands::CleanupDrafts(o) => cleanup_drafts::run(&o)
            .await
            .map(ReleaseResult::CleanupDrafts),
        ReleaseCommands::BundleLinux(o) => bundle_linux::run(&o, working_directory)
            .await
            .map(ReleaseResult::BundleLinux),
        ReleaseCommands::SignLinux(o) => sign_linux::run(&o).await.map(ReleaseResult::SignLinux),
    }
}

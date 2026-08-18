//! Production-eligibility gate for an application release.
//!
//! The revision is resolved from the TAG ITSELF: the release payload's
//! `target_commitish` is a branch name for releases cut from an existing tag
//! (this tool's own publish path passes the literal string "main"), so it is
//! never trusted as a revision. The resolved commit must equal the revision
//! the workflow checked out, must be an ancestor of main (cherry-picked
//! hotfixes ship as pre-releases), must carry green check runs (production
//! ships verified main), and the human-typed version must equal the
//! workspace version in the tagged tree so a typo cannot burn a wrong number
//! into the immutable, monotonic production index.

use std::fmt::{Display, Formatter};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};
use clap::Parser;
use serde::Serialize;

use crate::PrettyPrintable;

#[derive(Debug, Parser, Clone)]
#[command(
    about = "Verify a release commit is eligible for production publication",
    disable_version_flag = true
)]
pub struct Options {
    /// The release tag to resolve.
    #[arg(long, env = "RELEASE_TAG")]
    pub tag: String,
    /// The revision the workflow checked out (github.sha).
    #[arg(long)]
    pub sha: String,
    /// The released version; verified against the workspace version file.
    #[arg(long)]
    pub version: Option<String>,
    /// Manifest whose `version` field binds the release version.
    #[arg(long, default_value = "fdk_apps/Cargo.toml")]
    pub workspace_version_file: PathBuf,
    /// Branch the commit must be an ancestor of.
    #[arg(long, default_value = "origin/main")]
    pub main_ref: String,
    /// Skip the check-run gate (only for environments with no API access).
    #[arg(long, default_value_t = false)]
    pub skip_check_runs: bool,
}

#[derive(Debug, Serialize, Clone)]
pub struct VerifyProductionResult {
    pub source_revision: String,
    pub checks_verified: bool,
}

impl Display for VerifyProductionResult {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "source_revision={} (checks {})",
            self.source_revision,
            if self.checks_verified {
                "verified"
            } else {
                "SKIPPED"
            }
        )
    }
}

impl PrettyPrintable for VerifyProductionResult {
    fn pretty_print(&self) -> String {
        self.to_string()
    }
}

fn git(repo_root: &Path, args: &[&str]) -> anyhow::Result<std::process::Output> {
    Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output()
        .context("git invocation failed")
}

pub async fn run(options: &Options, repo_root: PathBuf) -> anyhow::Result<VerifyProductionResult> {
    // Resolve the tag to a commit. Requires a full-history checkout with
    // tags (fetch-depth: 0).
    let out = git(
        &repo_root,
        &[
            "rev-list",
            "-n1",
            &format!("refs/tags/{}^{{commit}}", options.tag),
        ],
    )?;
    if !out.status.success() {
        bail!(
            "tag {} does not resolve to a commit in this checkout (fetch tags with fetch-depth: 0)",
            options.tag
        );
    }
    let tag_sha = String::from_utf8(out.stdout)?.trim().to_string();
    if tag_sha != options.sha {
        bail!(
            "tag {} points at {tag_sha} but this run checked out {}; refusing to publish a revision other than the tagged one",
            options.tag,
            options.sha
        );
    }

    let ancestor = git(
        &repo_root,
        &["merge-base", "--is-ancestor", &tag_sha, &options.main_ref],
    )?;
    if !ancestor.status.success() {
        bail!(
            "commit {tag_sha} is not an ancestor of {}; production releases must ship main. Cherry-picked builds ship as pre-releases.",
            options.main_ref
        );
    }

    if let Some(version) = &options.version {
        let manifest_path = repo_root.join(&options.workspace_version_file);
        let contents = std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("cannot read {}", manifest_path.display()))?;
        let value: toml::Value = toml::from_str(&contents)?;
        let workspace_version = value
            .get("workspace")
            .and_then(|w| w.get("package"))
            .and_then(|p| p.get("version"))
            .or_else(|| value.get("package").and_then(|p| p.get("version")))
            .and_then(|v| v.as_str())
            .with_context(|| format!("no workspace version in {}", manifest_path.display()))?;
        if version != workspace_version {
            bail!(
                "release version {version} does not match the {} workspace version {workspace_version}; bump the workspace version first",
                options.workspace_version_file.display()
            );
        }
    }

    // Built from verified main: the tagged commit's check runs must be green.
    let mut checks_verified = false;
    if !options.skip_check_runs {
        let (Ok(token), Ok(repository)) = (
            std::env::var("GH_TOKEN").or_else(|_| std::env::var("GITHUB_TOKEN")),
            std::env::var("GITHUB_REPOSITORY"),
        ) else {
            bail!(
                "GITHUB_TOKEN/GITHUB_REPOSITORY unavailable for the check-run gate; pass --skip-check-runs only where API access is impossible"
            );
        };
        let (owner, repo) = repository
            .split_once('/')
            .context("GITHUB_REPOSITORY is not owner/repo")?;
        let octocrab = octocrab::OctocrabBuilder::new()
            .personal_token(token)
            .build()?;
        let mut bad = Vec::new();
        let mut total = 0usize;
        let mut page = 1u32;
        loop {
            let response: serde_json::Value = octocrab
                .get(
                    format!("/repos/{owner}/{repo}/commits/{tag_sha}/check-runs?per_page=100&page={page}"),
                    None::<&()>,
                )
                .await
                .context("check-runs query failed")?;
            let runs = response["check_runs"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            if runs.is_empty() {
                break;
            }
            for run in &runs {
                total += 1;
                let conclusion = run["conclusion"].as_str().unwrap_or("");
                if matches!(
                    conclusion,
                    "failure" | "timed_out" | "cancelled" | "action_required"
                ) {
                    bad.push(format!(
                        "{} ({conclusion})",
                        run["name"].as_str().unwrap_or("?")
                    ));
                }
            }
            if runs.len() < 100 {
                break;
            }
            page += 1;
        }
        if !bad.is_empty() {
            bail!(
                "commit {tag_sha} has failed check run(s): {}; production releases ship verified main",
                bad.join(", ")
            );
        }
        if total == 0 {
            bail!(
                "commit {tag_sha} has no check runs at all; production releases ship verified main"
            );
        }
        checks_verified = true;
    }

    Ok(VerifyProductionResult {
        source_revision: tag_sha,
        checks_verified,
    })
}

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
    /// Check-run names that must have completed successfully on the tagged
    /// commit. Required unless --skip-check-runs: see the gate below for why a
    /// count of check runs cannot substitute for named ones.
    #[arg(long, value_delimiter = ',')]
    pub required_checks: Vec<String>,
}

/// Ordered worst-to-best so `max` lets a successful re-run supersede an earlier
/// failure of the same check name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum CheckState {
    Failed,
    Pending,
    Passed,
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

/// Clear a commit for production against NAMED check runs.
///
/// Only the named ones count. A count-based rule is satisfied by this command's
/// own check run, which is attached to the commit under test, and a commit on
/// main routinely carries failures and cancellations from unrelated workflows.
/// A required check whose `status` is not yet `completed` is not a pass:
/// `conclusion` is null until then. The same name can arrive more than once
/// (postsubmit and nightly both attach to main), so the best outcome per name
/// wins.
fn evaluate_check_runs(runs: &[serde_json::Value], required: &[String]) -> anyhow::Result<()> {
    // Trimmed and emptied-out first: the value arrives from a repo variable, and
    // `RELEASE_REQUIRED_CHECKS` written as "test, clippy" would otherwise look
    // for a check named " clippy" and report it missing with an error visually
    // identical to the name that did run. Normalising before the emptiness guard
    // also stops a value of "," from passing the gate vacuously.
    let required: Vec<&str> = required
        .iter()
        .map(|name| name.trim())
        .filter(|name| !name.is_empty())
        .collect();
    if required.is_empty() {
        bail!(
            "--required-checks must name at least one check run, or pass --skip-check-runs. \
             A bare count of check runs cannot gate anything: this command runs inside a job \
             whose own check run is attached to the same commit, so the count is never zero."
        );
    }
    // name -> best state seen.
    let mut seen: std::collections::BTreeMap<String, CheckState> =
        std::collections::BTreeMap::new();
    for run in runs {
        let name = run["name"].as_str().unwrap_or("?").to_string();
        let status = run["status"].as_str().unwrap_or("");
        let conclusion = run["conclusion"].as_str().unwrap_or("");
        let state = match (status, conclusion) {
            ("completed", "success") => CheckState::Passed,
            // `neutral` and `skipped` are not success. A required check that
            // skipped itself did not verify anything.
            ("completed", _) => CheckState::Failed,
            _ => CheckState::Pending,
        };
        seen.entry(name)
            .and_modify(|best| *best = (*best).max(state))
            .or_insert(state);
    }
    let mut unmet = Vec::new();
    for name in &required {
        match seen.get(*name) {
            Some(CheckState::Passed) => {}
            Some(CheckState::Pending) => unmet.push(format!("{name} (still running)")),
            Some(CheckState::Failed) => unmet.push(format!("{name} (did not succeed)")),
            None => unmet.push(format!("{name} (never ran on this commit)")),
        }
    }
    if !unmet.is_empty() {
        bail!(
            "does not satisfy the required check run(s): {}. \
             Production releases ship verified main. Check runs seen: {}",
            unmet.join(", "),
            if seen.is_empty() {
                "none".to_string()
            } else {
                seen.keys().cloned().collect::<Vec<_>>().join(", ")
            }
        );
    }
    Ok(())
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
        // Raw JSON rather than octocrab's typed
        // `checks().list_check_runs_for_git_ref()`: that builder exists, but
        // `models::checks::CheckRun` carries `conclusion` and no `status`, and
        // `status` is exactly what separates a still-running check from a
        // completed one. The endpoint also returns a {total_count, check_runs}
        // envelope rather than a Page<T>, so `all_pages` does not apply.
        let mut runs: Vec<serde_json::Value> = Vec::new();
        let mut page = 1u32;
        loop {
            let response: serde_json::Value = octocrab
                .get(
                    format!("/repos/{owner}/{repo}/commits/{tag_sha}/check-runs?per_page=100&page={page}"),
                    None::<&()>,
                )
                .await
                .context("check-runs query failed")?;
            let batch = response["check_runs"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let batch_len = batch.len();
            runs.extend(batch);
            if batch_len < 100 {
                break;
            }
            page += 1;
        }
        evaluate_check_runs(&runs, &options.required_checks)
            .with_context(|| format!("commit {tag_sha} is not eligible for production"))?;
        checks_verified = true;
    }

    Ok(VerifyProductionResult {
        source_revision: tag_sha,
        checks_verified,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(name: &str, status: &str, conclusion: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"name": name, "status": status, "conclusion": conclusion})
    }

    fn required(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn an_in_progress_required_check_is_not_a_pass() {
        // The regression this gate exists for: `conclusion` is null while a
        // check is queued or running, and the previous rule ("not in the bad
        // list") read that as green.
        for status in ["queued", "in_progress", "waiting", "pending"] {
            let runs = [run("bazel", status, serde_json::Value::Null)];
            let err = evaluate_check_runs(&runs, &required(&["bazel"]))
                .unwrap_err()
                .to_string();
            assert!(err.contains("still running"), "status {status}: {err}");
        }
    }

    #[test]
    fn the_gate_is_not_satisfied_by_the_job_it_runs_inside() {
        // Exactly the shape seen in CI: this workflow's own check run is
        // attached to the tagged commit and is in_progress, and the required
        // check never ran. A count-based rule passes here; this must not.
        let runs = [run("facts", "in_progress", serde_json::Value::Null)];
        let err = evaluate_check_runs(&runs, &required(&["bazel"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("never ran on this commit"), "{err}");
        assert!(err.contains("facts"), "should name what it did see: {err}");
    }

    #[test]
    fn completed_success_passes_and_unrelated_runs_are_ignored() {
        let runs = [
            run("bazel", "completed", "success".into()),
            run("facts", "in_progress", serde_json::Value::Null),
            run("some-optional-linter", "completed", "skipped".into()),
        ];
        assert!(evaluate_check_runs(&runs, &required(&["bazel"])).is_ok());
    }

    #[test]
    fn unrelated_failures_do_not_block_the_release() {
        // Measured shape of fsl_libs main at 91075455: 20 check runs, including
        // a failed docker_build and cancellations from concurrency. Gating on
        // any bad run anywhere would have refused every release.
        let runs = [
            run("test", "completed", "success".into()),
            run(
                "docker_build (prod, spatialdrive)",
                "completed",
                "failure".into(),
            ),
            run(
                "docker_build (dev, spatialdrive-dev)",
                "completed",
                "cancelled".into(),
            ),
            run("report", "completed", "skipped".into()),
            run("facts", "in_progress", serde_json::Value::Null),
        ];
        assert!(evaluate_check_runs(&runs, &required(&["test"])).is_ok());
    }

    #[test]
    fn a_required_check_that_did_not_succeed_blocks_the_release() {
        for conclusion in ["failure", "timed_out", "cancelled", "action_required"] {
            let runs = [run("test", "completed", conclusion.into())];
            let err = evaluate_check_runs(&runs, &required(&["test"]))
                .unwrap_err()
                .to_string();
            assert!(err.contains("did not succeed"), "{conclusion}: {err}");
        }
    }

    #[test]
    fn a_skipped_required_check_is_not_a_pass() {
        // A required check that skipped itself verified nothing.
        for conclusion in ["skipped", "neutral"] {
            let runs = [run("test", "completed", conclusion.into())];
            let err = evaluate_check_runs(&runs, &required(&["test"]))
                .unwrap_err()
                .to_string();
            assert!(err.contains("did not succeed"), "{conclusion}: {err}");
        }
    }

    #[test]
    fn a_successful_rerun_supersedes_an_earlier_failure() {
        // Both the postsubmit and the nightly publish a check named `test`
        // against main's SHA, so the same name arrives twice with different
        // outcomes. Ordering must not decide the verdict.
        for runs in [
            vec![
                run("test", "completed", "failure".into()),
                run("test", "completed", "success".into()),
            ],
            vec![
                run("test", "completed", "success".into()),
                run("test", "completed", "failure".into()),
            ],
        ] {
            assert!(
                evaluate_check_runs(&runs, &required(&["test"])).is_ok(),
                "a success for the required name must win regardless of order"
            );
        }
    }

    #[test]
    fn no_required_checks_is_refused_rather_than_vacuously_true() {
        let runs = [run("bazel", "completed", "success".into())];
        let err = evaluate_check_runs(&runs, &[]).unwrap_err().to_string();
        assert!(err.contains("--required-checks"), "{err}");
    }

    #[test]
    fn zero_check_runs_fails_every_required_name() {
        let err = evaluate_check_runs(&[], &required(&["bazel", "clippy"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("bazel"), "{err}");
        assert!(err.contains("clippy"), "{err}");
        assert!(err.contains("none"), "{err}");
    }
}

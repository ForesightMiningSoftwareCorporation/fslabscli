//! Post findings from a build we did not run ourselves as GitHub check-run
//! annotations.
//!
//! `fslabscli rust-tests` annotates its own output as it goes, because it
//! invokes each cargo tool and holds the results. Bazel runs are driven by
//! `bazel_test.sh`, so there is nothing for that path to hook into: this
//! command reads what the run left behind instead - the console log and the
//! JUnit XML Bazel writes per target - and posts the same check run from it.

use std::fmt::{Display, Formatter};
use std::path::PathBuf;

use clap::Parser;
use serde::Serialize;

use crate::PrettyPrintable;
use crate::commands::tests::annotations::{
    Annotation, CheckStyle, GhContext, GhTarget, PackageStat, parse_bazel_log, parse_junit_paths,
    post_annotations, resolve_token,
};

#[derive(Debug, Parser)]
#[command(about = "Post build findings as GitHub check-run annotations")]
pub struct Options {
    /// Console log to parse. Repeatable.
    #[arg(long)]
    log: Vec<PathBuf>,
    /// JUnit XML file, or a directory to scan for `*.xml`. Repeatable.
    #[arg(long)]
    junit: Vec<PathBuf>,
    /// Name of the check run. One per job: GitHub keys check runs by name, so
    /// two jobs sharing a name overwrite each other's findings.
    #[arg(long, default_value = "bazel-test-annotations")]
    check_name: String,
    /// Command shown under "Reproduce" on the check's Details page.
    #[arg(long, default_value = "bazel test //...")]
    reproduce: String,
    /// Prow job shown under "Rerun", as `/test <job>`.
    #[arg(long, default_value = "bazel-tests")]
    rerun_job: String,
    /// Tool label the findings are grouped under in the summary.
    #[arg(long, default_value = "bazel test")]
    tool: String,
    /// App ID of a GitHub App holding `checks: write`, used when the job has
    /// no `GITHUB_TOKEN`.
    #[arg(long, env = "FSLABSCLI_CHECKS_APP_ID")]
    checks_app_id: Option<u64>,
    /// Path to the private key of the app named by `--checks-app-id`.
    #[arg(long, env = "FSLABSCLI_CHECKS_APP_PRIVATE_KEY")]
    checks_app_private_key: Option<PathBuf>,
}

#[derive(Serialize)]
pub struct AnnotateResult {
    posted: usize,
    skipped_reason: Option<String>,
}

impl Display for AnnotateResult {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match &self.skipped_reason {
            Some(reason) => write!(f, "not posted: {reason}"),
            None => write!(f, "posted {} annotation(s)", self.posted),
        }
    }
}

impl PrettyPrintable for AnnotateResult {
    fn pretty_print(&self) -> String {
        self.to_string()
    }
}

pub async fn annotate(
    options: Box<Options>,
    working_directory: PathBuf,
) -> anyhow::Result<AnnotateResult> {
    // Leak once, at the edge: Annotation::tool is &'static str so the summary
    // can group by it without cloning per finding.
    let tool: &'static str = Box::leak(options.tool.clone().into_boxed_str());

    let mut annotations: Vec<Annotation> = Vec::new();
    let mut stats: Vec<PackageStat> = Vec::new();

    for log in &options.log {
        match std::fs::read_to_string(log) {
            Ok(text) => annotations.extend(parse_bazel_log(&text, &working_directory)),
            // A missing log is normal: bazel_test.sh only writes one when the
            // build produced output, and a green run may produce none.
            Err(e) => tracing::warn!("Could not read log {}: {e}", log.display()),
        }
    }

    if !options.junit.is_empty() {
        let (anns, st) = parse_junit_paths(&options.junit, &working_directory, tool);
        annotations.extend(anns);
        stats.extend(st);
    }

    // Same dedupe the in-process collector applies: one finding can be reported
    // both by Bazel's own ERROR line and by the compiler diagnostic under it.
    let mut seen = std::collections::HashSet::new();
    annotations.retain(|a| seen.insert((a.tool, a.path.clone(), a.start_line)));

    if annotations.is_empty() {
        tracing::info!("No findings to annotate");
        return Ok(AnnotateResult {
            posted: 0,
            skipped_reason: None,
        });
    }

    let target = match GhTarget::from_env() {
        Ok(t) => t,
        Err(reason) => {
            tracing::warn!("Not posting {} annotation(s): {reason}", annotations.len());
            return Ok(AnnotateResult {
                posted: 0,
                skipped_reason: Some(reason.to_string()),
            });
        }
    };

    let token = resolve_token(
        &target,
        options.checks_app_id,
        options.checks_app_private_key.as_deref(),
    )
    .await?;

    let style = CheckStyle {
        check_name: options.check_name.clone(),
        reproduce: options.reproduce.clone(),
        rerun_job: options.rerun_job.clone(),
        reproduce_hint: None,
    };

    let posted = annotations.len();
    post_annotations(&GhContext { target, token }, &style, annotations, &stats).await?;

    Ok(AnnotateResult {
        posted,
        skipped_reason: None,
    })
}

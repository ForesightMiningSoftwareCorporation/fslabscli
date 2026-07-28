//! Post cargo tool failures as GitHub check-run annotations so they render
//! inline in the PR diff and roll up on the check's Details page.
//!
//! Conclusion is always `neutral`: this reports locations, not verdicts.
//! Prow's own `cargo-tests` check is the pass/fail gate. Posting is a no-op
//! outside Prow (any of `REPO_OWNER`, `REPO_NAME` or `PULL_PULL_SHA` missing,
//! or `FSLABSCLI_ANNOTATIONS_DISABLE=1`).
//!
//! The credential is either `GITHUB_TOKEN` or a GitHub App key, from which we
//! mint an installation token scoped to the repository under test. CI uses the
//! App: the token is short-lived and cannot reach any other repo, which matters
//! because this runs in a pod that compiles unreviewed pull-request code.
//!
//! API: <https://docs.github.com/en/rest/checks/runs>

use anyhow::{Context, Result};
use junit_report::Report;
use octocrab::Octocrab;
use regex::Regex;
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use crate::script::CommandOutput;
use crate::utils::github::{InstallationRetrievalMode, generate_github_app_token};

// GitHub caps a single check-runs API call at 50 annotations.
const MAX_ANNOTATIONS_PER_CALL: usize = 50;
// GitHub caps `output.summary` at 65,535 chars; going over fails the whole
// POST with a 422 and drops every annotation. Leave a small margin so the
// truncation notice itself doesn't push us back over.
const MAX_SUMMARY_CHARS: usize = 65_000;
const CHECK_NAME: &str = "test-annotations";

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnotationLevel {
    #[allow(dead_code)] // Reserved for future parsers; the GH API accepts it.
    Notice,
    Warning,
    Failure,
}

#[derive(Debug, Clone, Serialize)]
pub struct Annotation {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub annotation_level: AnnotationLevel,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Canonical tool name (e.g. "cargo clippy") used to group findings in the
    /// check-run title and summary. Not part of the GitHub Check Runs API
    /// payload, so it's dropped during serialization.
    #[serde(skip)]
    pub tool: &'static str,
}

#[derive(Default, Clone)]
pub struct AnnotationCollector {
    inner: Arc<Mutex<Vec<Annotation>>>,
}

impl AnnotationCollector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_many<I: IntoIterator<Item = Annotation>>(&self, items: I) {
        // Recover the guard on poison rather than silently dropping the input;
        // losing failure annotations is worse than acting on a data structure
        // that another thread may have left in an unexpected state (the Vec
        // itself is fine, poison just means some other push panicked while
        // holding the guard).
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.extend(items);
    }

    pub fn drain(&self) -> Vec<Annotation> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        // Dedupe by (tool, path, line): the same finding can arrive from more
        // than one code path (batch step and the per-package fallback of the
        // same lock/fmt/clippy failure), and duplicates chew up the 50-per-call
        // API cap without adding information.
        let mut seen: std::collections::HashSet<(&'static str, String, u32)> =
            std::collections::HashSet::new();
        let mut out = Vec::with_capacity(g.len());
        for a in g.drain(..) {
            if seen.insert((a.tool, a.path.clone(), a.start_line)) {
                out.push(a);
            }
        }
        out
    }
}

pub fn parse_output_for(
    tool_id: &str,
    output: &CommandOutput,
    package_dir: &Path,
    repo_root: &Path,
) -> Vec<Annotation> {
    let combined = format!("{}\n{}", output.stdout, output.stderr);
    match tool_id {
        "cargo_fmt" => parse_cargo_fmt(&combined, repo_root),
        "cargo_check" | "cargo_clippy" | "cargo_doc" => {
            parse_cargo_diagnostics(&combined, tool_id, package_dir, repo_root)
        }
        "cargo_lock" => parse_cargo_lock(package_dir, repo_root),
        "cargo_test" => {
            // Prefer nextest JUnit if it exists: per-test attribution, correct
            // failure-vs-timeout distinction, and no dependence on the panic
            // format staying stable. Fall back to the stdout panic regex when
            // fslabscli ran plain `cargo test` (no nextest binary available)
            // or when JUnit wasn't produced for any other reason.
            let junit_path = package_dir.join("target/nextest/default/junit.xml");
            if let Ok(xml) = std::fs::read_to_string(&junit_path) {
                let anns = parse_nextest_junit(&xml, package_dir, repo_root);
                if !anns.is_empty() {
                    return anns;
                }
            }
            parse_cargo_test(&combined, package_dir, repo_root)
        }
        _ => Vec::new(),
    }
}

// Modern rustfmt emits `Diff in /path/to/file.rs:LINE:`; older versions used
// `Diff in /path/to/file.rs at line LINE:`. Match both so we don't silently
// stop producing fmt annotations when the toolchain changes.
static FMT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^Diff in (.+?)(?: at line |:)(\d+):").unwrap());

// Matches the `--> path:line:col` span header cargo prints under each primary
// diagnostic. Leading whitespace varies. `:::` (secondary references pointing
// at other code from the same diagnostic) is intentionally excluded, otherwise
// one error produces two annotations.
static DIAG_SPAN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*-->\s+(?P<path>[^\s:]+):(?P<line>\d+):(?P<col>\d+)").unwrap()
});

// Matches the `error[E0308]: message` / `warning: message` diagnostic header.
static DIAG_MSG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(error|warning)(?:\[[A-Z0-9_]+\])?:\s*(.*)$").unwrap());

// Extracts `path.rs:line[:col]` from `... panicked at ..., path:line[:col]`.
static PANIC_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"panicked at\s+.*?([\w/.\-]+\.rs):(\d+)(?::(\d+))?").unwrap());

fn parse_cargo_fmt(text: &str, repo_root: &Path) -> Vec<Annotation> {
    let mut out = Vec::new();
    for line in text.lines() {
        if let Some(cap) = FMT_RE.captures(line.trim_start()) {
            let path_str = cap.get(1).unwrap().as_str();
            let line_no: u32 = cap.get(2).unwrap().as_str().parse().unwrap_or(1);
            // cargo fmt always emits absolute paths.
            if let Some(rel) = make_path_relative(path_str, repo_root, repo_root) {
                out.push(Annotation {
                    path: rel,
                    start_line: line_no,
                    end_line: line_no,
                    annotation_level: AnnotationLevel::Failure,
                    message: "cargo fmt reports a diff here. Run `cargo fmt` to fix.".into(),
                    title: Some("Format diff".into()),
                    tool: "cargo fmt",
                });
            }
        }
    }
    out
}

fn parse_cargo_diagnostics(
    text: &str,
    tool_id: &str,
    base: &Path,
    repo_root: &Path,
) -> Vec<Annotation> {
    let mut out = Vec::new();
    let mut recent_msg: Option<(AnnotationLevel, String)> = None;
    for line in text.lines() {
        if let Some(cap) = DIAG_MSG_RE.captures(line) {
            let level = match cap.get(1).unwrap().as_str() {
                "error" => AnnotationLevel::Failure,
                _ => AnnotationLevel::Warning,
            };
            recent_msg = Some((level, cap.get(2).unwrap().as_str().trim().to_string()));
        } else if let Some(cap) = DIAG_SPAN_RE.captures(line) {
            let path_str = cap.name("path").unwrap().as_str();
            let start_line: u32 = cap.name("line").unwrap().as_str().parse().unwrap_or(1);
            if let Some(rel) = make_path_relative(path_str, base, repo_root) {
                let (level, msg) = recent_msg
                    .clone()
                    .unwrap_or((AnnotationLevel::Warning, tool_id.replace('_', " ")));
                let tool_name: &'static str = match tool_id {
                    "cargo_check" => "cargo check",
                    "cargo_clippy" => "cargo clippy",
                    "cargo_doc" => "cargo doc",
                    _ => "cargo diagnostic",
                };
                out.push(Annotation {
                    path: rel,
                    start_line,
                    end_line: start_line,
                    annotation_level: level,
                    message: msg,
                    title: Some(tool_name.to_string()),
                    tool: tool_name,
                });
                // Consume the message so we don't attach it to secondary `:::`
                // spans that reference other locations in the same diagnostic.
                recent_msg = None;
            }
        }
    }
    out
}

fn parse_cargo_lock(package_dir: &Path, repo_root: &Path) -> Vec<Annotation> {
    // Nested workspaces have their own Cargo.lock; annotate the one the caller
    // actually checked (identified by the workspace dir), not the repo root.
    let rel_lock = package_dir
        .strip_prefix(repo_root)
        .ok()
        .map(|rel| {
            if rel.as_os_str().is_empty() {
                "Cargo.lock".to_string()
            } else {
                rel.join("Cargo.lock").to_string_lossy().into_owned()
            }
        })
        .unwrap_or_else(|| "Cargo.lock".to_string());
    vec![Annotation {
        path: rel_lock,
        start_line: 1,
        end_line: 1,
        annotation_level: AnnotationLevel::Failure,
        message: "Cargo.lock is out of date. Run `cargo-fslabscli fix-lock-files`.".into(),
        title: Some("Stale lockfile".into()),
        tool: "cargo lock",
    }]
}

#[derive(Debug, serde::Deserialize)]
struct JUnitTestSuites {
    #[serde(rename = "testsuite", default)]
    testsuites: Vec<JUnitTestSuite>,
}

#[derive(Debug, serde::Deserialize)]
struct JUnitTestSuite {
    #[serde(rename = "testcase", default)]
    testcases: Vec<JUnitTestCase>,
}

#[derive(Debug, serde::Deserialize)]
struct JUnitTestCase {
    #[serde(rename = "@name", default)]
    name: String,
    #[serde(default)]
    failure: Option<JUnitFailure>,
}

#[derive(Debug, serde::Deserialize)]
struct JUnitFailure {
    #[serde(rename = "@message", default)]
    message: String,
    #[serde(rename = "$text", default)]
    text: String,
}

// Truncate annotation message bodies so they stay readable in the diff popover.
// The check-runs API accepts much larger strings but the UI collapses anything
// long into a "..." teaser that hides the useful part.
const MAX_MESSAGE_CHARS: usize = 400;

fn parse_nextest_junit(xml: &str, base: &Path, repo_root: &Path) -> Vec<Annotation> {
    let doc: JUnitTestSuites = match quick_xml::de::from_str(xml) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    // Dedup key includes the test name so two distinct tests bottoming out in
    // the same shared assertion helper both produce annotations (each with the
    // correct test-name title). Without the name in the key, cross-testcase
    // collapse defeats the per-test attribution that motivates using JUnit.
    let mut seen: std::collections::HashSet<(String, String, u32)> =
        std::collections::HashSet::new();
    for suite in doc.testsuites {
        for tc in suite.testcases {
            let Some(f) = tc.failure else { continue };
            // We still need the panic-message regex here: nextest's JUnit
            // doesn't have file/line as first-class fields, they live inside
            // the free-form panic text.
            let Some(cap) = PANIC_RE.captures(&f.text) else {
                continue;
            };
            let path_str = cap.get(1).unwrap().as_str();
            let line_no: u32 = cap.get(2).unwrap().as_str().parse().unwrap_or(1);
            let Some(rel) = make_path_relative(path_str, base, repo_root) else {
                continue;
            };
            if !seen.insert((tc.name.clone(), rel.clone(), line_no)) {
                continue;
            }
            let title = if tc.name.is_empty() {
                "Test failure".to_string()
            } else {
                format!("Test failure: {}", tc.name)
            };
            // Nextest embeds literal newlines inside the @message attribute
            // (thread header, then assertion left/right). Collapse them so the
            // one-line summary bullet keeps the useful assertion detail.
            let raw_message = if f.message.is_empty() {
                f.text.trim().to_string()
            } else {
                f.message.replace('\n', " ")
            };
            let message: String = raw_message.chars().take(MAX_MESSAGE_CHARS).collect();
            out.push(Annotation {
                path: rel,
                start_line: line_no,
                end_line: line_no,
                annotation_level: AnnotationLevel::Failure,
                message,
                title: Some(title),
                tool: "cargo test",
            });
        }
    }
    out
}

fn parse_cargo_test(text: &str, base: &Path, repo_root: &Path) -> Vec<Annotation> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for cap in PANIC_RE.captures_iter(text) {
        let path_str = cap.get(1).unwrap().as_str();
        let line_no: u32 = cap.get(2).unwrap().as_str().parse().unwrap_or(1);
        if let Some(rel) = make_path_relative(path_str, base, repo_root) {
            // De-dupe: rustc / nextest prints the same panic once from stdout
            // (in the failure summary) and once from stderr (as it happens).
            if seen.insert((rel.clone(), line_no)) {
                out.push(Annotation {
                    path: rel,
                    start_line: line_no,
                    end_line: line_no,
                    annotation_level: AnnotationLevel::Failure,
                    message: "Test panicked here.".into(),
                    title: Some("Test failure".into()),
                    tool: "cargo test",
                });
            }
        }
    }
    out
}

// Lexical normalisation only. `.canonicalize()` would require the file to exist
// on disk, which is true during a real CI run but breaks unit tests and any
// stale-target scenario where the compiler complained about a file we later
// removed.
fn make_path_relative(candidate: &str, base: &Path, repo_root: &Path) -> Option<String> {
    let raw = Path::new(candidate);
    let abs = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        base.join(raw)
    };
    let normalized = lexical_normalize(&abs);
    let root_normalized = lexical_normalize(repo_root);
    normalized
        .strip_prefix(&root_normalized)
        .ok()
        .map(|p| p.to_string_lossy().to_string())
}

fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Which commit in which repository the check run describes. Holds no
/// credential, so it can be resolved from the environment before we know where
/// the token is coming from.
#[derive(Debug)]
pub struct GhTarget {
    pub owner: String,
    pub repo: String,
    pub head_sha: String,
    /// PR number, used to construct diff-view anchors. `None` on non-presubmit
    /// runs (postsubmit, periodic); in that case the summary falls back to
    /// blob-view links.
    pub pull_number: Option<u64>,
}

/// Why there is nothing to post to. Carried as a value rather than a bare
/// `None` so the caller can log the specific thing that is missing; the
/// original code logged all five candidates at once, at `debug!`, and the
/// feature sat silently disabled in CI for twelve days as a result.
#[derive(Debug, PartialEq, Eq)]
pub enum NoTarget {
    Disabled,
    Missing(Vec<&'static str>),
}

impl std::fmt::Display for NoTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => write!(f, "FSLABSCLI_ANNOTATIONS_DISABLE is set"),
            Self::Missing(vars) => {
                write!(f, "{} unset (not running under Prow?)", vars.join(", "))
            }
        }
    }
}

impl GhTarget {
    pub fn from_env() -> Result<Self, NoTarget> {
        Self::from_env_with(|k| std::env::var(k).ok())
    }

    /// Env-agnostic constructor: the getter returns the raw string a variable
    /// is set to, or `None` when unset. Split out from `from_env` so unit tests
    /// can exercise the missing-var / empty-string / present-value shapes with
    /// a HashMap-backed getter instead of mutating the shared process env
    /// (mutation would race with any parallel test that reads REPO_OWNER etc).
    pub fn from_env_with(get: impl Fn(&str) -> Option<String>) -> Result<Self, NoTarget> {
        let get_nonempty = |k: &str| get(k).filter(|s| !s.is_empty());
        if get("FSLABSCLI_ANNOTATIONS_DISABLE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
        {
            return Err(NoTarget::Disabled);
        }
        let owner = get_nonempty("REPO_OWNER");
        let repo = get_nonempty("REPO_NAME");
        let head_sha = get_nonempty("PULL_PULL_SHA").or_else(|| get_nonempty("PULL_BASE_SHA"));

        let mut missing = Vec::new();
        if owner.is_none() {
            missing.push("REPO_OWNER");
        }
        if repo.is_none() {
            missing.push("REPO_NAME");
        }
        if head_sha.is_none() {
            missing.push("PULL_PULL_SHA");
        }

        match (owner, repo, head_sha) {
            (Some(owner), Some(repo), Some(head_sha)) => Ok(Self {
                owner,
                repo,
                head_sha,
                pull_number: get_nonempty("PULL_NUMBER").and_then(|v| v.parse().ok()),
            }),
            _ => Err(NoTarget::Missing(missing)),
        }
    }
}

pub struct GhContext {
    pub target: GhTarget,
    pub token: String,
}

/// Obtain a credential for the check-runs API.
///
/// Prefers `GITHUB_TOKEN` when the job already has one. Otherwise mints an
/// installation token from a GitHub App key, scoped with
/// [`InstallationRetrievalMode::Repository`] to the repository under test, so
/// the credential a test pod holds cannot write anywhere else in the org.
pub async fn resolve_token(
    target: &GhTarget,
    app_id: Option<u64>,
    app_private_key: Option<&Path>,
) -> Result<String> {
    if let Some(token) = std::env::var("GITHUB_TOKEN").ok().filter(|s| !s.is_empty()) {
        return Ok(token);
    }
    let (Some(app_id), Some(key_path)) = (app_id, app_private_key) else {
        anyhow::bail!(
            "no GITHUB_TOKEN, and no GitHub App to mint one from \
             (set FSLABSCLI_CHECKS_APP_ID and FSLABSCLI_CHECKS_APP_PRIVATE_KEY)"
        );
    };
    generate_github_app_token(
        app_id,
        key_path.to_path_buf(),
        InstallationRetrievalMode::Repository,
        Some(format!("{}/{}", target.owner, target.repo)),
    )
    .await
    .with_context(|| {
        format!(
            "mint a checks token for {}/{} from app {app_id} (key {})",
            target.owner,
            target.repo,
            key_path.display(),
        )
    })
}

/// Build a click-through URL for one annotation. Prefers the PR files-diff
/// view (`/pull/N/files#diff-<sha256(path)>R<line>`) because GitHub renders
/// check-run annotations inline on that surface, so the reviewer lands on
/// the failing line WITH the annotation banner still visible. Falls back to
/// the blob view when there's no PR number (postsubmit, periodic) - the
/// anchor is stable but no annotation banner is drawn.
fn annotation_url(ctx: &GhTarget, path: &str, line: u32) -> String {
    match ctx.pull_number {
        Some(pr) => {
            let mut hasher = Sha256::new();
            hasher.update(path.as_bytes());
            let hash = hex_encode(&hasher.finalize());
            format!(
                "https://github.com/{}/{}/pull/{}/files#diff-{}R{}",
                ctx.owner, ctx.repo, pr, hash, line,
            )
        }
        None => format!(
            "https://github.com/{}/{}/blob/{}/{}#L{}",
            ctx.owner, ctx.repo, ctx.head_sha, path, line,
        ),
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

#[derive(Debug, Clone)]
struct PackageStat {
    package: String,
    passed: usize,
    failed: usize,
    skipped: usize,
}

/// Aggregate per-package test counts from the JUnit report fslabscli builds
/// while running. Suite names are formatted as
/// `"<Mandatory|Optional> {workspace} - {package} - {version}"`, so we combine
/// the two kinds into one row per (workspace, package).
fn collect_package_stats(report: &Report) -> Vec<PackageStat> {
    use std::collections::BTreeMap;
    let mut acc: BTreeMap<String, PackageStat> = BTreeMap::new();
    for suite in report.testsuites() {
        let name = suite.name.as_str();
        let key = match name.split_once(' ').and_then(|(_kind, rest)| {
            let mut parts = rest.split(" - ");
            let ws = parts.next()?;
            let pkg = parts.next()?;
            Some(format!("{ws} · {pkg}"))
        }) {
            Some(k) => k,
            None => continue,
        };
        let stat = acc.entry(key.clone()).or_insert(PackageStat {
            package: key,
            passed: 0,
            failed: 0,
            skipped: 0,
        });
        for tc in &suite.testcases {
            if tc.is_failure() || tc.is_error() {
                stat.failed += 1;
            } else if tc.is_skipped() {
                stat.skipped += 1;
            } else if tc.is_success() {
                stat.passed += 1;
            }
        }
    }
    acc.into_values().collect()
}

/// Reconstruct the Prow spyglass URL for this build from the env vars Prow
/// injects into every job pod. Only the base domain, bucket, and storage
/// scheme need per-deployment configuration; the rest follow Prow's fixed
/// URL layout. Returns None outside Prow (any required var missing).
fn prow_log_url() -> Option<String> {
    prow_log_url_with(|k| std::env::var(k).ok())
}

/// Same testability split as `GhContext::from_env_with`: unit-testable without
/// mutating the shared process env.
fn prow_log_url_with(get: impl Fn(&str) -> Option<String>) -> Option<String> {
    let get_nonempty = |k: &str| get(k).filter(|s| !s.is_empty());
    let repo_owner = get_nonempty("REPO_OWNER")?;
    let repo_name = get_nonempty("REPO_NAME")?;
    let pr = get_nonempty("PULL_NUMBER")?;
    let job = get_nonempty("JOB_NAME")?;
    let build = get_nonempty("BUILD_ID")?;
    let base = get_nonempty("PROW_DECK_URL").unwrap_or_else(|| "https://prow.fslabs.ca".into());
    let bucket = get_nonempty("PROW_LOG_BUCKET").unwrap_or_else(|| "prow".into());
    let scheme = get_nonempty("PROW_LOG_STORAGE").unwrap_or_else(|| "s3".into());
    Some(format!(
        "{base}/view/{scheme}/{bucket}/pr-logs/pull/{repo_owner}_{repo_name}/{pr}/{job}/{build}/"
    ))
}

/// Render a markdown body for the check-run's Details page. Structure:
/// reproduce command, prow log link (when in Prow), per-package test summary,
/// findings grouped by tool, rerun instructions. Each `file:line` links into
/// GitHub's blob view at the PR head SHA so a click lands on the exact line.
fn build_summary(ctx: &GhTarget, annotations: &[Annotation], junit: &Report) -> String {
    use std::collections::BTreeMap;
    let failures = annotations
        .iter()
        .filter(|a| matches!(a.annotation_level, AnnotationLevel::Failure))
        .count();
    let warnings = annotations
        .iter()
        .filter(|a| matches!(a.annotation_level, AnnotationLevel::Warning))
        .count();

    let mut by_tool: BTreeMap<&str, Vec<&Annotation>> = BTreeMap::new();
    for a in annotations {
        by_tool.entry(a.tool).or_default().push(a);
    }

    let mut out = String::new();
    let total = annotations.len();
    out.push_str(&format!(
        "**{total} finding(s)** across {} tool(s): {failures} failure(s), {warnings} warning(s).\n\n",
        by_tool.len(),
    ));
    out.push_str("Details also render inline on the *Files changed* tab.\n\n");

    out.push_str("### Reproduce\n\n");
    out.push_str(
        "Run the full suite locally with:\n\n```\ncargo-fslabscli rust-tests\n```\n\n\
         Narrow to a single crate with `--whitelist <path>`.\n\n",
    );

    if let Some(url) = prow_log_url() {
        out.push_str(&format!("[View full Prow log]({url})\n\n"));
    }

    let stats = collect_package_stats(junit);
    if !stats.is_empty() {
        out.push_str("### Test summary\n\n");
        out.push_str("| Package | Passed | Failed | Skipped |\n");
        out.push_str("|---|---:|---:|---:|\n");
        for s in &stats {
            out.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                s.package, s.passed, s.failed, s.skipped
            ));
        }
        out.push('\n');
    }

    out.push_str("### Findings\n\n");
    for (tool, items) in by_tool {
        let mut items = items.clone();
        items.sort_by(|a, b| {
            a.path
                .cmp(&b.path)
                .then_with(|| a.start_line.cmp(&b.start_line))
        });
        out.push_str(&format!("#### {tool} ({})\n\n", items.len()));
        for a in items {
            let link = annotation_url(ctx, &a.path, a.start_line);
            let anchor = format!("[`{}:{}`]({link})", a.path, a.start_line);
            let msg_summary: String = a
                .message
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(160)
                .collect();
            if msg_summary.is_empty() {
                out.push_str(&format!("- {anchor}\n"));
            } else {
                out.push_str(&format!("- {anchor} - {msg_summary}\n"));
            }
        }
        out.push('\n');
    }

    out.push_str("### Rerun\n\n");
    out.push_str(
        "Comment `/test cargo-tests` on this PR to rerun the job, or \
         `/test cargo-tests-verbose` for extra debug logs.\n",
    );

    // GitHub caps output.summary at 65,535 chars; oversize bodies fail the
    // whole POST with 422 and drop every annotation. Truncate on a char
    // boundary and note that the tail is inline on Files changed anyway.
    if out.chars().count() > MAX_SUMMARY_CHARS {
        let kept: String = out.chars().take(MAX_SUMMARY_CHARS - 200).collect();
        out = kept;
        out.push_str(
            "\n\n_Summary truncated to fit GitHub's 65 KB cap. \
             Remaining findings render inline on the Files changed tab._\n",
        );
    }

    out
}

// Credentials that occasionally turn up in build output. The common one is the
// URL rewrite CI installs to clone private dependencies, which puts a token in
// the userinfo of every git URL a failing fetch might echo.
static URL_CREDENTIAL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"://([^/\s:@]+):[^@\s]+@").unwrap());
static GITHUB_TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"gh[pousr]_[A-Za-z0-9]{16,}|github_pat_[A-Za-z0-9_]{20,}").unwrap()
});

/// Strip well-known credential shapes out of text taken from build output.
///
/// Annotations are copied verbatim from compiler and build-tool output into a
/// check run, which anyone who can see the pull request can read - and some of
/// the repositories this runs on are public. Prow censors what it uploads to
/// its own log store, but nothing censors what we POST to GitHub.
///
/// This is a backstop for the shapes we can recognise, not a guarantee that no
/// secret can ever reach a check run.
fn redact(text: &str) -> String {
    let without_url_creds = URL_CREDENTIAL_RE.replace_all(text, "://$1:REDACTED@");
    GITHUB_TOKEN_RE
        .replace_all(&without_url_creds, "REDACTED")
        .into_owned()
}

pub async fn post_annotations(
    ctx: &GhContext,
    annotations: Vec<Annotation>,
    junit: &Report,
) -> Result<()> {
    if annotations.is_empty() {
        return Ok(());
    }

    // Redact once, here, so every caller and every parser is covered and the
    // summary (built from these messages) inherits it.
    let annotations: Vec<Annotation> = annotations
        .into_iter()
        .map(|a| Annotation {
            message: redact(&a.message),
            title: a.title.as_deref().map(redact),
            ..a
        })
        .collect();

    let octocrab = Octocrab::builder()
        .personal_token(ctx.token.clone())
        .build()
        .context("build octocrab client")?;

    let path = format!("/repos/{}/{}/check-runs", ctx.target.owner, ctx.target.repo);
    let title = "Test summary".to_string();
    let summary = build_summary(&ctx.target, &annotations, junit);

    let mut chunks = annotations.chunks(MAX_ANNOTATIONS_PER_CALL);
    let first = chunks.next().unwrap_or(&[]);

    // Neutral: annotations describe locations, not verdicts. Prow's cargo-tests
    // check already shows failure; a second red X here adds no information.
    let body = json!({
        "name": CHECK_NAME,
        "head_sha": ctx.target.head_sha,
        "status": "completed",
        "conclusion": "neutral",
        "output": {
            "title": title,
            "summary": summary,
            "annotations": first,
        }
    });

    #[derive(serde::Deserialize)]
    struct CreatedCheck {
        id: u64,
    }

    let created: CreatedCheck = octocrab
        .post(&path, Some(&body))
        .await
        .context("create check run")?;

    let update_path = format!(
        "/repos/{}/{}/check-runs/{}",
        ctx.target.owner, ctx.target.repo, created.id
    );
    for chunk in chunks {
        let body = json!({
            "output": {
                "title": title,
                "summary": summary,
                "annotations": chunk,
            }
        });
        let _: serde_json::Value = octocrab
            .patch(&update_path, Some(&body))
            .await
            .context("append annotations to check run")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        PathBuf::from("/repo")
    }
    fn pkg() -> PathBuf {
        PathBuf::from("/repo/crates/foo")
    }

    #[test]
    fn fmt_ignores_diff_outside_repo() {
        let text = "Diff in /elsewhere/src/lib.rs at line 1:\n";
        let a = parse_cargo_fmt(text, &root());
        assert!(a.is_empty());
    }

    #[test]
    fn clippy_attaches_message_only_to_primary_span() {
        let text = "\
error[E0308]: mismatched types
  --> src/a.rs:1:1
   |
   = note: expected `u32`
::: src/b.rs:2:2
   |
";
        let a = parse_cargo_diagnostics(text, "cargo_clippy", &pkg(), &root());
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].path, "crates/foo/src/a.rs");
    }

    #[test]
    fn test_panic_deduplicates_repeated_report() {
        let text = "\
thread 'main' panicked at 'boom', src/lib.rs:12:5
... stack trace ...
thread 'main' panicked at 'boom', src/lib.rs:12:5
";
        let a = parse_cargo_test(text, &pkg(), &root());
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].path, "crates/foo/src/lib.rs");
    }

    #[test]
    fn lock_annotates_root_lockfile_at_repo_root() {
        // When the workspace under check IS the repo root, the annotation goes
        // on the root Cargo.lock.
        let a = parse_cargo_lock(&root(), &root());
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].path, "Cargo.lock");
    }

    #[test]
    fn lock_annotates_nested_workspace_lockfile() {
        // Sub-workspace at crates/foo/ has its own Cargo.lock; that's the
        // stale one to point at, not the repo-root file.
        let a = parse_cargo_lock(&pkg(), &root());
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].path, "crates/foo/Cargo.lock");
    }

    fn fake_ctx() -> GhTarget {
        GhTarget {
            owner: "acme".into(),
            repo: "widget".into(),
            head_sha: "cafef00d".into(),
            pull_number: Some(322),
        }
    }

    fn fake_ctx_no_pr() -> GhTarget {
        GhTarget {
            pull_number: None,
            ..fake_ctx()
        }
    }

    fn empty_report() -> junit_report::Report {
        junit_report::ReportBuilder::new().build()
    }

    #[test]
    fn summary_groups_by_tool_alphabetically() {
        let ann = |path: &str, line: u32, tool: &'static str, msg: &str| Annotation {
            path: path.into(),
            start_line: line,
            end_line: line,
            annotation_level: AnnotationLevel::Failure,
            message: msg.into(),
            title: None,
            tool,
        };
        let anns = vec![
            ann("src/a.rs", 10, "cargo clippy", "unused import"),
            ann("src/main.rs", 5, "cargo fmt", "diff"),
            ann("src/a.rs", 3, "cargo clippy", "borrow of moved value"),
        ];
        let s = build_summary(&fake_ctx(), &anns, &empty_report());
        assert!(s.contains("**3 finding(s)** across 2 tool(s)"));
        // Alphabetical: cargo clippy (2) before cargo fmt (1).
        let clippy = s.find("### cargo clippy (2)").unwrap();
        let fmt = s.find("### cargo fmt (1)").unwrap();
        assert!(clippy < fmt);
        // Within a tool, sorted by file then line: a.rs:3 before a.rs:10.
        let a3 = s.find("`src/a.rs:3`").unwrap();
        let a10 = s.find("`src/a.rs:10`").unwrap();
        assert!(a3 < a10);
    }

    #[test]
    fn summary_links_to_pr_files_diff_when_pr_known() {
        // sha256("src/foo.rs") in hex.
        let expected_hash = "7fd7529f654a1ef078f532d1b7e0bb1879df6e959ed8c4e56b609894bc25b85c";
        let anns = vec![Annotation {
            path: "src/foo.rs".into(),
            start_line: 42,
            end_line: 42,
            annotation_level: AnnotationLevel::Failure,
            message: "borrow of moved value".into(),
            title: None,
            tool: "cargo clippy",
        }];
        let s = build_summary(&fake_ctx(), &anns, &empty_report());
        assert!(
            s.contains(&format!(
                "https://github.com/acme/widget/pull/322/files#diff-{expected_hash}R42"
            )),
            "expected PR diff anchor in summary, got:\n{s}"
        );
    }

    #[test]
    fn summary_falls_back_to_blob_view_without_pr_number() {
        let anns = vec![Annotation {
            path: "src/foo.rs".into(),
            start_line: 42,
            end_line: 42,
            annotation_level: AnnotationLevel::Failure,
            message: "err".into(),
            title: None,
            tool: "cargo clippy",
        }];
        let s = build_summary(&fake_ctx_no_pr(), &anns, &empty_report());
        assert!(s.contains("/blob/cafef00d/src/foo.rs#L42"));
    }

    #[test]
    fn summary_includes_reproduce_and_rerun_sections() {
        let anns = vec![Annotation {
            path: "src/foo.rs".into(),
            start_line: 1,
            end_line: 1,
            annotation_level: AnnotationLevel::Failure,
            message: "boom".into(),
            title: None,
            tool: "cargo test",
        }];
        let s = build_summary(&fake_ctx(), &anns, &empty_report());
        assert!(s.contains("### Reproduce"));
        assert!(s.contains("cargo-fslabscli rust-tests"));
        assert!(s.contains("### Rerun"));
        assert!(s.contains("/test cargo-tests"));
    }

    #[test]
    fn summary_renders_per_package_stats() {
        use junit_report::{Duration, ReportBuilder, TestCase, TestSuiteBuilder};
        let mut report = ReportBuilder::new().build();
        let dur = Duration::milliseconds(1);
        let mut foo = TestSuiteBuilder::new("Mandatory ws1 - foo - 0.1.0").build();
        foo.add_testcase(TestCase::success("t1", dur));
        foo.add_testcase(TestCase::success("t2", dur));
        foo.add_testcase(TestCase::failure("t3", dur, "test", "boom"));
        let mut bar = TestSuiteBuilder::new("Mandatory ws1 - bar - 0.1.0").build();
        bar.add_testcase(TestCase::success("t1", dur));
        bar.add_testcase(TestCase::skipped("t2"));
        report.add_testsuite(foo);
        report.add_testsuite(bar);

        let anns = vec![Annotation {
            path: "src/foo.rs".into(),
            start_line: 1,
            end_line: 1,
            annotation_level: AnnotationLevel::Failure,
            message: "boom".into(),
            title: None,
            tool: "cargo test",
        }];
        let s = build_summary(&fake_ctx(), &anns, &report);
        assert!(s.contains("### Test summary"));
        assert!(s.contains("| ws1 · foo | 2 | 1 | 0 |"));
        assert!(s.contains("| ws1 · bar | 1 | 0 | 1 |"));
    }

    #[test]
    fn summary_handles_empty_message() {
        let anns = vec![Annotation {
            path: "src/fmt.rs".into(),
            start_line: 1,
            end_line: 1,
            annotation_level: AnnotationLevel::Failure,
            message: String::new(),
            title: None,
            tool: "cargo fmt",
        }];
        let s = build_summary(&fake_ctx(), &anns, &empty_report());
        assert!(s.contains("`src/fmt.rs:1`"));
        // No dangling " - " after the anchor when the message is empty.
        assert!(!s.contains(".rs#L1) -"));
    }

    fn ann(path: &str, line: u32, tool: &'static str) -> Annotation {
        Annotation {
            path: path.into(),
            start_line: line,
            end_line: line,
            annotation_level: AnnotationLevel::Failure,
            message: "m".into(),
            title: None,
            tool,
        }
    }

    #[test]
    fn collector_drain_is_idempotent() {
        let c = AnnotationCollector::new();
        c.push_many([ann("a", 1, "test")]);
        assert_eq!(c.drain().len(), 1);
        assert_eq!(c.drain().len(), 0);
    }

    #[test]
    fn collector_drain_dedupes_by_tool_path_line() {
        // Same finding pushed from batch phase and per-package fallback must
        // collapse to one annotation (the check-run API caps at 50 per call).
        let c = AnnotationCollector::new();
        c.push_many([ann("Cargo.lock", 1, "cargo lock")]);
        c.push_many([ann("Cargo.lock", 1, "cargo lock")]);
        c.push_many([ann("Cargo.lock", 1, "cargo lock")]);
        assert_eq!(c.drain().len(), 1);
    }

    #[test]
    fn collector_drain_keeps_distinct_findings() {
        let c = AnnotationCollector::new();
        c.push_many([
            ann("src/foo.rs", 10, "cargo clippy"),
            ann("src/foo.rs", 10, "cargo fmt"),
            ann("src/foo.rs", 11, "cargo clippy"),
        ]);
        assert_eq!(c.drain().len(), 3);
    }

    #[test]
    fn collector_recovers_from_mutex_poison() {
        // Poison the mutex by panicking while holding the guard, then verify
        // push_many/drain still work rather than silently dropping data.
        let c = AnnotationCollector::new();
        let inner = c.inner.clone();
        let _ = std::thread::spawn(move || {
            let _guard = inner.lock().unwrap();
            panic!("intentional poison");
        })
        .join();
        assert!(c.inner.is_poisoned());
        c.push_many([ann("src/foo.rs", 1, "cargo clippy")]);
        let drained = c.drain();
        assert_eq!(drained.len(), 1);
    }

    /// Build a getter that returns the value for the exact keys given and
    /// `None` for everything else. Empty strings are passed through as-is so
    /// callers can verify their own empty-string handling.
    fn env_from(pairs: &[(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn redact_strips_credentials_that_reach_a_public_check_run() {
        // The URL rewrite CI installs to clone private deps is the realistic
        // path: a failing fetch echoes the remote, token and all.
        let msg = "failed to fetch https://x-access-token:ghs_AbCdEfGhIjKlMnOpQrSt@github.com/fslabs/x.git";
        let out = redact(msg);
        assert!(!out.contains("ghs_AbCdEfGhIjKlMnOpQrSt"), "{out}");
        assert!(
            out.contains("https://x-access-token:REDACTED@github.com/"),
            "{out}"
        );

        // Bare tokens, wherever they appear.
        assert_eq!(
            redact("token ghp_0123456789abcdefghij here"),
            "token REDACTED here"
        );
        assert!(!redact("github_pat_11ABCDEFG0123456789_abcdefgh").contains("11ABCDEFG"));

        // Ordinary diagnostics must survive untouched, including URLs with no
        // credentials and colons that are not userinfo.
        let plain = "error[E0308]: mismatched types at https://docs.rs/foo/1.0/foo.html";
        assert_eq!(redact(plain), plain);
    }

    #[test]
    fn from_env_names_every_missing_var() {
        // The reason has to name the specific variables: a caller that can only
        // say "something was missing" is what let this sit broken unnoticed.
        let err = GhTarget::from_env_with(env_from(&[])).unwrap_err();
        assert_eq!(
            err,
            NoTarget::Missing(vec!["REPO_OWNER", "REPO_NAME", "PULL_PULL_SHA"])
        );
        let err = GhTarget::from_env_with(env_from(&[
            ("REPO_OWNER", "acme"),
            ("PULL_PULL_SHA", "sha"),
        ]))
        .unwrap_err();
        assert_eq!(err, NoTarget::Missing(vec!["REPO_NAME"]));
    }

    #[test]
    fn from_env_does_not_require_a_token() {
        // The credential is resolved separately (env var or GitHub App), so a
        // target must build without GITHUB_TOKEN present.
        let t = GhTarget::from_env_with(env_from(&[
            ("REPO_OWNER", "acme"),
            ("REPO_NAME", "widget"),
            ("PULL_PULL_SHA", "sha"),
        ]))
        .unwrap();
        assert_eq!(t.owner, "acme");
    }

    #[test]
    fn from_env_rejects_empty_string_env_var() {
        // Regression: env::var("X") returns Ok("") for an empty-string value,
        // so a naive .ok()? would build a target with an empty owner and POST
        // to /repos//<empty>/check-runs (422). All required vars must treat ""
        // the same as unset.
        let base = &[
            ("REPO_OWNER", "acme"),
            ("REPO_NAME", "widget"),
            ("PULL_PULL_SHA", "sha"),
        ];
        assert!(GhTarget::from_env_with(env_from(base)).is_ok());
        for empty_key in ["REPO_OWNER", "REPO_NAME", "PULL_PULL_SHA"] {
            let mut pairs = base.to_vec();
            for (k, v) in pairs.iter_mut() {
                if k == &empty_key {
                    *v = "";
                }
            }
            assert!(
                GhTarget::from_env_with(env_from(&pairs)).is_err(),
                "expected an error when {empty_key} is empty"
            );
        }
    }

    #[test]
    fn from_env_falls_back_to_pull_base_sha() {
        let g = GhTarget::from_env_with(env_from(&[
            ("REPO_OWNER", "acme"),
            ("REPO_NAME", "widget"),
            ("PULL_BASE_SHA", "basesha"),
        ]))
        .unwrap();
        assert_eq!(g.head_sha, "basesha");
        assert!(g.pull_number.is_none());
    }

    #[test]
    fn from_env_disable_flag_short_circuits() {
        // Even when every other var is valid, the disable flag wins.
        let err = GhTarget::from_env_with(env_from(&[
            ("FSLABSCLI_ANNOTATIONS_DISABLE", "1"),
            ("REPO_OWNER", "acme"),
            ("REPO_NAME", "widget"),
            ("PULL_PULL_SHA", "sha"),
        ]))
        .unwrap_err();
        assert_eq!(err, NoTarget::Disabled);
    }

    #[test]
    fn prow_log_url_returns_none_on_empty_required_var() {
        // Same empty-vs-unset regression as from_env; verify each required
        // Prow-injected var short-circuits when set to the empty string.
        let base = &[
            ("REPO_OWNER", "acme"),
            ("REPO_NAME", "widget"),
            ("PULL_NUMBER", "322"),
            ("JOB_NAME", "cargo-tests"),
            ("BUILD_ID", "42"),
        ];
        let ok = prow_log_url_with(env_from(base)).unwrap();
        assert!(
            ok.contains("view/s3/prow/pr-logs/pull/acme_widget/322/cargo-tests/42/"),
            "{ok}"
        );
        for empty_key in [
            "REPO_OWNER",
            "REPO_NAME",
            "PULL_NUMBER",
            "JOB_NAME",
            "BUILD_ID",
        ] {
            let mut pairs = base.to_vec();
            for (k, v) in pairs.iter_mut() {
                if k == &empty_key {
                    *v = "";
                }
            }
            assert!(
                prow_log_url_with(env_from(&pairs)).is_none(),
                "expected None when {empty_key} is empty"
            );
        }
    }

    #[test]
    fn prow_log_url_optional_overrides_fall_back_when_empty() {
        // Empty overrides for the base/bucket/scheme must not produce a URL
        // with `//` or a missing path segment; they should fall through to
        // the built-in defaults.
        let url = prow_log_url_with(env_from(&[
            ("REPO_OWNER", "acme"),
            ("REPO_NAME", "widget"),
            ("PULL_NUMBER", "322"),
            ("JOB_NAME", "job"),
            ("BUILD_ID", "42"),
            ("PROW_LOG_STORAGE", ""),
            ("PROW_LOG_BUCKET", ""),
            ("PROW_DECK_URL", ""),
        ]))
        .unwrap();
        assert!(!url.contains("//pr-logs"), "{url}");
        assert!(!url.contains("view//"), "{url}");
        assert!(
            url.starts_with("https://prow.fslabs.ca/view/s3/prow/"),
            "{url}"
        );
    }

    #[test]
    fn nextest_junit_keeps_two_tests_that_share_a_helper_line() {
        // Two failing tests both bottoming out in a shared assert helper at
        // src/testutil.rs:15 must produce TWO annotations (with the two test
        // names), not collapse to one.
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites>
  <testsuite name="foo">
    <testcase name="mod_a::test_x" classname="foo">
      <failure message="head_a">thread 'a' panicked at src/testutil.rs:15:5:
assertion failed</failure>
    </testcase>
    <testcase name="mod_b::test_y" classname="foo">
      <failure message="head_b">thread 'b' panicked at src/testutil.rs:15:5:
assertion failed</failure>
    </testcase>
  </testsuite>
</testsuites>
"#;
        let a = parse_nextest_junit(xml, &pkg(), &root());
        assert_eq!(a.len(), 2);
        assert!(
            a.iter()
                .any(|x| x.title.as_deref() == Some("Test failure: mod_a::test_x"))
        );
        assert!(
            a.iter()
                .any(|x| x.title.as_deref() == Some("Test failure: mod_b::test_y"))
        );
    }

    #[test]
    fn nextest_junit_collapses_multiline_message_to_one_line() {
        // Nextest embeds literal LFs inside @message. The rendered message
        // must not contain them (else summary bullet drops the assertion
        // detail via `lines().next()`).
        let xml = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<testsuites><testsuite name=\"probe\">\
<testcase name=\"boom\" classname=\"probe\">\
<failure message=\"thread 'boom' panicked at src/lib.rs:12:5\n\
assertion left == right failed\n  left: 1\n  right: 2\">\
thread 'boom' panicked at src/lib.rs:12:5:\nassertion left == right failed\
</failure></testcase></testsuite></testsuites>";
        let a = parse_nextest_junit(xml, &pkg(), &root());
        assert_eq!(a.len(), 1);
        assert!(!a[0].message.contains('\n'), "message: {:?}", a[0].message);
        assert!(
            a[0].message.contains("left: 1"),
            "message: {:?}",
            a[0].message
        );
    }

    #[test]
    fn summary_truncates_when_it_would_exceed_github_cap() {
        // Manufacture enough annotations to blow past MAX_SUMMARY_CHARS and
        // verify the output ends with the truncation notice and stays under
        // the cap.
        let anns: Vec<Annotation> = (0..500)
            .map(|i| Annotation {
                path: format!("src/very_long_path_that_takes_bytes_{i}.rs"),
                start_line: i,
                end_line: i,
                annotation_level: AnnotationLevel::Failure,
                message: "borrow of moved value: x ".repeat(6),
                title: None,
                tool: "cargo clippy",
            })
            .collect();
        let s = build_summary(&fake_ctx(), &anns, &empty_report());
        assert!(
            s.chars().count() <= MAX_SUMMARY_CHARS,
            "len={}",
            s.chars().count()
        );
        assert!(s.contains("Summary truncated"), "{s}");
    }
}

// Integration tests that spawn the real cargo/clippy/rustfmt currently on
// PATH. They exist so a stdout format change in a future toolchain version
// breaks the build here rather than silently producing empty annotations
// in CI. Fixture crates are tiny enough that the whole suite finishes
// well under a second.
#[cfg(test)]
mod real_cargo {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn write_crate(dir: &std::path::Path, name: &str, lib_rs: &str) {
        std::fs::write(
            dir.join("Cargo.toml"),
            format!(
                "[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n"
            ),
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), lib_rs).unwrap();
    }

    fn run(dir: &std::path::Path, args: &[&str]) -> CommandOutput {
        let out = Command::new("cargo")
            .args(args)
            .current_dir(dir)
            .env("CARGO_TERM_COLOR", "never")
            // Isolate target dir so parallel tests don't fight over a lock file.
            .env("CARGO_TARGET_DIR", dir.join("target"))
            .output()
            .expect("failed to spawn cargo");
        CommandOutput {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            success: out.status.success(),
        }
    }

    // Cargo resolves cwd through symlinks (so on macOS a /var/folders/... temp
    // dir shows up in output as /private/var/folders/...). Our parser uses a
    // lexical strip_prefix, so tests must feed it the canonical path or the
    // path won't be recognised as inside the "repo root".
    fn canon(dir: &std::path::Path) -> std::path::PathBuf {
        dir.canonicalize().unwrap()
    }

    #[test]
    fn cargo_fmt_check() {
        let tmp = TempDir::new().unwrap();
        // Deliberately mis-indented; rustfmt will produce a diff at line 2.
        write_crate(
            tmp.path(),
            "fmt_probe",
            "fn foo() {\n let x=1;\n let _=x;\n}\n",
        );
        let out = run(tmp.path(), &["fmt", "--", "--check"]);
        assert!(
            !out.success,
            "cargo fmt --check unexpectedly succeeded: {}",
            out.stdout
        );
        let anns = parse_output_for("cargo_fmt", &out, &canon(tmp.path()), &canon(tmp.path()));
        assert!(
            !anns.is_empty(),
            "no annotations parsed from: {}",
            out.stdout
        );
        assert!(anns.iter().all(|a| a.path == "src/lib.rs"));
        assert!(matches!(anns[0].annotation_level, AnnotationLevel::Failure));
    }

    #[test]
    fn cargo_clippy_error() {
        let tmp = TempDir::new().unwrap();
        // `x.clone()` on a Copy type reliably triggers `clippy::clone_on_copy`.
        // `-D warnings` promotes it to an error and forces a `--> src/lib.rs:LINE:COL` header.
        write_crate(
            tmp.path(),
            "clippy_probe",
            "pub fn f() {\n    let x: u32 = 1;\n    let _ = x.clone();\n}\n",
        );
        let out = run(
            tmp.path(),
            &["clippy", "--all-targets", "--", "-D", "warnings"],
        );
        assert!(
            !out.success,
            "cargo clippy unexpectedly succeeded: {} {}",
            out.stdout, out.stderr
        );
        let anns = parse_output_for("cargo_clippy", &out, &canon(tmp.path()), &canon(tmp.path()));
        assert!(
            !anns.is_empty(),
            "no annotations parsed from clippy output: {}",
            out.stderr
        );
        assert!(anns.iter().any(|a| a.path == "src/lib.rs"));
    }

    #[test]
    fn cargo_check_type_error() {
        let tmp = TempDir::new().unwrap();
        // Type mismatch at src/lib.rs:2 forces a rustc `error[E0308]` with
        // a `-->` span header.
        write_crate(
            tmp.path(),
            "check_probe",
            "pub fn f() {\n    let _: u32 = \"hi\";\n}\n",
        );
        let out = run(tmp.path(), &["check", "--all-targets"]);
        assert!(!out.success);
        let anns = parse_output_for("cargo_check", &out, &canon(tmp.path()), &canon(tmp.path()));
        assert!(
            !anns.is_empty(),
            "no annotations from cargo check: {}",
            out.stderr
        );
        assert!(
            anns.iter()
                .any(|a| a.path == "src/lib.rs" && a.start_line == 2)
        );
        assert!(
            anns.iter()
                .any(|a| matches!(a.annotation_level, AnnotationLevel::Failure))
        );
    }

    #[test]
    fn cargo_test_panic_stdout_fallback() {
        // Exercises the non-nextest fallback: parse panic location from raw
        // `cargo test` stdout. This path is used when nextest isn't installed
        // in the runner environment.
        let tmp = TempDir::new().unwrap();
        write_crate(
            tmp.path(),
            "panic_probe",
            "#[test]\nfn boom() {\n    assert_eq!(1, 2);\n}\n",
        );
        let out = run(tmp.path(), &["test", "--", "--nocapture"]);
        assert!(!out.success);
        let anns = parse_output_for("cargo_test", &out, &canon(tmp.path()), &canon(tmp.path()));
        assert!(
            !anns.is_empty(),
            "no annotations from cargo test panic: {} {}",
            out.stdout,
            out.stderr
        );
        assert!(anns.iter().any(|a| a.path == "src/lib.rs"));
    }

    #[test]
    fn cargo_test_panic_via_nextest_junit() {
        // Exercises the JUnit path: nextest writes junit.xml, we read + parse
        // it. This catches breaking changes to nextest's JUnit schema in
        // addition to the underlying panic format.
        if !nextest_available() {
            eprintln!("skipping: cargo-nextest not installed");
            return;
        }
        let tmp = TempDir::new().unwrap();
        write_crate(
            tmp.path(),
            "nextest_probe",
            "#[test]\nfn boom() {\n    assert_eq!(1, 2);\n}\n",
        );
        // Ask nextest to emit a JUnit report at the standard path.
        std::fs::create_dir_all(tmp.path().join(".config")).unwrap();
        std::fs::write(
            tmp.path().join(".config/nextest.toml"),
            "[profile.default.junit]\npath = \"junit.xml\"\n",
        )
        .unwrap();
        let out = run(tmp.path(), &["nextest", "run", "--no-fail-fast"]);
        assert!(
            !out.success,
            "nextest unexpectedly succeeded: {} {}",
            out.stdout, out.stderr
        );
        let junit = tmp.path().join("target/nextest/default/junit.xml");
        assert!(
            junit.exists(),
            "nextest did not produce junit at {:?}",
            junit
        );
        let anns = parse_output_for("cargo_test", &out, &canon(tmp.path()), &canon(tmp.path()));
        assert!(!anns.is_empty(), "no annotations from nextest junit");
        assert!(anns.iter().any(|a| a.path == "src/lib.rs"));
        // JUnit path should win over the stdout regex fallback, so titles
        // must include the test name.
        assert!(
            anns.iter()
                .any(|a| a.title.as_deref().unwrap_or("").contains("boom")),
            "annotation titles did not include test name: {:?}",
            anns.iter().map(|a| &a.title).collect::<Vec<_>>()
        );
    }

    #[test]
    fn cargo_test_err_return_produces_no_annotation() {
        // A #[test] fn that returns `Err` produces a JUnit <failure> with no
        // `panicked at path:line`, so we correctly emit zero annotations
        // (pinning to a wrong line would be worse than nothing). Documents
        // the known-gap: these failures are visible in the Prow log but not
        // inline in the diff.
        if !nextest_available() {
            eprintln!("skipping: cargo-nextest not installed");
            return;
        }
        let tmp = TempDir::new().unwrap();
        write_crate(
            tmp.path(),
            "err_probe",
            "#[test]\nfn returns_err() -> Result<(), String> { Err(\"nope\".into()) }\n",
        );
        std::fs::create_dir_all(tmp.path().join(".config")).unwrap();
        std::fs::write(
            tmp.path().join(".config/nextest.toml"),
            "[profile.default.junit]\npath = \"junit.xml\"\n",
        )
        .unwrap();
        let out = run(tmp.path(), &["nextest", "run", "--no-fail-fast"]);
        assert!(!out.success);
        let anns = parse_output_for("cargo_test", &out, &canon(tmp.path()), &canon(tmp.path()));
        assert!(
            anns.is_empty(),
            "expected no annotations for Result::Err failure, got: {:?}",
            anns
        );
    }

    fn nextest_available() -> bool {
        std::process::Command::new("cargo")
            .args(["nextest", "--version"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

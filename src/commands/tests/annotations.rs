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

/// The parts of the check run that differ per job.
///
/// Each job needs its own `check_name`: GitHub keys a check run by name, so two
/// jobs sharing one name would overwrite each other's findings, and a rerun
/// would not replace the run it belongs to.
pub struct CheckStyle {
    pub check_name: String,
    /// Command that reproduces the run locally, shown under "Reproduce".
    pub reproduce: String,
    /// Prow job to rerun, shown under "Rerun" as `/test <job>`.
    pub rerun_job: String,
    /// Optional extra line under "Reproduce", e.g. how to narrow the run.
    pub reproduce_hint: Option<String>,
}

impl CheckStyle {
    /// Style used by `fslabscli rust-tests`.
    pub fn rust_tests() -> Self {
        Self {
            check_name: "test-annotations".into(),
            reproduce: "cargo-fslabscli rust-tests".into(),
            rerun_job: "cargo-tests".into(),
            reproduce_hint: Some("Narrow to a single crate with `--whitelist <path>`.".into()),
        }
    }
}

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

/// A failing test the parser could not pin to a line.
///
/// A timeout is the common case: nextest reports it as
/// `<failure type="test timeout"/>` with no message, no body and no panic, so
/// there is nothing to hang an inline annotation on. A test that returns `Err`
/// is the same shape with a message but still no location. Dropping these left
/// the most common CI failure mode reported nowhere at all, so they are named
/// in the check summary instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnlocatedFailure {
    /// JUnit suite, i.e. the test binary.
    pub suite: String,
    pub test: String,
    pub timed_out: bool,
    /// First line of whatever the runner did say, possibly empty.
    pub detail: String,
}

/// Everything one parse pass found: what can be shown on the diff, and what
/// can only be named in the summary.
#[derive(Debug, Default)]
pub struct ParseOutcome {
    pub annotations: Vec<Annotation>,
    pub unlocated: Vec<UnlocatedFailure>,
}

impl ParseOutcome {
    pub fn is_empty(&self) -> bool {
        self.annotations.is_empty() && self.unlocated.is_empty()
    }
}

impl From<Vec<Annotation>> for ParseOutcome {
    fn from(annotations: Vec<Annotation>) -> Self {
        Self {
            annotations,
            unlocated: Vec::new(),
        }
    }
}

#[derive(Default, Clone)]
pub struct AnnotationCollector {
    inner: Arc<Mutex<Vec<Annotation>>>,
    unlocated: Arc<Mutex<Vec<UnlocatedFailure>>>,
}

impl AnnotationCollector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, outcome: ParseOutcome) {
        // Recover the guard on poison rather than silently dropping the input;
        // losing failure annotations is worse than acting on a data structure
        // that another thread may have left in an unexpected state (the Vec
        // itself is fine, poison just means some other push panicked while
        // holding the guard).
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.extend(outcome.annotations);
        let mut u = self.unlocated.lock().unwrap_or_else(|e| e.into_inner());
        u.extend(outcome.unlocated);
    }

    pub fn drain(&self) -> ParseOutcome {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        // Dedupe by (tool, path, line): the same finding can arrive from more
        // than one code path (batch step and the per-package fallback of the
        // same lock/fmt/clippy failure), and duplicates chew up the 50-per-call
        // API cap without adding information.
        let mut seen: std::collections::HashSet<(&'static str, String, u32)> =
            std::collections::HashSet::new();
        let mut annotations = Vec::with_capacity(g.len());
        for a in g.drain(..) {
            if seen.insert((a.tool, a.path.clone(), a.start_line)) {
                annotations.push(a);
            }
        }

        let mut u = self.unlocated.lock().unwrap_or_else(|e| e.into_inner());
        let mut seen_tests: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();
        let mut unlocated = Vec::with_capacity(u.len());
        for f in u.drain(..) {
            if seen_tests.insert((f.suite.clone(), f.test.clone())) {
                unlocated.push(f);
            }
        }

        ParseOutcome {
            annotations,
            unlocated,
        }
    }
}

/// What a parse pass needs to turn tool output into repo-relative paths.
///
/// `workspace_dir` and `repo_root` are the same only for a workspace that sits
/// at the repository root, and getting them the wrong way round is silent: the
/// annotation is still produced, just on a path no file has, so GitHub accepts
/// it and renders nothing on the diff.
pub struct ParseDirs<'a> {
    /// Root of the cargo workspace the package belongs to. Cargo prints
    /// relative source paths against this, whatever directory cargo itself
    /// was invoked from, and the lockfile a workspace is checked against
    /// lives here.
    pub workspace_dir: &'a Path,
    /// Repository root. Annotation paths are reported relative to it, which
    /// is what GitHub resolves them against.
    pub repo_root: &'a Path,
    /// Where nextest was told to write its JUnit report for this run, if it
    /// ran at all. Supplied by the caller rather than guessed here, because
    /// only the caller knows the target directory cargo resolved and the file
    /// name it asked nextest for.
    pub junit_path: Option<&'a Path>,
}

pub fn parse_output_for(
    tool_id: &str,
    output: &CommandOutput,
    dirs: &ParseDirs<'_>,
) -> ParseOutcome {
    let ParseDirs {
        workspace_dir,
        repo_root,
        junit_path,
    } = *dirs;
    let combined = format!("{}\n{}", output.stdout, output.stderr);
    match tool_id {
        "cargo_fmt" => parse_cargo_fmt(&combined, repo_root).into(),
        "cargo_check" | "cargo_clippy" | "cargo_doc" => {
            parse_cargo_diagnostics(&combined, tool_id, workspace_dir, repo_root).into()
        }
        "cargo_lock" => parse_cargo_lock(workspace_dir, repo_root).into(),
        "cargo_test" => {
            // Prefer nextest JUnit if it exists: per-test attribution, the
            // failure-versus-timeout distinction, and no dependence on the
            // panic format staying stable. Fall back to the stdout panic regex
            // when fslabscli ran plain `cargo test` (no nextest binary
            // available) or when JUnit wasn't produced for any other reason.
            if let Some(xml) = junit_path.and_then(|p| std::fs::read_to_string(p).ok()) {
                let outcome = parse_nextest_junit(&xml, workspace_dir, repo_root);
                if !outcome.is_empty() {
                    return outcome;
                }
            }
            parse_cargo_test(&combined, workspace_dir, repo_root).into()
        }
        _ => ParseOutcome::default(),
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
                    "bazel_rustc" => "rustc (via bazel)",
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

fn parse_cargo_lock(workspace_dir: &Path, repo_root: &Path) -> Vec<Annotation> {
    // Nested workspaces have their own Cargo.lock; annotate the one the caller
    // actually checked (identified by the workspace dir), not the repo root.
    let rel_lock = workspace_dir
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
    #[serde(rename = "@name", default)]
    name: String,
    #[serde(rename = "testcase", default)]
    testcases: Vec<JUnitTestCase>,
}

#[derive(Debug, serde::Deserialize)]
struct JUnitTestCase {
    #[serde(rename = "@name", default)]
    name: String,
    #[serde(default)]
    failure: Option<JUnitFailure>,
    /// Bazel's `test.xml` reports a crashed target as `<error>` rather than
    /// `<failure>`; nextest only ever emits the latter.
    #[serde(default)]
    error: Option<JUnitFailure>,
    #[serde(default)]
    skipped: Option<JUnitSkipped>,
}

#[derive(Debug, serde::Deserialize)]
struct JUnitSkipped {}

#[derive(Debug, serde::Deserialize)]
struct JUnitFailure {
    #[serde(rename = "@message", default)]
    message: String,
    /// Nextest's failure kind, e.g. `test timeout` or
    /// `test failure with exit code 101`. A timeout carries this and nothing
    /// else: no message, no body, no location.
    #[serde(rename = "@type", default)]
    kind: String,
    #[serde(rename = "$text", default)]
    text: String,
}

// Truncate annotation message bodies so they stay readable in the diff popover.
// The check-runs API accepts much larger strings but the UI collapses anything
// long into a "..." teaser that hides the useful part.
const MAX_MESSAGE_CHARS: usize = 400;

fn parse_nextest_junit(xml: &str, base: &Path, repo_root: &Path) -> ParseOutcome {
    parse_junit(xml, base, repo_root, "cargo test").0
}

/// Extract failure annotations and per-suite counts from a JUnit document.
///
/// Handles both roots: nextest and Bazel wrap suites in `<testsuites>`, but a
/// lone `<testsuite>` root is also valid JUnit and Bazel emits it for
/// single-target runs.
fn parse_junit(
    xml: &str,
    base: &Path,
    repo_root: &Path,
    tool: &'static str,
) -> (ParseOutcome, Vec<PackageStat>) {
    let suites = match quick_xml::de::from_str::<JUnitTestSuites>(xml) {
        Ok(d) if !d.testsuites.is_empty() => d.testsuites,
        _ => match quick_xml::de::from_str::<JUnitTestSuite>(xml) {
            Ok(s) => vec![s],
            Err(_) => return (ParseOutcome::default(), Vec::new()),
        },
    };

    let mut stats = Vec::new();
    for suite in &suites {
        let mut stat = PackageStat {
            package: suite.name.clone(),
            passed: 0,
            failed: 0,
            skipped: 0,
        };
        for tc in &suite.testcases {
            if tc.failure.is_some() || tc.error.is_some() {
                stat.failed += 1;
            } else if tc.skipped.is_some() {
                stat.skipped += 1;
            } else {
                stat.passed += 1;
            }
        }
        if !stat.package.is_empty() {
            stats.push(stat);
        }
    }

    (junit_findings(suites, base, repo_root, tool), stats)
}

fn junit_findings(
    suites: Vec<JUnitTestSuite>,
    base: &Path,
    repo_root: &Path,
    tool: &'static str,
) -> ParseOutcome {
    let mut out = Vec::new();
    let mut unlocated = Vec::new();
    // Dedup key includes the test name so two distinct tests bottoming out in
    // the same shared assertion helper both produce annotations (each with the
    // correct test-name title). Without the name in the key, cross-testcase
    // collapse defeats the per-test attribution that motivates using JUnit.
    let mut seen: std::collections::HashSet<(String, String, u32)> =
        std::collections::HashSet::new();
    for suite in suites {
        let suite_name = suite.name.clone();
        for tc in suite.testcases {
            let Some(f) = tc.failure.or(tc.error) else {
                continue;
            };
            // We still need the panic-message regex here: nextest's JUnit
            // doesn't have file/line as first-class fields, they live inside
            // the free-form panic text.
            let Some(cap) = PANIC_RE.captures(&f.text) else {
                // No location to annotate. A timeout has neither text nor
                // message, only `type="test timeout"`; a test that returns
                // `Err` has a message but still no file and line.
                unlocated.push(UnlocatedFailure {
                    suite: suite_name.clone(),
                    test: tc.name.clone(),
                    timed_out: f.kind.contains("timeout"),
                    detail: first_line(if f.message.is_empty() {
                        &f.text
                    } else {
                        &f.message
                    }),
                });
                continue;
            };
            let path_str = cap.get(1).unwrap().as_str();
            let line_no: u32 = cap.get(2).unwrap().as_str().parse().unwrap_or(1);
            let Some(rel) = make_path_relative(path_str, base, repo_root) else {
                // Panicked outside the repository, typically inside a
                // dependency. Still a failing test, still nothing GitHub can
                // render on the diff.
                unlocated.push(UnlocatedFailure {
                    suite: suite_name.clone(),
                    test: tc.name.clone(),
                    timed_out: false,
                    detail: first_line(if f.message.is_empty() {
                        &f.text
                    } else {
                        &f.message
                    }),
                });
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
                tool,
            });
        }
    }
    ParseOutcome {
        annotations: out,
        unlocated,
    }
}

/// First non-empty line, trimmed and capped, for a one-line summary bullet.
fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .chars()
        .take(MAX_MESSAGE_CHARS)
        .collect()
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

// Bazel reports its own failures as `ERROR: <path>:<line>:<col>: <message>`.
// BUILD-file errors and failed actions both use this shape, with an absolute
// path into the workspace.
static BAZEL_ERROR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^ERROR:\s+(?P<path>[^\s:]+):(?P<line>\d+):(?P<col>\d+):\s*(?P<msg>.*)$").unwrap()
});

/// Parse a Bazel console log.
///
/// Picks up Bazel's own `ERROR: file:line:col:` reports plus any rustc or
/// clippy diagnostics the compile actions printed through it: those go through
/// Bazel unmodified, so they carry the same `--> path:line:col` spans cargo
/// prints and the cargo diagnostic parser reads them as-is.
pub fn parse_bazel_log(text: &str, repo_root: &Path) -> Vec<Annotation> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(cap) = BAZEL_ERROR_RE.captures(line) else {
            continue;
        };
        let path_str = cap.name("path").unwrap().as_str();
        let line_no: u32 = cap.name("line").unwrap().as_str().parse().unwrap_or(1);
        let Some(rel) = make_path_relative(path_str, repo_root, repo_root) else {
            continue;
        };
        let msg = cap.name("msg").unwrap().as_str().trim();
        out.push(Annotation {
            path: rel,
            start_line: line_no,
            end_line: line_no,
            annotation_level: AnnotationLevel::Failure,
            message: msg.chars().take(MAX_MESSAGE_CHARS).collect(),
            title: Some("bazel".into()),
            tool: "bazel",
        });
    }
    out.extend(parse_cargo_diagnostics(
        text,
        "bazel_rustc",
        repo_root,
        repo_root,
    ));
    out.retain(|a| !is_generated_path(&a.path));
    out
}

/// Bazel's outputs and fetched externals live inside the workspace but are not
/// repository files, so an annotation on one can never render on the diff and
/// would only consume the 50-annotation-per-call budget.
fn is_generated_path(path: &str) -> bool {
    ["bazel-out/", "bazel-bin/", "bazel-testlogs/", "external/"]
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

/// Read JUnit XML from each path: a file is parsed directly, a directory is
/// walked for `*.xml`. Bazel writes one `test.xml` per target, which
/// `bazel_test.sh` flattens into the artifacts directory.
pub fn parse_junit_paths(
    paths: &[PathBuf],
    repo_root: &Path,
    tool: &'static str,
) -> (ParseOutcome, Vec<PackageStat>) {
    let mut files = Vec::new();
    for p in paths {
        collect_xml_files(p, &mut files);
    }
    files.sort();

    let mut outcome = ParseOutcome::default();
    let mut stats = Vec::new();
    for f in files {
        let Ok(xml) = std::fs::read_to_string(&f) else {
            continue;
        };
        let (found, st) = parse_junit(&xml, repo_root, repo_root, tool);
        outcome.annotations.extend(found.annotations);
        outcome.unlocated.extend(found.unlocated);
        stats.extend(st);
    }
    outcome.annotations.retain(|a| !is_generated_path(&a.path));
    (outcome, stats)
}

fn collect_xml_files(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_file() {
        out.push(path.to_path_buf());
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_xml_files(&p, out);
        } else if p.extension().is_some_and(|e| e == "xml") {
            out.push(p);
        }
    }
}

// Resolve a path as printed by a tool into one relative to the repo root.
// `base` is what a relative `candidate` is relative to: for everything cargo
// prints - compiler spans and panic locations alike - that is the workspace
// root, not the package directory and not the directory cargo ran in.
//
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
pub struct PackageStat {
    pub package: String,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
}

/// Aggregate per-package test counts from the JUnit report fslabscli builds
/// while running. Suite names are formatted as
/// `"<Mandatory|Optional> {workspace} - {package} - {version}"`, so we combine
/// the two kinds into one row per (workspace, package).
pub fn collect_package_stats(report: &Report) -> Vec<PackageStat> {
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
fn build_summary(
    ctx: &GhTarget,
    style: &CheckStyle,
    annotations: &[Annotation],
    unlocated: &[UnlocatedFailure],
    stats: &[PackageStat],
) -> String {
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
    if !unlocated.is_empty() {
        let timed_out = unlocated.iter().filter(|f| f.timed_out).count();
        out.push_str(&format!(
            "**{} test(s)** failed with no source location to annotate, {timed_out} of them by timeout. They are listed below and nowhere else.\n\n",
            unlocated.len(),
        ));
    }
    out.push_str("Details also render inline on the *Files changed* tab.\n\n");

    out.push_str("### Reproduce\n\n");
    out.push_str(&format!(
        "Run the full suite locally with:\n\n```\n{}\n```\n\n",
        style.reproduce
    ));
    if let Some(hint) = &style.reproduce_hint {
        out.push_str(&format!("{hint}\n\n"));
    }

    if let Some(url) = prow_log_url() {
        out.push_str(&format!("[View full Prow log]({url})\n\n"));
    }

    if !stats.is_empty() {
        out.push_str("### Test summary\n\n");
        out.push_str("| Package | Passed | Failed | Skipped |\n");
        out.push_str("|---|---:|---:|---:|\n");
        for s in stats {
            out.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                s.package, s.passed, s.failed, s.skipped
            ));
        }
        out.push('\n');
    }

    if !unlocated.is_empty() {
        // Ahead of the annotated findings: these are the ones a reviewer will
        // not stumble on anywhere else in the UI.
        out.push_str(&format!("### Not annotated ({})\n\n", unlocated.len()));
        out.push_str(
            "A timeout carries no panic, and neither does a test that returns `Err`, \
             so there is no line for GitHub to mark on the diff.\n\n",
        );
        let mut items: Vec<&UnlocatedFailure> = unlocated.iter().collect();
        // Timeouts first: they are the ones that cost a whole job's wall clock.
        items.sort_by(|a, b| {
            b.timed_out
                .cmp(&a.timed_out)
                .then_with(|| a.suite.cmp(&b.suite))
                .then_with(|| a.test.cmp(&b.test))
        });
        for f in items {
            let kind = if f.timed_out { "timed out" } else { "failed" };
            let suite = if f.suite.is_empty() {
                String::new()
            } else {
                format!(" (`{}`)", f.suite)
            };
            let detail: String = f.detail.chars().take(160).collect();
            if detail.is_empty() {
                out.push_str(&format!("- **{kind}** `{}`{suite}\n", f.test));
            } else {
                out.push_str(&format!("- **{kind}** `{}`{suite} - {detail}\n", f.test));
            }
        }
        out.push('\n');
    }

    if !by_tool.is_empty() {
        out.push_str("### Findings\n\n");
    }
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
    out.push_str(&format!(
        "Comment `/test {}` on this PR to rerun the job.\n",
        style.rerun_job
    ));

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
    style: &CheckStyle,
    outcome: ParseOutcome,
    stats: &[PackageStat],
) -> Result<()> {
    if outcome.is_empty() {
        return Ok(());
    }

    // Redact once, here, so every caller and every parser is covered and the
    // summary (built from these messages) inherits it.
    let annotations: Vec<Annotation> = outcome
        .annotations
        .into_iter()
        .map(|a| Annotation {
            message: redact(&a.message),
            title: a.title.as_deref().map(redact),
            ..a
        })
        .collect();
    let unlocated: Vec<UnlocatedFailure> = outcome
        .unlocated
        .into_iter()
        .map(|f| UnlocatedFailure {
            detail: redact(&f.detail),
            ..f
        })
        .collect();

    let octocrab = Octocrab::builder()
        .personal_token(ctx.token.clone())
        .build()
        .context("build octocrab client")?;

    let path = format!("/repos/{}/{}/check-runs", ctx.target.owner, ctx.target.repo);
    let title = "Test summary".to_string();
    let summary = build_summary(&ctx.target, style, &annotations, &unlocated, stats);

    let mut chunks = annotations.chunks(MAX_ANNOTATIONS_PER_CALL);
    let first = chunks.next().unwrap_or(&[]);

    // Neutral: annotations describe locations, not verdicts. Prow's cargo-tests
    // check already shows failure; a second red X here adds no information.
    let body = json!({
        "name": style.check_name,
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
    // A workspace that is not the repo root, e.g. fsl_libs' `fdk_apps/`.
    fn nested_ws() -> PathBuf {
        PathBuf::from("/repo/apps")
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
        // Spans are workspace-root relative, so a package at crates/foo is
        // reported as `crates/foo/src/a.rs`.
        let text = "\
error[E0308]: mismatched types
  --> crates/foo/src/a.rs:1:1
   |
   = note: expected `u32`
::: crates/foo/src/b.rs:2:2
   |
";
        let a = parse_cargo_diagnostics(text, "cargo_clippy", &root(), &root());
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].path, "crates/foo/src/a.rs");
    }

    #[test]
    fn diagnostics_resolve_against_a_workspace_below_the_repo_root() {
        // Sub-workspace at /repo/apps: cargo prints paths relative to it, and
        // the annotation has to come back out relative to the repo root.
        let text = "\
error[E0308]: mismatched types
  --> viewer/src/main.rs:4:9
";
        let a = parse_cargo_diagnostics(text, "cargo_check", &nested_ws(), &root());
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].path, "apps/viewer/src/main.rs");
    }

    #[test]
    fn test_panic_deduplicates_repeated_report() {
        let text = "\
thread 'main' panicked at 'boom', crates/foo/src/lib.rs:12:5
... stack trace ...
thread 'main' panicked at 'boom', crates/foo/src/lib.rs:12:5
";
        let a = parse_cargo_test(text, &root(), &root());
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].path, "crates/foo/src/lib.rs");
    }

    #[test]
    fn test_panic_is_not_resolved_against_the_package_directory() {
        // Regression: cargo prints panic locations relative to the workspace
        // root even when it was invoked from the package directory, so using
        // the package directory as the base doubled the prefix
        // (`dagger/tests/dagger/tests/import_one_partition.rs`). GitHub accepts
        // such an annotation and then renders nothing, because no file in the
        // repo has that path.
        let text = "thread 'boom' panicked at crates/foo/tests/it.rs:65:5:\n";
        let dirs = ParseDirs {
            workspace_dir: &root(),
            repo_root: &root(),
            junit_path: None,
        };
        let out = CommandOutput {
            stdout: text.into(),
            stderr: String::new(),
            success: false,
        };
        let a = parse_output_for("cargo_test", &out, &dirs).annotations;
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].path, "crates/foo/tests/it.rs");
        assert_eq!(a[0].start_line, 65);
    }

    #[test]
    fn lock_annotation_follows_the_workspace_not_the_package() {
        // The lockfile checked for a member of the root workspace is the root
        // one, whatever directory the member lives in.
        let out = CommandOutput {
            stdout: String::new(),
            stderr: String::new(),
            success: false,
        };
        let a = parse_output_for(
            "cargo_lock",
            &out,
            &ParseDirs {
                workspace_dir: &root(),
                repo_root: &root(),
                junit_path: None,
            },
        )
        .annotations;
        assert_eq!(a[0].path, "Cargo.lock");
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
        let s = build_summary(&fake_ctx(), &CheckStyle::rust_tests(), &anns, &[], &[]);
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
        let s = build_summary(&fake_ctx(), &CheckStyle::rust_tests(), &anns, &[], &[]);
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
        let s = build_summary(
            &fake_ctx_no_pr(),
            &CheckStyle::rust_tests(),
            &anns,
            &[],
            &[],
        );
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
        let s = build_summary(&fake_ctx(), &CheckStyle::rust_tests(), &anns, &[], &[]);
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
        let s = build_summary(
            &fake_ctx(),
            &CheckStyle::rust_tests(),
            &anns,
            &[],
            &collect_package_stats(&report),
        );
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
        let s = build_summary(&fake_ctx(), &CheckStyle::rust_tests(), &anns, &[], &[]);
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
        c.push(vec![ann("a", 1, "test")].into());
        assert_eq!(c.drain().annotations.len(), 1);
        assert_eq!(c.drain().annotations.len(), 0);
    }

    #[test]
    fn collector_drain_dedupes_by_tool_path_line() {
        // Same finding pushed from batch phase and per-package fallback must
        // collapse to one annotation (the check-run API caps at 50 per call).
        let c = AnnotationCollector::new();
        c.push(vec![ann("Cargo.lock", 1, "cargo lock")].into());
        c.push(vec![ann("Cargo.lock", 1, "cargo lock")].into());
        c.push(vec![ann("Cargo.lock", 1, "cargo lock")].into());
        assert_eq!(c.drain().annotations.len(), 1);
    }

    #[test]
    fn collector_drain_keeps_distinct_findings() {
        let c = AnnotationCollector::new();
        c.push(
            vec![
                ann("src/foo.rs", 10, "cargo clippy"),
                ann("src/foo.rs", 10, "cargo fmt"),
                ann("src/foo.rs", 11, "cargo clippy"),
            ]
            .into(),
        );
        assert_eq!(c.drain().annotations.len(), 3);
    }

    #[test]
    fn collector_recovers_from_mutex_poison() {
        // Poison the mutex by panicking while holding the guard, then verify
        // push/drain still work rather than silently dropping data.
        let c = AnnotationCollector::new();
        let inner = c.inner.clone();
        let _ = std::thread::spawn(move || {
            let _guard = inner.lock().unwrap();
            panic!("intentional poison");
        })
        .join();
        assert!(c.inner.is_poisoned());
        c.push(vec![ann("src/foo.rs", 1, "cargo clippy")].into());
        let drained = c.drain();
        assert_eq!(drained.annotations.len(), 1);
    }

    #[test]
    fn bazel_log_annotates_build_file_errors() {
        let log = "\
Loading: 0 packages loaded
ERROR: /repo/crates/foo/BUILD.bazel:12:5: no such attribute 'deps' in 'rust_library'
INFO: Elapsed time: 1.2s";
        let a = parse_bazel_log(log, &root());
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].path, "crates/foo/BUILD.bazel");
        assert_eq!(a[0].start_line, 12);
        assert_eq!(a[0].tool, "bazel");
        assert!(a[0].message.contains("no such attribute"));
    }

    #[test]
    fn bazel_log_reads_rustc_diagnostics_it_relays() {
        // Bazel passes compiler output through untouched, so the same span
        // format cargo prints shows up in a bazel log.
        let log = "\
ERROR: /repo/crates/foo/BUILD.bazel:3:1: Compiling Rust library foo failed
error[E0308]: mismatched types
  --> crates/foo/src/lib.rs:7:9
   |
7  |     let x: u32 = \"nope\";";
        let a = parse_bazel_log(log, &root());
        let rustc: Vec<_> = a.iter().filter(|x| x.tool == "rustc (via bazel)").collect();
        assert_eq!(rustc.len(), 1);
        assert_eq!(rustc[0].path, "crates/foo/src/lib.rs");
        assert_eq!(rustc[0].start_line, 7);
        assert_eq!(rustc[0].message, "mismatched types");
    }

    #[test]
    fn bazel_log_drops_generated_and_external_paths() {
        // These resolve inside the workspace but are not repository files, so
        // an annotation on one can never render and would only eat into the
        // 50-per-call cap.
        let log = "\
ERROR: /repo/bazel-out/k8-fastbuild/bin/generated.rs:3:1: boom
ERROR: /repo/external/crates_io__serde/src/lib.rs:9:2: boom
ERROR: /repo/crates/foo/BUILD.bazel:1:1: real one";
        let a = parse_bazel_log(log, &root());
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].path, "crates/foo/BUILD.bazel");
    }

    #[test]
    fn junit_counts_pass_fail_skip_per_suite() {
        let xml = r#"<?xml version="1.0"?>
<testsuites>
  <testsuite name="//crates/foo:foo_test">
    <testcase name="passes"/>
    <testcase name="skips"><skipped/></testcase>
    <testcase name="fails"><failure message="boom">thread 'fails' panicked at crates/foo/src/lib.rs:12:5</failure></testcase>
  </testsuite>
</testsuites>"#;
        let (found, stats) = parse_junit(xml, &root(), &root(), "bazel test");
        let anns = found.annotations;
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].package, "//crates/foo:foo_test");
        assert_eq!(
            (stats[0].passed, stats[0].failed, stats[0].skipped),
            (1, 1, 1)
        );
        assert_eq!(anns.len(), 1);
        assert_eq!(anns[0].path, "crates/foo/src/lib.rs");
        assert_eq!(anns[0].tool, "bazel test");
    }

    #[test]
    fn junit_reads_a_bare_testsuite_root() {
        // Bazel writes a lone <testsuite> root for a single-target run; both
        // shapes are valid JUnit and both turn up in bazel-testlogs.
        let xml = r#"<testsuite name="//crates/bar:bar_test">
  <testcase name="fails"><failure message="boom">panicked at crates/bar/src/lib.rs:4:1</failure></testcase>
</testsuite>"#;
        let (found, stats) = parse_junit(xml, &root(), &root(), "bazel test");
        let anns = found.annotations;
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].failed, 1);
        assert_eq!(anns.len(), 1);
        assert_eq!(anns[0].start_line, 4);
    }

    #[test]
    fn junit_treats_error_element_as_a_failure() {
        // A crashed Bazel target is reported as <error>, not <failure>.
        let xml = r#"<testsuite name="//crates/bar:bar_test">
  <testcase name="crashes"><error message="signal 6">panicked at crates/bar/src/lib.rs:9:1</error></testcase>
</testsuite>"#;
        let (found, stats) = parse_junit(xml, &root(), &root(), "bazel test");
        let anns = found.annotations;
        assert_eq!(stats[0].failed, 1);
        assert_eq!(anns.len(), 1);
        assert_eq!(anns[0].start_line, 9);
    }

    #[test]
    fn summary_uses_the_style_it_is_given() {
        let anns = vec![Annotation {
            path: "src/foo.rs".into(),
            start_line: 1,
            end_line: 1,
            annotation_level: AnnotationLevel::Failure,
            message: "boom".into(),
            title: None,
            tool: "bazel test",
        }];
        let style = CheckStyle {
            check_name: "bazel-test-annotations".into(),
            reproduce: "bazel test //...".into(),
            rerun_job: "bazel-tests".into(),
            reproduce_hint: None,
        };
        let s = build_summary(&fake_ctx(), &style, &anns, &[], &[]);
        assert!(s.contains("bazel test //..."));
        assert!(s.contains("/test bazel-tests"));
        assert!(!s.contains("cargo-fslabscli rust-tests"));
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
        let a = parse_nextest_junit(xml, &root(), &root()).annotations;
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
        let a = parse_nextest_junit(xml, &root(), &root()).annotations;
        assert_eq!(a.len(), 1);
        assert!(!a[0].message.contains('\n'), "message: {:?}", a[0].message);
        assert!(
            a[0].message.contains("left: 1"),
            "message: {:?}",
            a[0].message
        );
    }

    fn unlocated(test: &str, timed_out: bool, detail: &str) -> UnlocatedFailure {
        UnlocatedFailure {
            suite: "dagger::tests".into(),
            test: test.into(),
            timed_out,
            detail: detail.into(),
        }
    }

    #[test]
    fn junit_reports_a_timeout_that_has_no_location() {
        // Nextest's shape for a terminated test: a type and nothing else.
        let xml = r#"<testsuite name="tests">
  <testcase name="import_one_partition"><failure type="test timeout"/></testcase>
</testsuite>"#;
        let (found, stats) = parse_junit(xml, &root(), &root(), "cargo test");
        assert!(found.annotations.is_empty());
        assert_eq!(found.unlocated.len(), 1);
        assert!(found.unlocated[0].timed_out);
        assert_eq!(found.unlocated[0].test, "import_one_partition");
        assert_eq!(found.unlocated[0].suite, "tests");
        // Still counted as a failure in the per-package table.
        assert_eq!(stats[0].failed, 1);
    }

    #[test]
    fn junit_keeps_a_panic_outside_the_repo_as_unlocated() {
        // A panic inside a dependency resolves to a path outside the repo, so
        // it cannot be annotated. Dropping it silently would lose the failure.
        let xml = r#"<testsuite name="tests">
  <testcase name="calls_a_dep"><failure message="boom">panicked at /home/runner/.cargo/registry/src/x/lib.rs:9:1</failure></testcase>
</testsuite>"#;
        let (found, _) = parse_junit(xml, &root(), &root(), "cargo test");
        assert!(found.annotations.is_empty());
        assert_eq!(found.unlocated.len(), 1);
        assert!(!found.unlocated[0].timed_out);
    }

    #[test]
    fn summary_names_unlocated_failures_with_timeouts_first() {
        let anns = vec![Annotation {
            path: "src/foo.rs".into(),
            start_line: 1,
            end_line: 1,
            annotation_level: AnnotationLevel::Failure,
            message: "boom".into(),
            title: None,
            tool: "cargo test",
        }];
        let un = vec![
            unlocated("returns_err", false, "Error: \"nope\""),
            unlocated("hangs", true, ""),
        ];
        let s = build_summary(&fake_ctx(), &CheckStyle::rust_tests(), &anns, &un, &[]);
        assert!(s.contains("### Not annotated (2)"), "{s}");
        let timeout_at = s.find("**timed out** `hangs`").unwrap();
        let failure_at = s.find("**failed** `returns_err`").unwrap();
        assert!(timeout_at < failure_at, "timeouts should come first:\n{s}");
        // The detail survives for the ones that have any.
        assert!(s.contains("Error: \"nope\""), "{s}");
        // Counted in the header so the number is visible without scrolling.
        assert!(
            s.contains("**2 test(s)** failed with no source location"),
            "{s}"
        );
    }

    #[test]
    fn summary_without_annotations_still_lists_unlocated_failures() {
        // A run where every failure is a timeout: there are no findings to
        // group, so the Findings header must not be emitted empty, and the
        // timeouts must still be there.
        let un = vec![unlocated("hangs", true, "")];
        let s = build_summary(&fake_ctx(), &CheckStyle::rust_tests(), &[], &un, &[]);
        assert!(!s.contains("### Findings"), "{s}");
        assert!(s.contains("**timed out** `hangs`"), "{s}");
    }

    #[test]
    fn summary_omits_the_unlocated_section_when_there_are_none() {
        let anns = vec![Annotation {
            path: "src/foo.rs".into(),
            start_line: 1,
            end_line: 1,
            annotation_level: AnnotationLevel::Failure,
            message: "boom".into(),
            title: None,
            tool: "cargo test",
        }];
        let s = build_summary(&fake_ctx(), &CheckStyle::rust_tests(), &anns, &[], &[]);
        assert!(!s.contains("Not annotated"), "{s}");
        assert!(!s.contains("no source location"), "{s}");
    }

    #[test]
    fn collector_dedupes_unlocated_by_suite_and_test() {
        // The same failing test can arrive twice when a package is parsed by
        // both the batch step and the per-package fallback.
        let c = AnnotationCollector::new();
        c.push(ParseOutcome {
            annotations: Vec::new(),
            unlocated: vec![unlocated("hangs", true, "")],
        });
        c.push(ParseOutcome {
            annotations: Vec::new(),
            unlocated: vec![unlocated("hangs", true, ""), unlocated("other", true, "")],
        });
        let drained = c.drain();
        assert_eq!(drained.unlocated.len(), 2);
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
        let s = build_summary(&fake_ctx(), &CheckStyle::rust_tests(), &anns, &[], &[]);
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

    fn run<S: AsRef<std::ffi::OsStr>>(dir: &std::path::Path, args: &[S]) -> CommandOutput {
        // Isolate target dir so parallel tests don't fight over a lock file.
        // For a lone crate this is also where cargo would put it anyway.
        run_in(dir, &dir.join("target"), args)
    }

    fn run_in<S: AsRef<std::ffi::OsStr>>(
        dir: &std::path::Path,
        target_dir: &std::path::Path,
        args: &[S],
    ) -> CommandOutput {
        let out = Command::new("cargo")
            .args(args)
            .current_dir(dir)
            .env("CARGO_TERM_COLOR", "never")
            .env("CARGO_TARGET_DIR", target_dir)
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
        let anns = parse_output_for(
            "cargo_fmt",
            &out,
            &ParseDirs {
                workspace_dir: &canon(tmp.path()),
                repo_root: &canon(tmp.path()),
                junit_path: None,
            },
        )
        .annotations;
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
        let anns = parse_output_for(
            "cargo_clippy",
            &out,
            &ParseDirs {
                workspace_dir: &canon(tmp.path()),
                repo_root: &canon(tmp.path()),
                junit_path: None,
            },
        )
        .annotations;
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
        let anns = parse_output_for(
            "cargo_check",
            &out,
            &ParseDirs {
                workspace_dir: &canon(tmp.path()),
                repo_root: &canon(tmp.path()),
                junit_path: None,
            },
        )
        .annotations;
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
        let anns = parse_output_for(
            "cargo_test",
            &out,
            &ParseDirs {
                workspace_dir: &canon(tmp.path()),
                repo_root: &canon(tmp.path()),
                junit_path: None,
            },
        )
        .annotations;
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
        let out = run(tmp.path(), &nextest_args(tmp.path(), JUNIT_NAME));
        assert!(
            !out.success,
            "nextest unexpectedly succeeded: {} {}",
            out.stdout, out.stderr
        );
        let junit = tmp.path().join("target/nextest/default").join(JUNIT_NAME);
        assert!(
            junit.exists(),
            "nextest did not produce junit at {:?}",
            junit
        );
        let anns = parse_output_for(
            "cargo_test",
            &out,
            &ParseDirs {
                workspace_dir: &canon(tmp.path()),
                repo_root: &canon(tmp.path()),
                junit_path: Some(&junit),
            },
        )
        .annotations;
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
    fn cargo_test_err_return_is_reported_without_a_location() {
        // A #[test] fn that returns `Err` produces a JUnit <failure> with no
        // `panicked at path:line`. Pinning it to a guessed line would be worse
        // than nothing, so it produces no annotation, but it must still be
        // named: before, a failing test disappeared from the check entirely.
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
        let out = run(tmp.path(), &nextest_args(tmp.path(), JUNIT_NAME));
        assert!(!out.success);
        let junit = tmp.path().join("target/nextest/default").join(JUNIT_NAME);
        let outcome = parse_output_for(
            "cargo_test",
            &out,
            &ParseDirs {
                workspace_dir: &canon(tmp.path()),
                repo_root: &canon(tmp.path()),
                junit_path: Some(&junit),
            },
        );
        assert!(
            outcome.annotations.is_empty(),
            "expected no annotations for Result::Err failure, got: {:?}",
            outcome.annotations
        );
        assert_eq!(outcome.unlocated.len(), 1, "{:?}", outcome.unlocated);
        let f = &outcome.unlocated[0];
        assert_eq!(f.test, "returns_err");
        assert!(!f.timed_out);
        assert!(f.detail.contains("nope"), "detail: {:?}", f.detail);
    }

    #[test]
    fn timed_out_test_is_reported_without_a_location() {
        // The failure mode that motivated this: nextest reports a terminated
        // test as `<failure type="test timeout"/>` with no message, no body and
        // no panic, so there is nothing to annotate and it used to be dropped.
        // A timeout wave then produced a check run with nothing in it.
        if !nextest_available() {
            eprintln!("skipping: cargo-nextest not installed");
            return;
        }
        let tmp = TempDir::new().unwrap();
        write_crate(
            tmp.path(),
            "timeout_probe",
            "#[test]\nfn hangs() { std::thread::sleep(std::time::Duration::from_secs(60)); }\n",
        );
        let args = nextest_args(tmp.path(), JUNIT_NAME);
        // Same tool config the helper just wrote, plus a one-second kill so the
        // test does not wait out a realistic timeout.
        std::fs::write(
            canon(tmp.path()).join("fslabscli-nextest.toml"),
            format!(
                "[profile.default]\nslow-timeout = {{ period = \"1s\", terminate-after = 1 }}\n\
                 [profile.default.junit]\npath = \"{JUNIT_NAME}\"\n"
            ),
        )
        .unwrap();

        let out = run(tmp.path(), &args);
        assert!(!out.success);
        let junit = tmp.path().join("target/nextest/default").join(JUNIT_NAME);
        let outcome = parse_output_for(
            "cargo_test",
            &out,
            &ParseDirs {
                workspace_dir: &canon(tmp.path()),
                repo_root: &canon(tmp.path()),
                junit_path: Some(&junit),
            },
        );
        assert!(
            outcome.annotations.is_empty(),
            "a timeout has no location to annotate: {:?}",
            outcome.annotations
        );
        assert_eq!(outcome.unlocated.len(), 1, "{:?}", outcome.unlocated);
        assert_eq!(outcome.unlocated[0].test, "hangs");
        assert!(
            outcome.unlocated[0].timed_out,
            "expected the timeout kind to be recognised: {:?}",
            outcome.unlocated[0]
        );
    }

    fn nextest_available() -> bool {
        std::process::Command::new("cargo")
            .args(["nextest", "--version"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    // A per-package report name, as the runner generates. The exact value does
    // not matter, only that the fixtures ask for the same one they then read.
    const JUNIT_NAME: &str = "junit-probe-0.0.0.xml";

    /// The nextest invocation the runner builds: a generated tool config
    /// naming the report, layered under whatever config the repository has.
    /// Returns the arguments; the config file is written under `dir`.
    fn nextest_args(dir: &std::path::Path, junit_name: &str) -> Vec<String> {
        let cfg = canon(dir).join("fslabscli-nextest.toml");
        std::fs::write(
            &cfg,
            format!("[profile.default.junit]\npath = \"{junit_name}\"\n"),
        )
        .unwrap();
        vec![
            "nextest".into(),
            "run".into(),
            "--no-fail-fast".into(),
            "--profile".into(),
            "default".into(),
            "--tool-config-file".into(),
            format!("fslabscli:{}", cfg.display()),
        ]
    }

    /// Write a workspace root with one member at `sub/pkg`, mirroring the
    /// layout every fsl_libs package has (and which no other fixture here
    /// covers, since a lone crate is its own workspace root).
    fn write_workspace_member(root: &std::path::Path, name: &str, lib_rs: &str) -> PathBuf {
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"sub/pkg\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        let pkg = root.join("sub/pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        write_crate(&pkg, name, lib_rs);
        pkg
    }

    #[test]
    fn member_package_paths_are_relative_to_the_workspace_root() {
        // Regression for `dagger/tests/dagger/tests/import_one_partition.rs`:
        // cargo prints `sub/pkg/src/lib.rs` (workspace-root relative) even
        // though it was invoked from `sub/pkg`, so resolving that against the
        // package directory doubled the prefix and the annotation silently
        // failed to render on the diff.
        let tmp = TempDir::new().unwrap();
        let pkg = write_workspace_member(
            tmp.path(),
            "member_probe",
            "pub fn f() {\n    let _: u32 = \"hi\";\n}\n",
        );
        let out = run(&pkg, &["check", "--all-targets"]);
        assert!(!out.success);
        let anns = parse_output_for(
            "cargo_check",
            &out,
            &ParseDirs {
                workspace_dir: &canon(tmp.path()),
                repo_root: &canon(tmp.path()),
                junit_path: None,
            },
        )
        .annotations;
        assert!(
            anns.iter().any(|a| a.path == "sub/pkg/src/lib.rs"),
            "expected a workspace-relative path, got: {:?}",
            anns.iter().map(|a| &a.path).collect::<Vec<_>>()
        );
    }

    #[test]
    fn member_package_panic_paths_are_relative_to_the_workspace_root() {
        // Same regression on the path that actually produced the bad
        // annotation in CI: a panicking test in a workspace member.
        let tmp = TempDir::new().unwrap();
        let pkg = write_workspace_member(
            tmp.path(),
            "member_panic_probe",
            "#[test]\nfn boom() {\n    assert_eq!(1, 2);\n}\n",
        );
        let out = run(&pkg, &["test", "--", "--nocapture"]);
        assert!(!out.success);
        let anns = parse_output_for(
            "cargo_test",
            &out,
            &ParseDirs {
                workspace_dir: &canon(tmp.path()),
                repo_root: &canon(tmp.path()),
                junit_path: None,
            },
        )
        .annotations;
        assert!(
            anns.iter().any(|a| a.path == "sub/pkg/src/lib.rs"),
            "expected a workspace-relative path, got: {:?}",
            anns.iter().map(|a| &a.path).collect::<Vec<_>>()
        );
    }

    #[test]
    fn member_package_junit_lands_in_the_workspace_store() {
        // Regression: the report for a member was looked for under the package
        // directory. Nextest writes it into its store, `<target-dir>/nextest/
        // <profile>/`, which for a member is the workspace target directory,
        // so nothing was ever found and every failure fell back to the stdout
        // panic regex (no test name, no assertion text).
        if !nextest_available() {
            eprintln!("skipping: cargo-nextest not installed");
            return;
        }
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("target");
        let pkg = write_workspace_member(
            tmp.path(),
            "member_junit_probe",
            "#[test]\nfn boom() {\n    assert_eq!(1, 2);\n}\n",
        );
        let out = run_in(&pkg, &target, &nextest_args(tmp.path(), JUNIT_NAME));
        assert!(!out.success);

        assert!(
            !pkg.join("target").exists(),
            "fixture did not reproduce the shared-target-dir layout"
        );
        let junit = target.join("nextest/default").join(JUNIT_NAME);
        assert!(junit.exists(), "no report at {junit:?}");

        let anns = parse_output_for(
            "cargo_test",
            &out,
            &ParseDirs {
                workspace_dir: &canon(tmp.path()),
                repo_root: &canon(tmp.path()),
                junit_path: Some(&junit),
            },
        )
        .annotations;
        assert!(
            anns.iter().any(|a| a.path == "sub/pkg/src/lib.rs"),
            "expected a workspace-relative path, got: {:?}",
            anns.iter().map(|a| &a.path).collect::<Vec<_>>()
        );
        // Reading the report is what buys per-test attribution; the stdout
        // fallback can only say "Test panicked here."
        assert!(
            anns.iter()
                .any(|a| a.title.as_deref().unwrap_or("").contains("boom")),
            "annotation titles did not include the test name: {:?}",
            anns.iter().map(|a| &a.title).collect::<Vec<_>>()
        );
    }

    #[test]
    fn tool_config_coexists_with_a_repository_nextest_config() {
        // The runner asks for the report through `--tool-config-file` rather
        // than by generating `.config/nextest.toml`, because nextest reads
        // that file only from the workspace root, which is where a repository
        // keeps its real settings (fsl_libs has per-test timeouts and a test
        // group there). Verify the tool config adds JUnit output and leaves
        // the repository's file both untouched and in effect.
        if !nextest_available() {
            eprintln!("skipping: cargo-nextest not installed");
            return;
        }
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("target");
        let pkg = write_workspace_member(
            tmp.path(),
            "member_config_probe",
            "#[test]\nfn boom() {\n    assert_eq!(1, 2);\n}\n",
        );
        let repo_config = "[profile.default]\nslow-timeout = { period = \"60s\", terminate-after = 5 }\nstatus-level = \"all\"\n";
        std::fs::create_dir_all(tmp.path().join(".config")).unwrap();
        std::fs::write(tmp.path().join(".config/nextest.toml"), repo_config).unwrap();

        let out = run_in(&pkg, &target, &nextest_args(tmp.path(), JUNIT_NAME));
        assert!(
            !out.success,
            "nextest rejected the layered config: {} {}",
            out.stdout, out.stderr
        );
        assert!(
            target.join("nextest/default").join(JUNIT_NAME).exists(),
            "tool config did not produce a report: {} {}",
            out.stdout,
            out.stderr
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join(".config/nextest.toml")).unwrap(),
            repo_config,
            "the repository's nextest config must be left alone"
        );
    }
}

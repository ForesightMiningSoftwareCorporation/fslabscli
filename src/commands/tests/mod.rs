pub mod annotations;
mod docker_service;

use crate::commands::tests::annotations::{
    AnnotationCollector, CheckStyle, GhContext, GhTarget, ParseDirs, collect_package_stats,
    parse_output_for, post_annotations, resolve_token,
};

use anyhow::Context;
use clap::Parser;
use humanize_duration::{Truncate, prelude::DurationExt};
use junit_report::{OffsetDateTime, Report, ReportBuilder, TestCase, TestSuiteBuilder};
use opentelemetry::{
    KeyValue, global,
    metrics::{Counter, Histogram, MeterProvider},
};
use port_check::free_local_port;
use serde::Serialize;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env,
    fmt::{Display, Formatter},
    fs::{File, create_dir_all},
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::OnceCell;
use tokio::sync::Semaphore;
use tracing::Instrument;

/// Nextest profile the `cargo_test` step runs under. It also names the store
/// subdirectory the JUnit report is written into,
/// `<target-dir>/nextest/<profile>/`, so the two must agree.
const NEXTEST_PROFILE: &str = "default";

/// Write the nextest config that asks for a JUnit report at a path we know.
///
/// Handed to nextest as `--tool-config-file`, which layers *below* the
/// repository's own `.config/nextest.toml` instead of replacing it. Generating
/// that file ourselves is not an option: nextest reads it only from the
/// workspace root, which is exactly where a repository keeps its real nextest
/// settings, so writing one there would overwrite them.
///
/// A repository that sets `junit.path` itself still wins, being higher
/// priority. The report then lands somewhere we do not look, and annotations
/// fall back to reading panic locations out of stdout.
///
/// The file goes under the target directory: absolute, which nextest requires,
/// already ignored by git, and named per package so concurrent runs in one
/// workspace never share it.
fn write_nextest_tool_config(
    target_directory: &Path,
    package: &str,
    version: &str,
    junit_file_name: &str,
) -> std::io::Result<PathBuf> {
    let dir = target_directory.join("fslabscli");
    create_dir_all(&dir)?;
    let path = dir.join(format!("nextest-{package}-{version}.toml"));
    std::fs::write(
        &path,
        format!("[profile.{NEXTEST_PROFILE}.junit]\npath = \"{junit_file_name}\"\n"),
    )?;
    Ok(path)
}

/// Checks whether a step should be skipped based on the env var `SKIP_{ID}_TEST` (uppercased).
/// Returns `true` only when the env var is present and equals `"true"`.
#[allow(dead_code)] // false positive: module is named `tests`
fn should_skip_step(id: &str) -> bool {
    let skip_env = format!("SKIP_{}_TEST", id).to_uppercase();
    matches!(env::var(skip_env), Ok(v) if v == "true")
}

use crate::{
    PackageRelatedOptions, PrettyPrintable,
    cli_args::DiffOptions,
    commands::{
        check_workspace::{Options as CheckWorkspaceOptions, check_workspace},
        fix_lock_files::fix_workspace_lockfile,
        tests::docker_service::{DockerContainer, postgres_url},
    },
    init_metrics,
    script::{CommandOutput, Script},
    test_args::TestArgs,
    utils::cargo::Cargo,
};

/// Env var that, when set to `"true"`, skips the `cargo_test` step for
/// packages that declare no external services (Bazel already covers
/// compilation and unit tests for serviceless crates in CI).
const SKIP_TESTS_WITHOUT_SERVICES_ENV: &str = "SKIP_TESTS_WITHOUT_SERVICES";

/// Whether a package needs external test fixtures: any known service
/// (postgres/azurite/minio), a custom service, or a pre-service/pre-test
/// script.
#[allow(dead_code)] // false positive: module is named `tests`
fn package_requires_services(test_args: &TestArgs) -> bool {
    test_args.services.postgres
        || test_args.services.azurite
        || test_args.services.minio
        || !test_args.custom_services.is_empty()
        || test_args.pre_service_script.is_some()
        || test_args.pre_test_script.is_some()
}

/// Checks whether the `cargo_test` step should be skipped for a package that
/// declares no services. Returns `true` only when `SKIP_TESTS_WITHOUT_SERVICES`
/// equals `"true"`, the package requires no services, and it uses the default
/// test command (custom test commands, e.g. `wasm-pack test`, are not covered
/// by Bazel).
#[allow(dead_code)] // false positive: module is named `tests`
fn should_skip_serviceless_cargo_test(test_args: &TestArgs) -> bool {
    matches!(env::var(SKIP_TESTS_WITHOUT_SERVICES_ENV), Ok(v) if v == "true")
        && !package_requires_services(test_args)
        && test_args.test_command.is_none()
}

#[derive(Debug, Parser, Default, Clone)]
#[command(about = "Run tests")]
pub struct Options {
    #[clap(long, env, default_value = ".")]
    artifacts: PathBuf,
    #[clap(
        long,
        env,
        default_value = "https://raw.githubusercontent.com/ForesightMiningSoftwareCorporation/github/main/deny.toml"
    )]
    default_deny_location: String,
    #[clap(flatten)]
    diff: DiffOptions,
    /// Run tests on all packages, ignoring change detection
    #[arg(long)]
    run_all: bool,
    /// App ID of a GitHub App holding `checks: write`, used to post test
    /// annotations when the job has no `GITHUB_TOKEN`.
    #[arg(long, env = "FSLABSCLI_CHECKS_APP_ID")]
    checks_app_id: Option<u64>,
    /// Path to the private key of the app named by `--checks-app-id`.
    #[arg(long, env = "FSLABSCLI_CHECKS_APP_PRIVATE_KEY")]
    checks_app_private_key: Option<PathBuf>,
}

#[derive(Serialize)]
pub struct TestResult {}

impl Display for TestResult {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "")
    }
}

impl PrettyPrintable for TestResult {
    fn pretty_print(&self) -> String {
        "".to_string()
    }
}

#[derive(Default, Clone)]
struct FslabsTest {
    pub id: String,
    pub optional: bool,
    pub command: String,
    pub pre_command: Option<String>,
    pub post_command: Option<String>,
    pub envs: HashMap<String, String>,
    pub skip: bool,
    pub parse_subtests: bool,
}

async fn has_cargo_nextest() -> bool {
    if let Ok(output) = tokio::process::Command::new("cargo")
        .args(["nextest", "--version"])
        .output()
        .await
    {
        output.status.success()
    } else {
        false
    }
}

#[derive(Debug, serde::Deserialize)]
struct JUnitTestSuites {
    #[serde(rename = "testsuite", default)]
    testsuite: Vec<JUnitTestSuite>,
}

#[derive(Debug, serde::Deserialize)]
struct JUnitTestSuite {
    #[serde(rename = "testcase", default)]
    testcase: Vec<JUnitTestCase>,
}

#[derive(Debug, serde::Deserialize)]
struct JUnitTestCase {
    #[serde(rename = "@name")]
    name: String,
    #[serde(rename = "@time")]
    time: f64,
    #[serde(default)]
    failure: Option<JUnitFailure>,
    #[serde(default)]
    skipped: Option<JUnitSkipped>,
}

#[derive(Debug, serde::Deserialize)]
struct JUnitFailure {
    #[serde(rename = "@message", default)]
    message: String,
    #[serde(rename = "$text", default)]
    text: String,
}

#[derive(Debug, serde::Deserialize)]
struct JUnitSkipped {
    #[serde(rename = "@message", default)]
    #[allow(dead_code)]
    message: String,
}

fn merge_nextest_junit(
    testsuite: &mut junit_report::TestSuite,
    junit_path: &PathBuf,
    member: &super::check_workspace::Result,
    current_step: usize,
    total_steps: usize,
    metrics: &Metrics,
) -> anyhow::Result<()> {
    use quick_xml::de::from_str;

    if !junit_path.exists() {
        tracing::debug!("Nextest JUnit file not found at {:?}", junit_path);
        return Ok(());
    }

    let xml_content = std::fs::read_to_string(junit_path)?;

    // Nextest generates: <testsuites><testsuite><testcase/></testsuite></testsuites>
    let workspace_name = member.workspace.clone();
    let package_name = member.package.clone();
    let package_version = member.version.clone();

    match from_str::<JUnitTestSuites>(&xml_content) {
        Ok(junit_data) => {
            let mut merged_count = 0;
            let mut subtest_num = current_step;
            for suite in junit_data.testsuite {
                for test_case in suite.testcase {
                    let duration = junit_report::Duration::nanoseconds(
                        (test_case.time * 1_000_000_000.0) as i64,
                    );

                    // Format with package name and step count like high-level steps
                    let test_name = format!(
                        "{package_name:30.30} {subtest_num}/{total_steps} │ {}",
                        test_case.name
                    );

                    let test_command = format!("nextest {}", test_case.name);

                    let (tc, status) = if let Some(failure) = test_case.failure {
                        (
                            TestCase::failure(
                                &test_name,
                                duration,
                                "test",
                                &format!("{}\n{}", failure.message, failure.text),
                            ),
                            "FAIL",
                        )
                    } else if test_case.skipped.is_some() {
                        (TestCase::skipped(&test_name), "SKIPPED")
                    } else {
                        (TestCase::success(&test_name, duration), "PASS")
                    };
                    // Register metrics and tc for the all test
                    metrics.test_duration_h.record(
                        duration.as_seconds_f64(),
                        &[
                            KeyValue::new("workspace_name", workspace_name.clone()),
                            KeyValue::new("package_name", package_name.clone()),
                            KeyValue::new("package_version", package_version.clone()),
                            KeyValue::new("test_command", test_command.clone()),
                            KeyValue::new("status", status),
                        ],
                    );
                    metrics.test_counter.add(
                        1,
                        &[
                            KeyValue::new("workspace_name", workspace_name.clone()),
                            KeyValue::new("package_name", package_name.clone()),
                            KeyValue::new("package_version", package_version.clone()),
                            KeyValue::new("test_command", test_command.clone()),
                            KeyValue::new("status", status),
                        ],
                    );

                    testsuite.add_testcase(tc);
                    merged_count += 1;
                    subtest_num += 1;
                }
            }
            if merged_count > 0 {
                tracing::debug!(
                    "Merged {} nextest test cases into JUnit report",
                    merged_count
                );
            }
            Ok(())
        }
        Err(e) => {
            tracing::warn!("Failed to parse nextest JUnit XML: {}", e);
            Ok(())
        }
    }
}

#[tracing::instrument(skip_all, name = "tests", fields(otel.status_code = tracing::field::Empty))]
pub async fn tests(
    common_options: &PackageRelatedOptions,
    options: &Options,
    repo_root: PathBuf,
) -> anyhow::Result<TestResult> {
    let meter = global::meter("tests");
    let overall_duration_h = meter.f64_histogram("rust_tests_workspace").build();
    let overall_counter = meter.u64_counter("rust_tests_workspace").build();
    let member_duration_h = meter.f64_histogram("rust_tests_member").build();
    let member_counter = meter.u64_counter("rust_tests_member").build();
    let test_duration_h = meter.f64_histogram("rust_tests_test").build();
    let test_counter = meter.u64_counter("rust_tests_test").build();
    let changed_counter = meter.u64_counter("rust_tests_changed").build();
    let common_meter = init_metrics(false).meter("common_tests");
    let common_member_duration_h = common_meter
        .f64_histogram("rust_tests_common_member")
        .build();
    let common_member_counter = common_meter.u64_counter("rust_tests_common_member").build();
    let overall_start_time = OffsetDateTime::now_utc();
    let diff_strategy = options.diff.strategy();
    // Get Directory information
    tracing::info!("Running the tests with the following arguments:");
    tracing::info!("* `check_changed`: true");
    tracing::info!("* `check_publish`: false");
    tracing::info!("* `base_sha`: {:?}", options.diff.base_sha);
    tracing::info!("* `diff_strategy`: {}", diff_strategy);
    tracing::info!("* `whitelist`: {}", common_options.whitelist.join(","));
    tracing::info!("* `blacklist`: {}", common_options.blacklist.join(","));

    let check_workspace_options = CheckWorkspaceOptions::new()
        .with_diff_strategy(diff_strategy.clone())
        .with_check_publish(false)
        .with_check_changed(!options.run_all);

    let results =
        check_workspace::<Cargo>(common_options, &check_workspace_options, repo_root.clone())
            .instrument(tracing::info_span!("check_workspace"))
            .await
            .map_err(|e| {
                tracing::error!("Check directory for crates that need publishing: {}", e);
                e
            })
            .with_context(|| "Could not get directory information")?;

    let mut global_junit_report = ReportBuilder::new().build();

    // Global fail tracker
    let mut global_failed = false;

    let metrics = Metrics {
        member_duration_h,
        member_counter,
        test_duration_h,
        test_counter,
        common_member_duration_h,
        common_member_counter,
        changed_counter,
    };
    let semaphore = Arc::new(Semaphore::new(common_options.job_limit));
    let annotation_collector = AnnotationCollector::new();
    let mut handles = vec![];

    // Precompute transitive dependency counts for sorting and display.
    let dep_graph = results.crate_graph.dependency_graph();
    let transitive_dep_counts: HashMap<String, usize> = results
        .members
        .values()
        .filter_map(|m| {
            m.package_id.as_ref().map(|id| {
                (
                    m.package.clone(),
                    dep_graph.get_transitive_dependencies(id.clone()).len(),
                )
            })
        })
        .collect();

    // Sort members so directly changed crates run first (fail fast), then
    // dependency-changed, then unchanged (only present with --run-all).
    // Within each group, crates with more transitive dependencies run first
    // (higher in the tree, more likely to surface issues), then alphabetically.
    let mut members: Vec<_> = results
        .members
        .into_values()
        .filter(|member| {
            !member.test_detail.skip.unwrap_or_default() && (member.perform_test || options.run_all)
        })
        .collect();
    members.sort_by(|a, b| {
        let a_deps = transitive_dep_counts.get(&a.package).copied().unwrap_or(0);
        let b_deps = transitive_dep_counts.get(&b.package).copied().unwrap_or(0);
        b.changed
            .cmp(&a.changed)
            .then(b.dependencies_changed.cmp(&a.dependencies_changed))
            .then(b_deps.cmp(&a_deps))
            .then(a.package.cmp(&b.package))
    });

    // Print execution order grouped by change status.
    {
        let name_w = members
            .iter()
            .map(|m| m.package.len())
            .max()
            .unwrap_or(7)
            .max(7);
        let deps_w = 4; // "Deps" header
        #[expect(clippy::type_complexity)]
        let groups: &[(&str, Box<dyn Fn(&super::check_workspace::Result) -> bool>)] = &[
            ("Directly changed", Box::new(|m| m.changed)),
            (
                "Dependency changed",
                Box::new(|m| !m.changed && m.dependencies_changed),
            ),
            (
                "Unchanged",
                Box::new(|m| !m.changed && !m.dependencies_changed),
            ),
        ];
        let active_groups: Vec<_> = groups
            .iter()
            .filter_map(|(label, pred)| {
                let rows: Vec<_> = members.iter().filter(|m| pred(m)).collect();
                if rows.is_empty() {
                    None
                } else {
                    Some((*label, rows))
                }
            })
            .collect();
        if !active_groups.is_empty() {
            // Column widths including padding: " content "
            let nc = name_w + 2; // name column cell width
            let dc = deps_w + 2; // deps column cell width
            // Total width between outer box chars: nc + 1(│) + dc
            let total = nc + 1 + dc;
            let nb = "─".repeat(nc);
            let db = "─".repeat(dc);
            let fb = "─".repeat(total);
            let mut table = String::new();
            table.push_str(&format!("╭{fb}╮\n"));
            for (i, (label, rows)) in active_groups.iter().enumerate() {
                if i > 0 {
                    table.push_str(&format!("├{nb}┴{db}┤\n"));
                }
                table.push_str(&format!("│ {:<w$}│\n", label, w = total - 1));
                table.push_str(&format!("├{nb}┬{db}┤\n"));
                table.push_str(&format!(
                    "│ {:<name_w$} │ {:<deps_w$} │\n",
                    "Package", "Deps"
                ));
                table.push_str(&format!("├{nb}┼{db}┤\n"));
                for m in rows {
                    let dep_count = transitive_dep_counts.get(&m.package).copied().unwrap_or(0);
                    table.push_str(&format!(
                        "│ {:<name_w$} │ {:<deps_w$} │\n",
                        m.package, dep_count,
                    ));
                }
            }
            table.push_str(&format!("╰{nb}┴{db}╯"));
            tracing::info!("Test execution order:\n{table}");
        }
    }

    // --- Batch compile phase ---
    // Run cargo check/clippy/doc once per (workspace, additional_args)
    // group instead of once per package. This avoids target-dir lock
    // contention that serialises concurrent per-package cargo invocations
    // when many packages share the same workspace.
    // Key includes workspace + version so two packages with the same name in
    // different workspaces or at different versions (vendored baselines) do
    // not collide. Keying on name alone silently skips fmt/check/clippy/doc
    // for un-batched duplicates.
    let batched_packages: Arc<HashSet<(String, String, String)>> = {
        // Group packages by (workspace, additional_args).
        // Store (name, version) so we can use `name@version` for -p flags
        // to disambiguate packages that exist at multiple versions (e.g.
        // vendor baselines). BTreeMap so iteration order is deterministic
        // across runs of the same commit; with HashMap the fail-fast
        // `break 'batch` would surface a different workspace's failure each
        // rerun.
        let mut batch_groups: BTreeMap<String, BTreeMap<String, Vec<(String, String)>>> =
            BTreeMap::new();
        for member in &members {
            let args = member.test_detail.args.clone().unwrap_or_default();
            batch_groups
                .entry(member.workspace.clone())
                .or_default()
                .entry(args.additional_args.clone())
                .or_default()
                .push((member.package.clone(), member.version.clone()));
        }

        let mut batched = HashSet::new();
        let jobs_flag = if common_options.inner_job_limit != 0 {
            format!("--jobs {}", common_options.inner_job_limit)
        } else {
            String::new()
        };

        let mut batch_junit_report = ReportBuilder::new().build();

        let base_revspec_batch = options
            .diff
            .base_sha
            .clone()
            .unwrap_or_else(|| "origin/main".into());

        'batch: for (workspace, args_groups) in &batch_groups {
            let ws_path = repo_root.join(workspace);

            // Run lock check once per workspace, before any cargo command
            // that could modify the lock file as a side effect.
            if !should_skip_step("cargo_lock") {
                let lock_span = tracing::info_span!(
                    "batch_step",
                    otel.name = format!("batch_step: {workspace}::batch_lock"),
                    otel.status_code = tracing::field::Empty,
                    step = "batch_lock",
                    workspace = %workspace,
                );
                let _lock_entered = lock_span.enter();

                let all_packages: Vec<_> =
                    args_groups.values().flat_map(|pkgs| pkgs.iter()).collect();
                let tc_prefix = format!(
                    "{workspace:30.30} batch │ {:50.50}",
                    "fix-lock-files --check"
                );

                tracing::info!(
                    "│ {} │ ► START ({} packages)",
                    tc_prefix,
                    all_packages.len()
                );
                let start_time = OffsetDateTime::now_utc();

                let lock_result =
                    fix_workspace_lockfile(&repo_root, &ws_path, &base_revspec_batch, true)
                        .unwrap_or_else(|e| e.into());

                let end_time = OffsetDateTime::now_utc();
                let duration = end_time - start_time;

                let mut ts = TestSuiteBuilder::new(&format!("Batch {workspace} lock"))
                    .set_timestamp(start_time)
                    .build();

                let (status, tc) = if lock_result.success {
                    tracing::info!(
                        "│ {} │ 🟢 PASS in {}",
                        &tc_prefix,
                        duration.human(Truncate::Second)
                    );
                    lock_span.record("otel.status_code", "OK");
                    ("PASS", TestCase::success(&tc_prefix, duration))
                } else {
                    tracing::info!(
                        "│ {} │ 🟥 FAIL in {}",
                        &tc_prefix,
                        duration.human(Truncate::Second)
                    );
                    if !lock_result.stderr.is_empty() {
                        tracing::warn!(
                            "│ {} │ stderr:\n{}",
                            &tc_prefix,
                            lock_result.stderr.trim_end()
                        );
                    }
                    lock_span.record("otel.status_code", "ERROR");
                    (
                        "FAIL",
                        TestCase::failure(
                            &tc_prefix,
                            duration,
                            "fix-lock-files --check",
                            "required",
                        ),
                    )
                };

                for (pkg_name, pkg_version) in &all_packages {
                    metrics.test_duration_h.record(
                        duration.as_seconds_f64(),
                        &[
                            KeyValue::new("workspace_name", workspace.clone()),
                            KeyValue::new("package_name", pkg_name.clone()),
                            KeyValue::new("package_version", pkg_version.clone()),
                            KeyValue::new("test_command", "fix-lock-files --check".to_string()),
                            KeyValue::new("status", status),
                        ],
                    );
                    metrics.test_counter.add(
                        1,
                        &[
                            KeyValue::new("workspace_name", workspace.clone()),
                            KeyValue::new("package_name", pkg_name.clone()),
                            KeyValue::new("package_version", pkg_version.clone()),
                            KeyValue::new("test_command", "fix-lock-files --check".to_string()),
                            KeyValue::new("status", status),
                        ],
                    );
                }

                ts.add_testcase(tc);
                batch_junit_report.add_testsuite(ts);

                if !lock_result.success {
                    annotation_collector.push(parse_output_for(
                        "cargo_lock",
                        &lock_result,
                        &ParseDirs {
                            workspace_dir: &ws_path,
                            repo_root: &repo_root,
                            junit_path: None,
                        },
                    ));
                    global_failed = true;
                    // The after-loop clone at the end of the labeled `'batch`
                    // block picks up batch_junit_report; cloning again here
                    // would emit every suite twice into the final report.
                    break 'batch;
                }
            }

            for (additional_args, packages) in args_groups {
                if packages.len() <= 1 {
                    // No benefit to batching a single package.
                    continue;
                }
                // cargo fmt only accepts plain package names.
                let fmt_flags: String = packages
                    .iter()
                    .map(|(name, _)| format!("-p {name}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                // cargo check/clippy/doc need name@version to
                // disambiguate packages that exist at multiple versions
                // (e.g. vendor baselines).
                let pkg_flags: String = packages
                    .iter()
                    .map(|(name, ver)| format!("-p {name}@{ver}"))
                    .collect::<Vec<_>>()
                    .join(" ");

                let steps: Vec<(&str, String, HashMap<String, String>)> = vec![
                    (
                        "batch_fmt",
                        format!("cargo fmt --verbose {fmt_flags} -- --check"),
                        HashMap::new(),
                    ),
                    (
                        "batch_check",
                        format!(
                            "cargo check --all-targets {additional_args} {pkg_flags} {jobs_flag}"
                        ),
                        HashMap::new(),
                    ),
                    (
                        "batch_clippy",
                        format!(
                            "cargo clippy --all-targets {additional_args} {pkg_flags} -- -D warnings"
                        ),
                        HashMap::new(),
                    ),
                    (
                        "batch_doc",
                        // TODO: remove `+nightly ... -Z...` when stable rustdoc
                        // isn't insanely slow anymore
                        // see https://github.com/rust-lang/rust/issues/146895
                        //
                        // Paradoxically, we removed `--no-deps` to prevent an
                        // issue where rustdoc just gets stuck on the last 10
                        // packages and never makes progress.
                        format!(
                            "cargo +nightly doc -Zrustdoc-mergeable-info {pkg_flags} {jobs_flag}"
                        ),
                        HashMap::from([("RUSTDOCFLAGS".to_string(), "-D warnings".to_string())]),
                    ),
                ];

                let steps: Vec<_> = steps
                    .into_iter()
                    .filter(|(id, _, _)| !should_skip_step(id))
                    .collect();

                let mut ts = TestSuiteBuilder::new(&format!("Batch {workspace}"))
                    .set_timestamp(OffsetDateTime::now_utc())
                    .build();

                for (id, command, envs) in &steps {
                    let step_span = tracing::info_span!(
                        "batch_step",
                        otel.name = format!("batch_step: {workspace}::{id}"),
                        otel.status_code = tracing::field::Empty,
                        step = %id,
                        workspace = %workspace,
                    );
                    let _step_entered = step_span.enter();

                    let tc_prefix = format!("{workspace:30.30} batch │ {command:50.50}",);

                    tracing::info!("│ {} │ ► START ({} packages)", tc_prefix, packages.len());
                    let start_time = OffsetDateTime::now_utc();

                    let output = Script::new(command, true)
                        .name(format!("{workspace}::{id}"))
                        .current_dir(&ws_path)
                        .envs(envs)
                        .log_stdout(tracing::Level::DEBUG)
                        .log_stderr(tracing::Level::DEBUG)
                        .execute()
                        .await;

                    let end_time = OffsetDateTime::now_utc();
                    let duration = end_time - start_time;

                    let (status, tc) = if output.success {
                        tracing::info!(
                            "│ {} │ 🟢 PASS in {}",
                            &tc_prefix,
                            duration.human(Truncate::Second)
                        );
                        step_span.record("otel.status_code", "OK");
                        ("PASS", TestCase::success(&tc_prefix, duration))
                    } else {
                        tracing::info!(
                            "│ {} │ 🟥 FAIL in {}",
                            &tc_prefix,
                            duration.human(Truncate::Second)
                        );
                        if !output.stderr.is_empty() {
                            tracing::warn!(
                                "│ {} │ stderr:\n{}",
                                &tc_prefix,
                                output.stderr.trim_end()
                            );
                        }
                        if !output.stdout.is_empty() {
                            tracing::warn!(
                                "│ {} │ stdout:\n{}",
                                &tc_prefix,
                                output.stdout.trim_end()
                            );
                        }
                        step_span.record("otel.status_code", "ERROR");
                        (
                            "FAIL",
                            TestCase::failure(&tc_prefix, duration, command, "required"),
                        )
                    };

                    // Record per-step metrics for each package in the batch.
                    for (pkg_name, pkg_version) in packages {
                        metrics.test_duration_h.record(
                            duration.as_seconds_f64(),
                            &[
                                KeyValue::new("workspace_name", workspace.clone()),
                                KeyValue::new("package_name", pkg_name.clone()),
                                KeyValue::new("package_version", pkg_version.clone()),
                                KeyValue::new("test_command", command.clone()),
                                KeyValue::new("status", status),
                            ],
                        );
                        metrics.test_counter.add(
                            1,
                            &[
                                KeyValue::new("workspace_name", workspace.clone()),
                                KeyValue::new("package_name", pkg_name.clone()),
                                KeyValue::new("package_version", pkg_version.clone()),
                                KeyValue::new("test_command", command.clone()),
                                KeyValue::new("status", status),
                            ],
                        );
                    }

                    ts.add_testcase(tc);

                    if !output.success {
                        // Batch step ids are `batch_fmt`/`batch_check`/... but the
                        // parsers dispatch on `cargo_fmt`/`cargo_check`/...
                        let parser_id = id.replace("batch_", "cargo_");
                        annotation_collector.push(parse_output_for(
                            &parser_id,
                            &output,
                            &ParseDirs {
                                workspace_dir: &ws_path,
                                repo_root: &repo_root,
                                junit_path: None,
                            },
                        ));
                        global_failed = true;
                        batch_junit_report.add_testsuite(ts);
                        // Same reason as the lock failure path: don't clone
                        // into global here; the after-loop clone catches it.
                        break 'batch;
                    }
                }

                batch_junit_report.add_testsuite(ts);
                batched.extend(
                    packages
                        .iter()
                        .map(|(name, version)| (workspace.clone(), name.clone(), version.clone())),
                );
            }
        }
        // Runs on both the success fall-through and the `break 'batch` failure
        // paths; the failure paths therefore MUST NOT clone batch_junit_report
        // into global_junit_report themselves or every batch suite is added
        // twice.
        global_junit_report.add_testsuites(batch_junit_report.testsuites().clone());
        Arc::new(batched)
    };

    let lock_check_cache: LockCheckCache = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

    if !global_failed {
        for member in members {
            let common_opts = Arc::new(common_options.clone());
            let base_revspec = options
                .diff
                .base_sha
                .clone()
                .unwrap_or_else(|| "origin/main".into());
            let task_handle = tokio::spawn(
                do_test_on_package(
                    common_opts,
                    repo_root.clone(),
                    base_revspec,
                    member,
                    metrics.clone(),
                    semaphore.clone(),
                    lock_check_cache.clone(),
                    batched_packages.clone(),
                    annotation_collector.clone(),
                )
                .instrument(tracing::Span::current()),
            );
            handles.push(task_handle);
        }
    }

    // Wait for every spawned package task to complete before we drain the
    // annotation collector. Breaking early on the first failure would leave
    // sibling tokio tasks running detached (dropping a JoinHandle doesn't
    // abort), so annotations they push after we drain silently vanish. The
    // semaphore already caps parallelism, so the wall-clock cost of waiting
    // is just the longest still-running task after the first failure.
    while !handles.is_empty() {
        let (result, _, remaining) = futures::future::select_all(handles).await;
        handles = remaining;
        match result {
            Ok((failed, junit_report)) => {
                global_junit_report.add_testsuites(junit_report.testsuites().clone());
                global_failed |= failed;
            }
            Err(_) => global_failed = true,
        }
    }

    let collected = annotation_collector.drain();
    if !collected.is_empty() {
        // Both halves are reported by the same check run: annotations render
        // on the diff, unlocated failures are named in its summary.
        let count = collected.annotations.len() + collected.unlocated.len();
        // Every failure here is reported at warn! and then dropped. Posting
        // annotations is a reporting nicety; the job's pass/fail verdict is
        // decided by global_failed below and must not depend on GitHub being
        // reachable. But it is logged loudly, because a silent skip is exactly
        // how this feature shipped disabled and stayed that way.
        match GhTarget::from_env() {
            Ok(target) => {
                match resolve_token(
                    &target,
                    options.checks_app_id,
                    options.checks_app_private_key.as_deref(),
                )
                .await
                {
                    Ok(token) => {
                        let gh = GhContext { target, token };
                        let stats = collect_package_stats(&global_junit_report);
                        match post_annotations(&gh, &CheckStyle::rust_tests(), collected, &stats)
                            .await
                        {
                            Ok(()) => tracing::info!(
                                "Posted {} GitHub check-run annotation(s) to {}/{}@{}",
                                count,
                                gh.target.owner,
                                gh.target.repo,
                                gh.target.head_sha
                            ),
                            Err(e) => tracing::warn!("Failed to post annotations: {:#}", e),
                        }
                    }
                    Err(e) => tracing::warn!(
                        "Not posting {count} GitHub check-run annotation(s): {:#}",
                        e
                    ),
                }
            }
            Err(reason) => {
                tracing::warn!("Not posting {count} GitHub check-run annotation(s): {reason}")
            }
        }
    }

    let total_duration = global_junit_report
        .testsuites()
        .iter()
        .flat_map(|ts| ts.testcases().iter().map(|tc| tc.time()))
        .sum::<junit_report::Duration>();

    let mut junit_file = File::create(options.artifacts.join("junit.rust.xml"))?;
    global_junit_report.write_xml(&mut junit_file)?;
    let overall_end_time = OffsetDateTime::now_utc();
    let overall_duration = overall_end_time - overall_start_time;
    tracing::info!(
        "Workspace tests ran in {} (for a cumulated duration of {})",
        overall_duration,
        total_duration
    );
    match global_failed {
        false => {
            overall_duration_h.record(
                overall_duration.as_seconds_f64(),
                &[KeyValue::new("status", "success")],
            );
            overall_counter.add(1, &[KeyValue::new("success", true)]);
            tracing::Span::current().record("otel.status_code", "OK");
            Ok(TestResult {})
        }
        true => {
            overall_duration_h.record(
                overall_duration.as_seconds_f64(),
                &[KeyValue::new("status", "failed")],
            );
            overall_counter.add(1, &[KeyValue::new("success", false)]);
            tracing::Span::current().record("otel.status_code", "ERROR");
            Err(anyhow::anyhow!("tests failed"))
        }
    }
}

#[derive(Clone)]
struct Metrics {
    member_duration_h: Histogram<f64>,
    member_counter: Counter<u64>,
    test_duration_h: Histogram<f64>,
    test_counter: Counter<u64>,
    changed_counter: Counter<u64>,
    common_member_duration_h: Histogram<f64>,
    common_member_counter: Counter<u64>,
}

/// Per-workspace cache for the `cargo_lock` check so it runs once per workspace
/// rather than once per package.
type LockCheckCache = Arc<tokio::sync::Mutex<HashMap<PathBuf, Arc<OnceCell<LockCheckResult>>>>>;

#[derive(Clone)]
struct LockCheckResult {
    success: bool,
    stdout: String,
    stderr: String,
}

#[allow(clippy::too_many_arguments)]
async fn do_test_on_package(
    common_options: Arc<PackageRelatedOptions>,
    repo_root: PathBuf,
    base_revspec: String,
    member: super::check_workspace::Result,
    metrics: Metrics,
    semaphore: Arc<Semaphore>,
    lock_check_cache: LockCheckCache,
    batched_packages: Arc<HashSet<(String, String, String)>>,
    annotation_collector: AnnotationCollector,
) -> (bool, Report) {
    let permit = semaphore.acquire().await;

    let result = run_package_tests(
        common_options,
        repo_root,
        base_revspec,
        member,
        metrics,
        lock_check_cache,
        batched_packages,
        annotation_collector,
    )
    .await;
    drop(permit);
    result
}

#[tracing::instrument(
    skip_all,
    name = "test_package",
    fields(
        otel.name = format!("test_package: {}", member.package),
        otel.status_code = tracing::field::Empty,
        workspace = %member.workspace,
        package = %member.package,
        version = %member.version,
    )
)]
#[allow(clippy::too_many_arguments)]
async fn run_package_tests(
    common_options: Arc<PackageRelatedOptions>,
    repo_root: PathBuf,
    base_revspec: String,
    member: super::check_workspace::Result,
    metrics: Metrics,
    lock_check_cache: LockCheckCache,
    batched_packages: Arc<HashSet<(String, String, String)>>,
    annotation_collector: AnnotationCollector,
) -> (bool, Report) {
    let mut junit_report = ReportBuilder::new().build();
    let mut failed = false;

    let member_start_time = OffsetDateTime::now_utc();
    let workspace_name = &member.workspace;
    let package_name = &member.package;
    let package_version = &member.version;
    let package_path = repo_root.join(&member.path);
    // Cargo reports source paths relative to the workspace root even when it is
    // run from a member directory, so annotations resolve against this rather
    // than against `package_path`.
    let workspace_path = repo_root.join(&member.workspace);
    let test_args = member.test_detail.args.clone().unwrap_or_default();
    let use_nextest = has_cargo_nextest().await;
    // Nextest resolves `junit.path` against its store directory,
    // `<target-dir>/nextest/<profile>/`, which is shared by every package in
    // the workspace. Packages are tested concurrently, so the file name has to
    // be unique per package or two runs overwrite each other's report.
    let junit_file_name = format!("junit-{package_name}-{package_version}.xml");
    let nextest_junit_path = member
        .target_directory
        .join("nextest")
        .join(NEXTEST_PROFILE)
        .join(&junit_file_name);
    let nextest_tool_config = match use_nextest {
        false => None,
        true => match write_nextest_tool_config(
            &member.target_directory,
            package_name,
            package_version,
            &junit_file_name,
        ) {
            Ok(path) => Some(path),
            Err(e) => {
                // Only costs the report: the run still happens, and failures
                // fall back to reading panic locations out of stdout.
                tracing::warn!("Failed to write nextest tool config: {e}");
                None
            }
        },
    };
    let mut postgres_process = None;
    let mut database_url = None;
    let mut azurite_process = None;
    let mut minio_process = None;
    let mut minio_endpoint = None;

    if member.changed {
        metrics.changed_counter.add(
            1,
            &[
                KeyValue::new("workspace_name", workspace_name.clone()),
                KeyValue::new("package_name", package_name.clone()),
                KeyValue::new("package_version", package_version.clone()),
            ],
        );
    }

    let ts_name = format!("{workspace_name} - {package_name} - {package_version}");
    tracing::info!("Testing {ts_name}");
    let mut ts_mandatory = TestSuiteBuilder::new(&format!("Mandatory {ts_name}"))
        .set_timestamp(OffsetDateTime::now_utc())
        .build();
    let mut ts_optional = TestSuiteBuilder::new(&format!("Optional {ts_name}"))
        .set_timestamp(OffsetDateTime::now_utc())
        .build();

    // Handle Pre-Service Script
    if !failed && let Some(pre_service_script) = test_args.pre_service_script.clone() {
        tracing::info!("│ {package_name:30.30}     │ Running pre-service script command");
        let start_time = OffsetDateTime::now_utc();
        let CommandOutput {
            stdout,
            stderr,
            success,
        } = Script::new(pre_service_script, true)
            .name(format!("{package_name}::pre_service_script"))
            .current_dir(&package_path)
            .log_stdout(tracing::Level::DEBUG)
            .log_stderr(tracing::Level::DEBUG)
            .execute()
            .await;
        let end_time = OffsetDateTime::now_utc();
        let duration = end_time - start_time;
        let mut pre_service_script_tx = match success {
            true => TestCase::success("pre_service_script", duration),
            false => {
                failed = true;
                TestCase::failure("pre_service_script", duration, "", "required")
            }
        };
        pre_service_script_tx.set_system_out(&stdout);
        pre_service_script_tx.set_system_err(&stderr);
        ts_mandatory.add_testcase(pre_service_script_tx);
    }

    // Handle service database
    if !failed && test_args.services.postgres {
        tracing::info!("│ {package_name:30.30}     │ Setting up service database");
        let start_time = OffsetDateTime::now_utc();
        let pg_port = free_local_port().unwrap();
        let docker_process = DockerContainer::postgres(pg_port).create().await;
        let end_time = OffsetDateTime::now_utc();
        let duration = end_time - start_time;
        let service_db_tc = match docker_process {
            Ok(process) => {
                postgres_process = Some(process);
                database_url = Some(postgres_url(pg_port));
                TestCase::success("service_database", duration)
            }
            Err(e) => {
                failed = true;
                TestCase::failure(
                    "service_database",
                    duration,
                    "service_database",
                    e.to_string().as_str(),
                )
            }
        };
        ts_mandatory.add_testcase(service_db_tc);
    }
    // Handle service azurite
    let mut azurite_blob_port = None;
    if !failed && test_args.services.azurite {
        tracing::info!("│ {package_name:30.30}     │ Setting up service azurite");
        let start_time = OffsetDateTime::now_utc();
        let blob_port = free_local_port().unwrap();
        let docker_process = DockerContainer::azurite(blob_port).create().await;
        let end_time = OffsetDateTime::now_utc();
        let duration = end_time - start_time;
        let service_azurite_tc = match docker_process {
            Ok(process) => {
                azurite_process = Some(process);
                azurite_blob_port = Some(blob_port);
                TestCase::success("service_azurite", duration)
            }
            Err(e) => {
                failed = true;
                TestCase::failure(
                    "service_azurite",
                    duration,
                    "service_azurite",
                    e.to_string().as_str(),
                )
            }
        };
        ts_mandatory.add_testcase(service_azurite_tc);
    }

    // Handle service minio
    if !failed && test_args.services.minio {
        tracing::info!("│ {package_name:30.30}     │ Setting up service minio");
        let start_time = OffsetDateTime::now_utc();
        let minio_port = free_local_port().unwrap();
        let docker_process = DockerContainer::minio(minio_port).create().await;
        let end_time = OffsetDateTime::now_utc();
        let duration = end_time - start_time;
        let service_minio_tc = match docker_process {
            Ok(process) => {
                minio_process = Some(process.clone());
                minio_endpoint = Some(format!("http://127.0.0.1:{minio_port}"));
                TestCase::success("service_minio", duration)
            }
            Err(e) => {
                failed = true;
                TestCase::failure(
                    "service_minio",
                    duration,
                    "service_minio",
                    e.to_string().as_str(),
                )
            }
        };
        ts_mandatory.add_testcase(service_minio_tc);
    }

    // Handle custom services.
    let mut daemon_children = Vec::new();
    for (name, command) in &test_args.custom_services {
        tracing::info!("│ {package_name:30.30}     │ Starting custom service '{name}'");
        let start_time = OffsetDateTime::now_utc();
        match Script::new(command, true)
            .name(format!("{package_name}::{name}"))
            .current_dir(&package_path)
            .maybe_env("DATABASE_URL", database_url.clone())
            .maybe_env(
                "AZURITE_BLOB_PORT",
                azurite_blob_port.map(|p| p.to_string()),
            )
            .maybe_env("S3_ENDPOINT", minio_endpoint.clone())
            .env("S3_ACCESS_KEY", "minioadmin")
            .env("S3_SECRET_ACCESS_KEY", "minioadmin")
            .log_stdout(tracing::Level::DEBUG)
            .log_stderr(tracing::Level::DEBUG)
            .spawn()
        {
            Ok(daemon) => daemon_children.push((name, daemon)),
            Err(err) => {
                failed = true;
                let duration = OffsetDateTime::now_utc() - start_time;
                ts_mandatory.add_testcase(TestCase::failure(
                    name,
                    duration,
                    "custom service",
                    &err.to_string(),
                ));
            }
        }
    }

    // Handle cache miss (this should be dropped and only use pre_test_script)
    if !failed && let Some(cache_miss_command) = &test_args.additional_cache_miss {
        tracing::info!("│ {package_name:30.30}     │ Running cache miss command");
        let start_time = OffsetDateTime::now_utc();
        let command_output = Script::new(cache_miss_command, true)
            .current_dir(&repo_root)
            .maybe_env("DATABASE_URL", database_url.clone())
            .execute()
            .await;
        let end_time = OffsetDateTime::now_utc();
        let duration = end_time - start_time;
        tracing::debug!("cache_miss: {}", command_output.stdout);
        let mut cache_miss_tc = match command_output.success {
            true => TestCase::success(cache_miss_command, duration),
            false => {
                failed = true;
                TestCase::failure(cache_miss_command, duration, "", "required")
            }
        };
        cache_miss_tc.set_system_out(&command_output.stderr);
        cache_miss_tc.set_system_err(&command_output.stdout);
        ts_mandatory.add_testcase(cache_miss_tc);
    }

    // Handle Pre-Test Script
    if !failed && let Some(pre_test_script) = test_args.pre_test_script.clone() {
        tracing::info!("│ {package_name:30.30}     │ Running pre-test script command");
        let start_time = OffsetDateTime::now_utc();
        let CommandOutput {
            stdout,
            stderr,
            success,
        } = Script::new(pre_test_script, true)
            .name(format!("{package_name}::pre_test_script"))
            .current_dir(&package_path)
            .maybe_env("DATABASE_URL", database_url.clone())
            .maybe_env(
                "AZURITE_BLOB_PORT",
                azurite_blob_port.map(|p| p.to_string()),
            )
            .maybe_env("S3_ENDPOINT", minio_endpoint.clone())
            .env("S3_ACCESS_KEY", "minioadmin")
            .env("S3_SECRET_ACCESS_KEY", "minioadmin")
            .log_stdout(tracing::Level::DEBUG)
            .log_stderr(tracing::Level::DEBUG)
            .execute()
            .await;
        let end_time = OffsetDateTime::now_utc();
        let duration = end_time - start_time;
        let mut pre_test_script_tx = match success {
            true => TestCase::success("pre_test_script", duration),
            false => {
                failed = true;
                TestCase::failure("pre_test_script", duration, "", "required")
            }
        };
        pre_test_script_tx.set_system_out(&stdout);
        pre_test_script_tx.set_system_err(&stderr);
        ts_mandatory.add_testcase(pre_test_script_tx);
    }
    // Handle Tests
    let test_command = &test_args.test_command;
    let additional_args = &test_args.additional_args;
    let fslabs_tests: Vec<_> = [
        FslabsTest {
            id: "cargo_fmt".to_string(),
            command: "cargo fmt --verbose -- --check".to_string(),
            ..Default::default()
        },
        // Needs to be done soon as the next one can update the lock file
        FslabsTest {
            id: "cargo_lock".to_string(),
            command: "fslabscli fix-lock-files --check".to_string(),
            ..Default::default()
        },
        FslabsTest {
            id: "cargo_check".to_string(),
            command: format!(
                "cargo check --all-targets {additional_args} {}",
                if common_options.inner_job_limit != 0 {
                    format!("--jobs {}", common_options.inner_job_limit)
                } else {
                    "".to_string()
                }
            ),
            ..Default::default()
        },
        FslabsTest {
            id: "cargo_clippy".to_string(),
            command: format!("cargo clippy --all-targets {additional_args} -- -D warnings"),
            ..Default::default()
        },
        FslabsTest {
            id: "cargo_doc".to_string(),
            // TODO: remove `+nightly ... -Z...` when stable rustdoc
            // isn't insanely slow anymore
            // see https://github.com/rust-lang/rust/issues/146895
            //
            // Paradoxically, we removed `--no-deps` to prevent an
            // issue where rustdoc just gets stuck on the last 10
            // packages and never makes progress.
            command: format!(
                "cargo +nightly doc -Zrustdoc-mergeable-info {}",
                if common_options.inner_job_limit != 0 {
                    format!("--jobs {}", common_options.inner_job_limit)
                } else {
                    "".to_string()
                }
            ),
            envs: HashMap::from([("RUSTDOCFLAGS".to_string(), "-D warnings".to_string())]),
            ..Default::default()
        },
        FslabsTest {
            id: "cargo_test".to_string(),
            command: if let Some(test_command) = test_command {
                format!("{test_command} {additional_args}")
            } else if use_nextest {
                format!(
                    "cargo nextest run --all-targets {additional_args} --profile {NEXTEST_PROFILE}{} --no-fail-fast --no-tests=pass {}",
                    nextest_tool_config
                        .as_ref()
                        .map(|p| format!(" --tool-config-file 'fslabscli:{}'", p.display()))
                        .unwrap_or_default(),
                    if common_options.inner_job_limit != 0 {
                        format!("--test-threads {}", common_options.inner_job_limit)
                    } else {
                        "".to_string()
                    }
                )
            } else {
                format!(
                    "cargo test --all-targets {additional_args} {}",
                    if common_options.inner_job_limit != 0 {
                        format!("--jobs {}", common_options.inner_job_limit)
                    } else {
                        "".to_string()
                    }
                )
            },
            pre_command: {
                let mut env_lines = Vec::new();

                if let Some(blob_port) = azurite_blob_port {
                    env_lines.push(format!("AZURITE_BLOB_PORT={blob_port}"));
                }
                if let Some(db_url) = &database_url {
                    env_lines.push(format!("DATABASE_URL={db_url}"));
                }
                if let Some(endpoint) = minio_endpoint.clone() {
                    env_lines.push(format!("S3_ENDPOINT={}", endpoint));
                    env_lines.push("S3_REGION=us-east-1".to_string());
                    env_lines.push("S3_BUCKET=test-bucket".to_string());
                    env_lines.push("S3_ACCESS_KEY_ID=minioadmin".to_string());
                    env_lines.push("S3_SECRET_ACCESS_KEY=minioadmin".to_string());
                }

                if !env_lines.is_empty() {
                    Some(format!("echo -e '{}' > .env", env_lines.join("\\n")))
                } else {
                    None
                }
            },
            post_command: if database_url.is_some() || minio_endpoint.is_some() {
                Some("rm .env".to_string())
            } else {
                None
            },
            parse_subtests: use_nextest,
            ..Default::default()
        },
    ]
    .iter()
    .cloned()
    .map(|mut t| {
        // Skip steps already handled by the workspace batch phase.
        if step_is_covered_by_batch(&batched_packages, workspace_name, package_name, package_version, &t.id) {
            t.skip = true;
        }
        // Let's check if the test need to be skip
        if should_skip_step(&t.id) {
            t.skip = true;
        }
        // Bazel covers serviceless crates in CI: skip cargo_test when the
        // package declares no services and the opt-in env var is set.
        if t.id == "cargo_test" && !t.skip && should_skip_serviceless_cargo_test(&test_args) {
            tracing::info!(
                "│ {package_name:30.30}     │ Skipping cargo_test: {SKIP_TESTS_WITHOUT_SERVICES_ENV}=true and package declares no services"
            );
            t.skip = true;
        }
        t
    })
    .collect();

    let test_steps = fslabs_tests.len();

    for (mut i, fslabs_test) in fslabs_tests.into_iter().enumerate() {
        i += 1;
        if fslabs_test.skip {
            continue;
        }
        let step_span = tracing::info_span!(
            "test_step",
            otel.name = format!("test_step: {}", fslabs_test.command),
            otel.status_code = tracing::field::Empty,
            step = %fslabs_test.id,
        );
        let _step_entered = step_span.enter();
        let tc_prefix = format!(
            "{package_name:30.30} {i}/{test_steps} │ {:50.50}",
            fslabs_test.command
        );
        if failed {
            tracing::info!("│ {} │ ⏭ SKIPPED", tc_prefix,);
            step_span.record("otel.status_code", "OK");

            metrics.test_duration_h.record(
                0.0,
                &[
                    KeyValue::new("workspace_name", workspace_name.clone()),
                    KeyValue::new("package_name", package_name.clone()),
                    KeyValue::new("package_version", package_version.clone()),
                    KeyValue::new("test_command", fslabs_test.command.clone()),
                    KeyValue::new("status", "SKIPPED"),
                ],
            );
            metrics.test_counter.add(
                1,
                &[
                    KeyValue::new("workspace_name", workspace_name.clone()),
                    KeyValue::new("package_name", package_name.clone()),
                    KeyValue::new("package_version", package_version.clone()),
                    KeyValue::new("test_command", fslabs_test.command.clone()),
                    KeyValue::new("status", "SKIPPED"),
                ],
            );
            let tc = TestCase::skipped(tc_prefix.as_str());
            match fslabs_test.optional {
                true => ts_optional.add_testcase(tc),
                false => ts_mandatory.add_testcase(tc),
            };
        } else {
            tracing::info!("│ {} │ ► START", tc_prefix,);
            let start_time = OffsetDateTime::now_utc();

            // Delete any stale JUnit report before this step. The annotations
            // parser reads that path whenever it exists; leaving a prior
            // run's file in place would let ghost annotations attach to
            // this run even when we invoked plain `cargo test` and never
            // regenerated it.
            if fslabs_test.id == "cargo_test" && nextest_junit_path.exists() {
                let _ = std::fs::remove_file(&nextest_junit_path);
            }

            if let Some(pre_command) = fslabs_test.pre_command {
                let pre_output = Script::new(&pre_command, true)
                    .current_dir(&package_path)
                    .envs(&fslabs_test.envs)
                    .execute()
                    .await;
                if !pre_output.success {
                    tracing::warn!(
                        "│ {} │ pre_command failed: {}",
                        &tc_prefix,
                        pre_output.stderr.trim_end()
                    );
                }
            }
            let test_output = match fslabs_test.id == "cargo_lock" {
                true => {
                    let cell = {
                        let mut cache = lock_check_cache.lock().await;
                        cache
                            .entry(workspace_path.clone())
                            .or_insert_with(|| Arc::new(OnceCell::new()))
                            .clone()
                    };
                    let result = cell
                        .get_or_init(|| async {
                            let r = fix_workspace_lockfile(
                                &repo_root,
                                &workspace_path,
                                &base_revspec,
                                true,
                            )
                            .unwrap_or_else(|e| e.into());
                            LockCheckResult {
                                success: r.success,
                                stdout: r.stdout,
                                stderr: r.stderr,
                            }
                        })
                        .await;
                    CommandOutput {
                        success: result.success,
                        stdout: result.stdout.clone(),
                        stderr: result.stderr.clone(),
                    }
                }

                false => {
                    Script::new(&fslabs_test.command, true)
                        .name(format!("{package_name}::test_command"))
                        .current_dir(&package_path)
                        .envs(&fslabs_test.envs)
                        .log_stdout(tracing::Level::DEBUG)
                        .log_stderr(tracing::Level::DEBUG)
                        .execute()
                        .await
                }
            };
            if let Some(post_command) = fslabs_test.post_command {
                let post_output = Script::new(&post_command, true)
                    .current_dir(&package_path)
                    .envs(&fslabs_test.envs)
                    .execute()
                    .await;
                if !post_output.success {
                    tracing::warn!(
                        "│ {} │ post_command failed: {}",
                        &tc_prefix,
                        post_output.stderr.trim_end()
                    );
                }
            }

            step_span.record(
                "otel.status_code",
                if test_output.success { "OK" } else { "ERROR" },
            );

            let end_time = OffsetDateTime::now_utc();
            let duration = end_time - start_time;

            if !test_output.success {
                let anns = parse_output_for(
                    &fslabs_test.id,
                    &test_output,
                    &ParseDirs {
                        workspace_dir: &workspace_path,
                        repo_root: &repo_root,
                        junit_path: Some(&nextest_junit_path),
                    },
                );
                if !anns.is_empty() {
                    annotation_collector.push(anns);
                }
            }

            let mut status = "PASS";
            let mut tc = match test_output.success {
                true => {
                    tracing::info!(
                        "│ {} │ 🟢 PASS in {}",
                        &tc_prefix,
                        duration.human(Truncate::Second)
                    );
                    TestCase::success(&tc_prefix, duration)
                }
                false => {
                    tracing::info!(
                        "│ {} │ 🟥 FAIL in {}",
                        &tc_prefix,
                        duration.human(Truncate::Second)
                    );
                    if !test_output.stderr.is_empty() {
                        tracing::warn!(
                            "│ {} │ stderr:\n{}",
                            &tc_prefix,
                            test_output.stderr.trim_end()
                        );
                    }
                    if !test_output.stdout.is_empty() {
                        tracing::warn!(
                            "│ {} │ stdout:\n{}",
                            &tc_prefix,
                            test_output.stdout.trim_end()
                        );
                    }
                    status = "FAIL";
                    failed = !fslabs_test.optional; // fail all if not optional
                    TestCase::failure(
                        &tc_prefix,
                        duration,
                        &fslabs_test.command,
                        if fslabs_test.optional {
                            "optional"
                        } else {
                            "required"
                        },
                    )
                }
            };

            if fslabs_test.parse_subtests && fslabs_test.id == "cargo_test" {
                // Parse and merge nextest JUnit XML if this is a cargo_test step with subtests
                if let Err(e) = merge_nextest_junit(
                    if fslabs_test.optional {
                        &mut ts_optional
                    } else {
                        &mut ts_mandatory
                    },
                    &nextest_junit_path,
                    &member,
                    i, // current_step - this is the cargo_test step number
                    test_steps,
                    &metrics,
                ) {
                    tracing::warn!("Failed to merge nextest JUnit results: {}", e);
                }
            } else {
                // Register metrics and tc for the all test
                metrics.test_duration_h.record(
                    duration.as_seconds_f64(),
                    &[
                        KeyValue::new("workspace_name", workspace_name.clone()),
                        KeyValue::new("package_name", package_name.clone()),
                        KeyValue::new("package_version", package_version.clone()),
                        KeyValue::new("test_command", fslabs_test.command.clone()),
                        KeyValue::new("status", status),
                    ],
                );
                metrics.test_counter.add(
                    1,
                    &[
                        KeyValue::new("workspace_name", workspace_name.clone()),
                        KeyValue::new("package_name", package_name.clone()),
                        KeyValue::new("package_version", package_version.clone()),
                        KeyValue::new("test_command", fslabs_test.command.clone()),
                        KeyValue::new("status", status),
                    ],
                );
                tc.set_system_out(&test_output.stderr);
                tc.set_system_err(&test_output.stdout);
                match fslabs_test.optional {
                    true => ts_optional.add_testcase(tc),
                    false => ts_mandatory.add_testcase(tc),
                };
            }
        }
    }

    // Tear down docker containers
    if let Some(process) = postgres_process {
        tracing::info!("│ {package_name:30.30}     │ Tearing down service database");
        process.teardown().await;
    }
    if let Some(process) = azurite_process {
        tracing::info!("│ {package_name:30.30}     │ Tearing down service azurite");
        process.teardown().await;
    }
    if let Some(process) = minio_process {
        tracing::info!("│ {package_name:30.30}     │ Tearing down service minio");
        process.teardown().await;
    }
    // Tear down custom services.
    for (name, daemon) in daemon_children {
        tracing::info!("│ {package_name:30.30}     │ Tearing down custom service {name}");
        if let Err(err) = daemon.kill().await {
            tracing::error!("Failed to kill custom service {name} failed: {err}");
        }
    }

    junit_report.add_testsuite(ts_mandatory);
    junit_report.add_testsuite(ts_optional);

    let member_end_time = OffsetDateTime::now_utc();
    let member_duration = member_end_time - member_start_time;
    let attributes = [
        KeyValue::new("workspace_name", workspace_name.clone()),
        KeyValue::new("package_name", package_name.clone()),
        KeyValue::new("package_version", package_version.clone()),
        KeyValue::new("success", !failed),
    ];
    metrics
        .member_duration_h
        .record(member_duration.as_seconds_f64(), &attributes);
    metrics.member_counter.add(1, &attributes);
    metrics
        .common_member_duration_h
        .record(member_duration.as_seconds_f64(), &attributes);
    metrics.common_member_counter.add(1, &attributes);
    tracing::Span::current().record("otel.status_code", if failed { "ERROR" } else { "OK" });
    (failed, junit_report)
}

/// Return true if a step for `(workspace, package, version)` is already
/// covered by the workspace-level batch phase and should be skipped in the
/// per-package loop. Keying the batched set on the full tuple (rather than
/// just the package name) prevents a batched `foo@0.1.0` in workspace W1
/// from silently skipping an un-batched `foo@0.2.0` in workspace W2.
fn step_is_covered_by_batch(
    batched_packages: &HashSet<(String, String, String)>,
    workspace: &str,
    package: &str,
    version: &str,
    step_id: &str,
) -> bool {
    if !matches!(
        step_id,
        "cargo_fmt" | "cargo_lock" | "cargo_check" | "cargo_clippy" | "cargo_doc"
    ) {
        return false;
    }
    let key = (
        workspace.to_string(),
        package.to_string(),
        version.to_string(),
    );
    batched_packages.contains(&key)
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    /// Serialises all tests that mutate the process environment: setenv and
    /// getenv are not thread-safe and cargo runs tests in parallel within one
    /// process.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Helper: sets an env var, runs the closure, then removes the var.
    /// Keeps the unsafe surface small and centralised.
    fn with_env_var(key: &str, value: &str, f: impl FnOnce()) {
        let _guard = lock_env();
        unsafe { env::set_var(key, value) };
        f();
        unsafe { env::remove_var(key) };
    }

    /// Helper: ensures an env var is absent, runs the closure, then cleans up.
    fn without_env_var(key: &str, f: impl FnOnce()) {
        let _guard = lock_env();
        unsafe { env::remove_var(key) };
        f();
    }

    #[test]
    fn should_not_skip_when_env_var_is_absent() {
        without_env_var("SKIP_MYFAKESTEP_TEST", || {
            assert!(!should_skip_step("myfakestep"));
        });
    }

    #[test]
    fn should_skip_when_env_var_is_true() {
        with_env_var("SKIP_SKIPME_TEST", "true", || {
            assert!(should_skip_step("skipme"));
        });
    }

    #[test]
    fn should_not_skip_when_env_var_is_false() {
        with_env_var("SKIP_KEEPME_TEST", "false", || {
            assert!(!should_skip_step("keepme"));
        });
    }

    #[test]
    fn should_not_skip_when_env_var_is_other_value() {
        with_env_var("SKIP_OTHERSTEP_TEST", "yes", || {
            assert!(!should_skip_step("otherstep"));
        });
    }

    #[test]
    fn should_uppercase_step_id_for_env_var_lookup() {
        // step id "cargo_fmt" should check "SKIP_CARGO_FMT_TEST"
        with_env_var("SKIP_CARGO_FMT_TEST", "true", || {
            assert!(should_skip_step("cargo_fmt"));
        });
    }

    #[test]
    fn should_handle_mixed_case_step_id() {
        // step id "MyStep" should check "SKIP_MYSTEP_TEST"
        with_env_var("SKIP_MYSTEP_TEST", "true", || {
            assert!(should_skip_step("MyStep"));
        });
    }

    #[test]
    fn should_not_skip_when_env_var_is_empty_string() {
        with_env_var("SKIP_EMPTYSTEP_TEST", "", || {
            assert!(!should_skip_step("emptystep"));
        });
    }

    fn batched_key(ws: &str, pkg: &str, ver: &str) -> (String, String, String) {
        (ws.to_string(), pkg.to_string(), ver.to_string())
    }

    #[test]
    fn batch_check_skips_batched_step_for_exact_tuple_match() {
        let mut set = HashSet::new();
        set.insert(batched_key("ws1", "foo", "0.1.0"));
        assert!(step_is_covered_by_batch(
            &set,
            "ws1",
            "foo",
            "0.1.0",
            "cargo_fmt"
        ));
        assert!(step_is_covered_by_batch(
            &set,
            "ws1",
            "foo",
            "0.1.0",
            "cargo_clippy"
        ));
    }

    #[test]
    fn batch_check_does_not_skip_when_version_differs() {
        // Regression: batched foo@0.1.0 must not cause un-batched foo@0.2.0 to
        // be skipped. This is the vendored-baseline scenario the review
        // flagged as silent test coverage loss.
        let mut set = HashSet::new();
        set.insert(batched_key("ws1", "foo", "0.1.0"));
        assert!(!step_is_covered_by_batch(
            &set,
            "ws1",
            "foo",
            "0.2.0",
            "cargo_fmt"
        ));
    }

    #[test]
    fn batch_check_does_not_skip_when_workspace_differs() {
        // Same-name package in a different workspace also must not be skipped.
        let mut set = HashSet::new();
        set.insert(batched_key("ws1", "foo", "0.1.0"));
        assert!(!step_is_covered_by_batch(
            &set,
            "ws2",
            "foo",
            "0.1.0",
            "cargo_fmt"
        ));
    }

    #[test]
    fn batch_check_ignores_non_batched_step_ids() {
        // cargo_test is never covered by the batch phase, so it must run
        // per-package regardless of whether the package was batched.
        let mut set = HashSet::new();
        set.insert(batched_key("ws1", "foo", "0.1.0"));
        assert!(!step_is_covered_by_batch(
            &set,
            "ws1",
            "foo",
            "0.1.0",
            "cargo_test"
        ));
    }

    #[test]
    fn batch_check_returns_false_for_empty_set() {
        let set = HashSet::new();
        assert!(!step_is_covered_by_batch(
            &set,
            "ws1",
            "foo",
            "0.1.0",
            "cargo_fmt"
        ));
    }

    #[test]
    fn serviceless_should_not_skip_when_env_var_is_absent() {
        without_env_var(SKIP_TESTS_WITHOUT_SERVICES_ENV, || {
            assert!(!should_skip_serviceless_cargo_test(&TestArgs::default()));
        });
    }

    #[test]
    fn serviceless_should_skip_when_env_var_is_true_and_no_services() {
        with_env_var(SKIP_TESTS_WITHOUT_SERVICES_ENV, "true", || {
            assert!(should_skip_serviceless_cargo_test(&TestArgs::default()));
        });
    }

    #[test]
    fn serviceless_should_not_skip_when_env_var_is_false() {
        with_env_var(SKIP_TESTS_WITHOUT_SERVICES_ENV, "false", || {
            assert!(!should_skip_serviceless_cargo_test(&TestArgs::default()));
        });
    }

    #[test]
    fn serviceless_should_not_skip_when_package_declares_a_service() {
        with_env_var(SKIP_TESTS_WITHOUT_SERVICES_ENV, "true", || {
            let mut test_args = TestArgs::default();
            test_args.services.postgres = true;
            assert!(!should_skip_serviceless_cargo_test(&test_args));
        });
    }

    #[test]
    fn serviceless_should_not_skip_when_package_declares_a_custom_service() {
        with_env_var(SKIP_TESTS_WITHOUT_SERVICES_ENV, "true", || {
            let mut test_args = TestArgs::default();
            test_args
                .custom_services
                .insert("mock-server".to_string(), "run-mock-server".to_string());
            assert!(!should_skip_serviceless_cargo_test(&test_args));
        });
    }

    #[test]
    fn serviceless_should_not_skip_when_package_has_pre_service_script() {
        with_env_var(SKIP_TESTS_WITHOUT_SERVICES_ENV, "true", || {
            let test_args = TestArgs {
                pre_service_script: Some("./setup.sh".to_string()),
                ..Default::default()
            };
            assert!(!should_skip_serviceless_cargo_test(&test_args));
        });
    }

    #[test]
    fn serviceless_should_not_skip_when_package_has_pre_test_script() {
        with_env_var(SKIP_TESTS_WITHOUT_SERVICES_ENV, "true", || {
            let test_args = TestArgs {
                pre_test_script: Some("./fixtures.sh".to_string()),
                ..Default::default()
            };
            assert!(!should_skip_serviceless_cargo_test(&test_args));
        });
    }

    #[test]
    fn serviceless_should_not_skip_when_package_has_custom_test_command() {
        with_env_var(SKIP_TESTS_WITHOUT_SERVICES_ENV, "true", || {
            let test_args = TestArgs {
                test_command: Some("wasm-pack test".to_string()),
                ..Default::default()
            };
            assert!(!should_skip_serviceless_cargo_test(&test_args));
        });
    }
}

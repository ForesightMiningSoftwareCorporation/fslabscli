mod docker_service;

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
    collections::HashMap,
    env,
    fmt::{Display, Formatter},
    fs::{File, create_dir_all, remove_dir_all},
    io::Write,
    path::PathBuf,
    sync::Arc,
};
use tokio::sync::Semaphore;

use crate::{
    PackageRelatedOptions, PrettyPrintable,
    command_ext::{CommandOutput, Script},
    commands::{
        check_workspace::{Options as CheckWorkspaceOptions, check_workspace},
        fix_lock_files::fix_workspace_lockfile,
        tests::docker_service::{DockerContainer, postgres_url},
    },
    init_metrics,
};

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
    #[arg(long)]
    run_all: bool,
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

/// Count the number of test cases using `cargo nextest list`
async fn count_nextest_tests(package_path: &PathBuf) -> usize {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct NextestList {
        #[serde(rename = "test-count")]
        test_count: usize,
    }

    let output = tokio::process::Command::new("cargo")
        .arg("nextest")
        .arg("list")
        .arg("--message-format")
        .arg("json")
        .current_dir(package_path)
        .output()
        .await;

    let Ok(output) = output else {
        return 0;
    };

    if !output.status.success() {
        return 0;
    }

    let Ok(json_str) = String::from_utf8(output.stdout) else {
        return 0;
    };

    // Parse the JSON output
    match serde_json::from_str::<NextestList>(&json_str) {
        Ok(list) => list.test_count,
        Err(_) => 0,
    }
}

fn merge_nextest_junit(
    testsuite: &mut junit_report::TestSuite,
    junit_path: &PathBuf,
    package_name: &str,
    current_step: usize,
    total_steps: usize,
) -> anyhow::Result<()> {
    use quick_xml::de::from_str;

    if !junit_path.exists() {
        tracing::debug!("Nextest JUnit file not found at {:?}", junit_path);
        return Ok(());
    }

    let xml_content = std::fs::read_to_string(junit_path)?;

    // Nextest generates: <testsuites><testsuite><testcase/></testsuite></testsuites>
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
                        "{:30.30} {}/{} │ {}",
                        package_name, subtest_num, total_steps, test_case.name
                    );

                    let tc = if let Some(failure) = test_case.failure {
                        TestCase::failure(
                            &test_name,
                            duration,
                            "test",
                            &format!("{}\n{}", failure.message, failure.text),
                        )
                    } else if test_case.skipped.is_some() {
                        TestCase::skipped(&test_name)
                    } else {
                        TestCase::success(&test_name, duration)
                    };

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
    let base_rev = common_options.base_rev.as_deref().unwrap_or("HEAD~");
    // Get Directory information
    tracing::info!("Running the tests with the following arguments:");
    tracing::info!("* `check_changed`: true");
    tracing::info!("* `check_publish`: false");
    tracing::info!("* `changed_head_rev`: {}", common_options.head_rev);
    tracing::info!("* `changed_base_rev`: {:?}", base_rev);
    tracing::info!("* `whitelist`: {}", common_options.whitelist.join(","));
    tracing::info!("* `blacklist`: {}", common_options.blacklist.join(","));

    let check_workspace_options = CheckWorkspaceOptions::new()
        .with_check_changed(!options.run_all)
        .with_check_publish(false);

    let results = check_workspace(common_options, &check_workspace_options, repo_root.clone())
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
    let mut handles = vec![];

    for (_, member) in results.members.into_iter().filter(|(_, member)| {
        !member.test_detail.skip.unwrap_or_default() && (member.perform_test || options.run_all)
    }) {
        let common_opts = Arc::new(common_options.clone());
        let task_handle = tokio::spawn(do_test_on_package(
            common_opts,
            repo_root.clone(),
            member,
            metrics.clone(),
            semaphore.clone(),
        ));
        handles.push(task_handle);
    }

    // using `select_all` to allow fast failure
    while !handles.is_empty() {
        let (result, _, remaining) = futures::future::select_all(handles).await;
        handles = remaining;
        if let Ok((failed, junit_report)) = result {
            global_junit_report.add_testsuites(junit_report.testsuites().clone());
            global_failed |= failed;
            if failed {
                break;
            }
        } else {
            global_failed = true;
            break;
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
            Ok(TestResult {})
        }
        true => {
            overall_duration_h.record(
                overall_duration.as_seconds_f64(),
                &[KeyValue::new("status", "failed")],
            );
            overall_counter.add(1, &[KeyValue::new("success", false)]);
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

async fn do_test_on_package(
    common_options: Arc<PackageRelatedOptions>,
    repo_root: PathBuf,
    member: super::check_workspace::Result,
    metrics: Metrics,
    semaphore: Arc<Semaphore>,
) -> (bool, Report) {
    let permit = semaphore.acquire().await;

    let mut junit_report = ReportBuilder::new().build();
    let mut failed = false;

    let member_start_time = OffsetDateTime::now_utc();
    let workspace_name = member.workspace;
    let package_name = member.package;
    let package_version = member.version;
    let package_path = repo_root.join(member.path);
    let test_args = member.test_detail.args.unwrap_or_default();
    let base_rev = common_options.base_rev.as_deref().unwrap_or("HEAD~");
    let use_nextest = has_cargo_nextest().await;
    let nextest_junit_path = package_path.join("target/nextest/default/junit.xml");
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

    // Handle service database
    if !failed && test_args.service_database {
        tracing::info!("│ {:30.30}     │ Setting up service database", package_name);
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
    if !failed && test_args.service_azurite {
        tracing::info!("│ {:30.30}     │ Setting up service azurite", package_name);
        let start_time = OffsetDateTime::now_utc();
        let docker_process = DockerContainer::azurite().create().await;
        let end_time = OffsetDateTime::now_utc();
        let duration = end_time - start_time;
        let service_azurite_tc = match docker_process {
            Ok(process) => {
                azurite_process = Some(process);
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
    if !failed && test_args.service_minio {
        tracing::info!("│ {:30.30}     │ Setting up service minio", package_name);
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

    // Handle cache miss (this should be dropped and only use pre_test_script)
    if !failed && let Some(cache_miss_command) = &test_args.additional_cache_miss {
        tracing::info!("│ {:30.30}     │ Running cache miss command", package_name);
        let start_time = OffsetDateTime::now_utc();
        let mut envs: HashMap<String, String> = HashMap::new();
        if let Some(db_url) = database_url.clone() {
            envs.insert("DATABASE_URL".to_string(), db_url.clone());
        }
        let command_output = Script::new(cache_miss_command)
            .current_dir(&repo_root)
            .envs(&envs)
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
        tracing::info!(
            "│ {:30.30}     │ Running pre-test script command",
            package_name
        );
        let start_time = OffsetDateTime::now_utc();
        let mut script = Script::new(pre_test_script);
        if let Some(url) = &database_url {
            script = script.env("DATABASE_URL", url);
        }
        let CommandOutput {
            stdout,
            stderr,
            success,
        } = script.execute().await;
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
    let additional_args = &test_args.additional_args;
    let fslabs_tests: Vec<FslabsTest> = vec![
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
            command: format!(
                "cargo doc --no-deps {}",
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
            command: if use_nextest {
                format!(
                    "cargo nextest run --all-targets {additional_args} --profile default --no-fail-fast --no-tests pass {}",
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

                if let Some(db_url) = database_url.clone() {
                    env_lines.push(format!("DATABASE_URL={}", db_url));
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
        // Let's check if the test need to be skip
        let skip_env = format!("SKIP_{}_TEST", t.id).to_uppercase();
        if let Ok(skip) = env::var(skip_env) {
            t.skip = skip == "true";
        }
        t
    })
    .collect();

    // Count nextest subtests if available to adjust total step count
    let nextest_subtest_count = if use_nextest {
        count_nextest_tests(&package_path).await
    } else {
        0
    };
    let test_steps = fslabs_tests.len() + nextest_subtest_count;

    for (mut i, fslabs_test) in fslabs_tests.into_iter().enumerate() {
        i += 1;
        if fslabs_test.skip {
            continue;
        }
        let tc_prefix = format!(
            "{:30.30} {i}/{test_steps} │ {:50.50}",
            package_name, fslabs_test.command
        );
        if failed {
            tracing::info!("│ {} │ ⏭ SKIPPED", tc_prefix,);

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

            // Setup nextest configuration if this is the cargo_test step and nextest is available
            if fslabs_test.id == "cargo_test" && use_nextest {
                let config_dir = package_path.join(".config");
                let config_file = config_dir.join("nextest.toml");

                if let Err(e) = create_dir_all(&config_dir) {
                    tracing::warn!("Failed to create .config directory: {}", e);
                } else if let Ok(mut file) = File::create(&config_file) {
                    let config_content = "[profile.default.junit]\npath = \"junit.xml\"\n";
                    if let Err(e) = file.write_all(config_content.as_bytes()) {
                        tracing::warn!("Failed to write nextest config: {}", e);
                    }
                }
            }

            if let Some(pre_command) = fslabs_test.pre_command {
                Script::new(&pre_command)
                    .current_dir(&package_path)
                    .envs(&fslabs_test.envs)
                    .execute()
                    .await;
            }
            let test_output = match fslabs_test.id == "cargo_lock" {
                true => fix_workspace_lockfile(
                    &repo_root,
                    &package_path,
                    base_rev.to_string(),
                    None,
                    true,
                )
                .unwrap_or_else(|e| e.into()),

                false => {
                    Script::new(&fslabs_test.command)
                        .current_dir(&package_path)
                        .envs(&fslabs_test.envs)
                        .log_stdout(tracing::Level::DEBUG)
                        .log_stderr(tracing::Level::DEBUG)
                        .execute()
                        .await
                }
            };
            if let Some(post_command) = fslabs_test.post_command {
                Script::new(&post_command)
                    .current_dir(&package_path)
                    .envs(&fslabs_test.envs)
                    .execute()
                    .await;
            }

            // Cleanup nextest configuration if this is the cargo_test step and nextest was used
            if fslabs_test.id == "cargo_test" && use_nextest {
                let config_dir = package_path.join(".config");
                if let Err(e) = remove_dir_all(&config_dir) {
                    tracing::debug!("Failed to cleanup .config directory: {}", e);
                }
            }

            let end_time = OffsetDateTime::now_utc();
            let duration = end_time - start_time;

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

            // Parse and merge nextest JUnit XML if this is a cargo_test step with subtests
            if fslabs_test.parse_subtests
                && fslabs_test.id == "cargo_test"
                && let Err(e) = merge_nextest_junit(
                    if fslabs_test.optional {
                        &mut ts_optional
                    } else {
                        &mut ts_mandatory
                    },
                    &nextest_junit_path,
                    &package_name,
                    i, // current_step - this is the cargo_test step number
                    test_steps,
                )
            {
                tracing::warn!("Failed to merge nextest JUnit results: {}", e);
            }
        }
    }

    // Tear down docker containers
    if let Some(process) = postgres_process {
        tracing::info!(
            "│ {:30.30}     │ Tearing down service database",
            package_name
        );
        process.teardown().await;
    }
    if let Some(process) = azurite_process {
        tracing::info!(
            "│ {:30.30}     │ Tearing down service azurite",
            package_name
        );
        process.teardown().await;
    }
    if let Some(process) = minio_process {
        tracing::info!("│ {:30.30}     │ Tearing down service minio", package_name);
        process.teardown().await;
    }
    junit_report.add_testsuite(ts_mandatory);
    junit_report.add_testsuite(ts_optional);

    let member_end_time = OffsetDateTime::now_utc();
    let member_duration = member_end_time - member_start_time;
    metrics.member_duration_h.record(
        member_duration.as_seconds_f64(),
        &[
            KeyValue::new("workspace_name", workspace_name.clone()),
            KeyValue::new("package_name", package_name.clone()),
            KeyValue::new("package_version", package_version.clone()),
            KeyValue::new("success", !failed),
        ],
    );
    metrics.member_counter.add(
        1,
        &[
            KeyValue::new("workspace_name", workspace_name.clone()),
            KeyValue::new("package_name", package_name.clone()),
            KeyValue::new("package_version", package_version.clone()),
            KeyValue::new("success", !failed),
        ],
    );
    metrics.common_member_duration_h.record(
        member_duration.as_seconds_f64(),
        &[
            KeyValue::new("workspace_name", workspace_name.clone()),
            KeyValue::new("package_name", package_name.clone()),
            KeyValue::new("package_version", package_version.clone()),
            KeyValue::new("success", !failed),
        ],
    );
    metrics.common_member_counter.add(
        1,
        &[
            KeyValue::new("workspace_name", workspace_name.clone()),
            KeyValue::new("package_name", package_name.clone()),
            KeyValue::new("package_version", package_version.clone()),
            KeyValue::new("success", !failed),
        ],
    );
    drop(permit);
    // drop(package_span);
    (failed, junit_report)
}

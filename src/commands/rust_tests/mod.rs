use anyhow::Context;
use clap::Parser;
use indexmap::IndexMap;
use junit_report::{OffsetDateTime, ReportBuilder, TestCase, TestSuiteBuilder};
use opentelemetry::{KeyValue, global};
use port_check::free_local_port;
use rand::distr::{Alphanumeric, SampleString};
use serde::Serialize;
use serde_yaml::Value;
use std::{
    collections::HashMap,
    env,
    fmt::{Display, Formatter},
    fs::File,
    path::PathBuf,
    thread::sleep,
    time::Duration,
};

use crate::{
    PrettyPrintable,
    commands::check_workspace::{Options as CheckWorkspaceOptions, check_workspace},
    utils::{execute_command, execute_command_without_logging},
};

static DB_PASSWORD: &str = "mypassword";
static DB_NAME: &str = "tests";

#[derive(Debug, Parser, Default)]
#[command(about = "Run rust tests")]
pub struct Options {
    #[clap(long, env, default_value = ".")]
    artifacts: PathBuf,
    #[clap(long, env, default_value = "HEAD")]
    pull_pull_sha: String,
    #[clap(long, env, default_value = "HEAD~")]
    pull_base_sha: String,
    #[clap(
        long,
        env,
        default_value = "https://raw.githubusercontent.com/ForesightMiningSoftwareCorporation/github/main/deny.toml"
    )]
    default_deny_location: String,
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

#[derive(Default)]
struct FslabsTest {
    pub optional: bool,
    pub command: String,
    pub pre_command: Option<String>,
    pub post_command: Option<String>,
    pub envs: HashMap<String, String>,
}

async fn teardown_container(container_id: String) {
    let path = env::current_dir().unwrap();
    let envs: HashMap<String, String> = HashMap::default();
    execute_command_without_logging(&format!("docker stop {container_id}"), &path, &envs).await;
    execute_command_without_logging(&format!("docker rm {container_id}"), &path, &envs).await;
}

async fn create_docker_container(
    prefix: String,
    env: String,
    port: String,
    options: String,
    image: String,
) -> anyhow::Result<String> {
    let suffix = Alphanumeric.sample_string(&mut rand::rng(), 6);
    let container_name = format!("{prefix}_{suffix}");
    let path = env::current_dir().unwrap();
    let envs: HashMap<String, String> = HashMap::default();
    let (_, stderr, success) = execute_command_without_logging(
        &format!("docker run --name={container_name} -d {env} {port} {options} {image}"),
        &path,
        &envs,
    )
    .await;
    if !success {
        return Err(anyhow::anyhow!(stderr));
    }
    // Wait 5 Sec
    sleep(Duration::from_millis(5000));
    let (container_id, stderr, success) = execute_command_without_logging(
        &format!("docker ps -q -f name={container_name}"),
        &path,
        &envs,
    )
    .await;
    if !success {
        return Err(anyhow::anyhow!(stderr));
    }
    Ok(container_id)
}

fn get_test_arg(test_args: &IndexMap<String, Value>, arg: &str) -> Option<String> {
    test_args
        .get(arg)
        .and_then(|v| v.as_str().map(|s| s.to_string()))
}

fn get_test_arg_bool(test_args: &IndexMap<String, Value>, arg: &str) -> Option<bool> {
    test_args.get(arg).and_then(|v| v.as_bool())
}

pub async fn rust_tests(options: Box<Options>, repo_root: PathBuf) -> anyhow::Result<TestResult> {
    let meter = global::meter("rust_tests");
    let overall_duration_h = meter.f64_histogram("rust_tests_workspace").build();
    let overall_counter = meter.u64_counter("rust_tests_workspace").build();
    let member_duration_h = meter.f64_histogram("rust_tests_member").build();
    let member_counter = meter.u64_counter("rust_tests_member").build();
    let test_duration_h = meter.f64_histogram("rust_tests_test").build();
    let test_counter = meter.u64_counter("rust_tests_test").build();
    let overall_start_time = OffsetDateTime::now_utc();
    // Get Directory information
    tracing::info!("Running the tests with the following arguments:");
    tracing::info!("* `check_changed`: true");
    tracing::info!("* `check_publish`: false");
    tracing::info!("* `changed_head_ref`: {}", options.pull_pull_sha);
    tracing::info!("* `changed_base_ref`: {}", options.pull_base_sha);

    let check_workspace_options = CheckWorkspaceOptions::new()
        .with_check_changed(true)
        .with_check_publish(false)
        .with_changed_head_ref(options.pull_pull_sha)
        .with_changed_base_ref(options.pull_base_sha);

    let results = check_workspace(Box::new(check_workspace_options), repo_root.clone())
        .await
        .map_err(|e| {
            tracing::error!("Check directory for crates that need publishing: {}", e);
            e
        })
        .with_context(|| "Could not get directory information")?;

    let mut junit_report = ReportBuilder::new().build();

    // Global fail tracker
    let mut failed = false;

    for (_, member) in results.members {
        let member_start_time = OffsetDateTime::now_utc();
        let workspace_name = member.workspace;
        let package_name = member.package;
        let package_version = member.version;
        let package_path = repo_root.join(member.path);
        let test_args = member.test_detail.args.unwrap_or_default();
        let additional_args = get_test_arg(&test_args, "additional_args").unwrap_or_default();
        let mut service_database_container_id: Option<String> = None;
        let mut database_url: Option<String> = None;
        let mut service_azurite_container_id: Option<String> = None;

        if failed || member.test_detail.skip.unwrap_or_default() || !member.perform_test {
            continue;
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
        if !failed && get_test_arg_bool(&test_args, "service_database") == Some(true) {
            tracing::info!("Setting up service database");
            let start_time = OffsetDateTime::now_utc();
            let pg_port = free_local_port().unwrap();
            let service_db_container = create_docker_container(
                "postgres".to_string(),
                format!("-e POSTGRES_PASSWORD={DB_PASSWORD} -e POSTGRES_DB={DB_NAME}"),
                format!("-p {pg_port}:5432"),
                "".to_string(),
                "postgres:alpine".to_string(),
            )
            .await;
            let end_time = OffsetDateTime::now_utc();
            let duration = end_time - start_time;
            let service_db_tc = match service_db_container {
                Ok(container_id) => {
                    service_database_container_id = Some(container_id);
                    database_url = Some(format!(
                        "postgres://postgres:{DB_PASSWORD}@localhost:{pg_port}/{DB_NAME}"
                    ));
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
        if !failed && get_test_arg_bool(&test_args, "service_azurite") == Some(true) {
            tracing::info!("Setting up service azurite");
            let start_time = OffsetDateTime::now_utc();
            let azurite_container = create_docker_container(
                "azurite".to_string(),
                "".to_string(),
                "-p 10000:10000 -p 10001:10001 -p 10002:10002".to_string(),
                "".to_string(),
                "mcr.microsoft.com/azure-storage/azurite".to_string(),
            )
            .await;
            let end_time = OffsetDateTime::now_utc();
            let duration = end_time - start_time;
            let service_azurite_tc = match azurite_container {
                Ok(container_id) => {
                    service_azurite_container_id = Some(container_id);
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

        // Handle cache miss (this should be dropped and only additional script)
        if !failed {
            if let Some(cache_miss_command) = get_test_arg(&test_args, "additional_cache_miss") {
                tracing::info!("Running cache miss command");
                let start_time = OffsetDateTime::now_utc();
                let mut envs: HashMap<String, String> = HashMap::new();
                if let Some(db_url) = database_url.clone() {
                    envs.insert("DATABASE_URL".to_string(), db_url.clone());
                }
                let (stdout, stderr, success) =
                    execute_command_without_logging(&cache_miss_command, &package_path, &envs)
                        .await;
                let end_time = OffsetDateTime::now_utc();
                let duration = end_time - start_time;
                tracing::debug!("cache_miss: {stdout}");
                let cache_miss_tc = match success {
                    true => TestCase::success(&cache_miss_command, duration),
                    false => {
                        failed = true;
                        TestCase::failure(
                            &cache_miss_command,
                            duration,
                            "cache_miss",
                            stderr.as_str(),
                        )
                    }
                };
                ts_mandatory.add_testcase(cache_miss_tc);
            }
        }

        // Handle Additional Script
        if !failed {
            if let Some(additional_scripts) = get_test_arg(&test_args, "additional_script") {
                tracing::info!("Running additional script command");
                let start_time = OffsetDateTime::now_utc();
                let mut envs: HashMap<String, String> = HashMap::new();
                if let Some(db_url) = database_url.clone() {
                    envs.insert("DATABASE_URL".to_string(), db_url.clone());
                }
                let mut sub_fail: Option<String> = None;
                for line in additional_scripts.split("\n") {
                    if line.is_empty() {
                        continue;
                    }
                    if sub_fail.is_some() {
                        continue;
                    }
                    let (stdout, stderr, success) =
                        execute_command_without_logging(line, &package_path, &envs).await;
                    tracing::debug!("additional_script: {line} {stdout}");
                    if !success {
                        sub_fail = Some(stderr);
                    }
                }
                let end_time = OffsetDateTime::now_utc();
                let duration = end_time - start_time;
                let additional_script_tc = match sub_fail {
                    None => TestCase::success("additional_script", duration),
                    Some(stderr) => {
                        failed = true;
                        TestCase::failure(
                            "additional_script",
                            duration,
                            "additional_script",
                            stderr.as_str(),
                        )
                    }
                };
                ts_mandatory.add_testcase(additional_script_tc);
            }
        }
        // Handle Tests
        let fslabs_tests: Vec<FslabsTest> = vec![
            FslabsTest {
                command: "cargo fmt --verbose -- --check".to_string(),
                ..Default::default()
            },
            FslabsTest {
                command: format!("cargo check --all-targets {additional_args}"),
                ..Default::default()
            },
            FslabsTest {
                command: format!("cargo clippy --all-targets {additional_args} -- -D warnings"),
                ..Default::default()
            },
            FslabsTest {
                command: "cargo doc --no-deps".to_string(),
                envs: HashMap::from([("RUSTDOCFLAGS".to_string(), "-D warnings".to_string())]),
                ..Default::default()
            },
            FslabsTest {
                command: format!("cargo test --all-targets {additional_args}"),
                pre_command: database_url
                    .clone()
                    .map(|d| format!("echo DATABASE_URL={d} > .env")),
                post_command: database_url.clone().map(|_| "rm .env".to_string()),
                ..Default::default()
            },
        ];

        for fslabs_test in fslabs_tests {
            if failed {
                tracing::info!(
                    "╭────────────────────────────────────────────────────────────────╮"
                );
                tracing::info!("│ {:60}   │", fslabs_test.command);
                tracing::info!(
                    "╰─┬──────────────────────────────────────────────────────────────╯"
                );
                tracing::info!("  ╰───────⏵ ⏭ SKIPPED");

                test_duration_h.record(
                    0.0,
                    &[
                        KeyValue::new("workspace_name", workspace_name.clone()),
                        KeyValue::new("package_name", package_name.clone()),
                        KeyValue::new("package_version", package_version.clone()),
                        KeyValue::new("test_command", fslabs_test.command.clone()),
                        KeyValue::new("status", "SKIPPED"),
                    ],
                );
                test_counter.add(
                    1,
                    &[
                        KeyValue::new("workspace_name", workspace_name.clone()),
                        KeyValue::new("package_name", package_name.clone()),
                        KeyValue::new("package_version", package_version.clone()),
                        KeyValue::new("test_command", fslabs_test.command.clone()),
                        KeyValue::new("status", "SKIPPED"),
                    ],
                );
                let tc = TestCase::skipped(fslabs_test.command.as_str());
                match fslabs_test.optional {
                    true => ts_optional.add_testcase(tc),
                    false => ts_mandatory.add_testcase(tc),
                };
            } else {
                tracing::info!(
                    "╭────────────────────────────────────────────────────────────────╮"
                );
                tracing::info!("│ {:60}   │", fslabs_test.command);
                tracing::info!(
                    "╰─┬──────────────────────────────────────────────────────────────╯"
                );
                let start_time = OffsetDateTime::now_utc();
                if let Some(pre_command) = fslabs_test.pre_command {
                    execute_command_without_logging(&pre_command, &package_path, &fslabs_test.envs)
                        .await;
                }
                let (stdout, stderr, success) = execute_command(
                    &fslabs_test.command,
                    &package_path,
                    &fslabs_test.envs,
                    Some(tracing::Level::DEBUG),
                    Some(tracing::Level::DEBUG),
                )
                .await;
                if let Some(post_command) = fslabs_test.post_command {
                    execute_command_without_logging(
                        &post_command,
                        &package_path,
                        &fslabs_test.envs,
                    )
                    .await;
                }
                let end_time = OffsetDateTime::now_utc();
                let duration = end_time - start_time;

                let mut status = "PASS";
                let mut tc = match success {
                    true => {
                        tracing::info!("  ╰───────⏵ 🟢 PASS in {}", duration);
                        tracing::info!("");
                        TestCase::success(&fslabs_test.command, duration)
                    }
                    false => {
                        tracing::info!("  ╰───────⏵ 🟥 FAIL in {}", duration);
                        status = "FAIL";
                        tracing::info!("");
                        failed = !fslabs_test.optional; // fail all if not optional
                        TestCase::failure(
                            &fslabs_test.command,
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

                test_duration_h.record(
                    duration.as_seconds_f64(),
                    &[
                        KeyValue::new("workspace_name", workspace_name.clone()),
                        KeyValue::new("package_name", package_name.clone()),
                        KeyValue::new("package_version", package_version.clone()),
                        KeyValue::new("test_command", fslabs_test.command.clone()),
                        KeyValue::new("status", status),
                    ],
                );
                test_counter.add(
                    1,
                    &[
                        KeyValue::new("workspace_name", workspace_name.clone()),
                        KeyValue::new("package_name", package_name.clone()),
                        KeyValue::new("package_version", package_version.clone()),
                        KeyValue::new("test_command", fslabs_test.command.clone()),
                        KeyValue::new("status", status),
                    ],
                );
                tc.set_system_out(&stderr);
                tc.set_system_err(&stdout);
                match fslabs_test.optional {
                    true => ts_optional.add_testcase(tc),
                    false => ts_mandatory.add_testcase(tc),
                };
            }
        }

        // Tear down docker containers
        if let Some(container_id) = service_database_container_id {
            tracing::info!("Tearing down service database");
            teardown_container(container_id).await;
        }
        if let Some(container_id) = service_azurite_container_id {
            tracing::info!("Tearing down service azurite");
            teardown_container(container_id).await;
        }
        junit_report.add_testsuite(ts_mandatory);
        junit_report.add_testsuite(ts_optional);

        let member_end_time = OffsetDateTime::now_utc();
        let member_duration = member_end_time - member_start_time;
        member_duration_h.record(
            member_duration.as_seconds_f64(),
            &[
                KeyValue::new("workspace_name", workspace_name.clone()),
                KeyValue::new("package_name", package_name.clone()),
                KeyValue::new("package_version", package_version.clone()),
                KeyValue::new("success", !failed),
            ],
        );
        member_counter.add(
            1,
            &[
                KeyValue::new("workspace_name", workspace_name.clone()),
                KeyValue::new("package_name", package_name.clone()),
                KeyValue::new("package_version", package_version.clone()),
                KeyValue::new("success", !failed),
            ],
        );
    }
    let mut junit_file = File::create(options.artifacts.join("junit.rust.xml"))?;
    junit_report.write_xml(&mut junit_file)?;
    let overall_end_time = OffsetDateTime::now_utc();
    let overall_duration = overall_end_time - overall_start_time;
    tracing::info!("Workspace tests ran in {}", overall_duration);
    match failed {
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

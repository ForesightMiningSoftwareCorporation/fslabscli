use anyhow::Context;
use clap::Parser;
use indexmap::IndexMap;
use junit_report::{OffsetDateTime, ReportBuilder, TestCase, TestSuiteBuilder};
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
    process::Stdio,
    thread::sleep,
    time::Duration,
};
use tokio::io::AsyncBufReadExt;

use crate::{
    commands::check_workspace::{check_workspace, Options as CheckWorkspaceOptions},
    PrettyPrintable,
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

/// [`execute_command`] with intermediate logging disabled.
async fn execute_command_without_logging(
    command: &str,
    dir: &PathBuf,
    envs: &HashMap<String, String>,
) -> (String, String, bool) {
    execute_command(command, dir, envs, None, None).await
}

/// Execute the `command`, returning stdout and stderr as strings, and success state as a boolean.
///
/// Optionally, stdout and stderr can be logged asynchronously to the current process's stdout
/// during command execution. This is useful in cases where the command might hang. If the command
/// does hang, the partially complete output would never be visible without enabling this logging.
async fn execute_command(
    command: &str,
    dir: &PathBuf,
    envs: &HashMap<String, String>,
    log_stdout: Option<log::Level>,
    log_stderr: Option<log::Level>,
) -> (String, String, bool) {
    let mut child = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(command)
        .current_dir(dir)
        .envs(envs)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Unable to spawn command");

    let stdout = child.stdout.take().expect("Failed to get stdout");
    let mut stdout_stream = tokio::io::BufReader::new(stdout).lines();
    let mut stdout_string = String::new();

    let stderr = child.stderr.take().expect("Failed to get stderr");
    let mut stderr_stream = tokio::io::BufReader::new(stderr).lines();
    let mut stderr_string = String::new();

    loop {
        tokio::select! {
            Ok(Some(line)) = stdout_stream.next_line() =>  {
                stdout_string.push_str(&line);
                if let Some(level) = log_stdout {
                    log::log!(level," │ {}", line)
                }
            },
            Ok(Some(line)) = stderr_stream.next_line() =>  {
                stderr_string.push_str(&line);
                if let Some(level) = log_stderr {
                    log::log!(level," │ {}", line)
                }
            },
            else => break,
        }
    }

    let status = child.wait().await;

    match status {
        Ok(output) => {
            let exit_code = output.code().unwrap_or(1);
            (stdout_string.to_string(), stderr_string, exit_code == 0)
        }
        Err(e) => ("".to_string(), e.to_string(), false),
    }
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
    let overall_start_time = OffsetDateTime::now_utc();
    // Get Directory information
    log::info!("Running the tests with the following arguments:");
    log::info!("* `check_changed`: true");
    log::info!("* `check_publish`: false");
    log::info!("* `changed_head_ref`: {}", options.pull_pull_sha);
    log::info!("* `changed_base_ref`: {}", options.pull_base_sha);

    let check_workspace_options = CheckWorkspaceOptions::new()
        .with_check_changed(true)
        .with_check_publish(false)
        .with_changed_head_ref(options.pull_pull_sha)
        .with_changed_base_ref(options.pull_base_sha);

    let members = check_workspace(Box::new(check_workspace_options), repo_root.clone())
        .await
        .map_err(|e| {
            log::error!("Check directory for crates that need publishing: {}", e);
            e
        })
        .with_context(|| "Could not get directory information")?;

    let mut junit_report = ReportBuilder::new().build();

    // Global fail tracker
    let mut failed = false;

    for (_, member) in members.0 {
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
        log::info!("Testing {ts_name}");
        log::info!("");
        let mut ts_mandatory = TestSuiteBuilder::new(&format!("Mandatory {ts_name}"))
            .set_timestamp(OffsetDateTime::now_utc())
            .build();
        let mut ts_optional = TestSuiteBuilder::new(&format!("Optional {ts_name}"))
            .set_timestamp(OffsetDateTime::now_utc())
            .build();

        // Handle service database
        if !failed && get_test_arg_bool(&test_args, "service_database") == Some(true) {
            log::info!("Setting up service database");
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
            log::info!("Setting up service azurite");
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
                log::info!("Running cache miss command");
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
                log::debug!("cache_miss: {stdout}");
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
                log::info!("Running additional script command");
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
                    log::debug!("additional_script: {line} {stdout}");
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
                log::info!("╭────────────────────────────────────────────────────────────────╮");
                log::info!("│ {:60}   │", fslabs_test.command);
                log::info!("╰─┬──────────────────────────────────────────────────────────────╯");
                log::info!("  ╰───────⏵ ⏭ SKIPPED");
                let tc = TestCase::skipped(fslabs_test.command.as_str());
                match fslabs_test.optional {
                    true => ts_optional.add_testcase(tc),
                    false => ts_mandatory.add_testcase(tc),
                };
            } else {
                log::info!("╭────────────────────────────────────────────────────────────────╮");
                log::info!("│ {:60}   │", fslabs_test.command);
                log::info!("╰─┬──────────────────────────────────────────────────────────────╯");
                let start_time = OffsetDateTime::now_utc();
                if let Some(pre_command) = fslabs_test.pre_command {
                    execute_command_without_logging(&pre_command, &package_path, &fslabs_test.envs)
                        .await;
                }
                let (stdout, stderr, success) = execute_command(
                    &fslabs_test.command,
                    &package_path,
                    &fslabs_test.envs,
                    Some(log::Level::Debug),
                    Some(log::Level::Debug),
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

                let mut tc = match success {
                    true => {
                        log::info!("  ╰───────⏵ 🟢 PASS in {}", duration);
                        log::info!("");
                        TestCase::success(&fslabs_test.command, duration)
                    }
                    false => {
                        log::info!("  ╰───────⏵ 🟥 FAIL in {}", duration);
                        log::info!("");
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
            log::info!("Tearing down service database");
            teardown_container(container_id).await;
        }
        if let Some(container_id) = service_azurite_container_id {
            log::info!("Tearing down service azurite");
            teardown_container(container_id).await;
        }
        junit_report.add_testsuite(ts_mandatory);
        junit_report.add_testsuite(ts_optional);
    }
    let mut junit_file = File::create(options.artifacts.join("junit.rust.xml"))?;
    junit_report.write_xml(&mut junit_file)?;
    let overall_end_time = OffsetDateTime::now_utc();
    let overall_duration = overall_end_time - overall_start_time;
    log::info!("Workspace tests ran in {}", overall_duration);
    match failed {
        false => Ok(TestResult {}),
        true => Err(anyhow::anyhow!("tests failed")),
    }
}

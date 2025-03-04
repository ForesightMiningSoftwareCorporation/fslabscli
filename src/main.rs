use std::fmt::Display;
use std::path::PathBuf;
use std::{env, io};

use clap::{ArgAction, Parser, Subcommand};

use crate::commands::check_workspace::{check_workspace, Options as CheckWorkspaceOptions};
use crate::commands::docker_build_push::{docker_build_push, Options as DockerBuildPushOptions};
use crate::commands::download_artifacts::{
    download_artifacts, Options as DownloadArtifactsOptions,
};
use crate::commands::fix_lock_files::{fix_lock_files, Options as CheckLockFilesOptions};
use crate::commands::generate_wix::{generate_wix, Options as GenerateWixOptions};
use crate::commands::generate_workflow::{generate_workflow, Options as GenerateWorkflowOptions};
use crate::commands::github_app_token::{github_app_token, Options as GithubAppTokenOptions};
use crate::commands::rust_tests::{rust_tests, Options as RustTestsOptions};
use crate::commands::summaries::{summaries, Options as SummariesOptions};

use opentelemetry::{global, KeyValue};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_sdk::{
    logs::SdkLoggerProvider,
    metrics::{MeterProviderBuilder, SdkMeterProvider},
    trace::SdkTracerProvider,
    Resource,
};
use serde::Serialize;
use tracing_core::Level;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod commands;
mod crate_graph;
mod utils;

#[derive(Debug, Parser)] // requires `derive` feature
#[command(
    author,
    version,
    about,
    bin_name("fslabscli"),
    subcommand_required(true),
    propagate_version(true)
)]
struct Cli {
    /// Enables verbose logging
    #[arg(short, long, global = true, action = ArgAction::Count, default_value_t = 2)]
    verbose: u8,
    #[arg(long, global = true)]
    json: bool,
    #[arg(short, long, global = true)]
    pretty_print: bool,
    #[arg(short, long, global = true, default_value = ".", required = false)]
    working_directory: PathBuf,
    #[arg(hide = true, default_value = "fslabscli")]
    cargo_subcommand: CargoSubcommand,
    #[command(subcommand)]
    command: Commands,
}

#[derive(clap::ValueEnum, Clone, Default, Debug, Serialize)]
enum CargoSubcommand {
    #[default]
    Fslabscli,
}

#[derive(Debug, Subcommand)]
enum Commands {
    FixLockFiles(Box<CheckLockFilesOptions>),
    /// Check which crates needs to be published
    CheckWorkspace(Box<CheckWorkspaceOptions>),
    GenerateReleaseWorkflow(Box<GenerateWorkflowOptions>),
    GenerateWix(Box<GenerateWixOptions>),
    Summaries(Box<SummariesOptions>),
    DownloadArtifacts(Box<DownloadArtifactsOptions>),
    GithubAppToken(Box<GithubAppTokenOptions>),
    DockerBuildPush(Box<DockerBuildPushOptions>),
    RustTests(Box<RustTestsOptions>),
}

fn get_resource() -> Resource {
    Resource::builder()
        .with_service_name(env!("CARGO_PKG_NAME"))
        .with_attributes(
            [
                env::var("CARGO_PKG_VERSION").map(|v| KeyValue::new("service_version", v)),
                env::var("JOB_NAME").map(|v| KeyValue::new("prow_job_name", v)),
                env::var("JOB_TYPE").map(|v| KeyValue::new("prow_job_type", v)),
                env::var("PROW_JOB_ID").map(|v| KeyValue::new("prow_job_id", v)),
                env::var("BUILD_ID").map(|v| KeyValue::new("prow_build_id", v)),
                env::var("REPO_OWNER").map(|v| KeyValue::new("repo_owner", v)),
                env::var("REPO_NAME").map(|v| KeyValue::new("repo_name", v)),
                env::var("PULL_BASE_REF").map(|v| KeyValue::new("pull_base_ref", v)),
                env::var("PULL_BASE_SHA").map(|v| KeyValue::new("pull_base_sha", v)),
                env::var("PULL_NUMBER").map(|v| KeyValue::new("pull_number", v)),
            ]
            .into_iter()
            .filter_map(|x| x.ok()),
        )
        .build()
}

fn init_metrics() -> SdkMeterProvider {
    let exporter = opentelemetry_otlp::MetricExporter::builder()
        .with_tonic()
        .build()
        .unwrap();

    let meter_provider = MeterProviderBuilder::default()
        .with_resource(get_resource())
        .with_periodic_exporter(exporter)
        .build();

    global::set_meter_provider(meter_provider.clone());

    meter_provider
}

fn init_traces() -> SdkTracerProvider {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .build()
        .unwrap();

    SdkTracerProvider::builder()
        .with_resource(get_resource())
        .with_batch_exporter(exporter)
        .build()
}

fn init_logs() -> SdkLoggerProvider {
    let exporter = opentelemetry_otlp::LogExporter::builder()
        .with_tonic()
        .build()
        .unwrap();

    SdkLoggerProvider::builder()
        .with_resource(get_resource())
        .with_batch_exporter(exporter)
        .build()
}

pub fn setup_logging(verbosity: u8) -> OtelGuard {
    let logging_level = match verbosity {
        0 => Level::ERROR,
        1 => Level::WARN,
        2 => Level::INFO,
        3 => Level::DEBUG,
        4.. => Level::TRACE,
    };

    let filter = EnvFilter::from_default_env()
        .add_directive(logging_level.into())
        .add_directive("hyper=off".parse().unwrap())
        .add_directive("opentelemetry=off".parse().unwrap())
        .add_directive("opentelemetry_sdk=off".parse().unwrap())
        .add_directive("tonic=off".parse().unwrap())
        .add_directive("h2=off".parse().unwrap())
        .add_directive("reqwest=off".parse().unwrap());

    let log_provider = init_logs();
    let otel_layer = OpenTelemetryTracingBridge::new(&log_provider);
    let fmt_layer = tracing_subscriber::fmt::layer()
        .compact()
        .with_writer(io::stderr);
    tracing_subscriber::registry()
        .with(filter)
        .with(otel_layer)
        .with(fmt_layer)
        .init();

    let traces_provider = init_traces();
    global::set_tracer_provider(traces_provider.clone());
    let metrics_provider = init_metrics();
    global::set_meter_provider(metrics_provider.clone());

    OtelGuard {
        traces_provider,
        metrics_provider,
        log_provider,
    }
}

pub struct OtelGuard {
    traces_provider: SdkTracerProvider,
    metrics_provider: SdkMeterProvider,
    log_provider: SdkLoggerProvider,
}

impl OtelGuard {
    fn drop(&mut self) {
        if let Err(err) = self.traces_provider.shutdown() {
            eprintln!("{err:?}");
        }
        if let Err(err) = self.metrics_provider.shutdown() {
            eprintln!("{err:?}");
        }
        if let Err(err) = self.log_provider.shutdown() {
            eprintln!("{err:?}");
        }
    }
}

pub trait PrettyPrintable {
    fn pretty_print(&self) -> String;
}

fn display_results<T: Serialize + Display + PrettyPrintable>(
    json: bool,
    pretty_print: bool,
    results: T,
) -> String {
    if json {
        serde_json::to_string(&results).unwrap()
    } else if pretty_print {
        results.pretty_print()
    } else {
        format!("{}", results)
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let mut guard = setup_logging(cli.verbose);
    let working_directory = dunce::canonicalize(cli.working_directory)
        .expect("Could not get full path from working_directory");
    let result = match cli.command {
        Commands::FixLockFiles(options) => fix_lock_files(&options, &working_directory),
        Commands::CheckWorkspace(options) => check_workspace(options, working_directory)
            .await
            .map(|r| display_results(cli.json, cli.pretty_print, r)),
        Commands::GenerateReleaseWorkflow(options) => generate_workflow(options, working_directory)
            .await
            .map(|r| display_results(cli.json, cli.pretty_print, r)),
        Commands::GenerateWix(options) => generate_wix(options, working_directory)
            .await
            .map(|r| display_results(cli.json, cli.pretty_print, r)),
        Commands::Summaries(options) => summaries(options, working_directory)
            .await
            .map(|r| display_results(cli.json, cli.pretty_print, r)),
        Commands::DownloadArtifacts(options) => download_artifacts(options, working_directory)
            .await
            .map(|r| display_results(cli.json, cli.pretty_print, r)),
        Commands::GithubAppToken(options) => github_app_token(options, working_directory)
            .await
            .map(|r| display_results(cli.json, cli.pretty_print, r)),
        Commands::DockerBuildPush(options) => docker_build_push(options, working_directory)
            .await
            .map(|r| display_results(cli.json, cli.pretty_print, r)),
        Commands::RustTests(options) => rust_tests(options, working_directory)
            .await
            .map(|r| display_results(cli.json, cli.pretty_print, r)),
    };
    guard.drop();

    match result {
        Ok(r) => {
            println!("{}", r);
            std::process::exit(exitcode::OK);
        }
        Err(e) => {
            tracing::error!("Could not execute command: {}", e);
            std::process::exit(exitcode::DATAERR);
        }
    };
}

use std::fmt::Display;
use std::path::PathBuf;
use std::{env, io};

use clap::{ArgAction, CommandFactory, Parser, Subcommand};
use clap_complete::{Shell, generate};
use clap_mangen::Man;

use crate::commands::check_workspace::{Options as CheckWorkspaceOptions, check_workspace};
use crate::commands::docker_build_push::{Options as DockerBuildPushOptions, docker_build_push};
use crate::commands::download_artifacts::{
    Options as DownloadArtifactsOptions, download_artifacts,
};
use crate::commands::fix_lock_files::{Options as CheckLockFilesOptions, fix_lock_files};
use crate::commands::generate_wix::{Options as GenerateWixOptions, generate_wix};
use crate::commands::generate_workflow::{Options as GenerateWorkflowOptions, generate_workflow};
use crate::commands::github_app_token::{Options as GithubAppTokenOptions, github_app_token};
use crate::commands::publish::{Options as PublishOptions, publish};
use crate::commands::summaries::{Options as SummariesOptions, summaries};
use crate::commands::tests::{Options as TestsOptions, tests};

use opentelemetry::{KeyValue, global};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_sdk::{
    Resource,
    logs::SdkLoggerProvider,
    metrics::{MeterProviderBuilder, SdkMeterProvider},
    trace::SdkTracerProvider,
};
use serde::Serialize;
use tracing_core::Level;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod commands;
mod crate_graph;
mod utils;

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about,
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
    #[arg(env, long)]
    fslabscli_auto_update: bool,
}

#[derive(clap::ValueEnum, Clone, Default, Debug, Serialize)]
enum CargoSubcommand {
    #[default]
    Fslabscli,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Fix inconsistencies in all Cargo.lock files.
    FixLockFiles(Box<CheckLockFilesOptions>),
    /// Check which crates needs to be published
    CheckWorkspace(Box<CheckWorkspaceOptions>),
    GenerateReleaseWorkflow(Box<GenerateWorkflowOptions>),
    GenerateWix(Box<GenerateWixOptions>),
    /// Summarize a github action run
    Summaries(Box<SummariesOptions>),
    /// Download github run artifacts
    DownloadArtifacts(Box<DownloadArtifactsOptions>),
    /// Generate a github token for an github app
    GithubAppToken(Box<GithubAppTokenOptions>),
    /// Build and push docker image
    DockerBuildPush(Box<DockerBuildPushOptions>),
    /// Test workspace members
    #[command(visible_alias = "rust-tests")]
    Tests(Box<TestsOptions>),
    /// Publish workspace members
    Publish(Box<PublishOptions>),
    /// Generate a shell completions script
    Completions {
        /// The shell for which to generate the script
        shell: Shell,
    },
    /// Generate man pages
    ManPage,
}

fn get_resource(with_unique_attributes: bool) -> Resource {
    let mut attributes = [
        env::var("CARGO_PKG_VERSION").map(|v| KeyValue::new("service_version", v)),
        env::var("JOB_NAME").map(|v| KeyValue::new("prow_job_name", v)),
        env::var("JOB_TYPE").map(|v| KeyValue::new("prow_job_type", v)),
        env::var("REPO_OWNER").map(|v| KeyValue::new("repo_owner", v)),
        env::var("REPO_NAME").map(|v| KeyValue::new("repo_name", v)),
    ]
    .into_iter()
    .filter_map(|x| x.ok())
    .collect::<Vec<_>>();
    if with_unique_attributes {
        attributes.extend(
            [
                env::var("PROW_JOB_ID").map(|v| KeyValue::new("prow_job_id", v)),
                env::var("BUILD_ID").map(|v| KeyValue::new("prow_build_id", v)),
                env::var("PULL_BASE_REF").map(|v| KeyValue::new("pull_base_ref", v)),
                env::var("PULL_BASE_SHA").map(|v| KeyValue::new("pull_base_sha", v)),
                env::var("PULL_NUMBER").map(|v| KeyValue::new("pull_number", v)),
            ]
            .into_iter()
            .filter_map(|x| x.ok()),
        );
    }

    Resource::builder()
        .with_service_name(env!("CARGO_PKG_NAME"))
        .with_attributes(attributes)
        .build()
}

fn init_metrics(with_unique_attributes: bool) -> SdkMeterProvider {
    let exporter = opentelemetry_otlp::MetricExporter::builder()
        .with_tonic()
        .build()
        .unwrap();

    MeterProviderBuilder::default()
        .with_resource(get_resource(with_unique_attributes))
        .with_periodic_exporter(exporter)
        .build()
}

fn init_traces() -> SdkTracerProvider {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .build()
        .unwrap();

    SdkTracerProvider::builder()
        .with_resource(get_resource(true))
        .with_batch_exporter(exporter)
        .build()
}

fn init_logs() -> SdkLoggerProvider {
    let exporter = opentelemetry_otlp::LogExporter::builder()
        .with_tonic()
        .build()
        .unwrap();

    SdkLoggerProvider::builder()
        .with_resource(get_resource(true))
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
        .add_directive("tower=off".parse().unwrap())
        .add_directive("reqwest=off".parse().unwrap());

    let log_provider = init_logs();
    let otel_layer = OpenTelemetryTracingBridge::new(&log_provider);
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(io::stderr)
        .compact();

    tracing_subscriber::registry()
        .with(filter)
        .with(otel_layer)
        .with(fmt_layer)
        .init();

    let traces_provider = init_traces();
    global::set_tracer_provider(traces_provider.clone());
    let metrics_provider = init_metrics(true);
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
        format!("{results}")
    }
}

#[tokio::main]
async fn main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Could not install crypto provider");
    let matches = Cli::command()
        .disable_help_flag(true)
        .disable_version_flag(true)
        .ignore_errors(true)
        .try_get_matches()
        .ok();

    let log_level = matches
        .clone()
        .and_then(|matches| matches.get_one::<u8>("verbose").cloned())
        .unwrap_or(2);

    let mut guard = setup_logging(log_level);

    let fslabscli_auto_update = matches
        .and_then(|matches| matches.get_one::<bool>("fslabscli_auto_update").cloned())
        .unwrap_or_default();

    if fslabscli_auto_update {
        if let Err(err) = utils::auto_update::auto_update() {
            println!("Error trying to update:{err:?}");
        }
    }

    run().await;

    guard.drop();
}

async fn run() {
    let cli = Cli::parse();

    // generate man pages and completions upon request and exit
    match cli.command {
        Commands::Completions { shell } => {
            let mut clap_command = Cli::command();
            let output = io::stdout();
            let mut output_handle = output.lock();
            let bin_name = clap_command.get_name().to_owned();
            generate(shell, &mut clap_command, bin_name, &mut output_handle);
            return;
        }
        Commands::ManPage => {
            let clap_command = Cli::command();
            let output = io::stdout();
            let mut output_handle = output.lock();
            let man = Man::new(clap_command.clone());
            man.render(&mut output_handle).unwrap();
            for subcommand in clap_command.get_subcommands() {
                let primary = Man::new(subcommand.clone());
                primary.render_name_section(&mut output_handle).unwrap();
                primary.render_synopsis_section(&mut output_handle).unwrap();
                primary
                    .render_description_section(&mut output_handle)
                    .unwrap();
                primary.render_options_section(&mut output_handle).unwrap();
            }
            return;
        }
        _ => {} // nothing to do, will be handled later
    };
    let working_directory = dunce::canonicalize(cli.working_directory)
        .expect("Could not get full path from working_directory");
    let result = match cli.command {
        Commands::FixLockFiles(options) => fix_lock_files(&options, &working_directory)
            .map(|r| display_results(cli.json, cli.pretty_print, r)),
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
        Commands::Tests(options) => tests(options, working_directory)
            .await
            .map(|r| display_results(cli.json, cli.pretty_print, r)),
        Commands::Publish(options) => publish(options, working_directory)
            .await
            .map(|r| display_results(cli.json, cli.pretty_print, r)),
        Commands::Completions { shell: _ } | Commands::ManPage => {
            unreachable!(
                "Request for completions script or man pages should have been handled earlier and the program should have exited then."
            );
        }
    };

    match result {
        Ok(r) => {
            println!("{r}");
            std::process::exit(exitcode::OK);
        }
        Err(e) => {
            tracing::error!("Could not execute command: {}", e);
            std::process::exit(exitcode::DATAERR);
        }
    };
}

use std::fmt;
use std::fmt::Display;
use std::path::PathBuf;
use std::process::ExitCode;
use std::{env, io};

use clap::{ArgAction, CommandFactory, Parser, Subcommand};
use clap_complete::{Shell, generate};
use clap_mangen::Man;
use utils::cargo::Cargo;

use crate::commands::annotate::{Options as AnnotateOptions, annotate};
use crate::commands::check_workspace::{Options as CheckWorkspaceOptions, check_workspace};
use crate::commands::docker_build_push::{Options as DockerBuildPushOptions, docker_build_push};
use crate::commands::download_artifacts::{
    Options as DownloadArtifactsOptions, download_artifacts,
};
use crate::commands::draft_release::{Options as DraftReleaseOptions, draft_release};
use crate::commands::fix_lock_files::{Options as FixLockFilesOptions, fix_lock_files};
use crate::commands::generate_wix::{Options as GenerateWixOptions, generate_wix};
use crate::commands::generate_workflow::{Options as GenerateWorkflowOptions, generate_workflow};
use crate::commands::github_app_token::{Options as GithubAppTokenOptions, github_app_token};
use crate::commands::publish::{Options as PublishOptions, publish};
use crate::commands::summaries::{Options as SummariesOptions, summaries};
use crate::commands::tests::{Options as TestsOptions, tests};
use crate::crate_graph::find_git_root;

use opentelemetry::trace::TracerProvider;
use opentelemetry::{KeyValue, global};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_sdk::{
    Resource,
    logs::SdkLoggerProvider,
    metrics::{MeterProviderBuilder, SdkMeterProvider},
    trace::SdkTracerProvider,
};
use serde::Serialize;
use tracing::field::{Field, Visit};
use tracing_core::Level;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::field::RecordFields;
use tracing_subscriber::fmt::FormatFields;
use tracing_subscriber::fmt::format::{DefaultFields, Writer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod cli_args;
mod commands;
mod crate_graph;
mod script;
mod test_args;
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
    /// Auto-update to the latest release on startup
    #[arg(env, long)]
    fslabscli_auto_update: bool,
    /// Pin to a specific version (e.g. 2.42.0). Takes precedence over auto-update
    #[arg(env, long)]
    fslabscli_version: Option<String>,
}

#[derive(Debug, Parser, Clone, Default)]
#[command()]
pub struct PackageRelatedOptions {
    #[arg(long, env, default_value = "1")]
    job_limit: usize,
    #[arg(long, env, default_value = "0")]
    inner_job_limit: usize,
    #[arg(long, env, default_value = "fsl")]
    cargo_main_registry: String,
    /// Only considers the following packages
    #[arg(long, default_value = "", value_delimiter = ',')]
    whitelist: Vec<String>,
    /// Ignores the following packages
    #[arg(long, default_value = "", value_delimiter = ',')]
    blacklist: Vec<String>,
    /// Display progress
    #[arg(long, default_value_t = false)]
    progress: bool,
    /// Only publish to this cargo registry, skip all others
    #[arg(long, env)]
    cargo_target_registry: Option<String>,
}

#[derive(clap::ValueEnum, Clone, Default, Debug, Serialize)]
enum CargoSubcommand {
    #[default]
    Fslabscli,
}

#[derive(Debug, Subcommand)]
enum Commands {
    // Github Commands
    //
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
    /// Create or update a draft GitHub release and upload artifacts
    DraftRelease(Box<DraftReleaseOptions>),
    /// Post build findings (logs, JUnit XML) as GitHub check-run annotations
    Annotate(Box<AnnotateOptions>),

    // Packages Related Commands
    //
    #[command(about = FixLockFilesOptions::ABOUT, long_about = FixLockFilesOptions::LONG_ABOUT)]
    FixLockFiles {
        #[command(flatten)]
        common_options: Box<PackageRelatedOptions>,
        #[command(flatten)]
        options: Box<FixLockFilesOptions>,
    },
    /// Check which crates needs to be published
    CheckWorkspace {
        #[command(flatten)]
        common_options: Box<PackageRelatedOptions>,
        #[command(flatten)]
        options: Box<CheckWorkspaceOptions>,
    },
    /// Test workspace members
    #[command(visible_alias = "rust-tests")]
    Tests {
        #[command(flatten)]
        common_options: Box<PackageRelatedOptions>,
        #[command(flatten)]
        options: Box<TestsOptions>,
    },
    /// Publish workspace members
    Publish {
        #[command(flatten)]
        common_options: Box<PackageRelatedOptions>,
        #[command(flatten)]
        options: Box<PublishOptions>,
    },

    // Meta Commands
    //
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

    let provider = MeterProviderBuilder::default()
        .with_resource(get_resource(with_unique_attributes))
        .with_periodic_exporter(exporter)
        .build();

    global::set_meter_provider(provider.clone());

    provider
}

fn init_traces() -> SdkTracerProvider {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .build()
        .unwrap();

    let provider = SdkTracerProvider::builder()
        .with_resource(get_resource(true))
        .with_batch_exporter(exporter)
        .build();

    global::set_tracer_provider(provider.clone());

    provider
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

/// Fields that are only useful for the OTLP exporter layer, not stderr.
fn is_otel_only_field(name: &str) -> bool {
    name.starts_with("otel.") || matches!(name, "workspace" | "package" | "version" | "step")
}

/// Wraps `DefaultFields` to strip OTLP-only fields from stderr fmt output.
#[derive(Debug)]
struct FilterOtelFields(DefaultFields);

impl<'writer> FormatFields<'writer> for FilterOtelFields {
    fn format_fields<R: RecordFields>(&self, writer: Writer<'writer>, fields: R) -> fmt::Result {
        use tracing_subscriber::field::{MakeVisitor, VisitOutput};
        let mut v = self.0.make_visitor(writer);
        fields.record(&mut FilteringVisitor(&mut v));
        v.finish()
    }
}

struct FilteringVisitor<'a>(&'a mut dyn Visit);

macro_rules! forward_if_not_otel {
    ($method:ident, $ty:ty) => {
        fn $method(&mut self, field: &Field, value: $ty) {
            if !is_otel_only_field(field.name()) {
                self.0.$method(field, value);
            }
        }
    };
}

impl Visit for FilteringVisitor<'_> {
    forward_if_not_otel!(record_debug, &dyn fmt::Debug);
    forward_if_not_otel!(record_f64, f64);
    forward_if_not_otel!(record_i64, i64);
    forward_if_not_otel!(record_u64, u64);
    forward_if_not_otel!(record_i128, i128);
    forward_if_not_otel!(record_u128, u128);
    forward_if_not_otel!(record_bool, bool);
    forward_if_not_otel!(record_str, &str);
    forward_if_not_otel!(record_bytes, &[u8]);

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        if !is_otel_only_field(field.name()) {
            self.0.record_error(field, value);
        }
    }
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
        .compact()
        .fmt_fields(FilterOtelFields(DefaultFields::new()));

    let traces_provider = init_traces();
    let otel_tracer_layer =
        tracing_opentelemetry::layer().with_tracer(traces_provider.tracer("fslabscli"));

    tracing_subscriber::registry()
        .with(filter)
        .with(otel_layer)
        .with(otel_tracer_layer)
        .with(fmt_layer)
        .init();

    let metrics_provider = init_metrics(true);

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
    fn shutdown(&self) {
        let _ = self.traces_provider.force_flush();
        let _ = self.metrics_provider.force_flush();
        let _ = self.log_provider.force_flush();
        let _ = self.traces_provider.shutdown();
        let _ = self.metrics_provider.shutdown();
        let _ = self.log_provider.shutdown();
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
async fn main() -> ExitCode {
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

    let guard = setup_logging(log_level);

    let fslabscli_version = matches
        .as_ref()
        .and_then(|matches| matches.get_one::<String>("fslabscli_version").cloned());

    let fslabscli_auto_update = matches
        .and_then(|matches| matches.get_one::<bool>("fslabscli_auto_update").cloned())
        .unwrap_or_default();

    // self_update drives blocking HTTP, and dropping that client inside an
    // async context aborts the process ("Cannot drop a runtime in a context
    // where blocking is not allowed"). block_in_place hands this thread over to
    // blocking work, which makes the drop legal.
    if let Some(version) = fslabscli_version {
        if let Err(err) =
            tokio::task::block_in_place(|| utils::auto_update::auto_update(Some(&version)))
        {
            eprintln!("Error trying to update to pinned version: {err:?}");
            std::process::exit(1);
        }
    } else if fslabscli_auto_update
        && let Err(err) = tokio::task::block_in_place(|| utils::auto_update::auto_update(None))
    {
        eprintln!("Error trying to update: {err:?}");
    }

    let r = run().await;

    let exit_code = match &r {
        Ok(r) => {
            println!("{r}");
            0
        }
        Err(e) => {
            tracing::error!("Could not execute command: {:#}", e);
            1
        }
    };

    // Watchdog: force exit after 5s if telemetry flush hangs
    let wd = exit_code;
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(5));
        std::process::exit(wd);
    });

    guard.shutdown();
    drop(guard);
    std::process::exit(exit_code)
}

async fn run() -> anyhow::Result<String> {
    let cli = Cli::parse();

    // generate man pages and completions upon request and exit
    match cli.command {
        Commands::Completions { shell } => {
            let mut clap_command = Cli::command();
            let output = io::stdout();
            let mut output_handle = output.lock();
            let bin_name = clap_command.get_name().to_owned();
            generate(shell, &mut clap_command, bin_name, &mut output_handle);
            return Ok(String::new());
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
            return Ok(String::new());
        }
        _ => {} // nothing to do, will be handled later
    };
    let working_directory = dunce::canonicalize(&cli.working_directory)
        .expect("Could not get full path from working_directory");
    let repo_root = find_git_root(&cli.working_directory)
        .unwrap_or_else(|| panic!("Could not find git root from {working_directory:?}"));

    match cli.command {
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
        Commands::DraftRelease(options) => draft_release(options, working_directory)
            .await
            .map(|r| display_results(cli.json, cli.pretty_print, r)),
        Commands::Annotate(options) => annotate(options, working_directory)
            .await
            .map(|r| display_results(cli.json, cli.pretty_print, r)),
        Commands::FixLockFiles {
            common_options,
            options,
        } => fix_lock_files(&common_options, &options, &repo_root)
            .map(|r| display_results(cli.json, cli.pretty_print, r)),
        Commands::CheckWorkspace {
            common_options,
            options,
        } => check_workspace::<Cargo>(&common_options, &options, working_directory)
            .await
            .map(|r| display_results(cli.json, cli.pretty_print, r)),
        Commands::Tests {
            common_options,
            options,
        } => tests(&common_options, &options, repo_root)
            .await
            .map(|r| display_results(cli.json, cli.pretty_print, r)),
        Commands::Publish {
            mut common_options,
            options,
        } => publish(&mut common_options, &options, working_directory)
            .await
            .map(|r| display_results(cli.json, cli.pretty_print, r)),
        Commands::Completions { shell: _ } | Commands::ManPage => {
            unreachable!(
                "Request for completions script or man pages should have been handled earlier and the program should have exited then."
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use opentelemetry::trace::TracerProvider;
    use opentelemetry_sdk::trace::{InMemorySpanExporterBuilder, SdkTracerProvider};
    use tracing_opentelemetry::OpenTelemetryLayer;
    use tracing_subscriber::Registry;
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn tracing_opentelemetry_layer_exports_spans() {
        // Arrange
        let exporter = InMemorySpanExporterBuilder::new().build();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let tracer = provider.tracer("test");
        let otel_layer = OpenTelemetryLayer::new(tracer);
        let subscriber = Registry::default().with(otel_layer);

        // Act
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("test_span");
            let _guard = span.enter();
        });

        provider.force_flush().expect("flush failed");

        // Assert
        let finished = exporter.get_finished_spans().expect("failed to get spans");
        assert_eq!(finished.len(), 1, "expected exactly one exported span");
        assert_eq!(finished[0].name, "test_span");
    }
}

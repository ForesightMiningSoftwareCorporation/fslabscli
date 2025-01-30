use std::fmt::Display;
use std::path::PathBuf;

use clap::{ArgAction, Parser, Subcommand};
use log::LevelFilter;
use log4rs::append::console::ConsoleAppender;
use log4rs::config::{Appender, Root};
use log4rs::encode::pattern::PatternEncoder;
use serde::Serialize;

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

pub fn setup_logging(verbosity: u8) {
    let logging_level = match verbosity {
        0 => LevelFilter::Error,
        1 => LevelFilter::Warn,
        2 => LevelFilter::Info,
        3 => LevelFilter::Debug,
        4.. => LevelFilter::Trace,
    };

    // Encoders
    let stdout: ConsoleAppender = ConsoleAppender::builder()
        .encoder(Box::new(PatternEncoder::new(
            "{h({d(%Y-%m-%d %H:%M:%S)(utc)} - {l}: {m}{n})}",
        )))
        .build();

    let log_config = log4rs::config::Config::builder()
        .appender(Appender::builder().build("stderr", Box::new(stdout)))
        .build(Root::builder().appender("stderr").build(logging_level))
        .unwrap();
    log4rs::init_config(log_config)
        .map_err(|e| format!("Could not setup logging: {}", e))
        .unwrap();
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
    setup_logging(cli.verbose);
    let working_directory = cli
        .working_directory
        .canonicalize()
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
    match result {
        Ok(r) => {
            println!("{}", r);
            std::process::exit(exitcode::OK);
        }
        Err(e) => {
            log::error!("Could not execute command: {}", e);
            std::process::exit(exitcode::DATAERR);
        }
    };
}

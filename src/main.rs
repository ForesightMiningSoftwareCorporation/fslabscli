use std::fmt::Display;
use std::path::PathBuf;

use clap::{ArgAction, Parser, Subcommand};
use log4rs::append::console::ConsoleAppender;
use log4rs::config::{Appender, Root};
use log4rs::encode::pattern::PatternEncoder;
use log::LevelFilter;
use serde::Serialize;

use crate::commands::publishable;
use crate::commands::publishable::publishable;

mod commands;
mod utils;

#[derive(Debug, Parser)] // requires `derive` feature
#[command(
author,
version,
about,
bin_name("fslabsci"),
subcommand_required(true),
propagate_version(true),
)]
struct Cli {
    /// Enables verbose logging
    #[arg(short, long, global = true, action = ArgAction::Count)]
    verbose: u8,
    #[arg(long, global = true)]
    json: bool,
    #[arg(short, long, global = true, default_missing_value = ".", required = false)]
    working_directory: PathBuf,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Check which crates needs to be published
    Publishable(publishable::Options),
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
        .encoder(Box::new(PatternEncoder::new("{h({d(%Y-%m-%d %H:%M:%S)(utc)} - {l}: {m}{n})}")))
        .build();

    let log_config = log4rs::config::Config::builder()
        .appender(Appender::builder().build("stderr", Box::new(stdout)))
        .build(Root::builder().appender("stderr").build(logging_level))
        .unwrap();
    log4rs::init_config(log_config)
        .map_err(|e| format!("Could not setup logging: {}", e)).unwrap();
}

fn display_or_json<T: Serialize + Display>(json: bool, results: T) -> String {
    if json {
        serde_json::to_string(&results).unwrap()
    } else {
        format!("{}", results)
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    setup_logging(cli.verbose);
    let result = match cli.command {
        Commands::Publishable(options) => publishable(options, cli.working_directory).await.map(|r| display_or_json(cli.json, r)),
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

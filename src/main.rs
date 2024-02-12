use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::commands::publishable;
use crate::commands::publishable::publishable;

mod commands;

#[derive(Debug, Parser)] // requires `derive` feature
#[clap(
author,
version,
about,
bin_name("fslabsci"),
subcommand_required(true),
propagate_version(true),
)]
struct Cli {
    #[arg(short, long, global = true, default_value_t = None)]
    working_directory: Option<PathBuf>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Check which crates needs to be published
    #[command(arg_required_else_help = true)]
    Publishable(publishable::Options),
}


#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let _ = match cli.command {
        Commands::Publishable(options) => publishable(options, cli.working_directory).await,
    };
}

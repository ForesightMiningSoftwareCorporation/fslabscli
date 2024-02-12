use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
#[clap(about = "Check directory for crates that need to be published.")]
pub struct Options {}

pub async fn publishable(_options: Options, _working_directory: Option<PathBuf>) -> anyhow::Result<()> {
    Ok(())
}
use crate::crate_graph::{CrateGraph, DiffRevs};
use clap::Parser;
use std::path::Path;

#[derive(Debug, Parser, Default)]
#[command(about = "Fix inconsistencies in all Cargo.lock files.")]
pub struct Options {
    /// The branch's head revision string.
    #[arg(long, default_value = "HEAD")]
    head_rev: String,
    /// The branch's base revision string.
    #[arg(long)]
    base_rev: Option<String>,
}

pub fn fix_lock_files(options: &Options, repo_root: &Path) -> anyhow::Result<String> {
    let Options { head_rev, base_rev } = options;
    let diff = base_rev
        .as_ref()
        .map(|base_rev| DiffRevs { head_rev, base_rev });

    CrateGraph::new(repo_root, None)?.fix_lock_files(diff)?;

    Ok("".into())
}

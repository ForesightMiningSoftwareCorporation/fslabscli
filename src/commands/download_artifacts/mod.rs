use crate::PrettyPrintable;
use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use octocrab::models::workflows::WorkflowListArtifact;
use octocrab::Octocrab;
use serde::Serialize;
use std::fmt::{Display, Formatter};
use std::fs::File;
use std::io::{self, Cursor, Write};
use std::path::PathBuf;
use zip::ZipArchive;

#[derive(Debug, Parser)]
#[command(about = "Download github action artifacts")]
pub struct Options {
    #[arg(long)]
    output: PathBuf,
    #[arg(long, default_value_t = true)]
    unzip: bool,
    #[arg(long, env = "GITHUB_TOKEN")]
    github_token: String,
    #[arg()]
    owner: String,
    #[arg()]
    repo: String,
    #[arg()]
    run_id: u64,
}

#[derive(Serialize)]
pub struct GenerateResult {}

impl Display for GenerateResult {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "")
    }
}

impl PrettyPrintable for GenerateResult {
    fn pretty_print(&self) -> String {
        "".to_string()
    }
}

pub async fn download_artifacts(
    options: Box<Options>,
    _working_directory: PathBuf,
) -> anyhow::Result<GenerateResult> {
    // We have a github token we should try to update the pr
    let octocrab = Octocrab::builder()
        .personal_token(options.github_token)
        .build()?;
    let page = octocrab
        .actions()
        .list_workflow_run_artifacts(
            options.owner.clone(),
            options.repo.clone(),
            options.run_id.into(),
        )
        .send()
        .await?;
    let artifacts = octocrab
        .all_pages::<WorkflowListArtifact>(page.value.expect("they should have a page"))
        .await?;

    let pb = ProgressBar::new(artifacts.len() as u64).with_style(ProgressStyle::with_template(
        "{spinner} {wide_msg} {pos}/{len}",
    )?);
    for artifact in artifacts {
        pb.inc(1);
        pb.set_message(format!("{}", artifact.name));
        let artifact_data = octocrab
            .actions()
            .download_artifact(
                options.owner.clone(),
                options.repo.clone(),
                artifact.id,
                octocrab::params::actions::ArchiveFormat::Zip,
            )
            .await?;
        if options.unzip {
            let cursor = Cursor::new(artifact_data);
            let mut zip = ZipArchive::new(cursor)?;

            for i in 0..zip.len() {
                let mut file = zip.by_index(i)?;
                let file_name = match file.enclosed_name() {
                    Some(name) => name.to_owned(),
                    None => continue,
                };
                let outpath = options.output.join(file_name);
                if file.is_dir() {
                    std::fs::create_dir_all(&outpath)?;
                } else {
                    if let Some(parent) = outpath.parent() {
                        std::fs::create_dir_all(&parent)?;
                    }
                    let mut outfile = File::create(&outpath)?;
                    io::copy(&mut file, &mut outfile)?;
                }
            }
        } else {
            let outpath = options.output.join(format!("{}.zip", artifact.name));
            let mut outfile = File::create(&outpath)?;
            outfile.write_all(&artifact_data)?;
        }
    }
    Ok(GenerateResult {})
}

use crate::PrettyPrintable;
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::fmt::{Display, Formatter};
use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, Parser)]
#[command(about = "Build and Push a docker image")]
pub struct Options {
    /// List of build-time variables
    #[arg(long)]
    build_args: Vec<String>,
    /// External cache source (e.g., type=local,src=path/to/dir)
    #[arg(long, env = "DOCKER_CACHE_FROM")]
    cache_from: Option<String>,
    /// Cache export destination (e.g., type=local,dest=path/to/dir)
    #[arg(long, env = "DOCKER_CACHE_TO")]
    cache_to: Option<String>,
    /// Build's context is the set of files located in the specified PATH or URL (default to working directory)
    #[arg(long)]
    context: Option<String>,
    /// Path to the Dockerfile. (default {context}/Dockerfile)
    #[arg(long, short = 'f', default_value = "Dockerfile")]
    file: String,
    /// List of metadata for an image
    #[arg(long)]
    labels: Vec<String>,
    /// List of target platforms for build. (default [linux/amd64])
    #[arg(long, default_values_t = ["linux/amd64".to_string()])]
    platforms: Vec<String>,
    #[arg(long, default_value_t = true)]
    push: bool,
    /// List of secrets to expose to the build (e.g., key=string, GIT_AUTH_TOKEN=mytoken)
    #[arg(long)]
    secrets: Vec<String>,
    /// List of SSH agent socket or keys to expose to the build
    #[arg(long)]
    ssh: Vec<String>,
    /// Image name
    image: String,
    /// Additional Image tags
    #[arg(long)]
    additional_tags: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct DockerBuildPushResult {
    /// Image ID
    image_id: String,
    /// Image Digest
    digest: String,
}

#[derive(Serialize, Deserialize)]
pub struct CreateAccessToken {}

impl Display for DockerBuildPushResult {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.image_id)
    }
}

impl PrettyPrintable for DockerBuildPushResult {
    fn pretty_print(&self) -> String {
        self.image_id.clone()
    }
}

pub async fn docker_build_push(
    options: Box<Options>,
    working_directory: PathBuf,
) -> anyhow::Result<DockerBuildPushResult> {
    let mut build_command = Command::new("docker");
    build_command.arg("build");
    build_command.arg("-t").arg(&options.image);

    let context = options
        .context
        .map(PathBuf::from)
        .unwrap_or_else(|| working_directory);

    if let Some(cache_from) = &options.cache_from {
        build_command.arg("--cache-from").arg(cache_from);
    }
    if let Some(cache_to) = &options.cache_to {
        build_command.arg("--cache-to").arg(cache_to);
    }
    options.build_args.iter().for_each(|arg| {
        build_command.arg("--build-arg").arg(arg);
    });
    options.secrets.iter().for_each(|arg| {
        build_command.arg("--secret").arg(arg);
    });

    build_command.arg("--file").arg(context.join(&options.file));
    build_command
        .arg("--platform")
        .arg(options.platforms.join(","));

    let status = build_command.status()?;
    if !status.success() {
        anyhow::bail!("Could not build docker image {}", options.image,);
    }

    if options.push {
        let status = Command::new("docker")
            .arg("push")
            .arg(&options.image)
            .status()?;
        if !status.success() {
            anyhow::bail!("Could not push docker image",);
        }
    }
    for additional_tag in options.additional_tags {
        let status = Command::new("docker")
            .arg("tag")
            .arg(&options.image)
            .arg(&additional_tag)
            .status()?;
        if !status.success() {
            anyhow::bail!("Could not tag docker image");
        }
        if options.push {
            let status = Command::new("docker")
                .arg("push")
                .arg(&additional_tag)
                .status()?;
            if !status.success() {
                anyhow::bail!("Could not push docker image");
            }
        }
    }

    Ok(DockerBuildPushResult {
        image_id: "".to_string(),
        digest: "".to_string(),
    })
}

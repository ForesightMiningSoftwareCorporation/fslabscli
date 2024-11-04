use crate::PrettyPrintable;
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::fmt::{Display, Formatter};
use std::path::PathBuf;

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
    /// Build's context is the set of files located in the specified PATH or URL (default Git context)
    #[arg(long, default_value = ".")]
    context: String,
    /// Path to the Dockerfile. (default {context}/Dockerfile)
    #[arg(long)]
    file: Option<String>,
    /// List of metadata for an image
    #[arg(long)]
    labels: Vec<String>,
    /// List of target platforms for build
    #[arg(long)]
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
    _working_directory: PathBuf,
) -> anyhow::Result<DockerBuildPushResult> {
    Ok(DockerBuildPushResult {
        image_id: "".to_string(),
        digest: "".to_string(),
    })
}

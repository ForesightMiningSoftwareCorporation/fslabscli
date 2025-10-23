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

/// Extracts the cache reference from an image name by removing the tag or digest.
/// Examples:
/// - "ghcr.io/org/image:1.0.0" -> "ghcr.io/org/image:buildcache"
/// - "ghcr.io/org/image@sha256:abc..." -> "ghcr.io/org/image:buildcache"
/// - "ghcr.io/org/image" -> "ghcr.io/org/image:buildcache"
fn extract_cache_reference(image: &str) -> String {
    // First check for digest (@ takes precedence over :)
    let base_image = if let Some(pos) = image.find('@') {
        // Remove digest
        &image[..pos]
    } else if let Some(pos) = image.rfind(':') {
        // Check if this is a port number (contains '//' before the ':')
        // or if there's a '/' after the ':', which indicates it's part of the registry/path
        if image[..pos].rfind('/').is_some() {
            // If there's a slash before the colon, it's likely a tag, not a port
            &image[..pos]
        } else if image[..pos].contains("//") {
            // If there's '//' before the colon, it's a URL with port, keep the full image
            image
        } else {
            // Otherwise, it's a tag without registry path, remove it
            &image[..pos]
        }
    } else {
        // No tag or digest present
        image
    };

    format!("{}:buildcache", base_image)
}

pub async fn docker_build_push(
    options: Box<Options>,
    working_directory: PathBuf,
) -> anyhow::Result<DockerBuildPushResult> {
    let mut build_command = Command::new("docker");
    build_command
        .arg("buildx")
        .arg("build")
        .arg("--progress")
        .arg("plain");
    build_command.arg("-t").arg(&options.image);

    let context = options
        .context
        .map(PathBuf::from)
        .unwrap_or_else(|| working_directory);

    // Handle cache options: use explicit options if provided, otherwise auto-generate registry cache
    let has_explicit_cache = options.cache_from.is_some() || options.cache_to.is_some();

    if has_explicit_cache {
        // Use user-provided explicit cache options
        if let Some(cache_from) = &options.cache_from {
            build_command.arg("--cache-from").arg(cache_from);
        }
        if let Some(cache_to) = &options.cache_to {
            build_command.arg("--cache-to").arg(cache_to);
        }
    } else {
        // Auto-generate registry cache from image name
        let cache_ref = extract_cache_reference(&options.image);
        build_command
            .arg("--cache-from")
            .arg(format!("type=registry,ref={}", cache_ref));
        build_command
            .arg("--cache-to")
            .arg(format!("type=registry,ref={},mode=max", cache_ref));
    }
    options.build_args.iter().for_each(|arg| {
        build_command.arg("--build-arg").arg(arg);
    });
    options.secrets.iter().for_each(|arg| {
        build_command.arg("--secret").arg(arg);
    });

    for additional_tag in options.additional_tags {
        build_command.arg("-t").arg(&additional_tag);
    }

    if options.push {
        build_command.arg("--push");
    }

    build_command.arg("--file").arg(context.join(&options.file));
    build_command
        .arg("--platform")
        .arg(options.platforms.join(","))
        .arg(context);

    tracing::debug!("Running `{:?}`", build_command);

    let status = build_command.status()?;
    if !status.success() {
        anyhow::bail!("Could not build docker image {}", options.image,);
    }

    Ok(DockerBuildPushResult {
        image_id: "".to_string(),
        digest: "".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_cache_reference_with_tag() {
        assert_eq!(
            extract_cache_reference("ghcr.io/org/image:1.0.0"),
            "ghcr.io/org/image:buildcache"
        );
    }

    #[test]
    fn test_extract_cache_reference_with_digest() {
        assert_eq!(
            extract_cache_reference("ghcr.io/org/image@sha256:abcdef123456"),
            "ghcr.io/org/image:buildcache"
        );
    }

    #[test]
    fn test_extract_cache_reference_without_tag() {
        assert_eq!(
            extract_cache_reference("ghcr.io/org/image"),
            "ghcr.io/org/image:buildcache"
        );
    }

    #[test]
    fn test_extract_cache_reference_with_port() {
        assert_eq!(
            extract_cache_reference("localhost:5000/image:1.0.0"),
            "localhost:5000/image:buildcache"
        );
    }

    #[test]
    fn test_extract_cache_reference_simple_name() {
        assert_eq!(extract_cache_reference("nginx:latest"), "nginx:buildcache");
    }

    #[test]
    fn test_extract_cache_reference_nested_path() {
        assert_eq!(
            extract_cache_reference("registry.example.com/team/project/service:v2.3.4"),
            "registry.example.com/team/project/service:buildcache"
        );
    }
}

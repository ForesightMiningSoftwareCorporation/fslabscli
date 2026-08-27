//! Classify a published GitHub Release for the application-release pipeline.
//!
//! Classification keys on the release TAG, never the name: this same tool's
//! `publish` command creates library releases named from the tag or with no
//! name at all (measured in fsl_libs: 29 of 652 published releases carry a
//! null name), so a name-keyed rule would fail a third of library publishes
//! instead of skipping them. The library/application boundary therefore has
//! exactly one owner: [`LIBRARY_TAG_PREFIX`], shared with the publish path.
//!
//! An application release must carry equal tag and name matching
//! `<app>-<semver>`, where `<app>` names a package that declares
//! `[package.metadata.fslabs.release]`. The app segment forbids a dash, so
//! it can never swallow part of a version or collide with `publish-*`. An
//! empty name is a hard failure rather than a tag fallback: a fallback would
//! silently reinstate the tag as the contract.

use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};
use clap::Parser;
use serde::Serialize;

use super::types::AppReleaseConfig;
use crate::PrettyPrintable;

/// The tag namespace owned by library crate publishing (`fslabscli publish`
/// under Prow). Application release tags must NEVER start with this, or they
/// route to the library flow, nothing builds, and nothing says why.
pub const LIBRARY_TAG_PREFIX: &str = "publish-";

#[derive(Debug, Parser, Clone)]
#[command(
    about = "Classify a published GitHub Release as a library skip or a validated application release"
)]
pub struct Options {
    /// The release tag (github.event.release.tag_name); always present.
    #[arg(long, env = "RELEASE_TAG")]
    pub tag: String,
    /// The release name/title (github.event.release.name); may be empty.
    #[arg(long, env = "RELEASE_NAME", default_value = "")]
    pub name: String,
    /// The pre-release checkbox (github.event.release.prerelease).
    #[arg(long, env = "RELEASE_PRERELEASE")]
    pub prerelease: bool,
}

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// A library release created for a publish-* tag; skip cleanly.
    LibrarySkip,
    /// A valid application release; build it.
    Proceed,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Destination {
    Preview,
    Production,
}

impl Display for Destination {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Destination::Preview => write!(f, "preview"),
            Destination::Production => write!(f, "production"),
        }
    }
}

#[derive(Debug, Serialize, Clone)]
pub struct ClassifyResult {
    pub outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination: Option<Destination>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<AppReleaseConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package_directory: Option<String>,
}

impl Display for ClassifyResult {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self.outcome {
            Outcome::LibrarySkip => write!(f, "library release; nothing to bundle"),
            Outcome::Proceed => write!(
                f,
                "{} {} -> {}",
                self.app.as_deref().unwrap_or("?"),
                self.version.as_deref().unwrap_or("?"),
                self.destination.map(|d| d.to_string()).unwrap_or_default()
            ),
        }
    }
}

impl PrettyPrintable for ClassifyResult {
    fn pretty_print(&self) -> String {
        self.to_string()
    }
}

/// Split `<app>-<semver>`: the app segment is `[a-z0-9_]+` (no dash), the
/// remainder must parse as a semantic version.
pub fn parse_release_name(name: &str) -> Option<(String, semver::Version)> {
    // The app segment cannot contain '-', so the FIRST dash is the split.
    let (app, version) = name.split_once('-')?;
    if app.is_empty()
        || !app
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return None;
    }
    let version = semver::Version::parse(version).ok()?;
    Some((app.to_string(), version))
}

/// Discover applications: every git-tracked Cargo.toml declaring
/// `[package.metadata.fslabs.release]`, INCLUDING workspaces excluded from
/// the root one (fdk_apps is excluded by design and holds the only shipped
/// app), which is why this walks `git ls-files` rather than cargo metadata.
pub fn discover_apps(
    repo_root: &Path,
) -> anyhow::Result<BTreeMap<String, (PathBuf, AppReleaseConfig)>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["ls-files", "*Cargo.toml", "**/Cargo.toml"])
        .output()
        .context("git ls-files failed")?;
    if !output.status.success() {
        bail!(
            "git ls-files failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let mut apps = BTreeMap::new();
    for line in String::from_utf8(output.stdout)?.lines() {
        let manifest_path = repo_root.join(line);
        let Ok(contents) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        // Cheap pre-filter before parsing every manifest in the tree.
        if !contents.contains("[package.metadata.fslabs.release]") {
            continue;
        }
        let value: toml::Value = toml::from_str(&contents)
            .with_context(|| format!("unparseable manifest {}", manifest_path.display()))?;
        let Some(package) = value.get("package") else {
            continue;
        };
        let Some(name) = package.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        let Some(release) = package
            .get("metadata")
            .and_then(|m| m.get("fslabs"))
            .and_then(|f| f.get("release"))
        else {
            continue;
        };
        let config: AppReleaseConfig = release.clone().try_into().with_context(|| {
            format!(
                "invalid [package.metadata.fslabs.release] in {}",
                manifest_path.display()
            )
        })?;
        let dir = manifest_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        if apps.insert(name.to_string(), (dir, config)).is_some() {
            bail!("two packages named {name} declare [package.metadata.fslabs.release]");
        }
    }
    Ok(apps)
}

pub fn classify(
    options: &Options,
    apps: &BTreeMap<String, (PathBuf, AppReleaseConfig)>,
    repo_root: &Path,
) -> anyhow::Result<ClassifyResult> {
    if options.tag.is_empty() {
        bail!("release has no tag");
    }
    if options.tag.starts_with(LIBRARY_TAG_PREFIX) {
        return Ok(ClassifyResult {
            outcome: Outcome::LibrarySkip,
            app: None,
            version: None,
            destination: None,
            config: None,
            package_directory: None,
        });
    }

    // From here on this claims to be an application release, so every defect
    // is a hard failure.
    if options.name.is_empty() || options.name == "null" {
        bail!(
            "release name must be set to <app>-<version> (got an empty name; set the release title)"
        );
    }
    if options.name != options.tag {
        bail!(
            "release name ({}) must equal the release tag ({})",
            options.name,
            options.tag
        );
    }
    let Some((app, version)) = parse_release_name(&options.name) else {
        bail!(
            "release name must be <app>-<version> with a semantic version; got '{}'",
            options.name
        );
    };
    let Some((dir, config)) = apps.get(&app) else {
        bail!(
            "unknown application '{app}'; configured applications (packages with [package.metadata.fslabs.release]): {}",
            apps.keys().cloned().collect::<Vec<_>>().join(", ")
        );
    };

    let destination = if options.prerelease {
        Destination::Preview
    } else {
        if !version.pre.is_empty() || !version.build.is_empty() {
            bail!(
                "production versions must be plain X.Y.Z; '{version}' has a prerelease or build \
                 component. Tick the pre-release checkbox for one-off builds."
            );
        }
        Destination::Production
    };

    let package_directory = dir
        .strip_prefix(repo_root)
        .unwrap_or(dir)
        .to_string_lossy()
        .to_string();

    Ok(ClassifyResult {
        outcome: Outcome::Proceed,
        app: Some(app),
        version: Some(version.to_string()),
        destination: Some(destination),
        config: Some(config.clone()),
        package_directory: Some(package_directory),
    })
}

pub async fn run(options: &Options, repo_root: PathBuf) -> anyhow::Result<ClassifyResult> {
    let apps = discover_apps(&repo_root)?;
    classify(options, &apps, &repo_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apps() -> BTreeMap<String, (PathBuf, AppReleaseConfig)> {
        let mut m = BTreeMap::new();
        m.insert(
            "spatial_engine".to_string(),
            (
                PathBuf::from("/repo/fdk_apps/spatial_engine"),
                AppReleaseConfig {
                    verbose_name: "SpatialEngine".into(),
                    targets: vec![super::super::types::TARGET_WINDOWS.into()],
                    license_macos: None,
                },
            ),
        );
        m
    }

    fn opts(tag: &str, name: &str, prerelease: bool) -> Options {
        Options {
            tag: tag.into(),
            name: name.into(),
            prerelease,
        }
    }

    fn run(tag: &str, name: &str, prerelease: bool) -> anyhow::Result<ClassifyResult> {
        classify(&opts(tag, name, prerelease), &apps(), Path::new("/repo"))
    }

    #[test]
    fn plain_production_release_proceeds() {
        let r = run("spatial_engine-33.2.1", "spatial_engine-33.2.1", false).unwrap();
        assert_eq!(r.outcome, Outcome::Proceed);
        assert_eq!(r.version.as_deref(), Some("33.2.1"));
        assert_eq!(r.destination, Some(Destination::Production));
        assert_eq!(
            r.package_directory.as_deref(),
            Some("fdk_apps/spatial_engine")
        );
    }

    #[test]
    fn prerelease_routes_to_preview() {
        let r = run(
            "spatial_engine-33.2.1-rc.1",
            "spatial_engine-33.2.1-rc.1",
            true,
        )
        .unwrap();
        assert_eq!(r.destination, Some(Destination::Preview));
    }

    #[test]
    fn library_tags_skip_regardless_of_name_shape() {
        for (tag, name) in [
            ("publish-fdk-33.0.0", "publish-fdk-33.0.0"),
            ("publish-dagger_cli-32.0.0", "null"),
            ("publish-fse_state-0.14.2", ""),
        ] {
            let r = run(tag, name, false).unwrap();
            assert_eq!(r.outcome, Outcome::LibrarySkip, "tag {tag}");
        }
    }

    #[test]
    fn production_with_prerelease_version_fails() {
        assert!(
            run(
                "spatial_engine-33.2.1-rc.1",
                "spatial_engine-33.2.1-rc.1",
                false
            )
            .is_err()
        );
    }

    #[test]
    fn invalid_shapes_fail() {
        // Uppercase app segment.
        assert!(run("SpatialEngine-33.2.1", "SpatialEngine-33.2.1", false).is_err());
        // Two-component version.
        assert!(run("spatial_engine-33.2", "spatial_engine-33.2", false).is_err());
        // v-prefixed version.
        assert!(run("spatial_engine-v33.2.1", "spatial_engine-v33.2.1", false).is_err());
        // Unknown application (matches the grammar, is not configured).
        assert!(run("unknown_app-1.0.0", "unknown_app-1.0.0", false).is_err());
        // Crate-shaped name that mismatches the tag.
        assert!(run("fse_state-0.14.2", "fse_state-0.14.3", false).is_err());
        // No tag at all.
        assert!(run("", "spatial_engine-33.2.1", false).is_err());
    }

    #[test]
    fn empty_name_is_a_hard_failure_never_a_tag_fallback() {
        assert!(run("spatial_engine-33.2.1", "", false).is_err());
        assert!(run("spatial_engine-33.2.1", "null", false).is_err());
    }

    #[test]
    fn unknown_app_error_lists_configured_apps() {
        let err = run("unknown_app-1.0.0", "unknown_app-1.0.0", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("spatial_engine"), "{err}");
    }
}

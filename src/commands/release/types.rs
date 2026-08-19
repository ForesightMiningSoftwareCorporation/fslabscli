//! The application-release contract types: the manifest, the version index,
//! and the channel pointers. These serde definitions are the single source of
//! truth for the object shapes documented in fsl_libs'
//! `software_guide/content/docs/releases/`; clients gate on `schema_version`
//! and refuse unknown majors, so fields may be added freely within a major
//! but never repurposed or removed.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SCHEMA_VERSION: u64 = 1;

pub const TARGET_WINDOWS: &str = "x86_64-pc-windows-gnu";
pub const TARGET_MACOS: &str = "aarch64-apple-darwin";
pub const TARGET_LINUX: &str = "x86_64-unknown-linux-gnu";

/// Immutable record of one published application version. Its existence under
/// `<app>/<version>/manifest.json` is the atomic commit point of a
/// publication: it is written last, after every artifact object has been read
/// back and digest-verified, and it is never rewritten.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub schema_version: u64,
    pub app: String,
    pub version: String,
    pub prerelease: bool,
    /// 40-hex commit the artifacts were built from, resolved from the release
    /// tag itself (never from `target_commitish`, which is a branch name).
    pub source_revision: String,
    pub release_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_run: Option<String>,
    pub published_at: String,
    pub artifacts: Vec<ManifestArtifact>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestArtifact {
    pub target: String,
    pub format: ArtifactFormat,
    pub filename: String,
    pub url: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub signature: ArtifactSignature,
    /// Linux artifacts record the glibc floor the build container fixed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glibc_floor: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactFormat {
    Msi,
    Dmg,
    Deb,
    Appimage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ArtifactSignature {
    /// Windows: the OS verifies on install; publication re-verifies with
    /// osslsigncode.
    Authenticode,
    /// macOS: notarization is stapler-validated on the build host; a Linux
    /// publisher cannot re-check it.
    AppleNotarized,
    /// Linux has no OS signature: a detached ASCII-armoured signature whose
    /// key is published at `keys/fsl-release-linux.asc` in the production
    /// bucket. The manifest sha256 stays the primary trust anchor.
    OpenpgpDetached {
        url: String,
        key_fingerprint: String,
    },
}

/// Derived list of published versions for one application. The set of
/// committed manifests is the authority; this document is a convenience for
/// humans and the healthcheck, updated by compare-and-swap and repairable at
/// any time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Index {
    pub schema_version: u64,
    pub app: String,
    pub updated_at: String,
    pub versions: Vec<IndexEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    pub version: String,
    pub published_at: String,
    pub source_revision: String,
    pub manifest: String,
    pub targets: Vec<String>,
}

/// One version pointer per channel per target for one application. Only the
/// promotion flow writes this document; the publication credential cannot.
/// Clients read exactly one field: `.channels.<channel>.<target-triple>`,
/// and an absent key means "no pointer" with no fallback allowed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Channels {
    pub schema_version: u64,
    pub app: String,
    pub updated_at: String,
    pub manifest_base: String,
    pub channels: ChannelPointers,
    #[serde(default)]
    pub provenance: Vec<ProvenanceEntry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChannelPointers {
    #[serde(default)]
    pub latest: BTreeMap<String, String>,
    #[serde(default)]
    pub stable: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Latest,
    Stable,
}

impl std::fmt::Display for Channel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Channel::Latest => write!(f, "latest"),
            Channel::Stable => write!(f, "stable"),
        }
    }
}

impl ChannelPointers {
    pub fn get_mut(&mut self, channel: Channel) -> &mut BTreeMap<String, String> {
        match channel {
            Channel::Latest => &mut self.latest,
            Channel::Stable => &mut self.stable,
        }
    }

    pub fn get(&self, channel: Channel) -> &BTreeMap<String, String> {
        match channel {
            Channel::Latest => &self.latest,
            Channel::Stable => &self.stable,
        }
    }
}

/// Audit record of a pointer move, capped at the most recent 100 entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvenanceEntry {
    pub channel: Channel,
    pub targets: Vec<String>,
    /// Previous pointer per target (absent target key = no previous pointer).
    pub from: BTreeMap<String, Option<String>>,
    pub to: String,
    pub backward: bool,
    pub reason: Option<String>,
    pub moved_at: String,
    pub moved_by: String,
    pub run: String,
}

pub const PROVENANCE_CAP: usize = 100;

/// Per-application release configuration, read from
/// `[package.metadata.fslabs.release]` in the app crate's Cargo.toml. The
/// presence of this table is what makes a package an application the release
/// pipeline knows about.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AppReleaseConfig {
    /// The binary/product name (e.g. "SpatialEngine").
    pub verbose_name: String,
    /// Target triples this application ships for.
    pub targets: Vec<String>,
    /// Path to the macOS EULA, relative to the package directory. Omitted
    /// entirely when absent: `jq -r` renders a JSON null as the literal string
    /// "null", which the macOS leg would pass to create-dmg as `--eula <dir>/null`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license_macos: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_serialization_shape_is_the_contract() {
        let sig = ArtifactSignature::OpenpgpDetached {
            url: "https://x/y.asc".into(),
            key_fingerprint: "ABCD".into(),
        };
        let v = serde_json::to_value(&sig).unwrap();
        assert_eq!(v["kind"], "openpgp-detached");
        assert_eq!(v["url"], "https://x/y.asc");
        let plain = serde_json::to_value(ArtifactSignature::Authenticode).unwrap();
        assert_eq!(plain["kind"], "authenticode");
    }

    #[test]
    fn absent_license_macos_is_omitted_not_null() {
        // `jq -r .config.license_macos` renders a JSON null as the literal
        // string "null", which the macOS leg passed to create-dmg as
        // `--eula <dir>/null`. The key must not be present at all.
        let config = AppReleaseConfig {
            verbose_name: "App".into(),
            targets: vec![TARGET_LINUX.into()],
            license_macos: None,
        };
        let v = serde_json::to_value(&config).unwrap();
        assert!(
            v.get("license_macos").is_none(),
            "serialized as {v}, which jq -r renders as the string \"null\""
        );
        let with = AppReleaseConfig {
            license_macos: Some("eula.rtf".into()),
            ..config
        };
        assert_eq!(
            serde_json::to_value(&with).unwrap()["license_macos"],
            "eula.rtf"
        );
    }

    #[test]
    fn channels_roundtrip_preserves_unknown_free_shape() {
        let json = serde_json::json!({
            "schema_version": 1,
            "app": "spatial_engine",
            "updated_at": "2026-08-18T00:00:00Z",
            "manifest_base": "https://api.s3.fsl.dev/fsl-releases",
            "channels": {"latest": {"aarch64-apple-darwin": "1.0.0"}, "stable": {}},
        });
        let ch: Channels = serde_json::from_value(json).unwrap();
        assert_eq!(ch.channels.latest.get(TARGET_MACOS).unwrap(), "1.0.0");
        assert!(ch.provenance.is_empty());
    }
}

//! Resolve a download exactly as an installed client must: this command is
//! the client resolution contract in executable form
//! (`software_guide/content/docs/releases/client_resolution.md` is the same
//! contract in prose).
//!
//! Two credential-less reads: channels.json for the pointer at
//! `.channels.<channel>.<target>` (an ABSENT key means "no pointer" and the
//! client must not fall back to another target, channel, or the index), then
//! that version's manifest for the URL, sha256 and (Linux) the detached
//! signature record. Both documents carry `schema_version`; an unknown major
//! is refused, never guessed at. With `--download`, the artifact is fetched,
//! digest-verified, and on Linux signature-verified against the published
//! key, whose fingerprint must equal the manifest's.

use std::fmt::{Display, Formatter};
use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::Parser;
use serde::Serialize;

use super::types::{
    ArtifactFormat, ArtifactSignature, Channel, Channels, Manifest, ManifestArtifact,
    SCHEMA_VERSION,
};
use crate::PrettyPrintable;

#[derive(Debug, Parser, Clone)]
#[command(about = "Resolve a download exactly as an installed client would")]
pub struct Options {
    #[arg(long)]
    pub app: String,
    #[arg(long, value_enum)]
    pub channel: Channel,
    /// Target triple, e.g. x86_64-unknown-linux-gnu.
    #[arg(long)]
    pub target: String,
    /// Required when the target ships more than one format (Linux: deb or
    /// appimage).
    #[arg(long, value_enum)]
    pub format: Option<ArtifactFormat>,
    /// Download, digest-verify, and (Linux) signature-verify.
    #[arg(long, default_value_t = false)]
    pub download: bool,
    /// Directory downloads land in.
    #[arg(long, default_value = ".")]
    pub output_dir: PathBuf,
    #[arg(
        long,
        env = "RELEASE_PUBLIC_BASE_URL",
        default_value = "https://api.s3.fsl.dev"
    )]
    pub base_url: String,
    #[arg(long, default_value = "fsl-releases-channels")]
    pub channels_bucket: String,
    #[arg(long, default_value = "fsl-releases")]
    pub prod_bucket: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct ResolveResult {
    pub version: String,
    pub url: String,
    pub sha256: String,
    pub signature_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downloaded: Option<PathBuf>,
    pub digest_verified: bool,
    pub signature_verified: bool,
}

impl Display for ResolveResult {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "version={}", self.version)?;
        writeln!(f, "url={}", self.url)?;
        writeln!(f, "sha256={}", self.sha256)?;
        write!(f, "signature={}", self.signature_kind)?;
        if let Some(path) = &self.downloaded {
            write!(
                f,
                "\ndownloaded {} (digest {}, signature {})",
                path.display(),
                if self.digest_verified {
                    "verified"
                } else {
                    "NOT VERIFIED"
                },
                if self.signature_verified {
                    "verified"
                } else {
                    "n/a"
                },
            )?;
        }
        Ok(())
    }
}

impl PrettyPrintable for ResolveResult {
    fn pretty_print(&self) -> String {
        self.to_string()
    }
}

/// Thin shims over the shared client so call sites stay one line.
pub(crate) async fn http_get(url: &str) -> anyhow::Result<Vec<u8>> {
    super::http::get_bytes(&super::http::client()?, url).await
}

async fn http_get_to_file(url: &str, path: &std::path::Path) -> anyhow::Result<String> {
    super::http::get_to_file(&super::http::client()?, url, path).await
}

/// Gate on `schema_version` BEFORE deserializing the full shape: an unknown
/// major may not even parse into our types, and the contract is to refuse,
/// never to guess.
fn gate_schema(bytes: &[u8], what: &str) -> anyhow::Result<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .with_context(|| format!("{what} document is not valid JSON"))?;
    if value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        != Some(SCHEMA_VERSION)
    {
        bail!(
            "unknown {what} schema_version {}; refusing to guess",
            value
                .get("schema_version")
                .map(|v| v.to_string())
                .unwrap_or_else(|| "null".into())
        );
    }
    Ok(value)
}

/// Select the single artifact for a target (and format, where a target ships
/// more than one, e.g. deb vs appimage on Linux).
fn select_artifact<'a>(
    manifest: &'a Manifest,
    target: &str,
    format: Option<ArtifactFormat>,
) -> anyhow::Result<&'a ManifestArtifact> {
    let matches: Vec<&ManifestArtifact> = manifest
        .artifacts
        .iter()
        .filter(|a| a.target == target)
        .filter(|a| format.is_none_or(|f| a.format == f))
        .collect();
    match matches.len() {
        0 => bail!(
            "manifest for {} {} has no artifact for {target}{}",
            manifest.app,
            manifest.version,
            format
                .map(|f| format!(" ({})", format_name(f)))
                .unwrap_or_default()
        ),
        1 => Ok(matches[0]),
        n => bail!(
            "ambiguous: {n} artifacts for {target}; pass a format ({})",
            matches
                .iter()
                .map(|a| format_name(a.format))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn format_name(format: ArtifactFormat) -> &'static str {
    match format {
        ArtifactFormat::Msi => "msi",
        ArtifactFormat::Dmg => "dmg",
        ArtifactFormat::Deb => "deb",
        ArtifactFormat::Appimage => "appimage",
    }
}

/// The serde `kind` string of a signature record, verbatim from the contract.
fn signature_kind(signature: &ArtifactSignature) -> &'static str {
    match signature {
        ArtifactSignature::Authenticode => "authenticode",
        ArtifactSignature::AppleNotarized => "apple-notarized",
        ArtifactSignature::OpenpgpDetached { .. } => "openpgp-detached",
    }
}

use super::keyring::TempKeyring;

pub async fn run(options: &Options) -> anyhow::Result<ResolveResult> {
    // Step 1: the channel pointer.
    let channels_url = format!(
        "{}/{}/{}/channels.json",
        options.base_url, options.channels_bucket, options.app
    );
    let bytes = http_get(&channels_url)
        .await
        .with_context(|| format!("cannot fetch {channels_url}"))?;
    let mut value = gate_schema(&bytes, "channels")?;
    // The reference resolver defaults an absent manifest_base to the
    // production bucket; mirror that before deserializing the required field.
    if value.get("manifest_base").is_none() {
        value["manifest_base"] =
            serde_json::Value::String(format!("{}/{}", options.base_url, options.prod_bucket));
    }
    let channels: Channels = serde_json::from_value(value)
        .with_context(|| format!("{channels_url} does not deserialize as a channels document"))?;
    let Some(version) = channels
        .channels
        .get(options.channel)
        .get(&options.target)
        .cloned()
    else {
        bail!(
            "no pointer for target {} on channel {}; a client must not fall back to another target or channel",
            options.target,
            options.channel
        );
    };

    // Step 2: the manifest, from the CLIENT'S OWN production base, never from
    // `channels.manifest_base`.
    //
    // channels.json is written by the promotion credential, whose whole point
    // is that it cannot write artifacts. Building the manifest URL from a field
    // inside that document hands it exactly that power: point manifest_base at
    // any reachable host and every digest check downstream is satisfied against
    // the attacker's own manifest, while a `"kind": "authenticode"` entry skips
    // client signature verification altogether. manifest_base stays in the
    // contract as informational only.
    let manifest_base = format!("{}/{}", options.base_url, options.prod_bucket);
    if channels.manifest_base != manifest_base {
        tracing::warn!(
            "channels.json manifest_base is {} but resolution uses {manifest_base}; \
             the field is informational and is not trusted for resolution",
            channels.manifest_base
        );
    }
    let manifest_url = format!("{manifest_base}/{}/{}/manifest.json", options.app, version);
    let bytes = http_get(&manifest_url)
        .await
        .with_context(|| format!("cannot fetch {manifest_url}"))?;
    let value = gate_schema(&bytes, "manifest")?;
    let manifest: Manifest = serde_json::from_value(value)
        .with_context(|| format!("{manifest_url} does not deserialize as a manifest"))?;

    let artifact = select_artifact(&manifest, &options.target, options.format)?;
    let mut result = ResolveResult {
        version: version.clone(),
        url: artifact.url.clone(),
        sha256: artifact.sha256.clone(),
        signature_kind: signature_kind(&artifact.signature).to_string(),
        downloaded: None,
        digest_verified: false,
        signature_verified: false,
    };
    if !options.download {
        return Ok(result);
    }

    std::fs::create_dir_all(&options.output_dir)
        .with_context(|| format!("cannot create {}", options.output_dir.display()))?;
    let filename = artifact
        .url
        .rsplit('/')
        .next()
        .expect("rsplit yields at least one segment");
    let path = options.output_dir.join(filename);
    let got = http_get_to_file(&artifact.url, &path)
        .await
        .with_context(|| format!("download failed: {}", artifact.url))?;
    if got != artifact.sha256 {
        bail!(
            "digest mismatch: manifest {}, downloaded {got}; do not run this file",
            artifact.sha256
        );
    }
    result.digest_verified = true;

    if let ArtifactSignature::OpenpgpDetached {
        url: sig_url,
        key_fingerprint,
    } = &artifact.signature
    {
        let sig_path = options.output_dir.join(format!("{filename}.asc"));
        let sig_bytes = http_get(sig_url)
            .await
            .with_context(|| format!("cannot fetch detached signature {sig_url}"))?;
        std::fs::write(&sig_path, sig_bytes)
            .with_context(|| format!("cannot write {}", sig_path.display()))?;
        let key_url = format!(
            "{}/{}/keys/fsl-release-linux.asc",
            options.base_url, options.prod_bucket
        );
        let key = http_get(&key_url)
            .await
            .with_context(|| format!("cannot import the published signing key from {key_url}"))?;
        let keyring = TempKeyring::import(&key)
            .with_context(|| format!("cannot import the published signing key from {key_url}"))?;
        if !keyring.fingerprints()?.iter().any(|f| f == key_fingerprint) {
            bail!("published key fingerprint does not match the manifest's {key_fingerprint}");
        }
        keyring
            .verify_detached(&sig_path, &path)
            .context("detached signature verification failed; do not run this file")?;
        result.signature_verified = true;
    }
    result.downloaded = Some(path);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::super::types::{TARGET_LINUX, TARGET_MACOS, TARGET_WINDOWS};
    use super::*;

    fn artifact(
        target: &str,
        format: ArtifactFormat,
        signature: ArtifactSignature,
    ) -> ManifestArtifact {
        ManifestArtifact {
            target: target.to_string(),
            format,
            filename: format!("app.{}", format_name(format)),
            url: format!("https://x/b/app/1.0.0/{target}/app.{}", format_name(format)),
            size_bytes: 1,
            sha256: "0".repeat(64),
            signature,
            glibc_floor: None,
        }
    }

    fn manifest() -> Manifest {
        Manifest {
            schema_version: SCHEMA_VERSION,
            app: "app".into(),
            version: "1.0.0".into(),
            prerelease: false,
            source_revision: "0123456789abcdef0123456789abcdef01234567".into(),
            release_name: "app-1.0.0".into(),
            release_id: None,
            workflow_run: None,
            published_at: "2026-08-18T00:00:00Z".into(),
            artifacts: vec![
                artifact(
                    TARGET_WINDOWS,
                    ArtifactFormat::Msi,
                    ArtifactSignature::Authenticode,
                ),
                artifact(
                    TARGET_LINUX,
                    ArtifactFormat::Deb,
                    ArtifactSignature::OpenpgpDetached {
                        url: "https://x/b/a.asc".into(),
                        key_fingerprint: "F".into(),
                    },
                ),
                artifact(
                    TARGET_LINUX,
                    ArtifactFormat::Appimage,
                    ArtifactSignature::OpenpgpDetached {
                        url: "https://x/b/b.asc".into(),
                        key_fingerprint: "F".into(),
                    },
                ),
            ],
        }
    }

    #[test]
    fn schema_gate_accepts_ours_and_refuses_everything_else() {
        assert!(gate_schema(br#"{"schema_version": 1}"#, "channels").is_ok());
        let err = gate_schema(br#"{"schema_version": 2}"#, "channels")
            .unwrap_err()
            .to_string();
        assert!(err.contains("refusing to guess"), "{err}");
        assert!(err.contains("channels schema_version 2"), "{err}");
        assert!(gate_schema(br#"{}"#, "manifest").is_err());
        assert!(gate_schema(b"not json", "manifest").is_err());
    }

    #[test]
    fn single_artifact_target_needs_no_format() {
        let m = manifest();
        let a = select_artifact(&m, TARGET_WINDOWS, None).unwrap();
        assert_eq!(a.format, ArtifactFormat::Msi);
    }

    #[test]
    fn multi_artifact_target_without_format_is_ambiguous_and_lists_formats() {
        let m = manifest();
        let err = select_artifact(&m, TARGET_LINUX, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("ambiguous"), "{err}");
        assert!(err.contains("deb, appimage"), "{err}");
    }

    #[test]
    fn format_filters_to_exactly_one() {
        let m = manifest();
        let a = select_artifact(&m, TARGET_LINUX, Some(ArtifactFormat::Appimage)).unwrap();
        assert_eq!(a.format, ArtifactFormat::Appimage);
    }

    #[test]
    fn absent_target_or_format_is_an_error() {
        let m = manifest();
        assert!(select_artifact(&m, TARGET_MACOS, None).is_err());
        assert!(select_artifact(&m, TARGET_LINUX, Some(ArtifactFormat::Msi)).is_err());
    }

    #[test]
    fn signature_kind_strings_match_the_serde_contract() {
        // The strings a client sees must be byte-identical to the serialized
        // `kind` field.
        for signature in [
            ArtifactSignature::Authenticode,
            ArtifactSignature::AppleNotarized,
            ArtifactSignature::OpenpgpDetached {
                url: "u".into(),
                key_fingerprint: "f".into(),
            },
        ] {
            let serialized = serde_json::to_value(&signature).unwrap();
            assert_eq!(serialized["kind"], signature_kind(&signature));
        }
    }
}

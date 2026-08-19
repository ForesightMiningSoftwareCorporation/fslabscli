//! Atomic publication of a signed artifact set, ordered so nothing broken
//! can ever become published-and-immutable:
//!
//! 1. Collect the artifact directory and assert completeness against the
//!    app's configured targets (windows: msi, macos: dmg, linux: deb AND
//!    appimage, each linux artifact with a sibling `.asc`).
//! 2. Re-verify signatures against the bytes: `osslsigncode verify` for the
//!    MSI, `gpg --verify` against the PUBLISHED key (fetched from
//!    `keys/fsl-release-linux.asc` in the production bucket) for the Linux
//!    detached signatures. DMG notarization is stapler-validated on the
//!    macOS build host; a Linux publisher cannot re-check it.
//! 3. Digest everything and build the [`Manifest`].
//! 4. Production only: [`ReleaseStore::assert_monotonic`] over committed
//!    manifests.
//! 5. `put_immutable` every artifact under `<app>/<version>/<target>/`,
//!    then read each back and digest-compare (`verify_key`).
//! 6. `put_immutable` the manifest LAST: its existence is the atomic commit
//!    point. A failed production publication permanently spends its version
//!    number; the retry is a new version.
//! 7. `cas_update` the derived, repairable index (append entry, dedupe by
//!    version, sort descending by semver).
//!
//! GitHub-release attachment of convenience copies stays in the workflow
//! (a `gh release upload` step); this command owns only the bucket.

use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};
use clap::Parser;
use serde::Serialize;

use super::classify::Destination;
use super::http;
use super::keyring::TempKeyring;
use super::store::{ReleaseStore, sha256_hex};
use super::types::{
    ArtifactFormat, ArtifactSignature, Index, IndexEntry, Manifest, ManifestArtifact,
    SCHEMA_VERSION, TARGET_LINUX, TARGET_MACOS, TARGET_WINDOWS,
};
use crate::PrettyPrintable;

#[derive(Debug, Parser, Clone)]
#[command(
    about = "Atomically publish signed artifacts: digests, manifest-last, index CAS",
    disable_version_flag = true
)]
pub struct Options {
    #[arg(long)]
    pub app: String,
    #[arg(long)]
    pub version: String,
    #[arg(long, value_enum)]
    pub destination: Destination,
    /// 40-hex commit the artifacts were built from.
    #[arg(long)]
    pub source_revision: String,
    #[arg(long)]
    pub release_name: String,
    #[arg(long)]
    pub release_id: Option<String>,
    #[arg(long)]
    pub run_url: Option<String>,
    /// Directory holding every signed artifact (flat).
    #[arg(long)]
    pub artifacts_dir: PathBuf,
    /// Comma-separated target triples the app is configured to ship
    /// (completeness is asserted against this, not inferred from the dir).
    #[arg(long, value_delimiter = ',')]
    pub targets: Vec<String>,
    /// Public base URL recorded in manifest artifact URLs.
    #[arg(
        long,
        env = "RELEASE_PUBLIC_BASE_URL",
        default_value = "https://api.s3.fsl.dev"
    )]
    pub base_url: String,
    #[arg(long, default_value = "fsl-releases")]
    pub prod_bucket: String,
    #[arg(long, default_value = "fsl-releases-preview")]
    pub preview_bucket: String,
    /// Fingerprint of the Linux signing key (required when Linux artifacts
    /// are present; recorded in each detached-signature entry).
    #[arg(long)]
    pub linux_fingerprint: Option<String>,
    #[arg(long, default_value = "2.31")]
    pub glibc_floor: String,
    /// Skip the signature shell-outs (tests and stores without the key).
    #[arg(long, default_value_t = false)]
    pub skip_signature_verification: bool,
}

#[derive(Debug, Serialize, Clone)]
pub struct PublishResult {
    pub bucket: String,
    pub manifest_key: String,
    pub artifact_count: usize,
    /// Per-artifact object keys that were written and read back.
    pub written: Vec<String>,
    /// Versions that already had committed manifests (production only).
    pub previously_published: Vec<String>,
    /// Targets present in the manifest, for the auto-latest promotion.
    pub manifest_targets: Vec<String>,
    /// Local path of the manifest copy for GH-release attachment.
    pub manifest_path: String,
}

impl Display for PublishResult {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "published {} artifact(s); commit point s3://{}/{}",
            self.artifact_count, self.bucket, self.manifest_key
        )?;
        for key in &self.written {
            writeln!(f, "  {key}")?;
        }
        Ok(())
    }
}

impl PrettyPrintable for PublishResult {
    fn pretty_print(&self) -> String {
        self.to_string()
    }
}

/// Map a directory of signed artifacts to (file, target, format) triples and
/// assert completeness against the configured targets. Pure; unit-tested.
pub fn collect_artifacts(
    dir: &std::path::Path,
    targets: &[String],
) -> anyhow::Result<Vec<(PathBuf, String, super::types::ArtifactFormat)>> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("cannot read artifact directory {}", dir.display()))?
        .map(|entry| Ok(entry?.path()))
        .collect::<anyhow::Result<_>>()?;
    paths.sort();

    let mut artifacts = Vec::new();
    for path in paths {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .with_context(|| format!("non-UTF-8 artifact name: {}", path.display()))?
            .to_string();
        let (target, format) = if name.ends_with(".msi") {
            (TARGET_WINDOWS, ArtifactFormat::Msi)
        } else if name.ends_with(".dmg") {
            (TARGET_MACOS, ArtifactFormat::Dmg)
        } else if name.ends_with(".deb") {
            (TARGET_LINUX, ArtifactFormat::Deb)
        } else if name.ends_with(".AppImage") {
            (TARGET_LINUX, ArtifactFormat::Appimage)
        } else if name.ends_with(".asc") {
            // Recorded alongside the artifact they sign, below.
            continue;
        } else {
            bail!("unrecognised artifact {name}");
        };
        if target == TARGET_LINUX && !asc_sibling(&path).is_file() {
            bail!("missing detached signature {name}.asc");
        }
        artifacts.push((path, target.to_string(), format));
    }
    if artifacts.is_empty() {
        bail!("no artifacts found in {}", dir.display());
    }

    // Completeness is asserted against the CONFIGURED target list: fail-fast
    // is off in the build matrix and a re-run can hand over a partial set, so
    // presence is never inferred from the directory.
    for target in targets {
        let formats: Vec<ArtifactFormat> = artifacts
            .iter()
            .filter(|(_, t, _)| t == target)
            .map(|&(_, _, format)| format)
            .collect();
        if target == TARGET_LINUX {
            if formats.len() != 2
                || !formats.contains(&ArtifactFormat::Deb)
                || !formats.contains(&ArtifactFormat::Appimage)
            {
                bail!(
                    "target {target} requires exactly one .deb and one .AppImage; found {} artifact(s)",
                    formats.len()
                );
            }
        } else if formats.len() != 1 {
            bail!(
                "target {target} requires exactly one artifact; found {}",
                formats.len()
            );
        }
    }
    for (path, target, _) in &artifacts {
        if !targets.iter().any(|t| t == target) {
            bail!(
                "artifact {} is for target {target}, which the app is not configured to ship",
                path.file_name().unwrap_or_default().to_string_lossy()
            );
        }
    }
    Ok(artifacts)
}

fn asc_sibling(path: &Path) -> PathBuf {
    let mut asc = path.to_path_buf().into_os_string();
    asc.push(".asc");
    PathBuf::from(asc)
}

/// Build the manifest and per-file object keys. Pure; unit-tested.
#[allow(clippy::too_many_arguments)]
pub fn build_manifest(
    options: &Options,
    bucket: &str,
    artifacts: &[(PathBuf, String, super::types::ArtifactFormat)],
    digests: &BTreeMap<PathBuf, (String, u64)>,
) -> anyhow::Result<(super::types::Manifest, Vec<(PathBuf, String)>)> {
    if options.source_revision.len() != 40
        || !options
            .source_revision
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        bail!(
            "source revision must be 40 hex, got '{}'",
            options.source_revision
        );
    }
    if artifacts.is_empty() {
        bail!("no artifacts to publish");
    }

    let mut uploads = Vec::new();
    let mut manifest_artifacts = Vec::new();
    for (path, target, format) in artifacts {
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .with_context(|| format!("non-UTF-8 artifact name: {}", path.display()))?
            .to_string();
        let (sha256, size_bytes) = digests
            .get(path)
            .with_context(|| format!("no digest recorded for {}", path.display()))?
            .clone();
        let key = format!("{}/{}/{target}/{filename}", options.app, options.version);
        let url = format!("{}/{bucket}/{key}", options.base_url);
        uploads.push((path.clone(), key));

        let (signature, glibc_floor) = match format {
            ArtifactFormat::Msi => (ArtifactSignature::Authenticode, None),
            ArtifactFormat::Dmg => (ArtifactSignature::AppleNotarized, None),
            ArtifactFormat::Deb | ArtifactFormat::Appimage => {
                let Some(fingerprint) = options
                    .linux_fingerprint
                    .as_deref()
                    .filter(|f| !f.is_empty())
                else {
                    bail!("--linux-fingerprint is required when Linux artifacts are present");
                };
                let asc_key = format!(
                    "{}/{}/{target}/{filename}.asc",
                    options.app, options.version
                );
                uploads.push((asc_sibling(path), asc_key.clone()));
                (
                    ArtifactSignature::OpenpgpDetached {
                        url: format!("{}/{bucket}/{asc_key}", options.base_url),
                        key_fingerprint: fingerprint.to_string(),
                    },
                    Some(options.glibc_floor.clone()),
                )
            }
        };
        manifest_artifacts.push(ManifestArtifact {
            target: target.clone(),
            format: *format,
            filename,
            url,
            size_bytes,
            sha256,
            signature,
            glibc_floor,
        });
    }

    let manifest = Manifest {
        schema_version: SCHEMA_VERSION,
        app: options.app.clone(),
        version: options.version.clone(),
        prerelease: matches!(options.destination, Destination::Preview),
        source_revision: options.source_revision.clone(),
        release_name: options.release_name.clone(),
        release_id: options.release_id.clone(),
        workflow_run: options.run_url.clone(),
        published_at: now_utc_seconds(),
        artifacts: manifest_artifacts,
    };
    Ok((manifest, uploads))
}

fn now_utc_seconds() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Unique targets present in the manifest, sorted.
fn manifest_targets(manifest: &Manifest) -> Vec<String> {
    let mut targets: Vec<String> = manifest
        .artifacts
        .iter()
        .map(|a| a.target.clone())
        .collect();
    targets.sort();
    targets.dedup();
    targets
}

/// Fold one publication into the index: dedupe by version keeping the NEW
/// entry, sort descending with real semver precedence (preview versions may
/// carry prerelease suffixes; anything unparseable sorts last), and stamp
/// `updated_at`. Pure; runs inside the CAS retry loop.
fn index_with_entry(mut index: Index, entry: IndexEntry, now: &str) -> Index {
    index
        .versions
        .retain(|existing| existing.version != entry.version);
    index.versions.push(entry);
    index.versions.sort_by(|a, b| {
        use std::cmp::Ordering;
        match (
            semver::Version::parse(&a.version).ok(),
            semver::Version::parse(&b.version).ok(),
        ) {
            (Some(va), Some(vb)) => vb.cmp(&va),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => b.version.cmp(&a.version),
        }
    });
    index.updated_at = now.to_string();
    index
}

/// Re-verify each signature against the artifact BYTES, never build-job
/// state: osslsigncode for the MSI, gpg against the PUBLISHED key for the
/// Linux detached signatures. The DMG is deliberately absent here:
/// notarization is stapler-validated on the macOS build host, and a Linux
/// publisher cannot re-check it.
async fn verify_signatures(
    options: &Options,
    artifacts: &[(PathBuf, String, ArtifactFormat)],
) -> anyhow::Result<()> {
    for (path, _, format) in artifacts {
        if *format != ArtifactFormat::Msi {
            continue;
        }
        let output = Command::new("osslsigncode")
            .arg("verify")
            .arg("-in")
            .arg(path)
            .output()
            .context("cannot run osslsigncode")?;
        if !output.status.success() {
            bail!(
                "Authenticode verification failed for {}: {}{}",
                path.file_name().unwrap_or_default().to_string_lossy(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    let linux: Vec<&PathBuf> = artifacts
        .iter()
        .filter(|(_, _, format)| matches!(format, ArtifactFormat::Deb | ArtifactFormat::Appimage))
        .map(|(path, _, _)| path)
        .collect();
    if linux.is_empty() {
        return Ok(());
    }
    let key_url = format!(
        "{}/{}/keys/fsl-release-linux.asc",
        options.base_url, options.prod_bucket
    );
    let key = http::get_bytes(&http::client()?, &key_url).await.with_context(|| {
        format!("cannot fetch the published Linux signing key from {key_url}; publish it before releasing")
    })?;
    let keyring = TempKeyring::import(&key)
        .with_context(|| format!("cannot import the published Linux signing key from {key_url}"))?;
    for path in linux {
        keyring
            .verify_detached(&asc_sibling(path), path)
            .with_context(|| {
                format!(
                    "detached signature verification failed for {}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                )
            })?;
    }
    Ok(())
}

pub async fn run(options: &Options) -> anyhow::Result<PublishResult> {
    let artifacts = collect_artifacts(&options.artifacts_dir, &options.targets)?;

    if !options.skip_signature_verification {
        verify_signatures(options, &artifacts).await?;
    }

    let mut digests = BTreeMap::new();
    for (path, _, _) in &artifacts {
        let bytes =
            std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        digests.insert(path.clone(), (sha256_hex(&bytes), bytes.len() as u64));
    }

    let bucket = match options.destination {
        Destination::Production => options.prod_bucket.as_str(),
        Destination::Preview => options.preview_bucket.as_str(),
    };
    let (manifest, uploads) = build_manifest(options, bucket, &artifacts, &digests)?;

    let store = ReleaseStore::from_env(bucket)?;
    // Production versions increase strictly over COMMITTED manifests; a
    // preview may republish freely under new prerelease suffixes.
    let previously_published = match options.destination {
        Destination::Production => {
            store
                .assert_monotonic(&options.app, &options.version)
                .await?
        }
        Destination::Preview => Vec::new(),
    };

    let mut written = Vec::new();
    // Detached signatures are uploaded but are not manifest artifacts, so the
    // manifest carries no digest for them. Record what we sent, purely so they
    // can be read back below.
    let mut signature_digests = Vec::new();
    for (path, key) in &uploads {
        let bytes =
            std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        if !digests.contains_key(path) {
            signature_digests.push((key.clone(), sha256_hex(&bytes)));
        }
        store.put_immutable(key, bytes).await?;
        written.push(key.clone());
    }
    // Read back and digest-compare while no manifest exists yet, so nothing
    // broken can become published-and-immutable.
    //
    // Manifest artifacts are compared against the digest THE MANIFEST RECORDS,
    // not against a digest recomputed from the bytes we happened to send. The
    // latter is self-consistent by construction and would pass even if the file
    // changed between the digesting read and the upload read, committing a
    // manifest whose recorded digest no object matches. Going through
    // `key_from_url` also keeps that mapping exercised here rather than only in
    // promote.
    for artifact in &manifest.artifacts {
        let key = store.key_from_url(&artifact.url)?;
        store.verify_key(key, &artifact.sha256).await?;
    }
    // The signatures, which the loop above cannot cover: a truncated .asc would
    // otherwise ship unverified and fail every client verification permanently.
    for (key, want) in &signature_digests {
        store.verify_key(key, want).await?;
    }

    // The manifest is written LAST: its existence is the atomic commit
    // point. An AlreadyExists from the store propagates as-is; that version
    // number is spent.
    let manifest_key = format!("{}/{}/manifest.json", options.app, options.version);
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    store
        .put_immutable(&manifest_key, manifest_bytes.clone())
        .await?;

    // The index is derived and repairable; the committed manifests stay the
    // authority.
    let now = now_utc_seconds();
    let entry = IndexEntry {
        version: manifest.version.clone(),
        published_at: manifest.published_at.clone(),
        source_revision: manifest.source_revision.clone(),
        manifest: format!("{}/{bucket}/{manifest_key}", options.base_url),
        targets: manifest_targets(&manifest),
    };
    let initial = Index {
        schema_version: SCHEMA_VERSION,
        app: options.app.clone(),
        updated_at: now.clone(),
        versions: Vec::new(),
    };
    store
        .cas_update(
            &format!("{}/index.json", options.app),
            Some(initial),
            |index: Index| Ok(index_with_entry(index, entry.clone(), &now)),
        )
        .await?;

    // A local copy for the workflow to attach to the GitHub release
    // (convenience only; the bucket stays authoritative).
    let manifest_path = options.artifacts_dir.join("manifest.json");
    std::fs::write(&manifest_path, &manifest_bytes)
        .with_context(|| format!("cannot write {}", manifest_path.display()))?;

    Ok(PublishResult {
        bucket: bucket.to_string(),
        manifest_key,
        artifact_count: manifest.artifacts.len(),
        written,
        previously_published,
        manifest_targets: manifest_targets(&manifest),
        manifest_path: manifest_path.display().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA_SRC: &str = "0123456789abcdef0123456789abcdef01234567";
    const FPR: &str = "593674FBF5A2221533824CA7E27819A5417615EE";

    fn touch(dir: &Path, name: &str, contents: &str) {
        std::fs::write(dir.join(name), contents).unwrap();
    }

    fn full_set(dir: &Path) {
        touch(dir, "SpatialEngine-1.2.3.msi", "msi-bytes");
        touch(dir, "SpatialEngine-1.2.3.dmg", "dmg-bytes");
        touch(dir, "spatialengine_1.2.3_amd64.deb", "deb-bytes");
        touch(dir, "spatialengine_1.2.3_amd64.deb.asc", "deb-sig");
        touch(dir, "SpatialEngine-1.2.3-x86_64.AppImage", "appimage-bytes");
        touch(
            dir,
            "SpatialEngine-1.2.3-x86_64.AppImage.asc",
            "appimage-sig",
        );
    }

    fn all_targets() -> Vec<String> {
        vec![
            TARGET_WINDOWS.to_string(),
            TARGET_MACOS.to_string(),
            TARGET_LINUX.to_string(),
        ]
    }

    fn opts(dir: &Path, targets: Vec<String>) -> Options {
        Options {
            app: "spatial_engine".into(),
            version: "1.2.3".into(),
            destination: Destination::Production,
            source_revision: SHA_SRC.into(),
            release_name: "spatial_engine-1.2.3".into(),
            release_id: Some("42".into()),
            run_url: Some("https://github.com/fslabs/fsl_libs/actions/runs/1".into()),
            artifacts_dir: dir.to_path_buf(),
            targets,
            base_url: "https://api.s3.fsl.dev".into(),
            prod_bucket: "fsl-releases".into(),
            preview_bucket: "fsl-releases-preview".into(),
            linux_fingerprint: Some(FPR.into()),
            glibc_floor: "2.31".into(),
            skip_signature_verification: true,
        }
    }

    fn digests_of(
        artifacts: &[(PathBuf, String, ArtifactFormat)],
    ) -> BTreeMap<PathBuf, (String, u64)> {
        artifacts
            .iter()
            .map(|(path, _, _)| {
                let bytes = std::fs::read(path).unwrap();
                (path.clone(), (sha256_hex(&bytes), bytes.len() as u64))
            })
            .collect()
    }

    #[test]
    fn complete_set_collects_four_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        full_set(dir.path());
        let artifacts = collect_artifacts(dir.path(), &all_targets()).unwrap();
        assert_eq!(artifacts.len(), 4);
        let formats: Vec<ArtifactFormat> = artifacts.iter().map(|&(_, _, f)| f).collect();
        for format in [
            ArtifactFormat::Msi,
            ArtifactFormat::Dmg,
            ArtifactFormat::Deb,
            ArtifactFormat::Appimage,
        ] {
            assert!(formats.contains(&format), "missing {format:?}");
        }
    }

    #[test]
    fn missing_asc_fails() {
        let dir = tempfile::tempdir().unwrap();
        full_set(dir.path());
        std::fs::remove_file(dir.path().join("spatialengine_1.2.3_amd64.deb.asc")).unwrap();
        let err = collect_artifacts(dir.path(), &all_targets())
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing detached signature"), "{err}");
    }

    #[test]
    fn unrecognised_file_fails() {
        let dir = tempfile::tempdir().unwrap();
        full_set(dir.path());
        touch(dir.path(), "notes.txt", "stray");
        let err = collect_artifacts(dir.path(), &all_targets())
            .unwrap_err()
            .to_string();
        assert!(err.contains("unrecognised artifact notes.txt"), "{err}");
    }

    #[test]
    fn incomplete_set_for_configured_targets_fails() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "SpatialEngine-1.2.3.msi", "msi-bytes");
        // Windows alone cannot satisfy a three-target configuration.
        assert!(collect_artifacts(dir.path(), &all_targets()).is_err());
        // Linux with only a deb (even signed) is incomplete: deb AND appimage.
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "a.deb", "deb");
        touch(dir.path(), "a.deb.asc", "sig");
        let err = collect_artifacts(dir.path(), &[TARGET_LINUX.to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains(".deb and one .AppImage"), "{err}");
    }

    #[test]
    fn artifacts_for_unconfigured_targets_fail() {
        let dir = tempfile::tempdir().unwrap();
        full_set(dir.path());
        let err = collect_artifacts(dir.path(), &[TARGET_WINDOWS.to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("not configured to ship"), "{err}");
    }

    #[test]
    fn empty_directory_fails() {
        let dir = tempfile::tempdir().unwrap();
        assert!(collect_artifacts(dir.path(), &all_targets()).is_err());
    }

    #[test]
    fn manifest_records_digests_keys_and_urls() {
        let dir = tempfile::tempdir().unwrap();
        full_set(dir.path());
        let artifacts = collect_artifacts(dir.path(), &all_targets()).unwrap();
        let digests = digests_of(&artifacts);
        let options = opts(dir.path(), all_targets());
        let (manifest, uploads) =
            build_manifest(&options, "fsl-releases", &artifacts, &digests).unwrap();

        assert_eq!(manifest.schema_version, SCHEMA_VERSION);
        assert_eq!(manifest.artifacts.len(), 4);
        assert!(!manifest.prerelease);
        // 4 artifacts + 2 detached signatures.
        assert_eq!(uploads.len(), 6);

        let msi = manifest
            .artifacts
            .iter()
            .find(|a| a.format == ArtifactFormat::Msi)
            .unwrap();
        let want_sha =
            sha256_hex(&std::fs::read(dir.path().join("SpatialEngine-1.2.3.msi")).unwrap());
        assert_eq!(msi.sha256, want_sha);
        assert_eq!(msi.size_bytes, "msi-bytes".len() as u64);
        assert_eq!(
            msi.url,
            "https://api.s3.fsl.dev/fsl-releases/spatial_engine/1.2.3/x86_64-pc-windows-gnu/SpatialEngine-1.2.3.msi"
        );
        assert!(
            uploads.iter().any(|(_, key)| key
                == "spatial_engine/1.2.3/x86_64-pc-windows-gnu/SpatialEngine-1.2.3.msi")
        );
        assert!(matches!(msi.signature, ArtifactSignature::Authenticode));
        assert!(msi.glibc_floor.is_none());
    }

    #[test]
    fn linux_entries_carry_fingerprint_floor_and_asc_uploads() {
        let dir = tempfile::tempdir().unwrap();
        full_set(dir.path());
        let artifacts = collect_artifacts(dir.path(), &all_targets()).unwrap();
        let digests = digests_of(&artifacts);
        let options = opts(dir.path(), all_targets());
        let (manifest, uploads) =
            build_manifest(&options, "fsl-releases", &artifacts, &digests).unwrap();

        let linux: Vec<&ManifestArtifact> = manifest
            .artifacts
            .iter()
            .filter(|a| a.target == TARGET_LINUX)
            .collect();
        assert_eq!(linux.len(), 2);
        for artifact in linux {
            assert_eq!(artifact.glibc_floor.as_deref(), Some("2.31"));
            let ArtifactSignature::OpenpgpDetached {
                url,
                key_fingerprint,
            } = &artifact.signature
            else {
                panic!("linux artifact without a detached signature record");
            };
            assert_eq!(key_fingerprint, FPR);
            assert!(url.ends_with(".asc"), "{url}");
            assert_eq!(*url, format!("{}.asc", artifact.url));
        }
        // Both .asc files are uploaded, but they are NOT manifest artifacts.
        assert_eq!(
            uploads
                .iter()
                .filter(|(_, key)| key.ends_with(".asc"))
                .count(),
            2
        );
    }

    #[test]
    fn missing_fingerprint_with_linux_artifacts_fails() {
        let dir = tempfile::tempdir().unwrap();
        full_set(dir.path());
        let artifacts = collect_artifacts(dir.path(), &all_targets()).unwrap();
        let digests = digests_of(&artifacts);
        for fingerprint in [None, Some(String::new())] {
            let mut options = opts(dir.path(), all_targets());
            options.linux_fingerprint = fingerprint;
            let err = build_manifest(&options, "fsl-releases", &artifacts, &digests)
                .unwrap_err()
                .to_string();
            assert!(err.contains("linux-fingerprint"), "{err}");
        }
    }

    #[test]
    fn malformed_source_revision_fails() {
        let dir = tempfile::tempdir().unwrap();
        full_set(dir.path());
        let artifacts = collect_artifacts(dir.path(), &all_targets()).unwrap();
        let digests = digests_of(&artifacts);
        for bad in [
            "short".to_string(),
            SHA_SRC.to_uppercase(),
            format!("{SHA_SRC}00"),
        ] {
            let mut options = opts(dir.path(), all_targets());
            options.source_revision = bad;
            let err = build_manifest(&options, "fsl-releases", &artifacts, &digests)
                .unwrap_err()
                .to_string();
            assert!(err.contains("40 hex"), "{err}");
        }
    }

    #[test]
    fn preview_destination_marks_the_manifest_prerelease() {
        let dir = tempfile::tempdir().unwrap();
        full_set(dir.path());
        let artifacts = collect_artifacts(dir.path(), &all_targets()).unwrap();
        let digests = digests_of(&artifacts);
        let mut options = opts(dir.path(), all_targets());
        options.destination = Destination::Preview;
        let (manifest, _) =
            build_manifest(&options, "fsl-releases-preview", &artifacts, &digests).unwrap();
        assert!(manifest.prerelease);
    }

    fn entry(version: &str) -> IndexEntry {
        IndexEntry {
            version: version.to_string(),
            published_at: "2026-08-18T00:00:00Z".into(),
            source_revision: SHA_SRC.into(),
            manifest: format!("https://x/b/app/{version}/manifest.json"),
            targets: vec![TARGET_WINDOWS.to_string()],
        }
    }

    fn index(versions: &[&str]) -> Index {
        Index {
            schema_version: SCHEMA_VERSION,
            app: "app".into(),
            updated_at: "t0".into(),
            versions: versions.iter().map(|v| entry(v)).collect(),
        }
    }

    #[test]
    fn index_sorts_descending_with_semver_precedence() {
        let idx = index_with_entry(index(&["1.0.0", "1.10.0"]), entry("1.9.9"), "t1");
        let versions: Vec<&str> = idx.versions.iter().map(|e| e.version.as_str()).collect();
        assert_eq!(versions, ["1.10.0", "1.9.9", "1.0.0"]);
        assert_eq!(idx.updated_at, "t1");

        // Prerelease suffixes (preview) order below their plain version.
        let idx = index_with_entry(index(&["1.0.0"]), entry("1.0.0-rc.1"), "t1");
        let versions: Vec<&str> = idx.versions.iter().map(|e| e.version.as_str()).collect();
        assert_eq!(versions, ["1.0.0", "1.0.0-rc.1"]);
    }

    #[test]
    fn index_dedupes_by_version_keeping_the_new_entry() {
        let mut new_entry = entry("1.0.0");
        new_entry.manifest = "https://x/b/app/1.0.0/manifest.json?rewritten".into();
        let idx = index_with_entry(index(&["1.0.0", "1.1.0"]), new_entry.clone(), "t1");
        assert_eq!(idx.versions.len(), 2);
        let kept = idx.versions.iter().find(|e| e.version == "1.0.0").unwrap();
        assert_eq!(kept.manifest, new_entry.manifest);
    }

    #[test]
    fn unparseable_versions_sort_last_not_error() {
        let idx = index_with_entry(index(&["weird", "1.0.0"]), entry("1.1.0"), "t1");
        let versions: Vec<&str> = idx.versions.iter().map(|e| e.version.as_str()).collect();
        assert_eq!(versions, ["1.1.0", "1.0.0", "weird"]);
    }
}

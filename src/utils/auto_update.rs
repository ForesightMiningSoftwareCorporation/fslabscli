use std::{error, fs::File};

use http::header;
use self_update::{
    Download, cargo_crate_version, self_replace::self_replace, version::bump_is_greater,
};
use tempfile::TempDir;
use tracing::info;

/// Returns a list of compatible target patterns for the detected target.
///
/// This function handles platform-specific compatibility, particularly for Linux
/// where musl-built binaries can run on glibc-based systems. The function returns
/// the detected target first, followed by any compatible alternatives.
///
/// # Arguments
///
/// * `detected_target` - The target triple detected by self_update
///
/// # Returns
///
/// A vector of target patterns to search for in release assets, ordered by preference.
///
/// # Examples
///
/// ```
/// let targets = get_compatible_targets("x86_64-unknown-linux-gnu");
/// assert_eq!(targets, vec!["x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl"]);
/// ```
fn get_compatible_targets(detected_target: &str) -> Vec<String> {
    let mut targets = vec![detected_target.to_string()];

    // On Linux, musl binaries can run on glibc systems, so if we detect a gnu target,
    // we should also try the corresponding musl target as a fallback.
    // This handles the case where binaries are built with musl (e.g., via Nix)
    // but the runtime system uses glibc.
    match detected_target {
        "x86_64-unknown-linux-gnu" => {
            targets.push("x86_64-unknown-linux-musl".to_string());
        }
        "aarch64-unknown-linux-gnu" => {
            targets.push("aarch64-unknown-linux-musl".to_string());
        }
        "i686-unknown-linux-gnu" => {
            targets.push("i686-unknown-linux-musl".to_string());
        }
        "armv7-unknown-linux-gnueabihf" => {
            targets.push("armv7-unknown-linux-musleabihf".to_string());
        }
        // For other platforms (Windows, macOS, or already-musl Linux), no fallback needed
        _ => {}
    }

    targets
}

fn download_and_replace(
    release_asset_download_url: &str,
    from_version: &str,
    to_version: &str,
) -> Result<(), Box<dyn error::Error>> {
    info!("Updating to version {to_version} (from {from_version}).");

    let tmp_archive_dir = TempDir::new()?;
    let tmp_archive_path_a = tmp_archive_dir.path().join("downloaded");
    let tmp_archive_path_b = tmp_archive_dir.path().join("backup");
    let mut tmp_archive = File::create(&tmp_archive_path_a)?;

    let mut download = Download::from_url(release_asset_download_url);
    download.set_header(header::ACCEPT, "application/octet-stream".parse().unwrap());
    download.download_to(&mut tmp_archive)?;

    std::fs::copy(&tmp_archive_path_a, &tmp_archive_path_b)?;
    let current_exe = File::open(std::env::current_exe()?)?;
    let permissions = current_exe.metadata()?.permissions();
    let new_exe = File::open(&tmp_archive_path_b)?;
    new_exe.set_permissions(permissions)?;

    self_replace(&tmp_archive_path_a)?;

    cargo_util::ProcessBuilder::new(tmp_archive_path_b)
        .args(&std::env::args_os().skip(1).collect::<Vec<_>>())
        .exec_replace()?;

    Ok(())
}

/// Extracts a bare semver string from a release tag name.
///
/// Supports the following formats:
/// - `cargo-fslabscli-2.43.0` → `2.43.0`
/// - `v2.43.0` → `2.43.0`
/// - `cargo-fslabscli-v2.43.0` → `2.43.0`
/// - `2.43.0` → `2.43.0`
/// - `cargo-fslabscli-2.43.0-rc.1` → `2.43.0-rc.1`
///
/// The strategy: find the first ASCII digit — the version always starts there.
fn extract_version_from_tag(tag: &str) -> &str {
    match tag.find(|c: char| c.is_ascii_digit()) {
        Some(pos) => &tag[pos..],
        None => tag,
    }
}

/// Tag shapes a release may carry, most likely first.
///
/// `draft-release` and `publish` both default to the `{package_name}-{version}`
/// template, so releases are tagged `cargo-fslabscli-2.47.0`. Deriving the
/// prefix from `CARGO_PKG_NAME` keeps this in step with that default rather
/// than restating it. Two April 2026 releases (2.44.0 and 2.45.0) were tagged
/// `v{version}` instead, so keep that shape as a fallback rather than making
/// those two unpinnable, and try the bare version last.
fn candidate_tags(version: &str) -> [String; 3] {
    [
        format!("{}-{version}", env!("CARGO_PKG_NAME")),
        format!("v{version}"),
        version.to_string(),
    ]
}

pub fn auto_update(target_version: Option<&str>) -> Result<(), Box<dyn error::Error>> {
    let checker = self_update::backends::github::Update::configure()
        .repo_owner("fslabs")
        .repo_name("fslabscli")
        .bin_name("fslabscli")
        .current_version(cargo_crate_version!())
        .build()?;

    let current_version = checker.current_version();
    let detected_target = checker.target();
    let compatible_targets = get_compatible_targets(&detected_target);

    if let Some(version) = target_version {
        let normalized = version.trim_start_matches('v');

        if current_version == normalized {
            return Ok(());
        }

        let mut found = None;
        let mut last_error = None;
        for tag in candidate_tags(normalized) {
            match checker.get_release_version(&tag) {
                Ok(release) => {
                    found = Some(release);
                    break;
                }
                // Keep the underlying error: swallowing it reported a network
                // failure or a rate limit as "version not found", which sent
                // you looking in the wrong place entirely.
                Err(e) => last_error = Some(format!("{tag}: {e}")),
            }
        }
        let release = found.ok_or_else(|| match last_error {
            Some(e) => format!(
                "Requested version {version} not found in GitHub releases (last attempt {e})"
            ),
            None => format!("Requested version {version} not found in GitHub releases"),
        })?;

        let release_asset = compatible_targets
            .iter()
            .find_map(|target| release.assets.iter().find(|a| a.name.contains(target)));

        let asset = release_asset.ok_or_else(|| {
            let tried_targets = compatible_targets.join(", ");
            format!(
                "Version {} found but no pre-built binary for any compatible architecture. Tried: {}",
                normalized, tried_targets
            )
        })?;
        download_and_replace(&asset.download_url, &current_version, normalized)?;
    } else {
        let latest_release = checker.get_latest_release()?;
        let latest_version = extract_version_from_tag(&latest_release.version);

        if bump_is_greater(&current_version, latest_version).unwrap_or(false) {
            let release_asset = compatible_targets.iter().find_map(|target| {
                latest_release
                    .assets
                    .iter()
                    .find(|a| a.name.contains(target))
            });

            if let Some(asset) = release_asset {
                download_and_replace(&asset.download_url, &current_version, latest_version)?;
            } else {
                let tried_targets = compatible_targets.join(", ");
                info!(
                    "Update available ({current_version} to {latest_version}), but no pre-built version found for any compatible architecture. Tried: {}",
                    tried_targets
                );
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_candidate_tags_tries_the_real_scheme_first() {
        // Regression: this looked up "v{version}" only, so pinning to any
        // release tagged cargo-fslabscli-{version} 404'd and, because a failed
        // pin exits the process, the command never ran at all.
        let tags = candidate_tags("2.47.0");
        assert_eq!(tags[0], "cargo-fslabscli-2.47.0");
        assert_eq!(tags[1], "v2.47.0");
        assert_eq!(tags[2], "2.47.0");
    }

    #[test]
    fn test_candidate_tags_round_trip_through_extract() {
        // Whatever shape we ask for, parsing it back must yield the version we
        // started with, so the two halves cannot drift apart.
        for tag in candidate_tags("2.47.0") {
            assert_eq!(extract_version_from_tag(&tag), "2.47.0");
        }
    }

    #[test]
    fn test_extract_version_from_tag_v_prefix() {
        assert_eq!(extract_version_from_tag("v2.43.0"), "2.43.0");
    }

    #[test]
    fn test_extract_version_from_tag_package_prefix() {
        assert_eq!(extract_version_from_tag("cargo-fslabscli-2.43.0"), "2.43.0");
    }

    #[test]
    fn test_extract_version_from_tag_package_prefix_with_v() {
        assert_eq!(
            extract_version_from_tag("cargo-fslabscli-v2.43.0"),
            "2.43.0"
        );
    }

    #[test]
    fn test_extract_version_from_tag_bare() {
        assert_eq!(extract_version_from_tag("2.43.0"), "2.43.0");
    }

    #[test]
    fn test_extract_version_from_tag_prerelease() {
        assert_eq!(
            extract_version_from_tag("cargo-fslabscli-2.43.0-rc.1"),
            "2.43.0-rc.1"
        );
    }

    #[test]
    fn test_extract_version_from_tag_v_prerelease() {
        assert_eq!(extract_version_from_tag("v2.43.0-rc.1"), "2.43.0-rc.1");
    }

    #[test]
    fn test_get_compatible_targets_x86_64_gnu() {
        let targets = get_compatible_targets("x86_64-unknown-linux-gnu");
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0], "x86_64-unknown-linux-gnu");
        assert_eq!(targets[1], "x86_64-unknown-linux-musl");
    }

    #[test]
    fn test_get_compatible_targets_aarch64_gnu() {
        let targets = get_compatible_targets("aarch64-unknown-linux-gnu");
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0], "aarch64-unknown-linux-gnu");
        assert_eq!(targets[1], "aarch64-unknown-linux-musl");
    }

    #[test]
    fn test_get_compatible_targets_i686_gnu() {
        let targets = get_compatible_targets("i686-unknown-linux-gnu");
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0], "i686-unknown-linux-gnu");
        assert_eq!(targets[1], "i686-unknown-linux-musl");
    }

    #[test]
    fn test_get_compatible_targets_armv7_gnueabihf() {
        let targets = get_compatible_targets("armv7-unknown-linux-gnueabihf");
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0], "armv7-unknown-linux-gnueabihf");
        assert_eq!(targets[1], "armv7-unknown-linux-musleabihf");
    }

    #[test]
    fn test_get_compatible_targets_musl_no_fallback() {
        let targets = get_compatible_targets("x86_64-unknown-linux-musl");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0], "x86_64-unknown-linux-musl");
    }

    #[test]
    fn test_get_compatible_targets_macos_no_fallback() {
        let targets = get_compatible_targets("x86_64-apple-darwin");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0], "x86_64-apple-darwin");
    }

    #[test]
    fn test_get_compatible_targets_windows_no_fallback() {
        let targets = get_compatible_targets("x86_64-pc-windows-msvc");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0], "x86_64-pc-windows-msvc");
    }
}

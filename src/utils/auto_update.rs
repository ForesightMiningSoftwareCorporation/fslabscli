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

pub fn auto_update() -> Result<(), Box<dyn error::Error>> {
    let checker = self_update::backends::github::Update::configure()
        .repo_owner("ForesightMiningSoftwareCorporation")
        .repo_name("fslabscli")
        .bin_name("fslabscli")
        .current_version(cargo_crate_version!())
        .build()?;

    // Find latest and current versions
    let latest_release = checker.get_latest_release()?;
    let current_version = checker.current_version();
    let latest_version = latest_release.version.split("-").last().unwrap();

    if bump_is_greater(&current_version, latest_version).unwrap_or(false) {
        // Get the list of compatible targets to search for
        let detected_target = checker.target();
        let compatible_targets = get_compatible_targets(&detected_target);

        // Find the correct release asset to download by checking all compatible targets
        // We iterate through compatible targets in order of preference (detected first, then fallbacks)
        let release_asset_for_arch = compatible_targets.iter().find_map(|target| {
            latest_release
                .assets
                .iter()
                .find(|a| a.name.contains(target))
        });

        if let Some(release_asset) = release_asset_for_arch {
            info!("Updating to version {latest_version} (from {current_version}).");

            // Preparing temp files
            let tmp_archive_dir = TempDir::new()?;
            let tmp_archive_path_a = tmp_archive_dir.path().join("downloaded");
            let tmp_archive_path_b = tmp_archive_dir.path().join("backup");
            let mut tmp_archive = File::create(&tmp_archive_path_a)?;

            // Downloading the latest release
            let mut download = Download::from_url(&release_asset.download_url);
            download.set_header(header::ACCEPT, "application/octet-stream".parse().unwrap());
            download.download_to(&mut tmp_archive)?;

            // Preparing a copy in temp folder with the correct permissions
            std::fs::copy(&tmp_archive_path_a, &tmp_archive_path_b)?;
            let current_exe = File::open(std::env::current_exe()?)?;
            let permissions = current_exe.metadata()?.permissions();
            let new_exe = File::open(&tmp_archive_path_b)?;
            new_exe.set_permissions(permissions)?;

            // Replace the current executable with the new one
            self_replace(&tmp_archive_path_a)?;

            // Run the new version from the temp copy
            cargo_util::ProcessBuilder::new(tmp_archive_path_b)
                .args(&std::env::args_os().skip(1).collect::<Vec<_>>())
                .exec_replace()?;
        } else {
            let tried_targets = compatible_targets.join(", ");
            info!(
                "Update available ({current_version} to {latest_version}), but no pre-built version found for any compatible architecture. Tried: {}",
                tried_targets
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

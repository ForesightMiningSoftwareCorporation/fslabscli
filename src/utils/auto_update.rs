use std::{error, fs::File};

use http::header;
use self_update::{
    Download, cargo_crate_version, self_replace::self_replace, version::bump_is_greater,
};
use tempfile::TempDir;

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
        // Find the correct release asset to download
        let release_asset_for_arch = latest_release
            .assets
            .iter()
            .find(|a| a.name.contains(&checker.target()));

        if let Some(release_asset) = release_asset_for_arch {
            println!("Updating to version {latest_version} (from {current_version}).");

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
            println!(
                "Update available ({current_version} to {latest_version}), but no pre-built version found for your architecture \"{}\".",
                checker.target()
            );
        }
    }

    Ok(())
}

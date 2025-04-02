use std::{
    error,
    fs::{File, remove_file},
};

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

            let tmp_archive_dir = TempDir::new()?;
            let tmp_archive_path = tmp_archive_dir.path().join("cargo-fslabscli");
            let mut tmp_archive = File::create(&tmp_archive_path)?;

            let mut download = Download::from_url(&release_asset.download_url);
            download.set_header(header::ACCEPT, "application/octet-stream".parse().unwrap());
            download.download_to(&mut tmp_archive)?;

            // Replace the current executable with the downloaded one, and run it.
            self_replace(&tmp_archive_path)?;
            remove_file(&tmp_archive_path)?;
            cargo_util::ProcessBuilder::new(std::env::current_exe()?)
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

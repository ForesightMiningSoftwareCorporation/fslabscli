//! Build the Linux release artifacts for an app: a .deb from the package's
//! `[package.metadata.deb]` table (cargo-deb; the Depends line is authored
//! by hand because Vulkan/EGL/X11 are dlopened and absent from DT_NEEDED)
//! and an AppImage for distributions the .deb does not cover.
//!
//! Runs inside the pinned build container (debian bullseye), whose glibc is
//! the declared compatibility floor; the floor is asserted against the built
//! binary's versioned symbols (objdump -T), so a toolchain or container
//! change that raises it fails HERE instead of on a customer machine.
//!
//! The AppImage deliberately bundles NOTHING but the binary, desktop entry
//! and icon: the Vulkan loader, GL, X11 and GTK must come from the host,
//! because bundling a Vulkan loader against a host ICD is the standard
//! AppImage failure mode. appimagetool is fetched pinned by URL and sha256
//! and run with APPIMAGE_EXTRACT_AND_RUN=1 (no FUSE in a container).

use std::fmt::{Display, Formatter};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use clap::Parser;
use serde::Serialize;
use walkdir::WalkDir;

use super::store::sha256_hex;
use super::types::TARGET_LINUX;
use crate::PrettyPrintable;

pub const APPIMAGETOOL_URL: &str =
    "https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-x86_64.AppImage";
pub const APPIMAGETOOL_SHA256: &str =
    "ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0";

#[derive(Debug, Parser, Clone)]
#[command(
    about = "Build the Linux .deb and AppImage in the pinned floor container",
    disable_version_flag = true
)]
pub struct Options {
    /// Cargo package name (e.g. spatial_engine).
    #[arg(long)]
    pub package: String,
    /// Binary/product name (e.g. SpatialEngine).
    #[arg(long)]
    pub binary_name: String,
    #[arg(long)]
    pub version: String,
    /// Package directory relative to the repo root.
    #[arg(long)]
    pub package_dir: PathBuf,
    /// Icon file installed as the AppImage/desktop icon.
    #[arg(long)]
    pub icon: PathBuf,
    #[arg(long, default_value = "2.31")]
    pub glibc_floor: String,
    /// Skip the cargo build (the binary already exists in the target dir).
    #[arg(long, default_value_t = false)]
    pub no_build: bool,
}

#[derive(Debug, Serialize, Clone)]
pub struct BundleLinuxResult {
    pub deb: PathBuf,
    pub appimage: PathBuf,
    pub max_glibc_symbol: String,
}

impl Display for BundleLinuxResult {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "deb={}", self.deb.display())?;
        writeln!(f, "appimage={}", self.appimage.display())?;
        write!(f, "highest glibc symbol: {}", self.max_glibc_symbol)
    }
}

impl PrettyPrintable for BundleLinuxResult {
    fn pretty_print(&self) -> String {
        self.to_string()
    }
}

/// Files that must NEVER ship inside the AppDir: graphics stacks the host
/// provides. Bundling any of them against a host driver is the standard
/// AppImage failure mode.
const FORBIDDEN_LIB_PREFIXES: [&str; 4] = ["libvulkan", "libGL", "libEGL", "libX11"];

/// Parse "2.31" or "2.2.5" into numeric segments. String or `sort`
/// comparison gets 2.9 vs 2.31 wrong, which is the whole point of doing
/// this numerically.
fn parse_numeric_version(version: &str) -> anyhow::Result<Vec<u32>> {
    version
        .split('.')
        .map(|segment| {
            segment
                .parse::<u32>()
                .with_context(|| format!("unparseable version segment '{segment}' in '{version}'"))
        })
        .collect()
}

/// The highest GLIBC_x.y[.z] versioned symbol in `objdump -T` output, by
/// numeric segment comparison. Non-numeric suffixes (GLIBC_PRIVATE) are
/// ignored. `None` when no versioned glibc symbol appears at all.
fn max_glibc_symbol(objdump_output: &str) -> Option<String> {
    let mut best: Option<Vec<u32>> = None;
    for (idx, _) in objdump_output.match_indices("GLIBC_") {
        let rest = &objdump_output[idx + "GLIBC_".len()..];
        let end = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(rest.len());
        let version = rest[..end].trim_end_matches('.');
        if version.is_empty() {
            continue;
        }
        let Ok(parsed) = parse_numeric_version(version) else {
            continue;
        };
        if best.as_ref().is_none_or(|b| parsed > *b) {
            best = Some(parsed);
        }
    }
    best.map(|v| v.iter().map(u32::to_string).collect::<Vec<_>>().join("."))
}

/// True when the binary's highest glibc symbol exceeds the declared floor.
fn exceeds_floor(max_symbol: &str, floor: &str) -> anyhow::Result<bool> {
    Ok(parse_numeric_version(max_symbol)? > parse_numeric_version(floor)?)
}

/// The `Icon=` value of a desktop entry; it names the icon file installed
/// into the AppDir.
fn desktop_icon_name(desktop_contents: &str) -> Option<String> {
    desktop_contents
        .lines()
        .find_map(|line| line.trim().strip_prefix("Icon="))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

async fn run_step(
    program: &str,
    args: &[&str],
    dir: &Path,
    envs: &[(&str, &str)],
) -> anyhow::Result<()> {
    let mut command = tokio::process::Command::new(program);
    command.args(args).current_dir(dir);
    for (name, value) in envs {
        command.env(name, value);
    }
    let status = command
        .status()
        .await
        .with_context(|| format!("failed to run {program}"))?;
    if !status.success() {
        bail!("{program} {} failed with {status}", args.join(" "));
    }
    Ok(())
}

async fn capture_stdout(program: &str, args: &[&str]) -> anyhow::Result<String> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .output()
        .await
        .with_context(|| format!("failed to run {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} {} failed with {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Download a URL (GitHub release downloads redirect to object storage).
async fn download(url: &str) -> anyhow::Result<Vec<u8>> {
    super::http::get_bytes(&super::http::client()?, url).await
}

fn make_executable(path: &Path) -> anyhow::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("cannot chmod {}", path.display()))
}

pub async fn run(
    options: &Options,
    working_directory: PathBuf,
) -> anyhow::Result<BundleLinuxResult> {
    let package_dir = if options.package_dir.is_absolute() {
        options.package_dir.clone()
    } else {
        working_directory.join(&options.package_dir)
    };
    let icon = if options.icon.is_absolute() {
        options.icon.clone()
    } else {
        working_directory.join(&options.icon)
    };

    if !options.no_build {
        run_step(
            "cargo",
            &[
                "build",
                "--release",
                "--locked",
                "--package",
                &options.package,
                "--target",
                TARGET_LINUX,
            ],
            &package_dir,
            &[],
        )
        .await?;
    }

    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| package_dir.join("target"));
    let binary = target_dir
        .join(TARGET_LINUX)
        .join("release")
        .join(&options.binary_name);
    if !binary.is_file() {
        bail!("built binary not found at {}", binary.display());
    }

    // Assert the glibc floor against the binary's versioned symbols.
    let objdump_output = capture_stdout("objdump", &["-T", &binary.to_string_lossy()]).await?;
    let max_glibc = max_glibc_symbol(&objdump_output).with_context(|| {
        format!(
            "objdump -T {} shows no GLIBC_ versioned symbols; cannot assert the floor",
            binary.display()
        )
    })?;
    if exceeds_floor(&max_glibc, &options.glibc_floor)? {
        bail!(
            "binary requires glibc {max_glibc}, above the declared floor {}; the build container \
             no longer sets the floor it claims",
            options.glibc_floor
        );
    }

    // Package the .deb from the already-built binary.
    run_step(
        "cargo",
        &[
            "deb",
            "--package",
            &options.package,
            "--no-build",
            "--target",
            TARGET_LINUX,
            "--deb-version",
            &options.version,
        ],
        &package_dir,
        &[],
    )
    .await?;
    let debian_dir = target_dir.join(TARGET_LINUX).join("debian");
    let mut debs: Vec<PathBuf> = std::fs::read_dir(&debian_dir)
        .with_context(|| format!("cannot list {}", debian_dir.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "deb"))
        .collect();
    debs.sort();
    let Some(deb) = debs.into_iter().next() else {
        bail!("cargo deb produced no .deb under {}", debian_dir.display());
    };

    // Assemble the AppDir: binary, desktop entry, icon, AppRun. Nothing else.
    let out = target_dir.join(TARGET_LINUX).join("appimage");
    if out.exists() {
        std::fs::remove_dir_all(&out).with_context(|| format!("cannot clear {}", out.display()))?;
    }
    let appdir = out.join("AppDir");
    for dir in [
        appdir.join("usr/bin"),
        appdir.join("usr/share/applications"),
        appdir.join("usr/share/icons/hicolor/128x128/apps"),
    ] {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
    }
    std::fs::copy(&binary, appdir.join("usr/bin").join(&options.binary_name))
        .context("cannot copy the binary into the AppDir")?;

    let desktop_file = format!("{}.desktop", options.binary_name);
    let desktop_src = package_dir.join("packaging").join(&desktop_file);
    let desktop_contents = std::fs::read_to_string(&desktop_src)
        .with_context(|| format!("no desktop entry at {}", desktop_src.display()))?;
    std::fs::copy(&desktop_src, appdir.join(&desktop_file))?;
    std::fs::copy(
        &desktop_src,
        appdir.join("usr/share/applications").join(&desktop_file),
    )?;

    // The desktop entry's Icon= names the icon file appimagetool looks for.
    let icon_name = desktop_icon_name(&desktop_contents).with_context(|| {
        format!(
            "{} has no Icon= line; the AppImage needs one",
            desktop_src.display()
        )
    })?;
    let icon_file = format!("{icon_name}.png");
    std::fs::copy(&icon, appdir.join(&icon_file))
        .with_context(|| format!("cannot copy the icon from {}", icon.display()))?;
    std::fs::copy(
        &icon,
        appdir
            .join("usr/share/icons/hicolor/128x128/apps")
            .join(&icon_file),
    )?;

    let apprun = appdir.join("AppRun");
    std::fs::write(
        &apprun,
        format!(
            "#!/bin/sh\nHERE=\"$(dirname \"$(readlink -f \"$0\")\")\"\nexec \"$HERE/usr/bin/{}\" \"$@\"\n",
            options.binary_name
        ),
    )?;
    make_executable(&apprun)?;

    // Fetch appimagetool pinned by URL and digest.
    let tool_bytes = download(APPIMAGETOOL_URL).await?;
    let tool_digest = sha256_hex(&tool_bytes);
    if tool_digest != APPIMAGETOOL_SHA256 {
        bail!(
            "appimagetool digest mismatch: expected {APPIMAGETOOL_SHA256}, downloaded {tool_digest}"
        );
    }
    let tool = out.join("appimagetool");
    std::fs::write(&tool, &tool_bytes)?;
    make_executable(&tool)?;

    let appimage = out.join(format!(
        "{}-{}-x86_64.AppImage",
        options.binary_name, options.version
    ));
    run_step(
        &tool.to_string_lossy(),
        &[
            "--no-appstream",
            &appdir.to_string_lossy(),
            &appimage.to_string_lossy(),
        ],
        &out,
        // No FUSE inside a container; extract-and-run instead.
        &[("APPIMAGE_EXTRACT_AND_RUN", "1"), ("ARCH", "x86_64")],
    )
    .await?;
    if !appimage.is_file() {
        bail!(
            "appimagetool produced no AppImage at {}",
            appimage.display()
        );
    }

    // Prove the bundle-nothing rule held.
    let contraband: Vec<String> = WalkDir::new(&appdir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy();
            FORBIDDEN_LIB_PREFIXES
                .iter()
                .any(|prefix| name.starts_with(prefix))
        })
        .map(|entry| entry.path().display().to_string())
        .collect();
    if !contraband.is_empty() {
        bail!(
            "AppDir contains graphics libraries that must come from the host: {}",
            contraband.join(", ")
        );
    }

    Ok(BundleLinuxResult {
        deb,
        appimage,
        max_glibc_symbol: max_glibc,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OBJDUMP_231: &str = "\
DYNAMIC SYMBOL TABLE:
0000000000000000      DF *UND*\t0000000000000000 (GLIBC_2.2.5) __libc_start_main
0000000000000000      DF *UND*\t0000000000000000 (GLIBC_2.3.4) __printf_chk
0000000000000000      DF *UND*\t0000000000000000 (GLIBC_2.31) pthread_cond_clockwait
0000000000000000      DF *UND*\t0000000000000000  GLIBC_PRIVATE  __libc_dlopen_mode
";

    const OBJDUMP_234: &str = "\
DYNAMIC SYMBOL TABLE:
0000000000000000      DF *UND*\t0000000000000000 (GLIBC_2.2.5) __libc_start_main
0000000000000000      DF *UND*\t0000000000000000 (GLIBC_2.34) pthread_create
";

    #[test]
    fn max_symbol_is_extracted_numerically() {
        assert_eq!(max_glibc_symbol(OBJDUMP_231).as_deref(), Some("2.31"));
        assert_eq!(max_glibc_symbol(OBJDUMP_234).as_deref(), Some("2.34"));
        assert_eq!(max_glibc_symbol("no versioned symbols here"), None);
        // GLIBC_PRIVATE alone is not a version.
        assert_eq!(max_glibc_symbol("GLIBC_PRIVATE"), None);
    }

    #[test]
    fn floor_2_31_passes_a_2_31_binary_and_fails_a_2_34_one() {
        let max = max_glibc_symbol(OBJDUMP_231).unwrap();
        assert!(!exceeds_floor(&max, "2.31").unwrap());
        let max = max_glibc_symbol(OBJDUMP_234).unwrap();
        assert!(exceeds_floor(&max, "2.31").unwrap());
    }

    #[test]
    fn version_comparison_is_numeric_not_lexical() {
        // String comparison would call 2.9 the bigger one.
        assert!(!exceeds_floor("2.9", "2.31").unwrap());
        assert!(exceeds_floor("2.31", "2.9").unwrap());
        assert_eq!(
            max_glibc_symbol("(GLIBC_2.9) a (GLIBC_2.31) b").as_deref(),
            Some("2.31")
        );
        assert!(exceeds_floor("2.2.6", "2.2.5").unwrap());
        assert!(exceeds_floor("x.y", "2.31").is_err());
    }

    #[test]
    fn desktop_icon_line_is_parsed() {
        assert_eq!(
            desktop_icon_name(
                "[Desktop Entry]\nName=SpatialEngine\nIcon=spatialengine\nExec=SpatialEngine\n"
            )
            .as_deref(),
            Some("spatialengine")
        );
        assert_eq!(
            desktop_icon_name("  Icon= spaced \n").as_deref(),
            Some("spaced")
        );
        assert_eq!(desktop_icon_name("[Desktop Entry]\nName=X\n"), None);
        assert_eq!(desktop_icon_name("Icon=\n"), None);
    }
}

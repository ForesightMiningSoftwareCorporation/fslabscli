//! Detach-sign the Linux release artifacts with the organisation's OpenPGP
//! key. Linux has no operating-system signature (Windows gets Authenticode,
//! macOS gets notarization), so the .deb and AppImage each get a detached
//! ASCII-armoured signature; the manifest's sha256 remains the primary trust
//! anchor and the public key is published at keys/fsl-release-linux.asc in
//! the production bucket.
//!
//! Runs on an ephemeral GitHub-hosted runner (signing-only per the runner
//! policy): the key lives in a job-scoped GNUPGHOME under a temp dir wiped
//! on drop, and every signature is self-verified before the command exits.

use std::fmt::{Display, Formatter};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, bail};
use clap::Parser;
use serde::Serialize;
use tokio::io::AsyncWriteExt;

use crate::PrettyPrintable;

#[derive(Debug, Parser, Clone)]
#[command(about = "Detach-sign Linux artifacts with the org OpenPGP key")]
pub struct Options {
    /// Directory whose *.deb and *.AppImage files each get a .asc.
    #[arg(long)]
    pub dir: PathBuf,
    /// ASCII-armoured private key.
    #[arg(long, env = "LINUX_SIGNING_KEY", hide_env_values = true)]
    pub key: String,
    #[arg(long, env = "LINUX_SIGNING_PASSPHRASE", hide_env_values = true)]
    pub passphrase: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct SignLinuxResult {
    /// 40-hex fingerprint recorded in the manifest against the published key.
    pub fingerprint: String,
    pub signed: Vec<String>,
}

impl Display for SignLinuxResult {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        for s in &self.signed {
            writeln!(f, "signed {s}")?;
        }
        write!(f, "fingerprint={}", self.fingerprint)
    }
}

impl PrettyPrintable for SignLinuxResult {
    fn pretty_print(&self) -> String {
        self.to_string()
    }
}

/// Extract the key id (field 5 of the first `sec` line) and fingerprint
/// (field 10 of the first `fpr` line) from
/// `gpg --list-secret-keys --with-colons` output.
fn parse_secret_key_listing(listing: &str) -> anyhow::Result<(String, String)> {
    let field = |prefix: &str, index: usize| {
        listing
            .lines()
            .find(|line| line.starts_with(prefix))
            .and_then(|line| line.split(':').nth(index))
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let key_id = field("sec", 4).context("no secret key imported")?;
    let fingerprint = field("fpr", 9).context("imported secret key has no fingerprint line")?;
    Ok((key_id, fingerprint))
}

/// Run gpg with `stdin` fed to it. `label` is what appears in an error message:
/// the argv is NEVER echoed, because the signing invocation carries the
/// passphrase and a failure would otherwise print it into the job log. It is
/// masked there only by GitHub's secret filter, which does not apply to a log
/// read anywhere else.
async fn gpg_with_stdin(
    gnupg_home: &Path,
    label: &str,
    args: &[&str],
    stdin_data: Option<&[u8]>,
) -> anyhow::Result<std::process::Output> {
    let mut command = tokio::process::Command::new("gpg");
    command
        .args(args)
        .env("GNUPGHOME", gnupg_home)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if stdin_data.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to run gpg ({label})"))?;
    // The write and the drain run concurrently. Completing the stdin write
    // first would deadlock the moment gpg emits more than a pipe buffer of
    // status output before reading all of its input: gpg blocks writing
    // stderr, we block writing stdin, and the signing job hangs until the
    // job timeout with nothing to show for it.
    let mut stdin = match stdin_data {
        Some(_) => Some(
            child
                .stdin
                .take()
                .with_context(|| format!("no stdin on the gpg {label}"))?,
        ),
        None => None,
    };
    let feed = async {
        if let (Some(stdin), Some(data)) = (stdin.as_mut(), stdin_data) {
            stdin.write_all(data).await?;
            stdin.shutdown().await?;
        }
        drop(stdin.take());
        Ok::<_, std::io::Error>(())
    };
    let (fed, output) = tokio::join!(feed, child.wait_with_output());
    fed.with_context(|| format!("failed to feed gpg {label} on stdin"))?;
    let output = output?;
    if !output.status.success() {
        bail!(
            "gpg {label} failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output)
}

async fn gpg(
    gnupg_home: &Path,
    label: &str,
    args: &[&str],
) -> anyhow::Result<std::process::Output> {
    gpg_with_stdin(gnupg_home, label, args, None).await
}

pub async fn run(options: &Options) -> anyhow::Result<SignLinuxResult> {
    // Job-scoped GNUPGHOME: created 0700, wiped when the TempDir drops.
    let gnupg_home = tempfile::TempDir::new().context("cannot create a temporary GNUPGHOME")?;
    std::fs::set_permissions(gnupg_home.path(), std::fs::Permissions::from_mode(0o700))?;
    let home = gnupg_home.path();

    // Import the key via stdin so it never touches the filesystem outside
    // the GNUPGHOME.
    let mut key = options.key.clone().into_bytes();
    key.push(b'\n');
    gpg_with_stdin(
        home,
        "--import",
        &["--batch", "--quiet", "--import"],
        Some(&key),
    )
    .await?;

    let listing = gpg(
        home,
        "--list-secret-keys",
        &["--list-secret-keys", "--with-colons"],
    )
    .await?;
    let (key_id, fingerprint) =
        parse_secret_key_listing(&String::from_utf8_lossy(&listing.stdout))?;

    // Every *.deb and *.AppImage in the directory, deterministically ordered.
    let mut artifacts: Vec<PathBuf> = std::fs::read_dir(&options.dir)
        .with_context(|| format!("cannot list {}", options.dir.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path.file_name().is_some_and(|name| {
                    let name = name.to_string_lossy();
                    name.ends_with(".deb") || name.ends_with(".AppImage")
                })
        })
        .collect();
    artifacts.sort();
    if artifacts.is_empty() {
        bail!("no .deb or .AppImage found in {}", options.dir.display());
    }

    let mut signed = Vec::new();
    for artifact in &artifacts {
        let artifact_str = artifact.to_string_lossy().into_owned();
        let signature = format!("{artifact_str}.asc");
        // The passphrase goes over stdin via --passphrase-fd 0, never on the
        // argv, where every process on the runner could read it out of /proc.
        gpg_with_stdin(
            home,
            "--detach-sign",
            &[
                "--batch",
                "--yes",
                "--pinentry-mode",
                "loopback",
                "--passphrase-fd",
                "0",
                "--local-user",
                &key_id,
                "--armor",
                "--detach-sign",
                "--output",
                &signature,
                &artifact_str,
            ],
            Some(options.passphrase.as_bytes()),
        )
        .await
        .with_context(|| format!("signing {} failed", artifact.display()))?;
        // Self-verify before anything downstream trusts the .asc.
        gpg(home, "--verify", &["--verify", &signature, &artifact_str])
            .await
            .with_context(|| format!("self-verification failed for {}", artifact.display()))?;
        signed.push(
            artifact
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or(artifact_str),
        );
    }

    Ok(SignLinuxResult {
        fingerprint,
        signed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LISTING: &str = "\
tru::1:1650000000:0:3:1:5
sec:u:4096:1:0123456789ABCDEF:1650000000:::u:::scESC:::+:::23::0:
fpr:::::::::ABCDEF0123456789ABCDEF0123456789ABCDEF01:
grp:::::::::0000000000000000000000000000000000000000:
uid:u::::1650000000::AAAA::FSL Release <release@fslabs.ca>::::::::::0:
";

    #[test]
    fn key_id_and_fingerprint_come_from_the_colon_listing() {
        let (key_id, fingerprint) = parse_secret_key_listing(LISTING).unwrap();
        assert_eq!(key_id, "0123456789ABCDEF");
        assert_eq!(fingerprint, "ABCDEF0123456789ABCDEF0123456789ABCDEF01");
    }

    #[test]
    fn listing_without_a_secret_key_is_an_error() {
        let err = parse_secret_key_listing("tru::1:1650000000:0:3:1:5\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no secret key imported"), "{err}");
    }
}

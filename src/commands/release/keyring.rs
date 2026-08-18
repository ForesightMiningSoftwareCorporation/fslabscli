//! A gpg keyring in a throwaway GNUPGHOME holding exactly the published
//! release key, so verification can never be satisfied by an unrelated key
//! already on the host. Used by publish (re-verifying detached signatures
//! before anything is written) and resolve (client-side verification).

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, bail};

pub(crate) struct TempKeyring {
    home: tempfile::TempDir,
}

impl TempKeyring {
    pub(crate) fn import(key: &[u8]) -> anyhow::Result<Self> {
        let home = tempfile::tempdir().context("cannot create a temporary GNUPGHOME")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700))?;
        }
        let keyring = Self { home };
        let mut child = keyring
            .gpg()
            .args(["--batch", "--quiet", "--import"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("cannot run gpg")?;
        child
            .stdin
            .as_mut()
            .expect("gpg stdin is piped")
            .write_all(key)?;
        let output = child.wait_with_output()?;
        if !output.status.success() {
            bail!(
                "gpg --import failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(keyring)
    }

    fn gpg(&self) -> Command {
        let mut command = Command::new("gpg");
        command.env("GNUPGHOME", self.home.path());
        command
    }

    pub(crate) fn verify_detached(&self, signature: &Path, file: &Path) -> anyhow::Result<()> {
        let output = self
            .gpg()
            .arg("--verify")
            .arg(signature)
            .arg(file)
            .output()
            .context("cannot run gpg")?;
        if !output.status.success() {
            bail!(
                "gpg --verify failed for {}: {}",
                file.display(),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(())
    }
}

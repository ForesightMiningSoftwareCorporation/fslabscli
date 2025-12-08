use std::{ffi::OsStr, path::Path, process::Stdio};
use tokio::{
    io::AsyncBufReadExt,
    process::{ChildStderr, ChildStdout},
};

/// A wrapper around a [`tokio::process::Command`] that runs a Bash script.
///
/// On Windows, Powershell will be used, but there is no guarantee of
/// cross-platform compatibility.
///
/// Optionally, stdout and stderr can be logged asynchronously to the current process's stdout
/// during script execution. This is useful in cases where the script might hang. If the script
/// does hang, the partially complete output would never be visible without enabling this logging.
pub struct Script {
    name: Option<String>,
    inner: tokio::process::Command,
    log_stdout: Option<tracing::Level>,
    log_stderr: Option<tracing::Level>,
}

impl Script {
    pub fn new(script: impl AsRef<str>, xtrace: bool) -> Self {
        // See https://www.gnu.org/software/bash/manual/html_node/The-Set-Builtin.html
        #[cfg(not(target_os = "windows"))]
        let script = format!(
            "set -o errexit
            set -o nounset
            set -o pipefail
            {}
            {}",
            if xtrace { "set -o xtrace" } else { "" },
            script.as_ref()
        );
        #[cfg(target_os = "windows")]
        let script = script.as_ref().to_string();

        let shell = if cfg!(target_os = "windows") {
            "powershell.exe"
        } else {
            "bash"
        };
        let mut inner = tokio::process::Command::new(shell);
        inner.arg("-c").arg(script);
        Self {
            name: None,
            inner,
            log_stdout: Default::default(),
            log_stderr: Default::default(),
        }
    }

    /// Set the name of this script, which will appear at the start of all logs
    /// when using [`Self::log_stdout`] or [`Self::log_stderr`].
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn current_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.inner
            .current_dir(dunce::canonicalize(dir).expect("Failed to canonicalize"));
        self
    }

    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.inner.env(key, value);
        self
    }

    pub fn maybe_env(mut self, key: impl AsRef<OsStr>, value: Option<impl AsRef<OsStr>>) -> Self {
        if let Some(value) = value {
            self.inner.env(key, value);
        }
        self
    }

    pub fn envs<I, K, V>(mut self, vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.inner.envs(vars);
        self
    }

    pub fn env_removals<S: AsRef<OsStr>>(mut self, removals: impl IntoIterator<Item = S>) -> Self {
        for key in removals {
            self.inner.env_remove(key);
        }
        self
    }

    pub fn log_stdout(mut self, level: tracing::Level) -> Self {
        self.log_stdout = Some(level);
        self
    }
    pub fn log_stderr(mut self, level: tracing::Level) -> Self {
        self.log_stderr = Some(level);
        self
    }

    // TODO: this should return Result and allow for error handling.
    //
    pub async fn execute(self) -> CommandOutput {
        let collect_stdio = true;
        let task = self._spawn(collect_stdio).unwrap();
        match task.wait().await {
            Ok(out) => out,
            Err(e) => e.into(),
        }
    }

    pub fn spawn(self) -> anyhow::Result<ScriptTask> {
        let collect_stdio = false;
        self._spawn(collect_stdio)
    }

    fn _spawn(self, collect_stdio: bool) -> anyhow::Result<ScriptTask> {
        let Self {
            name,
            mut inner,
            log_stdout,
            log_stderr,
        } = self;

        inner.stdout(Stdio::piped()).stderr(Stdio::piped());
        // Disable colors in log to get clean strings
        inner.env("NO_COLOR", "true");

        let mut child = inner.spawn()?;

        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");
        let pipe_task = tokio::task::spawn(pipe_stdio(
            name,
            stdout,
            stderr,
            collect_stdio,
            collect_stdio,
            log_stdout,
            log_stderr,
        ));

        Ok(ScriptTask { child, pipe_task })
    }
}

pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
}

impl From<std::process::Output> for CommandOutput {
    fn from(output: std::process::Output) -> Self {
        CommandOutput {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            success: output.status.success(),
        }
    }
}

impl From<anyhow::Error> for CommandOutput {
    fn from(error: anyhow::Error) -> Self {
        CommandOutput {
            stdout: "error".to_string(),
            stderr: error.to_string(),
            success: false,
        }
    }
}

impl From<std::io::Error> for CommandOutput {
    fn from(error: std::io::Error) -> Self {
        CommandOutput {
            stdout: "error".to_string(),
            stderr: error.to_string(),
            success: false,
        }
    }
}

pub struct ScriptTask {
    child: tokio::process::Child,
    pipe_task: tokio::task::JoinHandle<(Option<String>, Option<String>)>,
}

impl ScriptTask {
    pub async fn kill(mut self) -> anyhow::Result<()> {
        // TODO: We should check the target_os and send SIGTERM on Unix.
        self.child.kill().await?;
        self.pipe_task.await?;
        Ok(())
    }

    pub async fn wait(mut self) -> anyhow::Result<CommandOutput> {
        let status = self.child.wait().await?;
        tracing::debug!("Child exited with status {status}");
        let (stdout, stderr) = self.pipe_task.await?;
        tracing::trace!("Child stdout/stderr pipe task exited");
        Ok(CommandOutput {
            stdout: stdout.unwrap_or_default(),
            stderr: stderr.unwrap_or_default(),
            success: status.success(),
        })
    }
}

// TODO: return Result
async fn pipe_stdio(
    script_name: Option<String>,
    stdout: ChildStdout,
    stderr: ChildStderr,
    collect_stdout: bool,
    collect_stderr: bool,
    log_stdout: Option<tracing::Level>,
    log_stderr: Option<tracing::Level>,
) -> (Option<String>, Option<String>) {
    // WARNING: It is important that these pipes are polled concurrently.
    // Otherwise it's possible to encounter a deadlock because process IO pipes
    // have fixed-size buffers.
    //
    // For example, if the stdout pipe's buffer is full, the child will block
    // before writing more; if we are not consuming that pipe, but instead
    // waiting for a new line from stderr, that is a deadlock.
    //
    tokio::join!(
        pipe_output(
            PipeChannel::Stdout,
            collect_stdout,
            log_stdout,
            stdout,
            script_name.as_deref()
        ),
        pipe_output(
            PipeChannel::Stderr,
            collect_stderr,
            log_stderr,
            stderr,
            script_name.as_deref(),
        )
    )
}

macro_rules! dyn_event {
    ($lvl:ident, target = $target:expr, $($arg:tt)+) => {
        match $lvl {
            ::tracing::Level::TRACE => ::tracing::trace!(target: $target, $($arg)+),
            ::tracing::Level::DEBUG => ::tracing::debug!(target: $target, $($arg)+),
            ::tracing::Level::INFO => ::tracing::info!(target: $target, $($arg)+),
            ::tracing::Level::WARN => ::tracing::warn!(target: $target, $($arg)+),
            ::tracing::Level::ERROR => ::tracing::error!(target: $target, $($arg)+),
        }
    };
}

enum PipeChannel {
    Stdout,
    Stderr,
}

async fn pipe_output<R: tokio::io::AsyncRead + Unpin>(
    channel: PipeChannel,
    collect: bool,
    log_level: Option<tracing::Level>,
    stream: R,
    script_name: Option<&str>,
) -> Option<String> {
    let mut stream = tokio::io::BufReader::new(stream).lines();
    let mut out_string = String::new();
    while let Ok(Some(line)) = stream.next_line().await {
        if collect {
            out_string.push_str(&line);
            out_string.push('\n');
        }
        if let Some(level) = log_level {
            // Override the target (module path) because the process name + pipe
            // name is more relevant.
            let message = if let Some(name) = &script_name {
                format!("[{name}] {line}")
            } else {
                line
            };
            match channel {
                PipeChannel::Stdout => {
                    dyn_event!(level, target = "stdout", "{message}");
                }
                PipeChannel::Stderr => {
                    dyn_event!(level, target = "stderr", "{message}");
                }
            }
        }
    }
    collect.then_some(out_string)
}

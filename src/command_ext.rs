use std::{
    ffi::{OsStr, OsString},
    path::Path,
    process::Stdio,
};
use tokio::{
    io::AsyncBufReadExt,
    process::{ChildStderr, ChildStdout},
};
use tracing::{debug, error, info, trace, warn};

/// A wrapper around [`tokio::process::Command`] with some extended behavior.
///
/// Optionally, stdout and stderr can be logged asynchronously to the current process's stdout
/// during command execution. This is useful in cases where the command might hang. If the command
/// does hang, the partially complete output would never be visible without enabling this logging.
pub struct Command {
    inner: tokio::process::Command,
    command: OsString,
    log_stdout: Option<tracing::Level>,
    log_stderr: Option<tracing::Level>,
}

impl Command {
    pub fn new(command: impl AsRef<OsStr>) -> Self {
        let shell = if cfg!(target_os = "windows") {
            "powershell.exe"
        } else {
            "bash"
        };
        let mut inner = tokio::process::Command::new(shell);
        inner.arg("-c").arg(command.as_ref());
        Self {
            inner,
            command: command.as_ref().into(),
            log_stdout: Default::default(),
            log_stderr: Default::default(),
        }
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

    pub fn spawn(self) -> anyhow::Result<CommandTask> {
        let collect_stdio = false;
        self._spawn(collect_stdio)
    }

    fn _spawn(self, collect_stdio: bool) -> anyhow::Result<CommandTask> {
        let Self {
            mut inner,
            command,
            log_stdout,
            log_stderr,
        } = self;

        info!("Running: {command:?}");

        inner.stdout(Stdio::piped()).stderr(Stdio::piped());
        // Disable colors in log to get clean strings
        inner.env("NO_COLOR", "true");

        let mut child = inner.spawn()?;

        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");
        let pipe_task = tokio::task::spawn(pipe_stdio(
            stdout,
            stderr,
            collect_stdio,
            collect_stdio,
            log_stdout,
            log_stderr,
        ));

        Ok(CommandTask { child, pipe_task })
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

pub struct CommandTask {
    child: tokio::process::Child,
    pipe_task: tokio::task::JoinHandle<(Option<String>, Option<String>)>,
}

impl CommandTask {
    pub async fn kill(mut self) -> anyhow::Result<()> {
        // TODO: We should check the target_os and send SIGTERM on Unix.
        self.child.kill().await?;
        self.pipe_task.await?;
        Ok(())
    }

    pub async fn wait(mut self) -> anyhow::Result<CommandOutput> {
        let status = self.child.wait().await?;
        let (stdout, stderr) = self.pipe_task.await?;
        Ok(CommandOutput {
            stdout: stdout.unwrap_or_default(),
            stderr: stderr.unwrap_or_default(),
            success: status.success(),
        })
    }
}

// TODO: return Result
async fn pipe_stdio(
    stdout: ChildStdout,
    stderr: ChildStderr,
    collect_stdout: bool,
    collect_stderr: bool,
    log_stdout: Option<tracing::Level>,
    log_stderr: Option<tracing::Level>,
) -> (Option<String>, Option<String>) {
    let mut stdout_stream = tokio::io::BufReader::new(stdout).lines();
    let mut stdout_string = String::new();

    let mut stderr_stream = tokio::io::BufReader::new(stderr).lines();
    let mut stderr_string = String::new();

    loop {
        tokio::select! {
            Ok(Some(line)) = stdout_stream.next_line() =>  {
                if collect_stdout {
                    stdout_string.push_str(&format!("{line}\n"));
                }
                if let Some(level) = log_stdout {
                    let stdout = format!(" | {line}");
                    match level {
                        tracing::Level::ERROR => error!(stdout),
                        tracing::Level::WARN => warn!(stdout),
                        tracing::Level::INFO => info!(stdout),
                        tracing::Level::DEBUG => debug!(stdout),
                        tracing::Level::TRACE => trace!(stdout),
                    }
                }
            },
            Ok(Some(line)) = stderr_stream.next_line() =>  {
                if collect_stderr {
                    stderr_string.push_str(&format!("{line}\n"));
                }
                if let Some(level) = log_stderr {
                    let stderr = format!(" | {line}");
                    match level {
                        tracing::Level::ERROR => error!(stderr),
                        tracing::Level::WARN => warn!(stderr),
                        tracing::Level::INFO => info!(stderr),
                        tracing::Level::DEBUG => debug!(stderr),
                        tracing::Level::TRACE => trace!(stderr),
                    }
                }
            },
            else => break,
        }
    }

    (
        collect_stdout.then_some(stdout_string),
        collect_stderr.then_some(stderr_string),
    )
}

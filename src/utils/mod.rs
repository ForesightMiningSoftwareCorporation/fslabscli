use std::env::VarError;

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    process::Stdio,
};
use tokio::io::AsyncBufReadExt;
use tracing::{debug, error, info, trace, warn};

pub mod auto_update;
pub mod cargo;
pub mod github;
#[cfg(test)]
pub mod test;

/// [`execute_command`] with intermediate logging disabled.
pub async fn execute_command_without_logging(
    command: &str,
    dir: &PathBuf,
    envs: &HashMap<String, String>,
    envs_remove: &HashSet<String>,
) -> CommandOutput {
    execute_command(command, dir, envs, envs_remove, None, None).await
}

/// Execute the `command`, returning stdout and stderr as strings, and success state as a boolean.
///
/// Optionally, stdout and stderr can be logged asynchronously to the current process's stdout
/// during command execution. This is useful in cases where the command might hang. If the command
/// does hang, the partially complete output would never be visible without enabling this logging.
pub async fn execute_command(
    command: &str,
    dir: &PathBuf,
    envs: &HashMap<String, String>,
    envs_remove: &HashSet<String>,
    log_stdout: Option<tracing::Level>,
    log_stderr: Option<tracing::Level>,
) -> CommandOutput {
    let shell = if cfg!(target_os = "windows") {
        "powershell.exe"
    } else {
        "bash"
    };

    let mut c = tokio::process::Command::new(shell);
    c.arg("-c")
        .arg(command)
        .current_dir(dunce::canonicalize(dir).expect("Failed to canonicalize"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    info!("Running: {}", command);

    for env in envs_remove {
        c.env_remove(env);
    }
    c.envs(envs);
    // disable colors in log to get clean strings
    c.env("NO_COLOR", "true");

    let mut child = c.spawn().expect("Unable to spawn command");

    let stdout = child.stdout.take().expect("Failed to get stdout");
    let mut stdout_stream = tokio::io::BufReader::new(stdout).lines();
    let mut stdout_string = String::new();

    let stderr = child.stderr.take().expect("Failed to get stderr");
    let mut stderr_stream = tokio::io::BufReader::new(stderr).lines();
    let mut stderr_string = String::new();

    loop {
        tokio::select! {
            Ok(Some(line)) = stdout_stream.next_line() =>  {
                stdout_string.push_str(&format!("{line}\n"));
                if let Some(l) = log_stdout {
                    let stdout = format!(" | {line}");
                    match l {
                        tracing::Level::ERROR => error!(stdout),
                        tracing::Level::WARN => warn!(stdout),
                        tracing::Level::INFO => info!(stdout),
                        tracing::Level::DEBUG => debug!(stdout),
                        tracing::Level::TRACE => trace!(stdout),
                    }
                }
            },
            Ok(Some(line)) = stderr_stream.next_line() =>  {
                stderr_string.push_str(&format!("{line}\n"));
                if let Some(l) = log_stderr {
                    let stderr = format!(" | {line}");
                    match l {
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

    let status = child.wait().await;

    match status {
        Ok(output) => {
            let exit_code = output.code().unwrap_or(1);
            CommandOutput {
                stdout: stdout_string.to_string(),
                stderr: stderr_string,
                success: exit_code == 0,
            }
        }
        Err(e) => e.into(),
    }
}

pub fn get_env_or_log(env_name: String) -> Result<String, VarError> {
    std::env::var(&env_name).map_err(|e| {
        warn!("Failed to load env `{}` {}", env_name, e);
        e
    })
}
pub fn get_registry_env(registry_name: String) -> HashMap<String, String> {
    let mut envs = HashMap::from([
        (
            "CARGO_NET_GIT_FETCH_WITH_CLI".to_string(),
            "true".to_string(),
        ),
        ("GIT_SSH_COMMAND".to_string(), "ssh".to_string()),
        ("SSH_AUTH_SOCK".to_string(), "".to_string()),
    ]);
    let registry_prefix =
        format!("CARGO_REGISTRIES_{}", registry_name.replace("-", "_")).to_uppercase();
    if let Ok(index) = get_env_or_log(format!("{registry_prefix}_INDEX")) {
        envs.insert(format!("{registry_prefix}_INDEX"), index.clone());
    }
    if let Ok(token) = get_env_or_log(format!("{registry_prefix}_TOKEN")) {
        envs.insert(format!("{registry_prefix}_TOKEN"), token.clone());
        envs.insert("Authorization".to_string(), token.clone());
    }
    if let Ok(user_agent) = get_env_or_log(format!("{registry_prefix}_USER_AGENT")) {
        envs.insert("CARGO_HTTP_USER_AGENT".to_string(), user_agent.clone());
    }
    if let Ok(private_key) = get_env_or_log(format!("{registry_prefix}_PRIVATE_KEY")) {
        envs.insert(
            "GIT_SSH_COMMAND".to_string(),
            format!("ssh -i {}", private_key.clone()),
        );
    }
    envs
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

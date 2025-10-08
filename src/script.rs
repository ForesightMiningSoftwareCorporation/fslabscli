use crate::command_ext::{Command, CommandOutput};
use std::{collections::HashMap, path::PathBuf};

pub struct Script {
    name: String,
    commands: Vec<String>,
    current_dir: Option<PathBuf>,
    env: HashMap<String, String>,
}

impl Script {
    pub fn new(name: impl Into<String>, script: String) -> Self {
        let commands = script.split("\n").map(ToString::to_string).collect();
        Self {
            name: name.into(),
            commands,
            current_dir: None,
            env: Default::default(),
        }
    }

    pub fn current_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(dir.into());
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    pub async fn run(self) -> CommandOutput {
        let Self {
            name,
            commands,
            current_dir,
            env,
        } = self;

        let mut all_stdout = String::default();
        let mut all_stderr = String::default();
        let mut success = true;
        for line in commands {
            if line.is_empty() {
                continue;
            }
            let mut command = Command::new(&line);
            for (key, value) in &env {
                command = command.env(key, value);
            }
            if let Some(dir) = &current_dir {
                command = command.current_dir(dir);
            }
            let command_output = command.execute().await;
            all_stdout.push_str(&command_output.stdout);
            all_stdout.push('\n');
            all_stderr.push_str(&command_output.stderr);
            all_stderr.push('\n');
            tracing::debug!(
                "{name}: CMD='{line}'\nSTDOUT={}\nSTDERR={}",
                command_output.stdout,
                command_output.stderr
            );
            if !command_output.success {
                success = false;
                break;
            }
        }

        CommandOutput {
            stdout: all_stdout,
            stderr: all_stderr,
            success,
        }
    }
}

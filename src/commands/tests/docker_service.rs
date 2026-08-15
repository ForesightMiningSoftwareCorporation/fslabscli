use rand::distr::{Alphanumeric, SampleString};
use std::time::Duration;

use crate::script::{CommandOutput, Script};

#[cfg_attr(test, mockall::automock)]
trait DockerCommandRunner {
    async fn execute(&self, command: String) -> CommandOutput;
}

struct ScriptDockerCommandRunner;

impl DockerCommandRunner for ScriptDockerCommandRunner {
    async fn execute(&self, command: String) -> CommandOutput {
        Script::new(command, true).execute().await
    }
}

pub struct DockerContainer {
    pub prefix: String,
    pub env: String,
    pub port: String,
    pub image: String,
    pub command: String,
}

impl DockerContainer {
    pub fn azurite(blob_port: u16) -> Self {
        Self {
            prefix: "azurite".into(),
            env: "".into(),
            port: format!("-p {blob_port}:10000"),
            image: "mcr.microsoft.com/azure-storage/azurite".into(),
            command: Default::default(),
        }
    }

    pub fn minio(service_port: u16) -> Self {
        Self {
            prefix: "minio".into(),
            env: "-e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin".into(),
            port: format!("-p {service_port}:9000"),
            image: "minio/minio:latest".into(),
            command: "server /data".into(),
        }
    }

    pub fn postgres(port: u16) -> Self {
        Self {
            prefix: "postgres".into(),
            env: format!("-e POSTGRES_PASSWORD={DB_PASSWORD} -e POSTGRES_DB={DB_NAME}"),
            port: format!("-p {port}:5432"),
            image: "postgres:alpine".into(),
            command: Default::default(),
        }
    }

    pub async fn create(self) -> anyhow::Result<DockerProcess> {
        let suffix = Alphanumeric.sample_string(&mut rand::rng(), 6);
        let ownership_token = Alphanumeric.sample_string(&mut rand::rng(), 16);
        let container_name = format!("{}_{suffix}", self.prefix);
        self.create_with_runner(
            &ScriptDockerCommandRunner,
            container_name,
            ownership_token,
            Duration::from_secs(5),
        )
        .await
    }

    async fn create_with_runner<R: DockerCommandRunner>(
        self,
        runner: &R,
        container_name: String,
        ownership_token: String,
        startup_delay: Duration,
    ) -> anyhow::Result<DockerProcess> {
        let Self {
            prefix: _,
            env,
            port,
            image,
            command,
        } = self;
        let run_output = runner
            .execute(format!(
                "docker run --label={TEST_RUN_LABEL}={ownership_token} --name={container_name} -d {env} {port} {image} {command}"
            ))
            .await;
        if !run_output.success {
            let error = output_error(&run_output).to_string();
            let logs = cleanup_owned_container(runner, &ownership_token).await;
            if let Some(logs) = logs {
                anyhow::bail!(
                    "failed to start Docker service {container_name}: {error}; {}",
                    format_logs(&logs)
                );
            }
            anyhow::bail!("failed to start Docker service {container_name}: {error}");
        }

        let container_id = run_output.stdout.trim().to_string();
        if !valid_container_id(&container_id) {
            let logs = cleanup_owned_container(runner, &ownership_token).await;
            anyhow::bail!(
                "docker run for service {container_name} returned an invalid container ID: {}{}",
                if container_id.is_empty() {
                    "empty output"
                } else {
                    "unexpected output"
                },
                logs.as_ref()
                    .map(|logs| format!("; {}", format_logs(logs)))
                    .unwrap_or_default()
            );
        }

        tokio::time::sleep(startup_delay).await;
        let status_output = runner
            .execute(format!(
                "docker ps --no-trunc -q --filter id={container_id}"
            ))
            .await;
        let running_id = status_output.stdout.trim();
        if !status_output.success || running_id != container_id {
            let process = DockerProcess {
                container_id: container_id.clone(),
            };
            let logs = process.logs_with_runner(runner).await;
            process.teardown_with_runner(runner).await;

            if !status_output.success {
                anyhow::bail!(
                    "failed to inspect Docker service {container_name} ({container_id}): {}; {}",
                    output_error(&status_output),
                    format_logs(&logs)
                );
            }
            if running_id.is_empty() {
                anyhow::bail!(
                    "Docker service {container_name} ({container_id}) exited during startup; {}",
                    format_logs(&logs)
                );
            }
            anyhow::bail!(
                "Docker service {container_name} returned an unexpected running container ID; {}",
                format_logs(&logs)
            );
        }

        Ok(DockerProcess { container_id })
    }
}

#[derive(Clone, Debug)]
pub struct DockerProcess {
    container_id: String,
}

impl DockerProcess {
    pub async fn teardown(self) {
        self.teardown_with_runner(&ScriptDockerCommandRunner).await;
    }

    async fn logs_with_runner<R: DockerCommandRunner>(&self, runner: &R) -> CommandOutput {
        runner
            .execute(format!("docker logs --tail 200 {}", self.container_id))
            .await
    }

    async fn teardown_with_runner<R: DockerCommandRunner>(self, runner: &R) {
        let Self { container_id } = self;
        let stop_output = runner.execute(format!("docker stop {container_id}")).await;
        if !stop_output.success {
            tracing::warn!(
                "docker stop {container_id} failed: {}",
                stop_output.stderr.trim_end()
            );
        }
        let rm_output = runner
            .execute(format!("docker rm --force --volumes {container_id}"))
            .await;
        if !rm_output.success {
            tracing::warn!(
                "docker rm --force --volumes {container_id} failed: {}",
                rm_output.stderr.trim_end()
            );
        }
    }
}

fn valid_container_id(container_id: &str) -> bool {
    container_id.len() == 64
        && container_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

async fn cleanup_owned_container<R: DockerCommandRunner>(
    runner: &R,
    ownership_token: &str,
) -> Option<CommandOutput> {
    let find_output = runner
        .execute(format!(
            "docker ps --all --no-trunc --quiet --filter label={TEST_RUN_LABEL}={ownership_token}"
        ))
        .await;
    if !find_output.success {
        tracing::warn!(
            "failed to find Docker service for cleanup: {}",
            output_error(&find_output)
        );
        return None;
    }

    let container_id = find_output.stdout.trim();
    if container_id.is_empty() {
        return None;
    }
    if !valid_container_id(container_id) {
        tracing::warn!("Docker cleanup query returned an invalid container ID");
        return None;
    }

    let process = DockerProcess {
        container_id: container_id.to_string(),
    };
    let logs = process.logs_with_runner(runner).await;
    process.teardown_with_runner(runner).await;
    Some(logs)
}

fn output_error(output: &CommandOutput) -> &str {
    let stderr = output.stderr.trim();
    if stderr.is_empty() {
        output.stdout.trim()
    } else {
        stderr
    }
}

fn format_logs(output: &CommandOutput) -> String {
    let stdout = output.stdout.trim();
    let stderr = output.stderr.trim();
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => "no container logs were available".to_string(),
        (false, true) => format!("container logs:\n{stdout}"),
        (true, false) => format!("container logs:\n{stderr}"),
        (false, false) => format!("container logs:\n{stdout}\n{stderr}"),
    }
}

pub fn postgres_url(port: u16) -> String {
    format!("postgres://postgres:{DB_PASSWORD}@localhost:{port}/{DB_NAME}")
}

static DB_PASSWORD: &str = "mypassword";
static DB_NAME: &str = "tests";
static TEST_RUN_LABEL: &str = "ca.fslabs.fslabscli.test-run";

#[cfg(test)]
mod tests {
    use super::*;
    use mockall::{Sequence, predicate::eq};

    const CONTAINER_ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const CONTAINER_NAME: &str = "minio_test";
    const OWNERSHIP_TOKEN: &str = "fslabsclitest1234";
    const RUN_COMMAND: &str = "docker run --label=ca.fslabs.fslabscli.test-run=fslabsclitest1234 --name=minio_test -d -e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin -p 9000:9000 minio/minio:latest server /data";
    const FIND_COMMAND: &str = "docker ps --all --no-trunc --quiet --filter label=ca.fslabs.fslabscli.test-run=fslabsclitest1234";

    fn output(success: bool, stdout: &str, stderr: &str) -> CommandOutput {
        CommandOutput {
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            success,
        }
    }

    fn expect_command(
        runner: &mut MockDockerCommandRunner,
        sequence: &mut Sequence,
        command: &str,
        result: CommandOutput,
    ) {
        runner
            .expect_execute()
            .with(eq(command.to_string()))
            .times(1)
            .in_sequence(sequence)
            .return_const(result);
    }

    #[tokio::test]
    async fn create_and_teardown_removes_volumes() {
        let mut runner = MockDockerCommandRunner::new();
        let mut sequence = Sequence::new();
        expect_command(
            &mut runner,
            &mut sequence,
            RUN_COMMAND,
            output(true, CONTAINER_ID, ""),
        );
        expect_command(
            &mut runner,
            &mut sequence,
            &format!("docker ps --no-trunc -q --filter id={CONTAINER_ID}"),
            output(true, CONTAINER_ID, ""),
        );
        expect_command(
            &mut runner,
            &mut sequence,
            &format!("docker stop {CONTAINER_ID}"),
            output(true, CONTAINER_ID, ""),
        );
        expect_command(
            &mut runner,
            &mut sequence,
            &format!("docker rm --force --volumes {CONTAINER_ID}"),
            output(true, CONTAINER_ID, ""),
        );

        let process = DockerContainer::minio(9000)
            .create_with_runner(
                &runner,
                CONTAINER_NAME.to_string(),
                OWNERSHIP_TOKEN.to_string(),
                Duration::ZERO,
            )
            .await
            .unwrap();
        process.teardown_with_runner(&runner).await;
    }

    #[tokio::test]
    async fn create_cleans_up_when_service_exits() {
        let mut runner = MockDockerCommandRunner::new();
        let mut sequence = Sequence::new();
        expect_command(
            &mut runner,
            &mut sequence,
            RUN_COMMAND,
            output(true, CONTAINER_ID, ""),
        );
        expect_command(
            &mut runner,
            &mut sequence,
            &format!("docker ps --no-trunc -q --filter id={CONTAINER_ID}"),
            output(true, "", ""),
        );
        expect_command(
            &mut runner,
            &mut sequence,
            &format!("docker logs --tail 200 {CONTAINER_ID}"),
            output(true, "disk full", ""),
        );
        expect_command(
            &mut runner,
            &mut sequence,
            &format!("docker stop {CONTAINER_ID}"),
            output(false, "", "container is not running"),
        );
        expect_command(
            &mut runner,
            &mut sequence,
            &format!("docker rm --force --volumes {CONTAINER_ID}"),
            output(true, CONTAINER_ID, ""),
        );

        let error = DockerContainer::minio(9000)
            .create_with_runner(
                &runner,
                CONTAINER_NAME.to_string(),
                OWNERSHIP_TOKEN.to_string(),
                Duration::ZERO,
            )
            .await
            .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("exited during startup"));
        assert!(message.contains("disk full"));
    }

    #[tokio::test]
    async fn create_cleans_up_when_status_check_fails() {
        let mut runner = MockDockerCommandRunner::new();
        let mut sequence = Sequence::new();
        expect_command(
            &mut runner,
            &mut sequence,
            RUN_COMMAND,
            output(true, CONTAINER_ID, ""),
        );
        expect_command(
            &mut runner,
            &mut sequence,
            &format!("docker ps --no-trunc -q --filter id={CONTAINER_ID}"),
            output(false, "", "Docker daemon unavailable"),
        );
        expect_command(
            &mut runner,
            &mut sequence,
            &format!("docker logs --tail 200 {CONTAINER_ID}"),
            output(true, "service output", ""),
        );
        expect_command(
            &mut runner,
            &mut sequence,
            &format!("docker stop {CONTAINER_ID}"),
            output(false, "", "Docker daemon unavailable"),
        );
        expect_command(
            &mut runner,
            &mut sequence,
            &format!("docker rm --force --volumes {CONTAINER_ID}"),
            output(true, CONTAINER_ID, ""),
        );

        let error = DockerContainer::minio(9000)
            .create_with_runner(
                &runner,
                CONTAINER_NAME.to_string(),
                OWNERSHIP_TOKEN.to_string(),
                Duration::ZERO,
            )
            .await
            .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("Docker daemon unavailable"));
        assert!(message.contains("service output"));
    }

    #[tokio::test]
    async fn create_rejects_empty_run_id_and_cleans_up_owned_container() {
        let mut runner = MockDockerCommandRunner::new();
        let mut sequence = Sequence::new();
        expect_command(
            &mut runner,
            &mut sequence,
            RUN_COMMAND,
            output(true, "", ""),
        );
        expect_command(
            &mut runner,
            &mut sequence,
            FIND_COMMAND,
            output(true, CONTAINER_ID, ""),
        );
        expect_command(
            &mut runner,
            &mut sequence,
            &format!("docker logs --tail 200 {CONTAINER_ID}"),
            output(true, "service output", ""),
        );
        expect_command(
            &mut runner,
            &mut sequence,
            &format!("docker stop {CONTAINER_ID}"),
            output(true, CONTAINER_ID, ""),
        );
        expect_command(
            &mut runner,
            &mut sequence,
            &format!("docker rm --force --volumes {CONTAINER_ID}"),
            output(true, CONTAINER_ID, ""),
        );

        let error = DockerContainer::minio(9000)
            .create_with_runner(
                &runner,
                CONTAINER_NAME.to_string(),
                OWNERSHIP_TOKEN.to_string(),
                Duration::ZERO,
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("empty output"));
    }

    #[tokio::test]
    async fn create_does_not_remove_a_container_when_run_fails() {
        let mut runner = MockDockerCommandRunner::new();
        let mut sequence = Sequence::new();
        expect_command(
            &mut runner,
            &mut sequence,
            RUN_COMMAND,
            output(false, "", "container name already in use"),
        );
        expect_command(
            &mut runner,
            &mut sequence,
            FIND_COMMAND,
            output(true, "", ""),
        );

        let error = DockerContainer::minio(9000)
            .create_with_runner(
                &runner,
                CONTAINER_NAME.to_string(),
                OWNERSHIP_TOKEN.to_string(),
                Duration::ZERO,
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("container name already in use"));
    }

    #[tokio::test]
    async fn create_cleans_up_an_owned_container_when_run_fails() {
        let mut runner = MockDockerCommandRunner::new();
        let mut sequence = Sequence::new();
        expect_command(
            &mut runner,
            &mut sequence,
            RUN_COMMAND,
            output(false, "", "port is already allocated"),
        );
        expect_command(
            &mut runner,
            &mut sequence,
            FIND_COMMAND,
            output(true, CONTAINER_ID, ""),
        );
        expect_command(
            &mut runner,
            &mut sequence,
            &format!("docker logs --tail 200 {CONTAINER_ID}"),
            output(true, "service failed to start", ""),
        );
        expect_command(
            &mut runner,
            &mut sequence,
            &format!("docker stop {CONTAINER_ID}"),
            output(false, "", "container is not running"),
        );
        expect_command(
            &mut runner,
            &mut sequence,
            &format!("docker rm --force --volumes {CONTAINER_ID}"),
            output(true, CONTAINER_ID, ""),
        );

        let error = DockerContainer::minio(9000)
            .create_with_runner(
                &runner,
                CONTAINER_NAME.to_string(),
                OWNERSHIP_TOKEN.to_string(),
                Duration::ZERO,
            )
            .await
            .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("port is already allocated"));
        assert!(message.contains("service failed to start"));
    }

    #[tokio::test]
    async fn teardown_removes_volumes_after_stop_failure() {
        let mut runner = MockDockerCommandRunner::new();
        let mut sequence = Sequence::new();
        expect_command(
            &mut runner,
            &mut sequence,
            &format!("docker stop {CONTAINER_ID}"),
            output(false, "", "stop failed"),
        );
        expect_command(
            &mut runner,
            &mut sequence,
            &format!("docker rm --force --volumes {CONTAINER_ID}"),
            output(true, CONTAINER_ID, ""),
        );

        DockerProcess {
            container_id: CONTAINER_ID.to_string(),
        }
        .teardown_with_runner(&runner)
        .await;
    }
}

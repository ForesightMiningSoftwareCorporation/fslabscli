use rand::distr::{Alphanumeric, SampleString};
use std::time::Duration;

use crate::command_ext::Command;

pub struct DockerContainer {
    pub prefix: String,
    pub env: String,
    pub port: String,
    pub options: String,
    pub image: String,
    pub command: String,
}

impl DockerContainer {
    pub fn azurite() -> Self {
        Self {
            prefix: "azurite".into(),
            env: "".into(),
            port: "-p 10000:10000 -p 10001:10001 -p 10002:10002".into(),
            options: "".into(),
            image: "mcr.microsoft.com/azure-storage/azurite".into(),
            command: Default::default(),
        }
    }

    pub fn minio(service_port: u16) -> Self {
        Self {
            prefix: "minio".into(),
            env: "-e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin".into(),
            port: format!("-p {service_port}:9000"),
            options: "server /data --console-address :9001".into(),
            image: "minio/minio:latest".into(),
            command: "server /data --address=0.0.0.0:9000".into(),
        }
    }

    pub fn postgres(port: u16) -> Self {
        Self {
            prefix: "postgres".into(),
            env: format!("-e POSTGRES_PASSWORD={DB_PASSWORD} -e POSTGRES_DB={DB_NAME}"),
            port: format!("-p {port}:5432"),
            options: "".into(),
            image: "postgres:alpine".into(),
            command: Default::default(),
        }
    }

    pub async fn create(self) -> anyhow::Result<DockerProcess> {
        let Self {
            prefix,
            env,
            port,
            options,
            image,
            command,
        } = self;
        let suffix = Alphanumeric.sample_string(&mut rand::rng(), 6);
        let container_name = format!("{prefix}_{suffix}");
        let path = std::env::current_dir().unwrap();
        let command_output = Command::new(format!(
            "docker run --name={container_name} -d {env} {port} {options} {image} {command}"
        ))
        .current_dir(&path)
        .execute()
        .await;
        if !command_output.success {
            return Err(anyhow::anyhow!(command_output.stderr));
        }
        // HACK: Wait 5 Sec
        tokio::time::sleep(Duration::from_millis(5000)).await;
        let command_output = Command::new(format!("docker ps -q -f name={container_name}"))
            .current_dir(&path)
            .execute()
            .await;
        if !command_output.success {
            return Err(anyhow::anyhow!(command_output.stderr));
        }
        Ok(DockerProcess {
            container_id: command_output.stdout,
        })
    }
}

#[derive(Clone)]
pub struct DockerProcess {
    container_id: String,
}

impl DockerProcess {
    pub async fn teardown(self) {
        let Self { container_id } = self;
        let path = std::env::current_dir().unwrap();
        Command::new(format!("docker stop {container_id}"))
            .current_dir(&path)
            .execute()
            .await;
        Command::new(format!("docker rm {container_id}"))
            .current_dir(&path)
            .execute()
            .await;
    }
}

pub fn postgres_url(port: u16) -> String {
    format!("postgres://postgres:{DB_PASSWORD}@localhost:{port}/{DB_NAME}")
}

static DB_PASSWORD: &str = "mypassword";
static DB_NAME: &str = "tests";

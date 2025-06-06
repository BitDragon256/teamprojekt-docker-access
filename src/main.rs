#![allow(dead_code)]

use bollard::Docker;
use bollard::container::{StartContainerOptions, Config, DownloadFromContainerOptions};
use futures_util::StreamExt;
use tar::Archive;

struct AgentEnvironment {
    pub agent_container_image: String,
    pub llm_responses: Vec<LLMResponse>,
}

struct GitCmd {}
struct LLMCall {
    pub prompt: String,
}
struct LLMResponse {
    pub message: Option<String>,
    pub tool_call: Option<String>,
}

struct AgentExpectedActions {
    pub files: Vec<String>,
    pub git_cmds: Vec<GitCmd>,
    pub llm_calls: Vec<LLMCall>,
}

fn test_agent(environment: AgentEnvironment, actions: AgentExpectedActions) -> bool {
    true
}

fn main() {
    tokio::runtime::Runtime::new().unwrap().block_on(async { start_docker_container("agent-test-42").await.unwrap(); });
}

async fn start_docker_container(image_name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let docker = Docker::connect_with_local_defaults()?;

    let container_config = Config {
        image: Some(image_name),
        ..Default::default()
    };

    let container = docker
        .create_container::<&str, &str>(None, container_config)
        .await?;

    let container_id = container.id;

    docker
        .start_container::<String>(&container_id, None::<StartContainerOptions<String>>)
        .await?;

    println!("started container with id {}", container_id);

    let options = DownloadFromContainerOptions {
        path: "/app".to_string(),
    };

    let mut tar_stream = docker.download_from_container(&container_id, Some(options));

    let mut archive_data = Vec::new();
    while let Some(Ok(chunk)) = tar_stream.next().await {
        archive_data.extend(chunk);
    }

    let mut archive = Archive::new(&archive_data[..]);
    for entry in archive.entries()? {
        let entry = entry?;
        let path = entry.path()?;
        println!("found file: {:?}", path);
    }

    Ok(())
}
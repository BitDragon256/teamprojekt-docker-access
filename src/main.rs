#![allow(dead_code)]

mod http_server;

use std::net::SocketAddr;

use bollard::Docker;
use bollard::container::{StartContainerOptions, Config, DownloadFromContainerOptions};
use futures_util::StreamExt;
use tar::Archive;
use axum::{Router};

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

// ==============================

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn get_docker() -> Result<Docker> {
    Ok(Docker::connect_with_local_defaults()?)
}

fn main() {
    tokio::runtime::Runtime::new().unwrap().block_on(async { main_loop().await.unwrap() });
}

async fn main_loop() -> Result<()> {
    let docker = get_docker()?;

    // setup LLM API endpoints
    setup_llm_api_endpoints().await?;

    // setup Task API endpoints

    // setup HTTP proxy

    // start agent container
    start_agent_container(&docker, "agent-42").await?;

    // ... running ...

    // end agent container

    // join server processes

    Ok(())
}

async fn llm_api_response() -> impl axum::response::IntoResponse {
    "hello world!"
}
async fn setup_llm_api_endpoints() -> Result<()> {
    let app = Router::new()
        .route("/data", axum::routing::get(llm_api_response));
    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

/// starts the agent container from the given image
/// returns the id of the started container
async fn start_agent_container(docker: &Docker, image_name: &str) -> Result<String> {
    let container_config = Config {
        image: Some(image_name),
        ..Default::default()
    };

    let container = docker
        .create_container::<&str, &str>(None, container_config)
        .await?;

    let container_id = container.id;

    docker
        .start_container::<&str>(&container_id, None)
        .await?;

    println!("started agent container with id {}", container_id);

    Ok(container_id)
}

/// extracts all files from the container under `container_id`
/// currently only returns the file paths
async fn get_files(docker: &Docker, container_id: &str) -> Result<Vec<String>> {
    let options = DownloadFromContainerOptions {
        path: "/app".to_string(),
    };

    let mut tar_stream = docker.download_from_container(container_id, Some(options));

    let mut archive_data = Vec::new();
    while let Some(Ok(chunk)) = tar_stream.next().await {
        archive_data.extend(chunk);
    }

    let mut file_paths = Vec::new();

    let mut archive = Archive::new(&archive_data[..]);
    for entry in archive.entries()? {
        let entry = entry?;
        let path = entry.path()?;
        println!("found file: {:?}", path);

        file_paths.push(path.to_str().unwrap().to_owned())
    }

    Ok(file_paths)
}
#![allow(dead_code)]

mod http_server;

use std::fmt::{Display, Formatter};
use std::net::SocketAddr;
use bollard::Docker;
use bollard::container::{AttachContainerOptions, Config, DownloadFromContainerOptions, LogOutput, WaitContainerOptions};
use bollard::models::HostConfig;
use futures_util::StreamExt;
use serde::Serialize;
use tar::Archive;
use crate::http_server::{HttpRequest, HttpResponse, Server};

// ==============================

enum APIEndpoint {
    TaskSuccess,
    TaskFailure,
    TaskInfo,

    LLMRequest,
}

impl Into<&str> for APIEndpoint {
    fn into(self) -> &'static str {
        match self {
            APIEndpoint::TaskSuccess => "/api/agent/task/complete",
            APIEndpoint::TaskFailure => "/api/agent/task/fail",
            APIEndpoint::TaskInfo => "/api/agent/task",
            APIEndpoint::LLMRequest => "/api/*",
        }
    }
}

#[derive(Debug)]
enum Error {
    AgentCrashed(String),
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Test Error: {:?}", self)
    }
}

impl std::error::Error for Error {}

// ==============================

struct AgentEnvironment {
    /// The name of the agent image in the local docker image list
    pub agent_image: String,
    /// Actions which have to be done in the given order.
    pub serial_actions: Vec<AgentExpectedAction>,
    /// Actions which can be done at any time and just have to happen at least once.
    pub parallel_actions: Vec<AgentExpectedAction>,

    /// The task which is provided to the agent.
    pub task: Task,

    /// The current index of the serial actions
    serial_action_index: usize,
    /// All incoming requests
    logged_requests: Vec<HttpRequest>,
}

struct GitCmd {}
struct LLMCall {
    pub prompt: Option<String>,
    pub response: LLMResponse,
}
impl LLMCall {
    fn new(response: LLMResponse, prompt: Option<&str>) -> Self {
        Self {
            response,
            prompt: prompt.map(|s| s.to_owned())
        }
    }
}

#[derive(Clone)]
pub enum LLMResponse {
    Message(String),
    ToolCall(String),
}

enum AgentExpectedAction {
    File(String, Option<String>),
    GitCmd(GitCmd),
    LLMCall(LLMCall),
    TaskSuccess(Option<String>),
    TaskFailure(Option<String>),
}

#[derive(Clone, Serialize)]
pub struct Task {
    pub description: String,
}

impl Task {
    pub fn new(description: &str) -> Self {
        Self {
            description: description.to_owned(),
        }
    }
}

struct ServerContext {
    pub task: Task,
}

// ==============================

impl AgentEnvironment {
    pub fn new(agent_image: &str) -> Self {
        Self {
            agent_image: agent_image.to_owned(),
            serial_actions: Vec::new(),
            parallel_actions: Vec::new(),
            task: Task::new(""),

            serial_action_index: 0,
            logged_requests: Vec::new(),
        }
    }

    pub fn with_task(mut self, task: &Task) -> Self {
        self.task = task.clone();
        self
    }

    /// Expect a llm prompt and send back the given `response`.
    /// If `exact_prompt` is not `None`, the received llm prompt has to correspond to it.
    pub fn expect_llm_call(mut self, response: &LLMResponse, exact_prompt: Option<&str>) -> Self {
        self.serial_actions.push(AgentExpectedAction::LLMCall(LLMCall::new(response.clone(), exact_prompt)));
        self
    }

    /// Expect a termination request of the agent indicating success
    pub fn expect_success(mut self, success_report: Option<&str>) -> Self {
        self.serial_actions.push(AgentExpectedAction::TaskSuccess(success_report.map(|s| s.to_owned())));
        self
    }

    /// Expect a termination request of the agent indicating failure with a corresponding failure report.
    /// If `failure_report` is not `None`, the received failure report has to correspond to it.
    pub fn expect_failure(mut self, failure_report: Option<&str>) -> Self {
        self.serial_actions.push(AgentExpectedAction::TaskFailure(failure_report.map(|s| s.to_owned())));
        self
    }

    /// Test for the existence of a file with file name `file_name`.
    /// If the file is under a specific path, this path has to be included in the file name.
    /// An optional file content can be specified with `content` (is only tested if `content` is not `None`).
    /// The check is done at no specific time.
    pub fn expect_file(mut self, file_name: &str, content: Option<&str>) -> Self {
        self.parallel_actions.push(AgentExpectedAction::File(file_name.to_owned(), content.map(|s| s.to_owned())));
        self
    }

    fn next_serial_action(&mut self) -> Option<&AgentExpectedAction> {
        let action = self.serial_actions.get(self.serial_action_index);
        self.serial_action_index += 1;
        action
    }
    fn peek_next_serial_action(&mut self) -> Option<&AgentExpectedAction> {
        self.serial_actions.get(self.serial_action_index)
    }
    fn following_serial_actions(&mut self) -> &[AgentExpectedAction] {
        if self.serial_action_index + 1 >= self.serial_actions.len() { return &[]; }
        &self.serial_actions[self.serial_action_index + 1..]
    }

    /// Checks if the given API request is valid.
    /// The request is valid if it is either next in the queue of serial actions or not at all in the queue.
    /// Logs the request.
    fn check_api_request(&mut self, request: HttpRequest) -> bool {
        self.log_request(request.clone());

        if let Some(action) = self.peek_next_serial_action() {
            if request.request_target == Self::action_to_endpoint(action) {
                self.next_serial_action();
                return true
            }
        }
        self.following_serial_actions().iter().find(|&action| request.request_target == Self::action_to_endpoint(action)).is_none()
    }

    /// Returns the API endpoint of a given action
    fn action_to_endpoint(action: &AgentExpectedAction) -> String {
        match action {
            AgentExpectedAction::LLMCall(_) => APIEndpoint::LLMRequest.into(),
            AgentExpectedAction::TaskSuccess(_) => APIEndpoint::TaskSuccess.into(),
            AgentExpectedAction::TaskFailure(_) => APIEndpoint::TaskFailure.into(),
            _ => "no endpoint",
        }.to_owned()
    }

    /// Logs the given HTTP request
    fn log_request(&mut self, request: HttpRequest) {
        self.logged_requests.push(request);
    }

    pub fn run(&self) {
        tokio::runtime::Runtime::new().unwrap().block_on(async { self.main_loop().await.unwrap() });
    }

    async fn main_loop(&self) -> Result<()> {
        let docker = get_docker()?;

        // setup LLM and Task API endpoints
        let server_process = self.setup_api_endpoints()?;

        // setup HTTP proxy
        // TODO

        // start agent container
        let container_id = start_agent_container(&docker, &self.agent_image).await?;

        let logging_process = attach_outstreams(&docker, &container_id).await?;

        // ... running ...

        // end agent container
        join_agent_container(&docker, &container_id).await?;

        // join processes
        server_process.await?;
        logging_process.await?;

        Ok(())
    }

    fn setup_api_endpoints(&self) -> Result<tokio::task::JoinHandle<()>> {
        let addr = SocketAddr::from(([0,0,0,0], 3000));
        let server =
            Server::new(addr, ServerContext{
                task: self.task.clone(),
            })
                // Task API
                .with_handle(APIEndpoint::TaskSuccess.into(), |request, context| {
                    println!("Task succeeded: {}", request.body);
                    HttpResponse::ok().terminate()
                })
                .with_handle(APIEndpoint::TaskFailure.into(), |request, context| {
                    println!("Task failed: {}", request.body);
                    HttpResponse::ok().terminate()
                })
                .with_handle(APIEndpoint::TaskInfo.into(), |request, context| {
                    println!("Task requested");
                    HttpResponse::ok().json(&context.task.clone())
                })
                // Rest of endpoints
                .with_else_handle(|request| {
                    println!("Request on unknown endpoint: {}", request.request_target);
                    HttpResponse::not_found()
                })
            ;

        Ok(tokio::spawn(async move {
            server.run().unwrap_or_else(|err| println!("The server encountered a critical error: {}", err));
        }))
    }
}

// ==============================

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn get_docker() -> Result<Docker> {
    Ok(Docker::connect_with_local_defaults()?)
}

fn main() {
    let test_env = AgentEnvironment::new("agent-42");
    test_env.run();
}

/// starts the agent container from the given image
/// returns the id of the started container
async fn start_agent_container(docker: &Docker, image_name: &str) -> Result<String> {
    let env_vars = vec![
        "MINION_API_BASE_URL=http://host.docker.internal:3000/api/",
        "MINION_API_TOKEN=42",
    ];
    let host_config = HostConfig {
        extra_hosts: Some(vec!["host.docker.internal:host-gateway".to_owned()]),
        ..Default::default()
    };

    let container_config = Config {
        image: Some(image_name),
        host_config: Some(host_config),
        env: Some(env_vars),
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

async fn attach_outstreams(docker: &Docker, container_id: &str) -> Result<tokio::task::JoinHandle<()>> {
    let attach_options = AttachContainerOptions::<&str> {
        stdout: Some(true),
        stderr: Some(true),
        stdin: None,
        stream: Some(true),
        logs: Some(true),
        ..Default::default()
    };

    let mut stream = docker.attach_container(&container_id, Some(attach_options)).await?;

    Ok(tokio::spawn(async move {
        while let Some(log) = stream.output.next().await {
            match log {
                Ok(LogOutput::StdOut { message }) => {
                    let msg = String::from_utf8_lossy(&message).to_string();
                    println!("STDOUT: {}", msg.strip_suffix("\n").unwrap_or(&msg));
                }
                Ok(LogOutput::StdErr { message }) => {
                    let err = String::from_utf8_lossy(&message).to_string();
                    eprintln!("STDERR: {}", err.strip_suffix("\n").unwrap_or(&err));
                }
                Ok(_) => {}
                Err(e) => eprintln!("Error: {}", e),
            }
        }
    }))
}

async fn join_agent_container(docker: &Docker, container_id: &str) -> Result<()> {
    let mut wait_stream = docker.wait_container(container_id, None::<WaitContainerOptions<String>>);

    if let Some(Ok(log)) = wait_stream.next().await {
        if log.status_code > 0 {
            return Err(Error::AgentCrashed(format!(
                "Container exited with status code {}",
                log.status_code
            )).into());
        }
    }

    Ok(())
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
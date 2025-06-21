#![allow(dead_code)]

mod http_server;

use std::fmt::{Display, Formatter};
use std::net::SocketAddr;
use bollard::Docker;
use bollard::container::{AttachContainerOptions, Config, DownloadFromContainerOptions, LogOutput, WaitContainerOptions};
use bollard::query_parameters::{StartContainerOptions};
use bollard::models::HostConfig;
use bollard::query_parameters::CreateContainerOptions;
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
            APIEndpoint::TaskInfo =>    "/api/agent/task",
            APIEndpoint::LLMRequest =>  "/api/chat/completions",
        }
    }
}

// ==============================

#[derive(Clone)]
enum TestLog {
    HttpRequest(HttpRequest),
    CallToFailure,
    CallToSuccess,
    CallToLLM(String),
    CouldNotStopAgent,
}

#[derive(Clone)]
enum TestFailure {
    CallToFailure,
    CallToSuccess,
    CallToLLM(String),
    AgentCrashed(String),
    Multiple(Box<TestFailure>, Box<TestFailure>),
}

impl Display for TestFailure {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            TestFailure::CallToFailure => write!(f, "Unexpected call to the failure endpoint"),
            TestFailure::CallToSuccess => write!(f, "Unexpected call to the success endpoint"),
            TestFailure::CallToLLM(prompt) =>  write!(f, "Unexpected LLM call with prompt: {}", prompt),
            TestFailure::AgentCrashed(err) => write!(f, "The agent crashed with error: {}", err),
            TestFailure::Multiple(a, b) => write!(f, "Multiple failure reasons:\n    {}\n    {}", *a, *b),
        }
    }
}

#[derive(Clone)]
struct AgentTestRun {
    /// All registered actions of the agent
    pub log: Vec<TestLog>,

    /// The Reason of Failure, if the Test failed
    pub failure: Option<TestFailure>,
}

#[derive(Clone)]
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
}

#[derive(Clone)]
struct GitCmd {}

#[derive(Clone)]
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

#[derive(Clone)]
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
    pub environment: AgentEnvironment,
    pub test_data: AgentTestRun,
}

// ==============================

impl Default for AgentTestRun {
    fn default() -> Self {
        Self {
            log: Vec::new(),
            failure: None,
        }
    }
}

impl AgentEnvironment {
    pub fn new(agent_image: &str) -> Self {
        Self {
            agent_image: agent_image.to_owned(),
            serial_actions: Vec::new(),
            parallel_actions: Vec::new(),
            task: Task::new(""),

            serial_action_index: 0,
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

    /// Start the docker container
    pub fn run(mut self) -> Result<AgentEnvironmentRunner> {
        AgentEnvironmentRunner::new(self)
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
    fn check_api_request(&mut self, request: HttpRequest) -> bool {
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
}

struct AgentEnvironmentRunner {
    environment: AgentEnvironment,
    test_data: AgentTestRun,
    tokio_runtime: tokio::runtime::Runtime,

    server_process: tokio::task::JoinHandle<http_server::Result<ServerContext>>,
    container_logging_process: tokio::task::JoinHandle<()>,
    docker: Docker,
    container_id: String,
}

impl AgentEnvironmentRunner {
    pub fn new(agent_environment: AgentEnvironment) -> Result<Self> {
        let tokio_runtime = tokio::runtime::Runtime::new().unwrap();

        let server_process = tokio_runtime.block_on(async { Self::setup_api_endpoints(agent_environment.clone()) })?;
        let (docker, container_id, logging_process) = tokio_runtime.block_on(async { Self::setup_docker(&agent_environment.agent_image).await })?;

        let runner = Self {
            test_data: Default::default(),
            environment: agent_environment,
            tokio_runtime,
            server_process,
            container_logging_process: logging_process,
            docker, container_id,
        };
        Ok(runner)
    }

    pub fn join(mut self) -> Result<AgentFinishedRunner> {
        let runtime = tokio::runtime::Runtime::new().unwrap();

        let (server_test_data, own_test_data) = runtime.block_on(async {
            let agent_stop_error = self.join_agent_container().await;
            if let Some(err) = agent_stop_error {
                self.test_data.failure = Some(err.clone());
                self.test_data.log.push(TestLog::CouldNotStopAgent)
            }

            let server_context = self.server_process.await??;
            self.container_logging_process.await?;

            self.tokio_runtime.shutdown_background();

            Ok::<(AgentTestRun, AgentTestRun), Box<dyn std::error::Error>>((server_context.test_data, self.test_data))
        })?;
        Ok(AgentFinishedRunner {
            test_data: Self::merge_test_data(server_test_data, own_test_data),
        })
    }

    fn merge_test_data(a: AgentTestRun, mut b: AgentTestRun) -> AgentTestRun {
        let mut log = a.log;
        log.append(&mut b.log);
        AgentTestRun {
            log,
            failure: Self::merge_test_failure(a.failure, b.failure),
        }
    }

    fn merge_test_failure(a: Option<TestFailure>, b: Option<TestFailure>) -> Option<TestFailure> {
        match (a, b) {
            (None, None) => None,
            (Some(f), None) => Some(f),
            (None, Some(f)) => Some(f),
            (Some(a), Some(b)) => Some(TestFailure::Multiple(Box::new(a), Box::new(b)))
        }
    }

    async fn setup_docker(agent_image: &str) -> Result<(Docker, String, tokio::task::JoinHandle<()>)> {
        let docker = get_docker()?;
        let container_id = start_agent_container(&docker, agent_image).await?;
        let logging_process = attach_outstreams(&docker, &container_id).await?;

        Ok((docker, container_id, logging_process))
    }

    fn setup_api_endpoints(agent_environment: AgentEnvironment) -> Result<tokio::task::JoinHandle<http_server::Result<ServerContext>>> {
        let addr = SocketAddr::from(([0,0,0,0], 3000));
        let mut server =
            Server::new(addr, ServerContext {
                environment: agent_environment,
                test_data: Default::default(),
            })
                // Task API
                .with_handle(APIEndpoint::TaskSuccess.into(), |request, context| {
                    println!("Task succeeded: {}", request.body);
                    context.environment.check_api_request(request);
                    HttpResponse::ok().terminate()
                })?
                .with_handle(APIEndpoint::TaskFailure.into(), |request, context| {
                    println!("Task failed: {}", request.body);
                    HttpResponse::ok().terminate()
                })?
                .with_handle(APIEndpoint::TaskInfo.into(), |request, context| {
                    println!("Task requested");
                    HttpResponse::ok().json(&context.environment.task.clone())
                })?
                .with_handle(APIEndpoint::LLMRequest.into(), |request, context| {
                    HttpResponse::ok().json(&serde_json::json!({
                        "choices": [
                            {
                                "finish_reason": "done",
                                "index": 0,
                                "message": {
                                    "role": "assistant",
                                    "content": "hello world",
                                },
                            },
                        ],
                        "usage": {
                            "completion_tokens": 0,
                            "prompt_tokens": 0,
                            "total_tokens": 0,
                        }
                    }))
                })?

                // Rest of endpoints
                .with_else_handle(|request| {
                    println!("Request on unknown endpoint: {}", request.request_target);
                    HttpResponse::not_found()
                })
            ;

        Ok(
            tokio::spawn(async move {
                server.run()
            })
        )
    }

    /// Wait for the agent container to stop running and return `None` on success.
    /// If something unexpected happens while stopping, it is treated as a test failure and the cause is returned.
    async fn join_agent_container(&self) -> Option<TestFailure> {
        let mut wait_stream = self.docker.wait_container(&self.container_id, None::<WaitContainerOptions<String>>);

        if let Some(Ok(log)) = wait_stream.next().await {
            if log.status_code > 0 {
                return Some(TestFailure::AgentCrashed(format!(
                    "Container exited with status code {} and error: {}",
                    log.status_code,
                    log.error.map(|err| err.message.unwrap_or("No message".to_owned())).unwrap_or("No error".to_owned())
                )));
            }
        }

        None
    }

}

struct AgentFinishedRunner {
    test_data: AgentTestRun,
}

impl AgentFinishedRunner {
    pub fn test_failure(&self) -> Option<TestFailure> {
        self.test_data.failure.clone()
    }
    pub fn test_logs(&self) -> Vec<TestLog> {
        self.test_data.log.clone()
    }
}

// ==============================

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn get_docker() -> Result<Docker> {
    Ok(Docker::connect_with_local_defaults()?)
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
        .create_container(None::<CreateContainerOptions>, container_config)
        .await?;

    let container_id = container.id;

    docker
        .start_container(&container_id, None::<StartContainerOptions>)
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

fn main() {
    let test_env = AgentEnvironment::new("agent-42")
        .with_task(&Task::new("Do nothing"))
        .expect_llm_call(&LLMResponse::Message("Do nothing".to_owned()), None)
        .expect_failure(None)
        ;
    match test_env.run() {
        Ok(mut runner) => {
            match runner.join() {
                Ok(runner) => {
                    println!("Test Result:");
                    match runner.test_failure() {
                        None => println!("Success!"),
                        Some(err) => println!("Failure with reason {}", err),
                    }
                }
                Err(err) => println!("An unrecoverable error occurred while stopping the agent: {}", err),
            }

        }
        Err(err) => {
            println!("Could not setup the agent runner due to an unrecoverable error: {}", err);
        }
    }
}
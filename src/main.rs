#![allow(dead_code)]

mod http_server;

use crate::http_server::{HttpRequest, HttpResponse, Server};
use bollard::Docker;
use bollard::container::{AttachContainerOptions, Config, LogOutput, WaitContainerOptions};
use bollard::models::{ContainerCreateBody, HostConfig};
use bollard::query_parameters::CreateContainerOptions;
use bollard::query_parameters::{
    AttachContainerOptionsBuilder, DownloadFromContainerOptionsBuilder, StartContainerOptions,
};
use futures_util::StreamExt;
use serde::Serialize;
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::io::Read;
use std::net::SocketAddr;
use tar::Archive;

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
            APIEndpoint::LLMRequest => "/api/chat/completions",
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

#[derive(Clone, Debug)]
enum TestFailure {
    CallToFailure,
    CallToSuccess,
    CallToTaskInfo,
    CallToLLM(String),
    AgentCrashed(String),
    ActionsMissing(Vec<AgentExpectedAction>),
    Multiple(Box<TestFailure>, Box<TestFailure>),
}

impl Display for TestFailure {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            TestFailure::CallToFailure => write!(f, "Unexpected call to the failure endpoint"),
            TestFailure::CallToSuccess => write!(f, "Unexpected call to the success endpoint"),
            TestFailure::CallToTaskInfo => write!(f, "Unexpected call to the Task API"),
            TestFailure::CallToLLM(prompt) => {
                write!(f, "Unexpected LLM call with prompt: {}", prompt)
            }
            TestFailure::AgentCrashed(err) => write!(f, "The agent crashed with error: {}", err),
            TestFailure::ActionsMissing(actions) => write!(
                f,
                "There are actions missing, but the agent already stopped: {:?}",
                actions
            ),
            TestFailure::Multiple(a, b) => {
                write!(f, "Multiple failure reasons:\n    {}\n    {}", *a, *b)
            }
        }
    }
}

#[derive(Clone)]
struct AgentTestFinishedRun {
    /// All registered actions of the agent
    pub log: Vec<TestLog>,

    /// The Reason of Failure, if the Test failed
    pub failure: Option<TestFailure>,
}

#[derive(Clone)]
struct AgentTestEnvironment {
    /// Actions which have to be done in the given order.
    pub serial_actions: Vec<AgentExpectedAction>,
    /// Actions which can be done at any time and just have to happen at least once.
    pub parallel_actions: Vec<AgentExpectedAction>,

    /// The task which is provided to the agent.
    pub task: Task,

    /// The current index of the serial actions
    serial_action_index: usize,
}

#[derive(Clone, Debug)]
struct GitCmd {}

#[derive(Clone, Debug)]
struct LLMCall {
    pub prompt: Option<String>,
    pub response: LLMResponse,
}
impl LLMCall {
    fn new(response: LLMResponse, prompt: Option<&str>) -> Self {
        Self {
            response,
            prompt: prompt.map(|s| s.to_owned()),
        }
    }
}

#[derive(Clone, Debug)]
pub enum LLMResponse {
    Message(String),
    ToolCall(String),
}

#[derive(Clone, Debug)]
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
    pub environment: AgentTestEnvironment,
    pub test_data: AgentTestFinishedRun,

    pub terminate_server: bool,
}

impl http_server::ServerContext for ServerContext {
    fn should_terminate_server(&self) -> bool {
        self.terminate_server
    }
}
impl ServerContext {
    pub fn terminate_server(&mut self) {
        self.terminate_server = true
    }
}

// ==============================

impl Default for AgentTestFinishedRun {
    fn default() -> Self {
        Self {
            log: Vec::new(),
            failure: None,
        }
    }
}

impl AgentTestEnvironment {
    /// Construct a new agent test environment with empty task and no requirements for the agent.
    pub fn new() -> Self {
        Self {
            serial_actions: Vec::new(),
            parallel_actions: Vec::new(),
            task: Task::new(""),

            serial_action_index: 0,
        }
    }

    /// Set the task the agent should do
    pub fn with_task(mut self, task: &Task) -> Self {
        self.task = task.clone();
        self
    }

    /// Expect a llm prompt and send back the given `response`.
    /// If `exact_prompt` is not `None`, the received llm prompt has to correspond to it.
    pub fn expect_llm_call(mut self, response: &LLMResponse, exact_prompt: Option<&str>) -> Self {
        self.serial_actions
            .push(AgentExpectedAction::LLMCall(LLMCall::new(
                response.clone(),
                exact_prompt,
            )));
        self
    }

    /// Expect a termination request of the agent indicating success
    pub fn expect_success(mut self, success_report: Option<&str>) -> Self {
        self.serial_actions.push(AgentExpectedAction::TaskSuccess(
            success_report.map(|s| s.to_owned()),
        ));
        self
    }

    /// Expect a termination request of the agent indicating failure with a corresponding failure report.
    /// If `failure_report` is not `None`, the received failure report has to correspond to it.
    pub fn expect_failure(mut self, failure_report: Option<&str>) -> Self {
        self.serial_actions.push(AgentExpectedAction::TaskFailure(
            failure_report.map(|s| s.to_owned()),
        ));
        self
    }

    /// Test for the existence of a file with file name `file_name`.
    /// If the file is under a specific path, this path has to be included in the file name.
    /// An optional file content can be specified with `content` (is only tested if `content` is not `None`).
    /// The check is done at no specific time.
    pub fn expect_file(mut self, file_name: &str, content: Option<&str>) -> Self {
        self.parallel_actions.push(AgentExpectedAction::File(
            file_name.to_owned(),
            content.map(|s| s.to_owned()),
        ));
        self
    }

    /// Start the docker container
    pub fn run(mut self, agent_image: &str) -> Result<AgentTestRunner> {
        AgentTestRunner::new(agent_image, self)
    }

    fn next_serial_action(&mut self) -> Option<&AgentExpectedAction> {
        let action = self.serial_actions.get(self.serial_action_index);
        self.serial_action_index += 1;
        action
    }
    fn peek_next_serial_action(&mut self) -> Option<&AgentExpectedAction> {
        self.serial_actions.get(self.serial_action_index)
    }
    fn following_serial_actions(&self) -> &[AgentExpectedAction] {
        let actions = self.uncompleted_serial_actions();
        if actions.len() > 0 {
            &actions[1..]
        } else {
            &[]
        }
    }
    fn uncompleted_serial_actions(&self) -> &[AgentExpectedAction] {
        if self.serial_action_index >= self.serial_actions.len() {
            return &[];
        }
        &self.serial_actions[self.serial_action_index..]
    }

    /// Checks if the given API request is valid.
    /// The request is valid if it is either next in the queue of serial actions or not at all in the queue.
    fn check_api_request(&mut self, request: HttpRequest, failure: &TestFailure) -> bool {
        if let Some(action) = self.peek_next_serial_action() {
            if Self::action_matches_failure_mode(action, failure) {
                self.next_serial_action();
                return true;
            }
        }
        self.following_serial_actions()
            .iter()
            .find(|&action| request.request_target == Self::action_to_endpoint(action))
            .is_none()
    }

    fn action_matches_failure_mode(action: &AgentExpectedAction, failure: &TestFailure) -> bool {
        match action {
            AgentExpectedAction::LLMCall(_) => matches!(failure, TestFailure::CallToLLM(_)),
            AgentExpectedAction::TaskSuccess(_) => matches!(failure, TestFailure::CallToSuccess),
            AgentExpectedAction::TaskFailure(_) => matches!(failure, TestFailure::CallToFailure),

            _ => true,
        }
    }

    fn check_api_request_failure_mode(
        &mut self,
        request: HttpRequest,
        failure: TestFailure,
    ) -> Option<TestFailure> {
        if self.check_api_request(request, &failure) {
            None
        } else {
            Some(failure)
        }
    }

    /// Returns the API endpoint of a given action
    fn action_to_endpoint(action: &AgentExpectedAction) -> String {
        match action {
            AgentExpectedAction::LLMCall(_) => APIEndpoint::LLMRequest.into(),
            AgentExpectedAction::TaskSuccess(_) => APIEndpoint::TaskSuccess.into(),
            AgentExpectedAction::TaskFailure(_) => APIEndpoint::TaskFailure.into(),
            _ => "no endpoint",
        }
        .to_owned()
    }
}

struct AgentTestRunner {
    environment: AgentTestEnvironment,
    test_data: AgentTestFinishedRun,
    tokio_runtime: tokio::runtime::Runtime,

    server_process: tokio::task::JoinHandle<http_server::Result<ServerContext>>,
    container_logging_process: tokio::task::JoinHandle<()>,
    docker: Docker,
    container_id: String,
}

impl AgentTestRunner {
    /// Construct a runner from an `AgentEnvironment`.
    /// This starts the docker container with the webserver for the API hooks
    pub fn new(agent_image: &str, agent_environment: AgentTestEnvironment) -> Result<Self> {
        let tokio_runtime = tokio::runtime::Runtime::new().unwrap();

        let server_process = tokio_runtime
            .block_on(async { Self::setup_api_endpoints(agent_environment.clone()) })?;
        let (docker, container_id, logging_process) =
            tokio_runtime.block_on(async { Self::setup_docker(agent_image).await })?;

        let runner = Self {
            test_data: Default::default(),
            environment: agent_environment,
            tokio_runtime,
            server_process,
            container_logging_process: logging_process,
            docker,
            container_id,
        };
        Ok(runner)
    }

    /// Block while waiting for the completion of the agent container and all processes around it, like the webserver.
    /// Returns the results of the test run.
    pub fn join(mut self) -> Result<AgentTestFinishedRunner> {
        let runtime = tokio::runtime::Runtime::new().unwrap();

        let (server_test_data, own_test_data) = runtime.block_on(async {
            // check for missing parallel actions
            let parallel_actions = self.environment.parallel_actions.clone();
            let uncompleted_parallel_actions =
                self.uncompleted_parallel_actions(parallel_actions).await?;
            if uncompleted_parallel_actions.len() > 0 {
                self.test_data.failure = Self::merge_test_failure(
                    self.test_data.failure,
                    Some(TestFailure::ActionsMissing(
                        uncompleted_parallel_actions.to_owned(),
                    )),
                )
            }

            // cleanup
            let agent_stop_error = self.join_agent_container().await;
            if let Some(err) = agent_stop_error {
                self.test_data.failure =
                    Self::merge_test_failure(self.test_data.failure, Some(err.clone()));
                self.test_data.log.push(TestLog::CouldNotStopAgent)
            }

            let server_context = self.server_process.await??;
            self.container_logging_process.await?;

            self.tokio_runtime.shutdown_background();

            // check for missing serial actions
            let uncompleted_serial_actions =
                server_context.environment.uncompleted_serial_actions();
            if uncompleted_serial_actions.len() > 0 {
                self.test_data.failure = Self::merge_test_failure(
                    self.test_data.failure,
                    Some(TestFailure::ActionsMissing(
                        uncompleted_serial_actions.to_owned(),
                    )),
                )
            }

            Ok::<(AgentTestFinishedRun, AgentTestFinishedRun), Box<dyn std::error::Error>>((
                server_context.test_data,
                self.test_data,
            ))
        })?;
        Ok(AgentTestFinishedRunner {
            test_data: Self::merge_test_data(server_test_data, own_test_data),
        })
    }

    /// Checks which of the given parallel actions were done and deletes them from the queue if yes.
    /// Returns the remaining uncompleted actions
    async fn uncompleted_parallel_actions(
        &self,
        actions: Vec<AgentExpectedAction>,
    ) -> Result<Vec<AgentExpectedAction>> {
        let mut action_filter = vec![];
        for action in actions.clone().into_iter() {
            action_filter.push(self.check_parallel_action(&action).await);
        }

        if let Some((i, _)) = action_filter.iter().enumerate().find(|(_, v)| v.is_err()) {
            Err(action_filter.into_iter().nth(i).unwrap().unwrap_err())
        } else {
            Ok(action_filter
                .into_iter()
                .zip(actions)
                .filter(|(completed, _)| !completed.as_ref().unwrap())
                .map(|(_, action)| action)
                .collect())
        }
    }

    /// Checks whether the given action is completed
    /// Returns `true` if it is and `false` if not
    async fn check_parallel_action(&self, action: &AgentExpectedAction) -> Result<bool> {
        match action {
            AgentExpectedAction::File(name, maybe_content) => {
                Ok(get_files(&self.docker, &self.container_id)
                    .await?
                    .into_iter()
                    .any(|(n, c)| {
                        name == &n && maybe_content.clone().map(|v| v == c).unwrap_or(true)
                    }))
            }

            // This action cannot be checked in parallel
            _ => Ok(true),
        }
    }

    /// Merge two instances of the data accumulated over an agent test run
    fn merge_test_data(
        a: AgentTestFinishedRun,
        mut b: AgentTestFinishedRun,
    ) -> AgentTestFinishedRun {
        let mut log = a.log;
        log.append(&mut b.log);
        AgentTestFinishedRun {
            log,
            failure: Self::merge_test_failure(a.failure, b.failure),
        }
    }

    /// Merge two instances of test results from an agent test run
    fn merge_test_failure(a: Option<TestFailure>, b: Option<TestFailure>) -> Option<TestFailure> {
        // just for fun as oneliner: vec![a, b].into_iter().filter(|t| t.is_some()).reduce(|l, r| Some(TestFailure::Multiple(Box::new(l.unwrap()), Box::new(r.unwrap())))).unwrap_or(None)
        match (a, b) {
            (None, None) => None,
            (Some(f), None) => Some(f),
            (None, Some(f)) => Some(f),
            (Some(a), Some(b)) => Some(TestFailure::Multiple(Box::new(a), Box::new(b))),
        }
    }

    /// Setup docker and start the agent container.
    /// Return the docker instance, the container id and the logging process.
    /// This has to be called from a tokio runtime as a new tokio task is spawned.
    async fn setup_docker(
        agent_image: &str,
    ) -> Result<(Docker, String, tokio::task::JoinHandle<()>)> {
        let docker = get_docker()?;
        let container_id = start_agent_container(&docker, agent_image).await?;
        let logging_process = attach_outstreams(&docker, &container_id).await?;

        Ok((docker, container_id, logging_process))
    }

    /// Sets up the endpoints the agent can request, e.g. for the Task API or the LLM requests.
    /// Returns a handle to the server process, which, on completion, returns the server context.
    /// This has to be called from a tokio runtime as a new tokio task is spawned.
    fn setup_api_endpoints(
        agent_environment: AgentTestEnvironment,
    ) -> Result<tokio::task::JoinHandle<http_server::Result<ServerContext>>> {
        let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
        let mut server = Server::new(
            addr,
            ServerContext {
                environment: agent_environment,
                test_data: Default::default(),
                terminate_server: false,
            },
        )
        // Task API
        // SUCCESS
        .with_endpoint(APIEndpoint::TaskSuccess.into(), |request, context| {
            println!("Task succeeded: {}", request.body);
            context.test_data.failure = context
                .environment
                .check_api_request_failure_mode(request, TestFailure::CallToSuccess);
            context.terminate_server();
            HttpResponse::ok()
        })?
        // FAILURE
        .with_endpoint(APIEndpoint::TaskFailure.into(), |request, context| {
            println!("Task failed: {}", request.body);
            context.test_data.failure = context
                .environment
                .check_api_request_failure_mode(request, TestFailure::CallToFailure);
            context.terminate_server();
            HttpResponse::ok()
        })?
        // TASK INFO
        .with_endpoint(APIEndpoint::TaskInfo.into(), |request, context| {
            println!("Task requested");
            context.test_data.failure = context
                .environment
                .check_api_request_failure_mode(request, TestFailure::CallToTaskInfo);
            if context.test_data.failure.is_some() {
                context.terminate_server()
            }

            HttpResponse::ok().json(&context.environment.task.clone())
        })?
        // LLM
        .with_endpoint(APIEndpoint::LLMRequest.into(), |request, context| {
            let llm_prompt = "".to_owned();
            context.test_data.failure = context.environment.check_api_request_failure_mode(
                request,
                TestFailure::CallToLLM(llm_prompt.clone()),
            );
            if context.test_data.failure.is_some() {
                context.terminate_server()
            }

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
        });

        Ok(tokio::spawn(async move { server.run() }))
    }

    /// Wait for the agent container to stop running and return `None` on success.
    /// If something unexpected happens while stopping, it is treated as a test failure and the cause is returned.
    async fn join_agent_container(&self) -> Option<TestFailure> {
        let mut wait_stream = self
            .docker
            .wait_container(&self.container_id, None::<WaitContainerOptions<String>>);

        if let Some(Ok(log)) = wait_stream.next().await {
            if log.status_code > 0 {
                return Some(TestFailure::AgentCrashed(format!(
                    "Container exited with status code {} and error: {}",
                    log.status_code,
                    log.error
                        .map(|err| err.message.unwrap_or("No message".to_owned()))
                        .unwrap_or("No error".to_owned())
                )));
            }
        }

        None
    }
}

struct AgentTestFinishedRunner {
    test_data: AgentTestFinishedRun,
}

impl AgentTestFinishedRunner {
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
        "MINION_API_BASE_URL=http://host.docker.internal:3000/api/".to_owned(),
        "MINION_API_TOKEN=42".to_owned(),
    ];
    let host_config = HostConfig {
        extra_hosts: Some(vec!["host.docker.internal:host-gateway".to_owned()]),
        ..Default::default()
    };

    let container_config = ContainerCreateBody {
        image: Some(image_name.to_owned()),
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

async fn attach_outstreams(
    docker: &Docker,
    container_id: &str,
) -> Result<tokio::task::JoinHandle<()>> {
    let attach_options = AttachContainerOptionsBuilder::new()
        .stdout(true)
        .stderr(true)
        .stream(true)
        .logs(true)
        .build();

    let mut stream = docker
        .attach_container(&container_id, Some(attach_options))
        .await?;

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

/// Extracts all files from the container under `container_id`.
/// Returns a map from the filenames to the file content.
/// Filenames are given with path, so e.g. `dev/my_project/main.rs`
async fn get_files(docker: &Docker, container_id: &str) -> Result<HashMap<String, String>> {
    let options = DownloadFromContainerOptionsBuilder::new()
        .path("~/app")
        .build();

    let mut tar_stream = docker.download_from_container(container_id, Some(options));

    let mut archive_data = Vec::new();
    while let Some(Ok(chunk)) = tar_stream.next().await {
        archive_data.extend(chunk);
    }

    let mut files = HashMap::new();

    let mut archive = Archive::new(&archive_data[..]);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_str().unwrap().to_owned();
        let mut file_contents = String::new();
        entry.read_to_string(&mut file_contents)?;

        println!("found file: {:?}", path);
        println!("file content: {:?}", file_contents);

        files.insert(path, file_contents);
    }

    Ok(files)
}

fn main() {
    let test_env = AgentTestEnvironment::new()
        .with_task(&Task::new("Do nothing"))
        // .expect_llm_call(&LLMResponse::Message("Do nothing".to_owned()), None)
        // .expect_llm_call(&LLMResponse::Message("Do nothing".to_owned()), None)
        .expect_file("test.txt", None)
        .expect_failure(None);
    match test_env.run("agent-42") {
        Ok(mut runner) => match runner.join() {
            Ok(runner) => {
                println!("Test Result:");
                match runner.test_failure() {
                    None => println!("Success!"),
                    Some(err) => println!("Failure with reason: {}", err),
                }
            }
            Err(err) => println!(
                "An unrecoverable error occurred while stopping the agent: {}",
                err
            ),
        },
        Err(err) => {
            println!(
                "Could not setup the agent runner due to an unrecoverable error: {}",
                err
            );
        }
    }
}

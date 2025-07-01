use serde::Serialize;
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::str::Lines;

#[derive(Debug)]
pub(crate) enum Error {
    ConnectionFailed(String),
    IO(String),
    InvalidRequest(String),
    InvalidRequestEndpoint(String),
    InvalidEndpoint(String),
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP Server Error: {self:?}")
    }
}

impl std::error::Error for Error {}

fn format_relative_url(url: &str) -> Option<String> {
    if !url.ends_with("/") {
        Some(format!("{url}/"))
    } else {
        Some(url.to_string())
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::IO(format!("{err}"))
    }
}

pub(crate) type Result<T> = std::result::Result<T, Error>;

// ==============================

#[derive(Clone)]
pub(crate) enum RequestType {
    Get,
    Post,
}

// ==============================

pub(crate) trait ServerContext {
    fn should_terminate_server(&self) -> bool;
}

pub(crate) struct Server<T: ServerContext> {
    addr: SocketAddr,

    // handles: HashMap<String, Box<dyn Fn(HttpRequest) -> HttpResponse + Send>>,
    handles: HashMap<String, fn(HttpRequest, &mut T) -> HttpResponse>,
    else_handle: fn(HttpRequest) -> HttpResponse,
    pub(crate) context: T,
}

#[derive(Clone)]
pub(crate) struct HttpRequest {
    /// Describes the meaning and desired outcome of the request, e.g. GET if the client wants a resource in return
    pub(crate) method: RequestType,

    /// An absolute or relative URL describing the requested target (endpoint).
    /// Most of the time, this is in relative form (also called origin form), e.g. /api/success
    pub(crate) request_target: String,

    /// The HTTP version, most of the time this is `HTTP/1.1` (older ones are discontinued, the newer aren't in this format)
    pub(crate) protocol: String,

    /// The headers of the request containing metadata
    pub(crate) headers: HashMap<String, String>,

    /// The actual content of the request
    pub(crate) body: String,
}
#[derive(Clone)]
pub(crate) struct HttpResponse {
    pub(crate) headers: HashMap<String, String>,
    pub(crate) status_code: StatusCode,
    pub(crate) body: String,
}

impl HttpResponse {
    fn new() -> Self {
        Self {
            headers: HashMap::new(),
            status_code: 0,
            body: String::new(),
        }
    }
    pub(crate) fn not_found() -> Self {
        let mut s = Self::new();
        s.status_code = 404;
        s
    }
    pub(crate) fn ok() -> Self {
        let mut s = Self::new();
        s.status_code = 200;
        s
    }

    pub(crate) fn text(mut self, body: &str) -> Self {
        self.body = body.to_owned();
        self
    }
    pub(crate) fn json<T: Serialize>(mut self, content: &T) -> Self {
        self.body = serde_json::to_string(content).unwrap(); // this should not panic
        self.headers
            .insert("Content-Type".to_owned(), "application/json".to_owned());
        self
    }
}

pub(crate) type StatusCode = u32;
fn format_status_code_message(status_code: StatusCode) -> String {
    match status_code {
        200 => "200 OK",
        404 => "404 Not Found",
        _ => "42 Misc",
    }
    .to_owned()
}

/// Parses the HTTP request start line from the given lines iterator.
/// Returns the remaining lines, HTTP method, request target, and protocol version.
/// Errors if the start line is malformed or contains unsupported HTTP methods.
fn parse_start_line(mut lines: Lines) -> Result<(Lines, RequestType, String, String)> {
    use Error::InvalidRequest as IR;

    let start_line = lines.next().ok_or(IR("Empty request".to_owned()))?;
    let mut start_line_parts = start_line.split_whitespace();

    let method = match start_line_parts
        .next()
        .ok_or(IR("Missing HTTP method".to_owned()))?
    {
        "GET" => RequestType::Get,
        "POST" => RequestType::Post,
        m => return Err(IR(format!("Unsupported HTTP method: {m}"))),
    };

    let request_target = start_line_parts
        .next()
        .ok_or(IR("Missing request target".to_owned()))?
        .to_string();

    let protocol = start_line_parts
        .next()
        .ok_or(IR("Missing HTTP protocol".to_owned()))?
        .to_string();

    Ok((lines, method, request_target, protocol))
}

/// Parses HTTP headers from the given lines iterator until an empty line is encountered. Skips malformed headers without exactly one ':'
/// Returns the remaining lines and a hash map of header key-value pairs.
fn parse_http_headers(mut lines: Lines) -> (Lines, HashMap<String, String>) {
    let mut headers = HashMap::new();

    for line in &mut lines {
        if line.is_empty() {
            break;
        }

        let parts: Vec<&str> = line.split(':').collect();
        // header is weird
        if parts.len() != 2 {
            continue;
        }

        let key = parts[0].trim().to_string();
        let value = parts[1].trim().to_string();
        headers.insert(key, value);
    }

    (lines, headers)
}

/// Parses a raw HTTP request string into an `HttpRequest` struct.
fn parse_http_request(content: &str) -> Result<HttpRequest> {
    let lines = content.lines();

    let (lines, method, request_target, protocol) = parse_start_line(lines)?;

    let (lines, headers) = parse_http_headers(lines);

    let body = lines.collect::<Vec<_>>().join("\n");

    Ok(HttpRequest {
        method,
        request_target,
        protocol,
        headers,
        body,
    })
}

fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest> {
    let mut buf = [0; 100000];
    #[allow(clippy::unused_io_amount)]
    stream.read(&mut buf)?;
    let content = String::from_utf8_lossy(&buf).to_string();
    parse_http_request(&content)
}

/// Formats the HTTP response as a string ready for transmission.
fn format_http_response(response: HttpResponse) -> Result<String> {
    let mut formatted_response = String::new();

    // hardcoding 200 OK as status for now
    formatted_response.push_str(&format!(
        "HTTP/1.1 {}\r\n",
        format_status_code_message(response.status_code)
    ));

    // headers
    for (key, value) in &response.headers {
        formatted_response.push_str(&format!("{key}: {value}\r\n"));
    }

    if !response.headers.contains_key("Content-Length") {
        formatted_response.push_str(&format!("Content-Length: {}\r\n", response.body.len()));
    }

    // body
    formatted_response.push_str("\r\n");
    formatted_response.push_str(&response.body);

    Ok(formatted_response)
}

fn write_http_response(stream: &mut TcpStream, response: HttpResponse) -> Result<()> {
    stream.write_all(format_http_response(response)?.as_bytes())?;
    Ok(())
}

impl<T: ServerContext> Server<T> {
    pub(crate) fn new(addr: SocketAddr, context: T) -> Self {
        Self {
            addr,
            handles: HashMap::new(),
            else_handle: |_| HttpResponse::not_found(),
            context,
        }
    }

    /// Set handle for a specific endpoint.
    pub(crate) fn with_endpoint(
        mut self,
        target: &str,
        callback: fn(HttpRequest, &mut T) -> HttpResponse,
    ) -> Result<Self> {
        // self.handles.insert(endpoint.to_owned(), Box::new(handle));
        self.handles.insert(
            format_relative_url(target).ok_or(Error::InvalidEndpoint(target.to_owned()))?,
            callback,
        );
        Ok(self)
    }

    /// Set handle which is called when the endpoint is not recognized.
    pub(crate) fn with_else_handle(mut self, handle: fn(HttpRequest) -> HttpResponse) -> Self {
        self.else_handle = handle;
        self
    }

    /// Starts the server, consuming it. It runs until a response is sent containing the termination flag.
    /// The context is returned on shutdown.
    pub(crate) fn run(mut self) -> Result<T> {
        let listener = TcpListener::bind(self.addr)?;

        for stream in listener.incoming() {
            match stream {
                Ok(mut stream) => {
                    let mut request = read_http_request(&mut stream)?;
                    request.request_target = format_relative_url(&request.request_target).ok_or(
                        Error::InvalidRequestEndpoint(request.request_target.clone()),
                    )?;

                    let response = self
                        .handles
                        .get(&request.request_target)
                        .map(|handle| handle(request.clone(), &mut self.context))
                        .unwrap_or_else(|| (self.else_handle)(request));

                    write_http_response(&mut stream, response)?;

                    if self.context.should_terminate_server() {
                        break;
                    }
                }
                Err(err) => return Err(Error::ConnectionFailed(format!("{err}"))),
            }
        }

        Ok(self.context)
    }
}

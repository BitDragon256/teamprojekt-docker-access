use std::fmt::{Display, Formatter};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, SocketAddr};
use tokio::task;

#[derive(Debug)]
pub enum Error {
    ConnectionFailed(String),
    IOError(String),
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::IOError(format!("{}", err))
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP Error: {:?}", self)
    }
}

impl std::error::Error for Error {}

type Result<T> = std::result::Result<T, Error>;

pub struct Server {
    addr: SocketAddr,
}

pub struct HttpRequest {
    pub content: String,
}
pub struct HttpResponse {
    pub content: String,
}

fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest> {
    // TODO timeout
    // TODO buffer size
    let mut buf = [0;1000]; stream.read(&mut buf);
    let content = String::from_utf8_lossy(&buf).to_string();
    Ok(HttpRequest {
        content,
    })
}

fn write_http_response(stream: &mut TcpStream, response: HttpResponse) -> Result<()> {
    // TODO timeout
    stream.write_all(response.content.as_bytes())?;
    Ok(())
}

impl Server {
    pub(crate) fn new(addr: SocketAddr) -> Self {
        Self {
            addr
        }
    }

    pub fn run<F>(self, handler: F) -> Result<()>
    where
        F: Fn(HttpRequest) -> HttpResponse
    {
        let listener = TcpListener::bind(self.addr)?;

        for stream in listener.incoming() {
            match stream {
                Ok(mut stream) => {
                    let request = read_http_request(&mut stream)?;

                    let response = handler(request);

                    write_http_response(&mut stream, response)?;
                }
                Err(err) => return Err(Error::ConnectionFailed(format!("{}", err)))
            }
        }

        Ok(())
    }
}
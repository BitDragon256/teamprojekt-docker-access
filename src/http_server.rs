use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, SocketAddr};
use tokio::task;

enum Error {
    ConnectionFailed(String),
    IOError(String),
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::IOError(format!("{}", err))
    }
}

type Result<T> = std::result::Result<T, Error>;

struct Server {
    addr: SocketAddr,
}

struct HttpRequest {
    content: String,
}
struct HttpResponse {
    content: String,
}

async fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest> {
    // TODO timeout
    let mut content = String::new(); stream.read_to_string(&mut content)?;
    Ok(HttpRequest {
        content,
    })
}

async fn write_http_response(stream: &mut TcpStream, response: HttpResponse) -> Result<()> {
    // TODO timeout
    stream.write_all(response.content.as_bytes())?;
    Ok(())
}

impl Server {
    pub async fn run<F>(self, handler: F) -> Result<()>
    where
        F: Fn(HttpRequest) -> HttpResponse
    {
        let listener = TcpListener::bind(self.addr)?;

        for stream in listener.incoming() {
            match stream {
                Ok(mut stream) => {
                    let request = read_http_request(&mut stream).await?;

                    task::spawn(async move {
                        let response = handler(request);

                        write_http_response(&mut stream, response).await?;
                    });
                }
                Err(err) => return Err(Error::ConnectionFailed(err.into()))
            }
        }

        Ok(())
    }
}
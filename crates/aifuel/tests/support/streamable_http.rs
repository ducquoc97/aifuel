use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::Duration;

pub struct StreamableHttpFixture {
    address: SocketAddr,
    requests: Receiver<PendingRequest>,
    stopping: Arc<AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
}

pub struct PendingRequest {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
    response: Option<SyncSender<HttpResponse>>,
}

#[allow(dead_code)]
pub struct StreamingResponse {
    chunks: SyncSender<Vec<u8>>,
}

#[allow(dead_code)]
impl StreamingResponse {
    pub fn send_chunk(&self, chunk: Vec<u8>) -> Result<(), mpsc::SendError<Vec<u8>>> {
        self.chunks.send(chunk)
    }
}

struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: ResponseBody,
}

enum ResponseBody {
    Fixed(Vec<u8>),
    Chunked(Vec<Vec<u8>>),
    #[allow(dead_code)]
    Streaming(Receiver<Vec<u8>>),
}

impl StreamableHttpFixture {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("fixture should bind loopback");
        listener
            .set_nonblocking(true)
            .expect("fixture listener should be nonblocking");
        let address = listener
            .local_addr()
            .expect("fixture address should be available");
        let stopping = Arc::new(AtomicBool::new(false));
        let server_stopping = Arc::clone(&stopping);
        let (requests_tx, requests_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            while !server_stopping.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let requests_tx = requests_tx.clone();
                        thread::spawn(move || serve_one(stream, requests_tx));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            address,
            requests: requests_rx,
            stopping,
            server: Some(server),
        }
    }

    pub fn url(&self) -> String {
        format!("http://{}/mcp", self.address)
    }

    pub fn next_request(&self, timeout: Duration) -> Result<PendingRequest, RecvTimeoutError> {
        self.requests.recv_timeout(timeout)
    }
}

impl Drop for StreamableHttpFixture {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

impl PendingRequest {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("fixture received valid JSON")
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    pub fn respond_json(self, status: u16, headers: Vec<(String, String)>, body: Value) {
        self.respond(
            status,
            Some("application/json"),
            headers,
            serde_json::to_vec(&body).expect("fixture JSON should serialize"),
        );
    }

    pub fn respond(
        mut self,
        status: u16,
        content_type: Option<&str>,
        mut headers: Vec<(String, String)>,
        body: impl Into<Vec<u8>>,
    ) {
        if let Some(content_type) = content_type {
            headers.push(("Content-Type".to_owned(), content_type.to_owned()));
        }
        let response = HttpResponse {
            status,
            headers,
            body: ResponseBody::Fixed(body.into()),
        };
        self.send_response(response);
    }

    pub fn respond_chunked(
        mut self,
        status: u16,
        content_type: Option<&str>,
        mut headers: Vec<(String, String)>,
        chunks: Vec<Vec<u8>>,
    ) {
        if let Some(content_type) = content_type {
            headers.push(("Content-Type".to_owned(), content_type.to_owned()));
        }
        let response = HttpResponse {
            status,
            headers,
            body: ResponseBody::Chunked(chunks),
        };
        self.send_response(response);
    }

    #[allow(dead_code)]
    pub fn respond_streaming(
        mut self,
        status: u16,
        content_type: Option<&str>,
        mut headers: Vec<(String, String)>,
    ) -> StreamingResponse {
        if let Some(content_type) = content_type {
            headers.push(("Content-Type".to_owned(), content_type.to_owned()));
        }
        let (chunks, receiver) = mpsc::sync_channel(1);
        let response = HttpResponse {
            status,
            headers,
            body: ResponseBody::Streaming(receiver),
        };
        self.send_response(response);
        StreamingResponse { chunks }
    }

    fn send_response(&mut self, response: HttpResponse) {
        self.response
            .take()
            .expect("fixture response is available")
            .send(response)
            .expect("gateway should await fixture response");
    }
}

fn serve_one(stream: TcpStream, requests: mpsc::Sender<PendingRequest>) {
    stream
        .set_nonblocking(false)
        .expect("fixture connection should be blocking");
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut reader = BufReader::new(stream);
    let mut first_line = String::new();
    if reader
        .read_line(&mut first_line)
        .ok()
        .filter(|count| *count > 0)
        .is_none()
    {
        return;
    }
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line.is_empty() {
            return;
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let content_length = headers
        .get("content-length")
        .and_then(|length| length.parse::<usize>().ok())
        .unwrap_or_default();
    let mut body = vec![0; content_length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let mut stream = reader.into_inner();

    let (response_tx, response_rx) = mpsc::sync_channel(1);
    if requests
        .send(PendingRequest {
            method,
            path,
            headers,
            body,
            response: Some(response_tx),
        })
        .is_err()
    {
        return;
    }
    let Ok(response) = response_rx.recv_timeout(Duration::from_secs(50)) else {
        return;
    };
    write_response(&mut stream, response);
}

fn write_response(stream: &mut TcpStream, response: HttpResponse) {
    let reason = match response.status {
        200 => "OK",
        202 => "Accepted",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "Fixture Response",
    };
    let mut headers = response.headers;
    match &response.body {
        ResponseBody::Fixed(body) => {
            headers.push(("Content-Length".to_owned(), body.len().to_string()));
        }
        ResponseBody::Chunked(_) => {
            headers.push(("Transfer-Encoding".to_owned(), "chunked".to_owned()));
        }
        ResponseBody::Streaming(_) => {
            headers.push(("Transfer-Encoding".to_owned(), "chunked".to_owned()));
        }
    }
    headers.push(("Connection".to_owned(), "close".to_owned()));
    let _ = write!(stream, "HTTP/1.1 {} {reason}\r\n", response.status);
    for (name, value) in headers {
        let _ = write!(stream, "{name}: {value}\r\n");
    }
    let _ = write!(stream, "\r\n");
    match response.body {
        ResponseBody::Fixed(body) => {
            let _ = stream.write_all(&body);
        }
        ResponseBody::Chunked(chunks) => {
            for chunk in chunks {
                let _ = write!(stream, "{:X}\r\n", chunk.len());
                let _ = stream.write_all(&chunk);
                let _ = stream.write_all(b"\r\n");
            }
            let _ = stream.write_all(b"0\r\n\r\n");
        }
        ResponseBody::Streaming(chunks) => {
            while let Ok(chunk) = chunks.recv() {
                let _ = write!(stream, "{:X}\r\n", chunk.len());
                if stream.write_all(&chunk).is_err()
                    || stream.write_all(b"\r\n").is_err()
                    || stream.flush().is_err()
                {
                    return;
                }
            }
            let _ = stream.write_all(b"0\r\n\r\n");
        }
    }
}

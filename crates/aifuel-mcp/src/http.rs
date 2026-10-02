//! Streamable-HTTP transport shared by the AI Fuel MCP servers.
//!
//! Implements the subset of MCP streamable HTTP (protocol 2025-11-25) that AI
//! Fuel's own gateway client already drives in `gateway::remote_transport`:
//!
//! - `POST /mcp` carries one JSON-RPC message. Messages with an `id` answer
//!   `200 application/json`; notifications and client responses answer
//!   `202 Accepted` with an empty body.
//! - The `initialize` response issues `MCP-Session-Id`; every later request
//!   repeats it. An unknown session answers `404`, which the client reads as
//!   "re-initialize".
//! - `GET /mcp` attaches the server-initiated SSE stream for sessions that
//!   expose one; servers without outbound traffic answer `405`.
//! - `DELETE /mcp` ends the session.
//!
//! There is no authentication layer: binding anything beyond loopback is the
//! operator's responsibility.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tiny_http::{Header, ListenAddr, Method, Request, Response, Server, StatusCode};

/// Default port for the MCP HTTP transport; 8787 is the dashboard.
pub const DEFAULT_PORT: u16 = 8788;

/// Heartbeat comment interval on an idle SSE stream so dead clients are
/// detected without waiting for the next event.
const SSE_KEEPALIVE: Duration = Duration::from_secs(15);

/// One MCP HTTP session created by an `initialize` POST.
pub(crate) trait HttpSession: Send + Sync + 'static {
    /// Handle one parsed JSON-RPC client message. `Some` answers
    /// `200 application/json`; `None` accepts notifications and client
    /// responses with `202 Accepted`.
    fn handle(&self, message: &Value) -> Option<Value>;

    /// The queue feeding this session's `GET /mcp` SSE stream, when the
    /// server emits server-initiated messages. `None` answers GET with `405`.
    fn events(&self) -> Option<Arc<EventQueue>> {
        None
    }

    /// Whether the session can still serve requests. Expired sessions are
    /// dropped on the next request so the client sees `404` and
    /// re-initializes.
    fn expired(&self) -> bool {
        false
    }

    /// End the session: cancel in-flight work and close the event stream.
    fn close(&self) {}
}

/// Why an [`EventQueue::push`] was rejected.
pub(crate) enum PushError {
    /// The buffered byte count is over the configured capacity.
    Full,
    /// The session is closed; no more frames are accepted.
    Closed,
}

/// One queued SSE frame. Its byte credit returns to the queue's budget when
/// the frame is read or dropped.
struct Queued {
    bytes: Vec<u8>,
    used: Arc<AtomicUsize>,
}

impl Drop for Queued {
    fn drop(&mut self) {
        self.used.fetch_sub(self.bytes.len(), Ordering::AcqRel);
    }
}

/// Outbound messages queued for a session's `GET /mcp` SSE stream, bounded to
/// `capacity` buffered bytes (0 means unbounded). The receiver moves into
/// whichever GET is attached and returns to the queue when that stream drops,
/// so a reconnect drains the backlog.
pub(crate) struct EventQueue {
    sender: Mutex<Option<mpsc::Sender<Queued>>>,
    receiver: Mutex<Option<mpsc::Receiver<Queued>>>,
    used: Arc<AtomicUsize>,
    capacity: usize,
}

impl EventQueue {
    pub(crate) fn new(capacity: usize) -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            sender: Mutex::new(Some(sender)),
            receiver: Mutex::new(Some(receiver)),
            used: Arc::new(AtomicUsize::new(0)),
            capacity,
        }
    }

    /// Buffer one serialized JSON-RPC message for SSE delivery.
    pub(crate) fn push(&self, frame: Vec<u8>) -> Result<(), PushError> {
        let used = self.used.fetch_add(frame.len(), Ordering::AcqRel);
        if self.capacity > 0 && used + frame.len() > self.capacity {
            self.used.fetch_sub(frame.len(), Ordering::AcqRel);
            return Err(PushError::Full);
        }
        let queued = Queued {
            bytes: frame,
            used: Arc::clone(&self.used),
        };
        let sender = self.sender.lock().expect("event queue sender mutex");
        match sender.as_ref() {
            Some(sender) => match sender.send(queued) {
                Ok(()) => Ok(()),
                // The returned value drops here and releases its credit.
                Err(mpsc::SendError(_)) => Err(PushError::Closed),
            },
            None => Err(PushError::Closed),
        }
    }

    /// Stop accepting frames; any attached stream ends once it drains.
    pub(crate) fn close(&self) {
        self.sender.lock().expect("event queue sender mutex").take();
    }

    fn attach(&self) -> Option<mpsc::Receiver<Queued>> {
        self.receiver
            .lock()
            .expect("event queue receiver mutex")
            .take()
    }

    fn detach(&self, receiver: mpsc::Receiver<Queued>) {
        *self.receiver.lock().expect("event queue receiver mutex") = Some(receiver);
    }
}

/// Stream queued messages to a `GET /mcp` client. The request's writer is
/// taken over so every `data:` frame is flushed immediately: `Response::raw_print`
/// would hold events in the connection's buffer until the stream ends.
fn stream_events(request: Request, queue: Arc<EventQueue>, receiver: mpsc::Receiver<Queued>) {
    let mut writer = request.into_writer();
    let head =
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\r\n";
    if writer
        .write_all(head.as_bytes())
        .and_then(|()| writer.flush())
        .is_err()
    {
        queue.detach(receiver);
        return;
    }
    loop {
        let frame = match receiver.recv_timeout(SSE_KEEPALIVE) {
            Ok(queued) => sse_frame(&queued.bytes),
            Err(mpsc::RecvTimeoutError::Timeout) => b": keep-alive\n\n".to_vec(),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if writer
            .write_all(&frame)
            .and_then(|()| writer.flush())
            .is_err()
        {
            break;
        }
    }
    queue.detach(receiver);
}

fn sse_frame(payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(payload.len() + 8);
    for line in payload.split(|byte| *byte == b'\n') {
        frame.extend_from_slice(b"data: ");
        frame.extend_from_slice(line);
        frame.push(b'\n');
    }
    frame.push(b'\n');
    frame
}

struct Shared {
    sessions: Mutex<HashMap<String, Arc<dyn HttpSession>>>,
    make_session: Box<dyn Fn(&Value) -> Result<Arc<dyn HttpSession>, String> + Send + Sync>,
    max_message_bytes: usize,
    /// DNS-rebinding protection only makes sense while bound to loopback; a
    /// deliberately non-loopback bind is the operator's responsibility.
    loopback_only: bool,
}

impl Shared {
    fn session(&self, session_id: &str) -> Option<Arc<dyn HttpSession>> {
        let mut sessions = self.sessions.lock().expect("session map mutex");
        let session = sessions.get(session_id)?.clone();
        if session.expired() {
            sessions.remove(session_id);
            session.close();
            return None;
        }
        Some(session)
    }
}

/// Bind `host:port` and serve `POST/GET/DELETE /mcp` until the process ends.
/// Each accepted request is handled on its own thread so a long-running
/// request or an open SSE stream never blocks the others.
pub(crate) fn serve<S, F>(
    server_name: &'static str,
    host: &str,
    port: u16,
    max_message_bytes: usize,
    make_session: F,
) -> Result<(), String>
where
    S: HttpSession,
    F: Fn(&Value) -> Result<S, String> + Send + Sync + 'static,
{
    let server = Server::http((host, port))
        .map_err(|error| format!("could not start the {server_name} HTTP server: {error}"))?;
    let address = server.server_addr().to_string();
    let loopback_only = match server.server_addr() {
        ListenAddr::IP(address) => address.ip().is_loopback(),
        #[allow(unreachable_patterns)]
        _ => false,
    };
    println!("{server_name}: http://{address}/mcp");
    println!("Press Ctrl-C to stop.");
    use std::io::Write;
    std::io::stdout()
        .flush()
        .map_err(|error| format!("could not flush the {server_name} address: {error}"))?;

    let shared = Arc::new(Shared {
        sessions: Mutex::new(HashMap::new()),
        make_session: Box::new(move |message| {
            make_session(message).map(|session| Arc::new(session) as Arc<dyn HttpSession>)
        }),
        max_message_bytes,
        loopback_only,
    });

    for request in server.incoming_requests() {
        let shared = Arc::clone(&shared);
        let _ = thread::Builder::new()
            .name(format!("{server_name}-http"))
            .spawn(move || handle(&shared, request));
    }
    Ok(())
}

fn handle(shared: &Shared, request: Request) {
    if shared.loopback_only && !is_local_request(&request) {
        respond(
            request,
            403,
            b"forbidden: cross-origin request rejected".to_vec(),
            Some("text/plain"),
            Vec::new(),
        );
        return;
    }
    let path = request.url().split('?').next().unwrap_or_default();
    match (request.method(), path) {
        (&Method::Post, "/mcp") => handle_post(shared, request),
        (&Method::Get, "/mcp") => handle_get(shared, request),
        (&Method::Delete, "/mcp") => handle_delete(shared, request),
        _ => respond(
            request,
            404,
            b"not found".to_vec(),
            Some("text/plain"),
            Vec::new(),
        ),
    }
}

fn handle_post(shared: &Shared, mut request: Request) {
    let mut body = Vec::new();
    let body_result = request
        .as_reader()
        .take(shared.max_message_bytes as u64 + 1)
        .read_to_end(&mut body);
    if body_result.is_err() {
        respond_rpc(
            request,
            400,
            Value::Null,
            -32700,
            "could not read the request body",
        );
        return;
    }
    if body.len() > shared.max_message_bytes {
        respond_rpc(
            request,
            413,
            Value::Null,
            -32600,
            "MCP message exceeds the configured byte limit",
        );
        return;
    }
    let message: Value = match serde_json::from_slice::<Value>(&body) {
        Ok(message) if message.is_object() => message,
        _ => {
            respond_rpc(
                request,
                400,
                Value::Null,
                -32700,
                "invalid JSON-RPC message",
            );
            return;
        }
    };

    match header_value(&request, "MCP-Session-Id") {
        Some(session_id) => {
            let Some(session) = shared.session(session_id) else {
                respond_rpc(
                    request,
                    404,
                    message.get("id").cloned().unwrap_or(Value::Null),
                    -32001,
                    "unknown MCP session",
                );
                return;
            };
            respond_message(request, session.handle(&message), None);
        }
        None => {
            let is_initialize = message.get("method").and_then(Value::as_str) == Some("initialize")
                && message.get("id").is_some();
            if !is_initialize {
                respond_rpc(
                    request,
                    400,
                    message.get("id").cloned().unwrap_or(Value::Null),
                    -32600,
                    "an MCP session is required; POST initialize first",
                );
                return;
            }
            let session = match (shared.make_session)(&message) {
                Ok(session) => session,
                Err(error) => {
                    respond_rpc(
                        request,
                        500,
                        message.get("id").cloned().unwrap_or(Value::Null),
                        -32603,
                        &error,
                    );
                    return;
                }
            };
            let session_id = new_session_id();
            shared
                .sessions
                .lock()
                .expect("session map mutex")
                .insert(session_id.clone(), Arc::clone(&session));
            respond_message(request, session.handle(&message), Some(session_id));
        }
    }
}

fn handle_get(shared: &Shared, request: Request) {
    let Some(session_id) = header_value(&request, "MCP-Session-Id") else {
        respond(
            request,
            400,
            b"an MCP session is required; POST initialize first".to_vec(),
            Some("text/plain"),
            Vec::new(),
        );
        return;
    };
    let Some(session) = shared.session(session_id) else {
        respond(
            request,
            404,
            b"unknown MCP session".to_vec(),
            Some("text/plain"),
            Vec::new(),
        );
        return;
    };
    let Some(queue) = session.events() else {
        respond(
            request,
            405,
            b"this MCP server does not open an event stream".to_vec(),
            Some("text/plain"),
            Vec::new(),
        );
        return;
    };
    let Some(receiver) = queue.attach() else {
        respond(
            request,
            409,
            b"an event stream is already attached to this session".to_vec(),
            Some("text/plain"),
            Vec::new(),
        );
        return;
    };
    stream_events(request, queue, receiver);
}

fn handle_delete(shared: &Shared, request: Request) {
    let Some(session_id) = header_value(&request, "MCP-Session-Id") else {
        respond(
            request,
            400,
            b"an MCP session is required".to_vec(),
            Some("text/plain"),
            Vec::new(),
        );
        return;
    };
    let session = shared
        .sessions
        .lock()
        .expect("session map mutex")
        .remove(session_id);
    match session {
        Some(session) => {
            session.close();
            respond(request, 200, Vec::new(), None, Vec::new());
        }
        None => respond(
            request,
            404,
            b"unknown MCP session".to_vec(),
            Some("text/plain"),
            Vec::new(),
        ),
    }
}

/// `Some` responses answer `200 application/json`; `None` accepts the message
/// with `202 Accepted` per the streamable-HTTP rules for notifications and
/// client responses.
fn respond_message(request: Request, response: Option<Value>, session_id: Option<String>) {
    match response {
        Some(response) => {
            let body = serde_json::to_vec(&response).expect("JSON-RPC responses serialize");
            let headers = session_id
                .map(|session_id| vec![("MCP-Session-Id".to_owned(), session_id)])
                .unwrap_or_default();
            respond(request, 200, body, Some("application/json"), headers);
        }
        None => respond(request, 202, Vec::new(), None, Vec::new()),
    }
}

fn respond_rpc(request: Request, status: u16, id: Value, code: i32, message: &str) {
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message}
    });
    respond(
        request,
        status,
        serde_json::to_vec(&body).expect("JSON-RPC errors serialize"),
        Some("application/json"),
        Vec::new(),
    );
}

fn respond(
    request: Request,
    status: u16,
    body: Vec<u8>,
    content_type: Option<&str>,
    headers: Vec<(String, String)>,
) {
    let mut response = Response::from_data(body).with_status_code(StatusCode(status));
    if let Some(content_type) = content_type {
        response.add_header(
            Header::from_bytes("Content-Type", content_type).expect("static content type is valid"),
        );
    }
    response.add_header(
        Header::from_bytes("Cache-Control", "no-store").expect("static header is valid"),
    );
    for (name, value) in headers {
        if let Ok(header) = Header::from_bytes(name, value) {
            response.add_header(header);
        }
    }
    let _ = request.respond(response);
}

fn header_value<'a>(request: &'a Request, name: &'static str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv(name))
        .map(|header| header.value.as_str())
}

fn new_session_id() -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let mut digest = Sha256::new();
    digest.update(std::process::id().to_be_bytes());
    digest.update(SEQUENCE.fetch_add(1, Ordering::Relaxed).to_be_bytes());
    digest.update(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_be_bytes(),
    );
    // An ASLR-influenced address folds into the digest so session ids are
    // not guessable from pid and time alone.
    let marker = 0_u8;
    digest.update((&marker as *const u8 as usize).to_be_bytes());
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Reject browser-originated cross-origin requests to a loopback listener
/// (DNS rebinding), matching the dashboard's guard.
fn is_local_request(request: &Request) -> bool {
    let host = header_value(request, "Host")
        .map(host_without_port)
        .unwrap_or_else(|| "127.0.0.1".to_owned());
    if !matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]" | "::1") {
        return false;
    }
    if let Some(origin) = header_value(request, "Origin") {
        let origin_host = origin
            .split_once("://")
            .map(|(_, value)| host_without_port(value))
            .unwrap_or_default();
        if origin != "null" && origin_host != host {
            return false;
        }
    }
    if request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Sec-Fetch-Site"))
        .is_some_and(|header| matches!(header.value.as_str(), "cross-site" | "same-site"))
    {
        return false;
    }
    true
}

fn host_without_port(value: &str) -> String {
    if let Some(value) = value.strip_prefix('[') {
        return value.split(']').next().unwrap_or("").to_owned();
    }
    value.split(':').next().unwrap_or("").to_owned()
}

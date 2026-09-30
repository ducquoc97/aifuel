//! A scripted `opencode serve` stand-in for the driver's HTTP surface.
//!
//! One `tiny_http` listener answers the routes the driver touches:
//! `GET /session` (readiness), `GET /event` (a live SSE stream fed by
//! `push_event`), `POST /session`, `GET /session/{id}`,
//! `POST /session/{id}/message`, `POST /session/{id}/abort`, and
//! `POST /session/{id}/permissions/{pid}`. Every request is recorded so
//! tests assert the exact wire calls; events pushed before the stream
//! attaches are queued, so ordering is deterministic without sleeps.

use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use tiny_http::{Response, Server};

/// One recorded request the fake received.
#[derive(Debug, Clone)]
pub(in crate::opencode_runtime) struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub body: String,
}

/// What a prompt POST should do.
enum PromptSlot {
    /// Respond immediately with the stored body.
    Respond(Value),
    /// Hold the connection until `settle_prompt` supplies a body.
    Held(mpsc::Receiver<Value>),
}

struct FakeState {
    requests: Mutex<Vec<RecordedRequest>>,
    /// Events queued for the `/event` stream before it attaches.
    queued: Mutex<VecDeque<String>>,
    sse: Mutex<Option<mpsc::Sender<String>>>,
    create_body: Mutex<Value>,
    get_status: Mutex<u16>,
    get_body: Mutex<Value>,
    prompt: Mutex<PromptSlot>,
    hold_next_prompt: AtomicBool,
}

/// The scripted server. Dropping it stops the listener thread when the
/// last request finishes.
pub(in crate::opencode_runtime) struct FakeServe {
    url: reqwest::Url,
    state: Arc<FakeState>,
    /// Senders that release held prompt POSTs, in order.
    settler: Mutex<Vec<mpsc::Sender<Value>>>,
}

impl FakeServe {
    pub fn start() -> Self {
        let server = Server::http("127.0.0.1:0").expect("fake server binds");
        let url = format!("http://{}/", server.server_addr());
        let state = Arc::new(FakeState {
            requests: Mutex::new(Vec::new()),
            queued: Mutex::new(VecDeque::new()),
            sse: Mutex::new(None),
            create_body: Mutex::new(json!({"id": "ses_fake"})),
            get_status: Mutex::new(200),
            get_body: Mutex::new(json!({"id": "ses_fake"})),
            prompt: Mutex::new(PromptSlot::Respond(json!({
                "info": {"id": "msg_asst", "tokens": {"input": 0, "output": 0}},
                "parts": [],
            }))),
            hold_next_prompt: AtomicBool::new(false),
        });
        let worker = {
            let state = Arc::clone(&state);
            thread::Builder::new()
                .name("opencode-fake-serve".to_owned())
                .spawn(move || {
                    while let Ok(request) = server.recv() {
                        let state = Arc::clone(&state);
                        thread::spawn(move || handle(request, state));
                    }
                })
                .expect("fake server thread spawns")
        };
        std::mem::forget(worker);
        Self {
            url: reqwest::Url::parse(&url).expect("fake server url parses"),
            state,
            settler: Mutex::new(Vec::new()),
        }
    }

    /// The base URL the connector reports for this fake.
    pub fn url(&self) -> reqwest::Url {
        self.url.clone()
    }

    /// Every request the driver made, in arrival order.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.state.requests.lock().expect("requests mutex").clone()
    }

    /// Queue one bus event for the `/event` stream, delivered as
    /// `data: <json>`. Events pushed before the stream attaches are
    /// delivered on connect.
    pub fn push_event(&self, event: Value) {
        let payload = event.to_string();
        let sender = self.state.sse.lock().expect("sse mutex").clone();
        match sender {
            Some(sender) => {
                let _ = sender.send(payload);
            }
            None => self
                .state
                .queued
                .lock()
                .expect("queued mutex")
                .push_back(payload),
        }
    }

    /// The next prompt POST parks until `settle_prompt` supplies its
    /// response body, so a test can script mid-run events.
    pub fn hold_prompt(&self) {
        let (tx, rx) = mpsc::channel();
        self.state.hold_next_prompt.store(true, Ordering::SeqCst);
        *self.state.prompt.lock().expect("prompt mutex") = PromptSlot::Held(rx);
        // Keep the sender reachable for `settle_prompt`.
        self.settler.lock().expect("settler mutex").push(tx);
    }

    /// Release the oldest held prompt POST with `body` as its response.
    pub fn settle_prompt(&self, body: Value) {
        let settler = self.settler.lock().expect("settler mutex").pop();
        if let Some(settler) = settler {
            let _ = settler.send(body);
        }
    }

    /// The body `POST /session` returns.
    pub fn set_create_session(&self, body: Value) {
        *self.state.create_body.lock().expect("create mutex") = body;
    }

    /// The response `GET /session/{id}` returns.
    pub fn set_get_session(&self, status: u16, body: Value) {
        *self.state.get_status.lock().expect("get status mutex") = status;
        *self.state.get_body.lock().expect("get body mutex") = body;
    }
}

/// Route one request. Prompt POSTs may park on `settle_prompt`; the
/// `/event` route streams queued then live events.
fn handle(mut request: tiny_http::Request, state: Arc<FakeState>) {
    let method = request.method().to_string();
    let path = request.url().split('?').next().unwrap_or("/").to_owned();
    let mut body = String::new();
    request
        .as_reader()
        .take(1024 * 1024)
        .read_to_string(&mut body)
        .ok();
    if path != "/event" {
        state
            .requests
            .lock()
            .expect("requests mutex")
            .push(RecordedRequest {
                method: method.clone(),
                path: path.clone(),
                body: body.clone(),
            });
    }

    match (method.as_str(), path.as_str()) {
        ("GET", "/session") => respond(request, 200, &json!([]).to_string()),
        ("GET", "/event") => {
            let (tx, rx) = mpsc::channel::<String>();
            {
                let mut queued = state.queued.lock().expect("queued mutex");
                while let Some(payload) = queued.pop_front() {
                    let _ = tx.send(payload);
                }
            }
            *state.sse.lock().expect("sse mutex") = Some(tx.clone());
            // `respond` buffers inside the connection's BufWriter until the
            // body ends, which an open stream never does. `into_writer` is
            // the documented CGI escape hatch: hand-write the chunked SSE
            // response and flush after every frame.
            let mut writer = request.into_writer();
            let _ = writer.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            );
            let _ = writer.flush();
            while let Ok(payload) = rx.recv() {
                let frame = format!("data: {payload}\n\n");
                let chunk = format!("{:x}\r\n{frame}\r\n", frame.len());
                if writer.write_all(chunk.as_bytes()).is_err() || writer.flush().is_err() {
                    break;
                }
            }
            let _ = writer.write_all(b"0\r\n\r\n");
            let _ = writer.flush();
        }
        ("POST", "/session") => {
            let body = state.create_body.lock().expect("create mutex").clone();
            respond(request, 200, &body.to_string());
        }
        ("POST", _) if path.ends_with("/message") => {
            let slot = std::mem::replace(
                &mut *state.prompt.lock().expect("prompt mutex"),
                PromptSlot::Respond(json!({"info": {}, "parts": []})),
            );
            let _ = state.hold_next_prompt.swap(false, Ordering::SeqCst);
            let body = match slot {
                PromptSlot::Respond(body) => body,
                PromptSlot::Held(rx) => rx.recv().unwrap_or_else(|_| {
                    json!({"info": {"error": {"name": "MessageAbortedError", "data": {}}}, "parts": []})
                }),
            };
            respond(request, 200, &body.to_string());
        }
        ("POST", _) if path.ends_with("/abort") => {
            respond(request, 200, "true");
        }
        ("POST", _) if path.contains("/permissions/") => {
            respond(request, 200, "true");
        }
        ("GET", _) if path.starts_with("/session/") => {
            let status = *state.get_status.lock().expect("get status mutex");
            let body = state.get_body.lock().expect("get body mutex").clone();
            respond(request, status, &body.to_string());
        }
        _ => respond(request, 404, &json!({"error": "not found"}).to_string()),
    }
}

fn respond(request: tiny_http::Request, status: u16, body: &str) {
    let response = Response::from_string(body).with_status_code(status);
    let _ = request.respond(response);
}

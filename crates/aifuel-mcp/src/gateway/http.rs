//! Streamable-HTTP transport for the MCP Gateway: one rmcp service per HTTP
//! session, requests answered on their POST, server-initiated notifications
//! published on the `GET /mcp` SSE stream.

use super::remote_transport::request_id_key;
use super::state::GatewayState;
use crate::http::{EventQueue, HttpSession, PushError};
use aifuel_app::McpGatewayFacade;
use rmcp::RoleServer;
use rmcp::model::ClientJsonRpcMessage;
use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;
use tokio::sync::mpsc as tokio_mpsc;
use tokio_util::sync::CancellationToken;

/// One streamable-HTTP MCP Gateway session.
pub(super) struct GatewayHttpSession {
    shared: Arc<SessionShared>,
}

struct SessionShared {
    /// Client messages queued for the rmcp service loop.
    inbound: Mutex<Option<tokio_mpsc::UnboundedSender<ClientJsonRpcMessage>>>,
    /// POST threads waiting on the service's response to one request id.
    pending: Mutex<HashMap<String, mpsc::Sender<Value>>>,
    events: Arc<EventQueue>,
    cancellation: CancellationToken,
    /// Cancelled when the SSE backlog exceeds the output budget, matching the
    /// "notification buffer filled" stop condition of the stdio transport.
    overflow: CancellationToken,
    closed: AtomicBool,
}

impl GatewayHttpSession {
    /// Build the session's `GatewayState` and spawn its rmcp service loop on
    /// the shared runtime. The initialize message itself still travels
    /// through [`HttpSession::handle`] so its response returns on the POST.
    pub(super) fn start(
        facade: McpGatewayFacade,
        allowed_tools: Option<Vec<String>>,
        optimize: aifuel_core::OptimizePlan,
        runtime: tokio::runtime::Handle,
    ) -> Result<Self, String> {
        let limits = facade.gateway_limits().clone();
        let overflow = CancellationToken::new();
        let state = Arc::new(GatewayState::new(
            facade,
            overflow.clone(),
            allowed_tools,
            optimize,
        ));
        let cancellation = CancellationToken::new();
        let (inbound_tx, inbound_rx) = tokio_mpsc::unbounded_channel();
        let shared = Arc::new(SessionShared {
            inbound: Mutex::new(Some(inbound_tx)),
            pending: Mutex::new(HashMap::new()),
            events: Arc::new(EventQueue::new(limits.max_output_buffer_bytes)),
            cancellation: cancellation.clone(),
            overflow: overflow.clone(),
            closed: AtomicBool::new(false),
        });
        let transport = HttpTransport {
            inbound: inbound_rx,
            shared: Arc::clone(&shared),
            max_message_bytes: limits.max_message_bytes,
        };
        let monitor = Arc::clone(&shared);
        runtime.spawn(async move {
            let result = super::run_session(state, transport, cancellation, overflow).await;
            if let Err(error) = result {
                eprintln!("aifuel: MCP Gateway HTTP session ended: {error}");
            }
            monitor.finish();
        });
        Ok(Self { shared })
    }
}

impl HttpSession for GatewayHttpSession {
    fn handle(&self, message: &Value) -> Option<Value> {
        let parsed: ClientJsonRpcMessage = match serde_json::from_value(message.clone()) {
            Ok(parsed) => parsed,
            Err(_) => {
                return Some(rpc_error(
                    message.get("id").cloned().unwrap_or(Value::Null),
                    -32700,
                    "invalid JSON-RPC message",
                ));
            }
        };
        let request_key = match &parsed {
            ClientJsonRpcMessage::Request(request) => serde_json::to_value(&request.id)
                .ok()
                .map(|id| request_id_key(&id)),
            _ => None,
        };
        // The waiter registers before the message is queued so the response
        // can never overtake it. A duplicate in-flight id would strand the
        // earlier waiter, so it is rejected instead.
        let waiter = match request_key.clone() {
            Some(key) => {
                let (sender, receiver) = mpsc::channel();
                if self
                    .shared
                    .pending
                    .lock()
                    .expect("gateway pending mutex")
                    .insert(key, sender)
                    .is_some()
                {
                    return Some(rpc_error(
                        message.get("id").cloned().unwrap_or(Value::Null),
                        -32600,
                        "request id is already in flight on this session",
                    ));
                }
                Some(receiver)
            }
            None => None,
        };
        {
            let inbound = self.shared.inbound.lock().expect("gateway inbound mutex");
            let queued = inbound
                .as_ref()
                .is_some_and(|sender| sender.send(parsed).is_ok());
            if !queued {
                if let Some(key) = &request_key {
                    self.shared
                        .pending
                        .lock()
                        .expect("gateway pending mutex")
                        .remove(key);
                }
                return Some(rpc_error(
                    message.get("id").cloned().unwrap_or(Value::Null),
                    -32001,
                    "the MCP Gateway session has ended",
                ));
            }
        }
        let Some(receiver) = waiter else {
            return None;
        };
        loop {
            match receiver.recv_timeout(Duration::from_millis(250)) {
                Ok(response) => return Some(response),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Some(rpc_error(
                        message.get("id").cloned().unwrap_or(Value::Null),
                        -32001,
                        "the MCP Gateway session has ended",
                    ));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if self.shared.closed.load(Ordering::Acquire) {
                        return Some(rpc_error(
                            message.get("id").cloned().unwrap_or(Value::Null),
                            -32001,
                            "the MCP Gateway session has ended",
                        ));
                    }
                }
            }
        }
    }

    fn events(&self) -> Option<Arc<EventQueue>> {
        Some(Arc::clone(&self.shared.events))
    }

    fn expired(&self) -> bool {
        self.shared.closed.load(Ordering::Acquire)
    }

    fn close(&self) {
        self.shared.finish();
    }
}

impl SessionShared {
    /// End the session: stop the service loop, fail every POST still waiting
    /// on a response, and close the SSE stream.
    fn finish(&self) {
        self.closed.store(true, Ordering::Release);
        self.cancellation.cancel();
        self.inbound.lock().expect("gateway inbound mutex").take();
        self.pending.lock().expect("gateway pending mutex").clear();
        self.events.close();
    }
}

fn rpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

/// The rmcp [`Transport`] backed by HTTP: `receive` reads queued client
/// messages from POSTs; `send` routes responses back to the waiting POST by
/// request id and publishes server-initiated messages on the SSE queue.
struct HttpTransport {
    inbound: tokio_mpsc::UnboundedReceiver<ClientJsonRpcMessage>,
    shared: Arc<SessionShared>,
    max_message_bytes: usize,
}

impl Transport<RoleServer> for HttpTransport {
    type Error = io::Error;

    fn send(
        &mut self,
        message: TxJsonRpcMessage<RoleServer>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let shared = Arc::clone(&self.shared);
        let max_message_bytes = self.max_message_bytes;
        async move {
            let value = serde_json::to_value(&message).map_err(io::Error::other)?;
            if value.get("method").is_some() {
                let bytes = serde_json::to_vec(&value).map_err(io::Error::other)?;
                if bytes.len() > max_message_bytes {
                    shared.cancellation.cancel();
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "MCP message exceeds the configured byte limit",
                    ));
                }
                match shared.events.push(bytes) {
                    Ok(()) => {}
                    Err(PushError::Full) => {
                        shared.overflow.cancel();
                        return Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "MCP Gateway notification buffer filled",
                        ));
                    }
                    Err(PushError::Closed) => {
                        return Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "gateway output closed",
                        ));
                    }
                }
                return Ok(());
            }
            let waiter = shared
                .pending
                .lock()
                .expect("gateway pending mutex")
                .remove(&request_id_key(&value["id"]));
            if let Some(waiter) = waiter {
                let _ = waiter.send(value);
            }
            Ok(())
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleServer>> {
        tokio::select! {
            _ = self.shared.cancellation.cancelled() => None,
            message = self.inbound.recv() => message,
        }
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.inbound.close();
        Ok(())
    }
}

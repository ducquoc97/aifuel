use super::remote_endpoint::RemoteEndpoint;
use aifuel_app::ServerLimits;
use rmcp::RoleClient;
use rmcp::model::ServerJsonRpcMessage;
use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64};
use tokio::sync::{Mutex, Notify, RwLock, mpsc, watch};
use tokio_util::sync::CancellationToken;

mod http;
mod messages;
mod response_sse;

pub(super) const PROTOCOL_VERSION: &str = "2025-11-25";

pub(super) struct RemoteHttpTransport {
    state: Arc<RemoteHttpState>,
    responses: mpsc::Receiver<ServerJsonRpcMessage>,
}

pub(super) struct RemoteSession {
    state: Arc<RemoteHttpState>,
}

#[derive(Debug)]
pub(super) enum RemoteHttpError {
    SessionExpired,
    Cancelled,
    Closed,
    TimedOut,
    Message(&'static str),
    HttpStatus(u16),
}

impl RemoteHttpError {
    pub(super) fn message(&self) -> String {
        match self {
            Self::SessionExpired => {
                "remote MCP session expired; the failed request was not replayed".to_owned()
            }
            Self::Cancelled => "remote MCP request was cancelled".to_owned(),
            Self::Closed => "remote MCP connection was closed".to_owned(),
            Self::TimedOut => "remote MCP request timed out; its outcome may be unknown".to_owned(),
            Self::Message(message) => (*message).to_owned(),
            Self::HttpStatus(status) => format!("remote MCP server returned HTTP {status}"),
        }
    }
}

pub(super) struct RemoteHttpState {
    pub(super) server_id: String,
    pub(super) endpoint: reqwest::Url,
    pub(super) client: reqwest::Client,
    pub(super) limits: ServerLimits,
    pub(super) responses: mpsc::Sender<ServerJsonRpcMessage>,
    pub(super) cancellation: CancellationToken,
    pub(super) pending: Mutex<HashMap<String, CancellationToken>>,
    pub(super) expired_pending: Mutex<HashSet<String>>,
    pub(super) cancellations_sent: Mutex<HashSet<String>>,
    pub(super) session_id: Mutex<Option<String>>,
    pub(super) initialize_request: Mutex<Option<Value>>,
    pub(super) initialized: AtomicBool,
    pub(super) ready: Notify,
    pub(super) session_expired: AtomicBool,
    pub(super) session_generation: watch::Sender<u64>,
    pub(super) session_gate: RwLock<()>,
    pub(super) reinitialize: Mutex<()>,
    pub(super) next_initialize_id: AtomicU64,
    pub(super) event_listener_started: AtomicBool,
    pub(super) delete_started: AtomicBool,
}

impl RemoteHttpState {
    pub(super) fn mark_session_expired(&self) {
        if !self
            .session_expired
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            self.session_generation.send_modify(|generation| {
                *generation = generation.wrapping_add(1);
            });
        }
    }
}

impl RemoteHttpTransport {
    pub(super) fn new(
        endpoint: RemoteEndpoint,
        server_id: String,
        limits: ServerLimits,
    ) -> (Self, RemoteSession) {
        let (response_tx, response_rx) = mpsc::channel(1);
        let (session_generation, _) = watch::channel(0);
        let state = Arc::new(RemoteHttpState {
            server_id,
            endpoint: endpoint.url,
            client: endpoint.client,
            limits,
            responses: response_tx,
            cancellation: CancellationToken::new(),
            pending: Mutex::new(HashMap::new()),
            expired_pending: Mutex::new(HashSet::new()),
            cancellations_sent: Mutex::new(HashSet::new()),
            session_id: Mutex::new(None),
            initialize_request: Mutex::new(None),
            initialized: AtomicBool::new(false),
            ready: Notify::new(),
            session_expired: AtomicBool::new(false),
            session_generation,
            session_gate: RwLock::new(()),
            reinitialize: Mutex::new(()),
            next_initialize_id: AtomicU64::new(1),
            event_listener_started: AtomicBool::new(false),
            delete_started: AtomicBool::new(false),
        });
        (
            Self {
                state: Arc::clone(&state),
                responses: response_rx,
            },
            RemoteSession { state },
        )
    }
}

impl RemoteSession {
    pub(super) async fn shutdown(&mut self) {
        http::shutdown_remote(&self.state).await;
    }
}

impl Transport<RoleClient> for RemoteHttpTransport {
    type Error = io::Error;

    fn send(
        &mut self,
        message: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let state = Arc::clone(&self.state);
        async move {
            if state.cancellation.is_cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "remote MCP transport is closed",
                ));
            }
            let body = serde_json::to_value(message).map_err(io::Error::other)?;
            let method = body
                .get("method")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let request_id = if method.is_some() {
                body.get("id").cloned()
            } else {
                None
            };
            let request_key = request_id.as_ref().map(request_id_key);
            let cancellation = state.cancellation.child_token();
            if let Some(key) = &request_key {
                state
                    .pending
                    .lock()
                    .await
                    .insert(key.clone(), cancellation.clone());
            }
            let mut should_send = true;
            if method.as_deref() == Some("notifications/cancelled")
                && let Some(cancelled_id) = body
                    .get("params")
                    .and_then(|params| params.get("requestId"))
                    .map(request_id_key)
            {
                let active = state.pending.lock().await.get(&cancelled_id).cloned();
                if let Some(active) = active {
                    active.cancel();
                    should_send = state.cancellations_sent.lock().await.insert(cancelled_id);
                } else {
                    should_send = false;
                }
            }
            if !should_send {
                return Ok(());
            }

            tokio::spawn(async move {
                let result = http::send_message(
                    Arc::clone(&state),
                    body,
                    method.as_deref(),
                    request_id.clone(),
                    cancellation.clone(),
                )
                .await;
                let succeeded = result.is_ok();
                if matches!(
                    &result,
                    Err(RemoteHttpError::TimedOut | RemoteHttpError::Cancelled)
                ) && method.as_deref() != Some("initialize")
                    && let Some(id) = request_id.as_ref()
                {
                    let _ = http::cancel_upstream_request(&state, id).await;
                }
                let expiry_cancelled = if let Some(key) = &request_key {
                    state.expired_pending.lock().await.remove(key)
                } else {
                    false
                };
                if let Err(error) = result
                    && let Some(id) = request_id.as_ref()
                    && !state.cancellation.is_cancelled()
                    && (!cancellation.is_cancelled() || expiry_cancelled)
                {
                    eprintln!(
                        "aifuel: remote MCP server {}: {}",
                        state.server_id,
                        error.message()
                    );
                    let error_cancellation = if expiry_cancelled {
                        state.cancellation.child_token()
                    } else {
                        cancellation.clone()
                    };
                    let _ =
                        http::send_error(&state, id, &error.message(), &error_cancellation).await;
                }
                if !succeeded && method.as_deref() != Some("notifications/initialized") {
                    // Failures without a request ID have no protocol response to carry them.
                    if request_id.is_none() {
                        eprintln!(
                            "aifuel: remote MCP server {}: notification failed",
                            state.server_id
                        );
                    }
                }
                if method.as_deref() == Some("notifications/initialized") && !succeeded {
                    state.cancellation.cancel();
                }
                if let Some(key) = request_key {
                    state.pending.lock().await.remove(&key);
                    state.expired_pending.lock().await.remove(&key);
                    state.cancellations_sent.lock().await.remove(&key);
                }
            });
            Ok(())
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        tokio::select! {
            _ = self.state.cancellation.cancelled() => None,
            message = self.responses.recv() => message,
        }
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        http::shutdown_remote(&self.state).await;
        Ok(())
    }
}

pub(super) fn request_id_key(id: &Value) -> String {
    serde_json::to_string(id).unwrap_or_default()
}

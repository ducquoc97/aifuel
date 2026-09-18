use rmcp::ClientHandler;
use rmcp::model::{
    ClientCapabilities, ClientInfo, Implementation, ProgressNotificationParam, ProgressToken,
    ProtocolVersion,
};
use rmcp::service::{NotificationContext, Peer, RoleClient, RoleServer};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;

pub(super) struct GatewayEvents {
    pub(super) tools_dirty: AtomicBool,
    pub(super) tools_changed: Notify,
    host_peer: Mutex<Option<Peer<RoleServer>>>,
}

impl Default for GatewayEvents {
    fn default() -> Self {
        Self {
            tools_dirty: AtomicBool::new(true),
            tools_changed: Notify::new(),
            host_peer: Mutex::new(None),
        }
    }
}

impl GatewayEvents {
    pub(super) async fn set_host_peer(&self, peer: Peer<RoleServer>) {
        *self.host_peer.lock().await = Some(peer);
    }

    pub(super) fn mark_tools_changed(&self) {
        self.tools_dirty.store(true, Ordering::Release);
        self.tools_changed.notify_one();
    }

    pub(super) async fn notify_tools_changed(&self) {
        if let Some(peer) = self.host_peer.lock().await.as_ref() {
            let _ = peer.notify_tool_list_changed().await;
        }
    }
}

pub(super) struct GatewayUpstreamHandler {
    pub(super) events: Arc<GatewayEvents>,
    pub(super) progress: Arc<ProgressRoutes>,
}

impl ClientHandler for GatewayUpstreamHandler {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::new(
            ClientCapabilities::default(),
            Implementation::new("aifuel-gateway", env!("CARGO_PKG_VERSION")),
        )
        .with_protocol_version(ProtocolVersion::V_2025_11_25)
    }

    async fn on_progress(
        &self,
        notification: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.progress.forward(notification).await;
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.events.mark_tools_changed();
    }
}

#[derive(Clone)]
struct ProgressRoute {
    host_peer: Peer<RoleServer>,
    host_token: ProgressToken,
}

pub(super) struct ProgressRoutes {
    state: Mutex<ProgressState>,
    active_progress_requests: AtomicUsize,
    max_pending_bytes: usize,
    overflow: CancellationToken,
}

#[derive(Default)]
struct ProgressState {
    routes: HashMap<ProgressToken, ProgressRoute>,
    early: HashMap<ProgressToken, (ProgressNotificationParam, usize)>,
    pending_bytes: usize,
}

impl ProgressRoutes {
    pub(super) fn new(max_pending_bytes: usize, overflow: CancellationToken) -> Self {
        Self {
            state: Mutex::new(ProgressState::default()),
            active_progress_requests: AtomicUsize::new(0),
            max_pending_bytes,
            overflow,
        }
    }

    pub(super) fn begin_request(&self) {
        self.active_progress_requests.fetch_add(1, Ordering::AcqRel);
    }

    pub(super) fn cancel_unbound_request(&self) {
        self.active_progress_requests
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                Some(active.saturating_sub(1))
            })
            .ok();
    }

    pub(super) async fn register(
        &self,
        upstream_token: ProgressToken,
        host_peer: Peer<RoleServer>,
        host_token: ProgressToken,
    ) {
        let pending = {
            let mut state = self.state.lock().await;
            state.routes.insert(
                upstream_token.clone(),
                ProgressRoute {
                    host_peer,
                    host_token,
                },
            );
            let pending = state.early.remove(&upstream_token);
            if let Some((_, bytes)) = pending.as_ref() {
                state.pending_bytes = state.pending_bytes.saturating_sub(*bytes);
            }
            pending.map(|(notification, _)| notification)
        };
        if let Some(notification) = pending {
            self.forward(notification).await;
        }
    }

    pub(super) async fn remove(&self, token: &ProgressToken) {
        let mut state = self.state.lock().await;
        state.routes.remove(token);
        if let Some((_, bytes)) = state.early.remove(token) {
            state.pending_bytes = state.pending_bytes.saturating_sub(bytes);
        }
        self.cancel_unbound_request();
    }

    async fn forward(&self, mut notification: ProgressNotificationParam) {
        let token = notification.progress_token.clone();
        let route = {
            let mut state = self.state.lock().await;
            if let Some(route) = state.routes.get(&token).cloned() {
                Some(route)
            } else if self.active_progress_requests.load(Ordering::Acquire) > 0 {
                let bytes = serde_json::to_vec(&notification)
                    .map(|notification| notification.len())
                    .unwrap_or(self.max_pending_bytes.saturating_add(1));
                if let Some((_, previous_bytes)) = state.early.remove(&token) {
                    state.pending_bytes = state.pending_bytes.saturating_sub(previous_bytes);
                }
                if bytes > self.max_pending_bytes
                    || state.pending_bytes.saturating_add(bytes) > self.max_pending_bytes
                {
                    self.overflow.cancel();
                } else {
                    state.pending_bytes += bytes;
                    state.early.insert(token, (notification.clone(), bytes));
                }
                None
            } else {
                None
            }
        };
        if let Some(route) = route {
            notification.progress_token = route.host_token;
            let _ = route.host_peer.notify_progress(notification).await;
        }
    }
}

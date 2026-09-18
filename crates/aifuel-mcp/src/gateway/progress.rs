use rmcp::ClientHandler;
use rmcp::model::{
    ClientCapabilities, ClientInfo, Implementation, ProgressNotificationParam, ProgressToken,
    ProtocolVersion,
};
use rmcp::service::{NotificationContext, Peer, RoleClient, RoleServer};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
    pub(super) server_id: String,
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
        self.progress.forward(&self.server_id, notification).await;
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
    max_pending_bytes: usize,
    overflow: CancellationToken,
}

#[derive(Default)]
struct ProgressState {
    routes: HashMap<(String, ProgressToken), ProgressRoute>,
    early: HashMap<(String, ProgressToken), (ProgressNotificationParam, usize)>,
    active_requests: HashMap<String, usize>,
    pending_bytes: usize,
}

impl ProgressRoutes {
    pub(super) fn new(max_pending_bytes: usize, overflow: CancellationToken) -> Self {
        Self {
            state: Mutex::new(ProgressState::default()),
            max_pending_bytes,
            overflow,
        }
    }

    pub(super) async fn begin_request(&self, server_id: &str) {
        *self
            .state
            .lock()
            .await
            .active_requests
            .entry(server_id.to_owned())
            .or_default() += 1;
    }

    pub(super) async fn cancel_unbound_request(&self, server_id: &str) {
        let mut state = self.state.lock().await;
        decrement_active_request(&mut state, server_id);
    }

    pub(super) async fn register(
        &self,
        server_id: &str,
        upstream_token: ProgressToken,
        host_peer: Peer<RoleServer>,
        host_token: ProgressToken,
    ) {
        let key = (server_id.to_owned(), upstream_token);
        let pending = {
            let mut state = self.state.lock().await;
            state.routes.insert(
                key.clone(),
                ProgressRoute {
                    host_peer,
                    host_token,
                },
            );
            let pending = state.early.remove(&key);
            if let Some((_, bytes)) = pending.as_ref() {
                state.pending_bytes = state.pending_bytes.saturating_sub(*bytes);
            }
            pending.map(|(notification, _)| notification)
        };
        if let Some(notification) = pending {
            self.forward(server_id, notification).await;
        }
    }

    pub(super) async fn remove(&self, server_id: &str, token: &ProgressToken) {
        let mut state = self.state.lock().await;
        let key = (server_id.to_owned(), token.clone());
        state.routes.remove(&key);
        if let Some((_, bytes)) = state.early.remove(&key) {
            state.pending_bytes = state.pending_bytes.saturating_sub(bytes);
        }
        decrement_active_request(&mut state, server_id);
    }

    async fn forward(&self, server_id: &str, mut notification: ProgressNotificationParam) {
        let token = notification.progress_token.clone();
        let key = (server_id.to_owned(), token.clone());
        let route = {
            let mut state = self.state.lock().await;
            if let Some(route) = state.routes.get(&key).cloned() {
                Some(route)
            } else if state
                .active_requests
                .get(server_id)
                .is_some_and(|active| *active > 0)
            {
                let bytes = serde_json::to_vec(&notification)
                    .map(|notification| notification.len())
                    .unwrap_or(self.max_pending_bytes.saturating_add(1));
                if let Some((_, previous_bytes)) = state.early.remove(&key) {
                    state.pending_bytes = state.pending_bytes.saturating_sub(previous_bytes);
                }
                if bytes > self.max_pending_bytes
                    || state.pending_bytes.saturating_add(bytes) > self.max_pending_bytes
                {
                    self.overflow.cancel();
                } else {
                    state.pending_bytes += bytes;
                    state.early.insert(key, (notification.clone(), bytes));
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

fn decrement_active_request(state: &mut ProgressState, server_id: &str) {
    let inactive = if let Some(active) = state.active_requests.get_mut(server_id) {
        *active = active.saturating_sub(1);
        *active == 0
    } else {
        false
    };
    if inactive {
        state.active_requests.remove(server_id);
        let stale_keys = state
            .early
            .keys()
            .filter(|(owner, _)| owner == server_id)
            .cloned()
            .collect::<Vec<_>>();
        for key in stale_keys {
            if let Some((_, bytes)) = state.early.remove(&key) {
                state.pending_bytes = state.pending_bytes.saturating_sub(bytes);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ProgressRoutes;
    use rmcp::model::ProgressNotificationParam;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn early_progress_is_buffered_only_for_its_originating_server() {
        let overflow = CancellationToken::new();
        let routes = ProgressRoutes::new(1024, overflow.clone());
        routes.begin_request("docs").await;
        let notification: ProgressNotificationParam = serde_json::from_value(json!({
            "progressToken": "same-token",
            "progress": 1,
            "total": 1,
            "message": "working"
        }))
        .expect("fixture progress notification should deserialize");

        routes.forward("search", notification.clone()).await;
        {
            let state = routes.state.lock().await;
            assert!(state.early.is_empty());
            assert_eq!(state.pending_bytes, 0);
        }

        routes.forward("docs", notification).await;
        {
            let state = routes.state.lock().await;
            assert_eq!(state.early.len(), 1);
            assert_eq!(state.early.keys().next().unwrap().0, "docs");
        }
        routes.cancel_unbound_request("docs").await;
        let state = routes.state.lock().await;
        assert!(state.early.is_empty());
        assert_eq!(state.pending_bytes, 0);
        assert!(!overflow.is_cancelled());
    }
}

mod prompts;
mod resources;
mod tools;

use super::connection::{
    ConnectedGatewayServer, ConnectionContext, ResolvedRemoteAuthentication,
    ResolvedRemoteSecretHeaders, snapshot_remote_authentication, snapshot_remote_secret_headers,
};
use super::process::{ResolvedEnvironment, snapshot_environment};
use super::progress::{GatewayEvents, ProgressRoutes};
use super::prompts::{PromptSnapshot, ServerPromptSnapshot};
use super::resources::ResourceSnapshot;
use super::snapshot::{ServerToolSnapshot, ToolSnapshot};
use aifuel_app::{
    GatewayLimits, McpGatewayFacade, McpServerDefinition, SelectedMcpServer, ServerLimits,
};
use futures_util::future::join_all;
use rmcp::model::{ErrorCode, ErrorData as McpError};
use rmcp::service::{Peer, RoleServer};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub(super) struct GatewayState {
    limits: GatewayLimits,
    servers: BTreeMap<String, Arc<GatewayServerState>>,
    cursor_scope: String,
    allowed_tools: Option<Vec<String>>,
    pub(super) events: Arc<GatewayEvents>,
    pub(super) progress: Arc<ProgressRoutes>,
    pending_tool_list_notification: AtomicBool,
    pending_resource_list_notification: AtomicBool,
    pending_prompt_list_notification: AtomicBool,
    request_slots: Arc<Semaphore>,
    snapshot: Mutex<Option<Arc<ToolSnapshot>>>,
    resource_aggregate: Mutex<Option<Arc<ResourceSnapshot>>>,
    prompt_snapshot: Mutex<Option<Arc<PromptSnapshot>>>,
    next_snapshot_id: AtomicU64,
    next_resource_snapshot_id: AtomicU64,
    next_prompt_snapshot_id: AtomicU64,
}

struct GatewayServerState {
    server: SelectedMcpServer,
    local_environment: Option<ResolvedEnvironment>,
    remote_authentication: ResolvedRemoteAuthentication,
    remote_secret_headers: Result<ResolvedRemoteSecretHeaders, &'static str>,
    connection: Mutex<Option<Arc<ConnectedGatewayServer>>>,
    snapshot: Mutex<Option<Arc<ServerToolSnapshot>>>,
    resource_snapshot: Mutex<Option<Arc<ResourceSnapshot>>>,
    prompt_snapshot: Mutex<Option<Arc<ServerPromptSnapshot>>>,
}

enum ServerSnapshotError {
    Retryable(McpError),
    IdentityConflict(McpError),
}

impl GatewayState {
    pub(super) fn new(
        facade: McpGatewayFacade,
        overflow: CancellationToken,
        allowed_tools: Option<Vec<String>>,
    ) -> Self {
        let limits = facade.gateway_limits().clone();
        let servers = facade
            .selected_servers()
            .iter()
            .cloned()
            .map(|server| {
                let local_environment = matches!(&server.definition, McpServerDefinition::Stdio(_))
                    .then(|| snapshot_environment(&server));
                let remote_authentication = snapshot_remote_authentication(&server);
                let remote_secret_headers = snapshot_remote_secret_headers(&server);
                (
                    server.id.clone(),
                    Arc::new(GatewayServerState {
                        server,
                        local_environment,
                        remote_authentication,
                        remote_secret_headers,
                        connection: Mutex::new(None),
                        snapshot: Mutex::new(None),
                        resource_snapshot: Mutex::new(None),
                        prompt_snapshot: Mutex::new(None),
                    }),
                )
            })
            .collect();
        let cursor_scope = gateway_cursor_scope(facade.host_id());
        let events = Arc::new(GatewayEvents::default());
        let progress = Arc::new(ProgressRoutes::new(
            limits.max_output_buffer_bytes,
            overflow,
        ));
        Self {
            request_slots: Arc::new(Semaphore::new(limits.max_concurrent_requests)),
            limits,
            servers,
            cursor_scope,
            allowed_tools,
            events,
            progress,
            pending_tool_list_notification: AtomicBool::new(false),
            pending_resource_list_notification: AtomicBool::new(false),
            pending_prompt_list_notification: AtomicBool::new(false),
            snapshot: Mutex::new(None),
            resource_aggregate: Mutex::new(None),
            prompt_snapshot: Mutex::new(None),
            next_snapshot_id: AtomicU64::new(1),
            next_resource_snapshot_id: AtomicU64::new(1),
            next_prompt_snapshot_id: AtomicU64::new(1),
        }
    }

    pub(super) fn max_message_bytes(&self) -> usize {
        self.limits.max_message_bytes
    }

    pub(super) fn try_request_slot(&self) -> Result<OwnedSemaphorePermit, McpError> {
        self.request_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| McpError::internal_error("MCP Gateway is busy", None))
    }

    pub(super) fn discovery_seconds(&self) -> u64 {
        self.servers
            .values()
            .map(|server| selected_server_limits(&server.server).discovery_seconds)
            .max()
            .unwrap_or(30)
    }

    pub(super) fn operation_seconds(&self) -> u64 {
        self.servers
            .values()
            .map(|server| selected_server_limits(&server.server).operation_seconds)
            .max()
            .unwrap_or(120)
    }

    pub(super) fn operation_seconds_for(&self, server_id: &str) -> Option<u64> {
        self.servers
            .get(server_id)
            .map(|server| selected_server_limits(&server.server).operation_seconds)
    }

    pub(super) async fn set_host_peer(&self, peer: Peer<RoleServer>) {
        self.events.set_host_peer(peer).await;
    }

    pub(super) async fn watch_tool_list_changes(self: Arc<Self>, cancellation: CancellationToken) {
        loop {
            tokio::select! {
                _ = self.events.tools_changed.notified() => {}
                _ = cancellation.cancelled() => return,
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                _ = cancellation.cancelled() => return,
            }

            let discovery_deadline = Instant::now() + Duration::from_secs(self.discovery_seconds());
            let _ = self
                .tool_snapshot(cancellation.child_token(), discovery_deadline)
                .await;
            if self
                .pending_tool_list_notification
                .swap(false, Ordering::AcqRel)
            {
                self.events.notify_tools_changed().await;
            }
        }
    }

    pub(super) async fn watch_resource_list_changes(
        self: Arc<Self>,
        cancellation: CancellationToken,
    ) {
        loop {
            tokio::select! {
                _ = self.events.resources_changed.notified() => {}
                _ = cancellation.cancelled() => return,
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                _ = cancellation.cancelled() => return,
            }

            let discovery_deadline = Instant::now() + Duration::from_secs(self.discovery_seconds());
            let _ = self
                .resource_snapshot(cancellation.child_token(), discovery_deadline)
                .await;
            if self
                .pending_resource_list_notification
                .swap(false, Ordering::AcqRel)
            {
                self.events.notify_resources_changed().await;
            }
        }
    }

    pub(super) async fn watch_prompt_list_changes(
        self: Arc<Self>,
        cancellation: CancellationToken,
    ) {
        loop {
            tokio::select! {
                _ = self.events.prompts_changed.notified() => {}
                _ = cancellation.cancelled() => return,
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                _ = cancellation.cancelled() => return,
            }

            let discovery_deadline = Instant::now() + Duration::from_secs(self.discovery_seconds());
            let _ = self
                .prompt_snapshot(cancellation.child_token(), discovery_deadline)
                .await;
            if self
                .pending_prompt_list_notification
                .swap(false, Ordering::AcqRel)
            {
                self.events.notify_prompts_changed().await;
            }
        }
    }

    pub(super) async fn connection(
        &self,
        server_id: &str,
        cancellation: CancellationToken,
        request_deadline: Instant,
    ) -> Result<Arc<ConnectedGatewayServer>, McpError> {
        let server = self.servers.get(server_id).ok_or_else(|| {
            McpError::new(
                ErrorCode::METHOD_NOT_FOUND,
                "unknown selected MCP server",
                None,
            )
        })?;
        let mut current = tokio::select! {
            result = tokio::time::timeout_at(request_deadline, server.connection.lock()) => {
                result.map_err(|_| McpError::internal_error("MCP server discovery timed out", None))?
            }
            _ = cancellation.cancelled() => {
                return Err(McpError::new(ErrorCode(-32800), "MCP request was cancelled", None));
            }
        };
        if let Some(connection) = current.as_ref()
            && !connection.is_closed().await
        {
            return Ok(Arc::clone(connection));
        }
        if let Some(stale) = current.take() {
            let _ = tokio::time::timeout_at(request_deadline, stale.shutdown()).await;
        }

        let connection = ConnectedGatewayServer::connect(
            server.server.clone(),
            ConnectionContext {
                local_environment: server.local_environment.clone(),
                remote_authentication: &server.remote_authentication,
                remote_secret_headers: &server.remote_secret_headers,
            },
            Duration::from_secs(self.limits.output_stall_seconds),
            Arc::clone(&self.events),
            Arc::clone(&self.progress),
            request_deadline,
            cancellation,
        )
        .await?;
        *current = Some(Arc::clone(&connection));
        for (upstream_uri, _) in self.events.subscriptions_for_server(server_id).await {
            if connection
                .subscribe(
                    upstream_uri.clone(),
                    request_deadline,
                    CancellationToken::new(),
                )
                .await
                .is_ok()
            {
                self.events
                    .notify_resource_updated(server_id, &upstream_uri)
                    .await;
            } else {
                eprintln!(
                    "aifuel: could not restore resource subscription for server {server_id:?}"
                );
            }
        }
        Ok(connection)
    }

    pub(super) async fn shutdown(&self) {
        let connections = join_all(
            self.servers
                .values()
                .map(|server| async { server.connection.lock().await.take() }),
        )
        .await;
        join_all(
            connections
                .into_iter()
                .flatten()
                .map(|connection| async move {
                    connection.shutdown().await;
                }),
        )
        .await;
    }
}

static NEXT_GATEWAY_SCOPE_ID: AtomicU64 = AtomicU64::new(1);

fn gateway_cursor_scope(host_id: &str) -> String {
    let started_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = NEXT_GATEWAY_SCOPE_ID.fetch_add(1, Ordering::Relaxed);
    format!("{host_id}-{}-{started_at}-{sequence}", std::process::id())
}

fn selected_server_limits(server: &SelectedMcpServer) -> &ServerLimits {
    match &server.definition {
        McpServerDefinition::Stdio(config) => &config.limits,
        McpServerDefinition::StreamableHttp(config) => &config.limits,
    }
}

#[cfg(test)]
mod tests {
    use super::GatewayState;
    use aifuel_app::McpGatewayFacade;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::time::Instant;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn tool_snapshot_wait_respects_its_absolute_deadline() {
        let catalog = br#"{
            "servers":{"local":{"transport":"stdio","command":"unused"}},
            "defaults":["local"]
        }"#;
        let facade = McpGatewayFacade::from_json(catalog, "codex", PathBuf::from("/home/test"))
            .expect("the test catalog should be valid");
        let state = Arc::new(GatewayState::new(facade, CancellationToken::new(), None));
        let _snapshot_guard = state.snapshot.lock().await;
        let deadline = Instant::now() + Duration::from_millis(20);

        let error = match state
            .tool_snapshot(CancellationToken::new(), deadline)
            .await
        {
            Ok(_) => panic!("waiting for the snapshot lock should hit the deadline"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("discovery timed out"));
    }
}

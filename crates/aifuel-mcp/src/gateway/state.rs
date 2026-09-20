use super::connection::{
    ConnectedGatewayServer, ConnectionContext, ResolvedRemoteAuthentication,
    ResolvedRemoteSecretHeaders, snapshot_remote_authentication, snapshot_remote_secret_headers,
};
use super::process::{ResolvedEnvironment, snapshot_environment};
use super::progress::{GatewayEvents, ProgressRoutes};
use super::resources::{ResourceSnapshot, make_snapshot as make_resource_snapshot};
use super::snapshot::{ServerToolSnapshot, SnapshotBuildError, ToolSnapshot, make_server_snapshot};
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
    pub(super) events: Arc<GatewayEvents>,
    pub(super) progress: Arc<ProgressRoutes>,
    pending_tool_list_notification: AtomicBool,
    pending_resource_list_notification: AtomicBool,
    request_slots: Arc<Semaphore>,
    snapshot: Mutex<Option<Arc<ToolSnapshot>>>,
    resource_aggregate: Mutex<Option<Arc<ResourceSnapshot>>>,
    next_snapshot_id: AtomicU64,
    next_resource_snapshot_id: AtomicU64,
}

struct GatewayServerState {
    server: SelectedMcpServer,
    local_environment: Option<ResolvedEnvironment>,
    remote_authentication: ResolvedRemoteAuthentication,
    remote_secret_headers: Result<ResolvedRemoteSecretHeaders, &'static str>,
    connection: Mutex<Option<Arc<ConnectedGatewayServer>>>,
    snapshot: Mutex<Option<Arc<ServerToolSnapshot>>>,
    resource_snapshot: Mutex<Option<Arc<ResourceSnapshot>>>,
}

enum ServerSnapshotError {
    Retryable(McpError),
    IdentityConflict(McpError),
}

impl GatewayState {
    pub(super) fn new(facade: McpGatewayFacade, overflow: CancellationToken) -> Self {
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
            events,
            progress,
            pending_tool_list_notification: AtomicBool::new(false),
            pending_resource_list_notification: AtomicBool::new(false),
            snapshot: Mutex::new(None),
            resource_aggregate: Mutex::new(None),
            next_snapshot_id: AtomicU64::new(1),
            next_resource_snapshot_id: AtomicU64::new(1),
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
                    "aifuel: could not restore resource subscription for server {:?}",
                    server_id
                );
            }
        }
        Ok(connection)
    }

    pub(super) async fn resource_snapshot(
        &self,
        cancellation: CancellationToken,
        operation_deadline: Instant,
    ) -> Result<Arc<ResourceSnapshot>, McpError> {
        if self.servers.is_empty() {
            return Ok(Arc::new(ResourceSnapshot::empty()));
        }
        let mut current = tokio::select! {
            result = tokio::time::timeout_at(operation_deadline, self.resource_aggregate.lock()) => {
                result.map_err(|_| McpError::internal_error("MCP server discovery timed out", None))?
            }
            _ = cancellation.cancelled() => {
                return Err(McpError::new(ErrorCode(-32800), "MCP request was cancelled", None));
            }
        };
        let refresh = self.events.resources_dirty.swap(false, Ordering::AcqRel);
        if !refresh && let Some(snapshot) = current.as_ref() {
            return Ok(Arc::clone(snapshot));
        }

        let server_ids = self.servers.keys().cloned().collect::<Vec<_>>();
        let results = join_all(server_ids.iter().cloned().map(|server_id| {
            let cancellation = cancellation.clone();
            async move {
                let result = self
                    .server_resource_snapshot(&server_id, refresh, cancellation, operation_deadline)
                    .await;
                (server_id, result)
            }
        }))
        .await;
        if cancellation.is_cancelled() {
            return Err(McpError::new(
                ErrorCode(-32800),
                "MCP request was cancelled",
                None,
            ));
        }

        let mut snapshots = Vec::new();
        let mut first_error = None;
        for (_, result) in results {
            match result {
                Ok(snapshot) => snapshots.push(snapshot),
                Err(ServerSnapshotError::Retryable(error)) => {
                    first_error.get_or_insert(error);
                }
                Err(ServerSnapshotError::IdentityConflict(error)) => {
                    self.events.resources_dirty.store(true, Ordering::Release);
                    return Err(error);
                }
            }
        }
        if snapshots.is_empty() {
            return Err(first_error.unwrap_or_else(|| {
                McpError::internal_error(
                    "no selected MCP server returned a usable resources/list snapshot",
                    None,
                )
            }));
        }

        let snapshot = match super::resources::aggregate(
            &self.cursor_scope,
            &snapshots,
            self.next_resource_snapshot_id
                .fetch_add(1, Ordering::Relaxed),
        ) {
            Ok(snapshot) => Arc::new(snapshot),
            Err(error) => {
                self.events.resources_dirty.store(true, Ordering::Release);
                return Err(error);
            }
        };
        if let Some(cached) = current.as_ref()
            && cached.resources == snapshot.resources
            && cached.templates == snapshot.templates
            && cached.resource_routes == snapshot.resource_routes
        {
            return Ok(Arc::clone(cached));
        }
        if current.is_some() {
            self.pending_resource_list_notification
                .store(true, Ordering::Release);
        }
        *current = Some(Arc::clone(&snapshot));
        Ok(snapshot)
    }

    async fn server_resource_snapshot(
        &self,
        server_id: &str,
        refresh: bool,
        cancellation: CancellationToken,
        operation_deadline: Instant,
    ) -> Result<Arc<ResourceSnapshot>, ServerSnapshotError> {
        let server = self.servers.get(server_id).expect("selected server exists");
        let mut current = tokio::select! {
            result = tokio::time::timeout_at(operation_deadline, server.resource_snapshot.lock()) => {
                result.map_err(|_| ServerSnapshotError::Retryable(McpError::internal_error(
                    "MCP server discovery timed out",
                    None,
                )))?
            }
            _ = cancellation.cancelled() => {
                return Err(ServerSnapshotError::Retryable(McpError::new(
                    ErrorCode(-32800),
                    "MCP request was cancelled",
                    None,
                )));
            }
        };
        if !refresh && let Some(snapshot) = current.as_ref() {
            return Ok(Arc::clone(snapshot));
        }

        let limits = selected_server_limits(&server.server);
        let discovery_deadline = Instant::now()
            + Duration::from_secs(limits.discovery_seconds)
                .min(operation_deadline.saturating_duration_since(Instant::now()));
        let result = async {
            let connection = self
                .connection(server_id, cancellation.clone(), discovery_deadline)
                .await
                .map_err(ServerSnapshotError::Retryable)?;
            let _request_permit = connection
                .try_request_slot()
                .map_err(ServerSnapshotError::Retryable)?;
            let resources = connection
                .list_resources(discovery_deadline, cancellation.clone())
                .await
                .map_err(ServerSnapshotError::Retryable)?;
            let templates = connection
                .list_resource_templates(discovery_deadline, cancellation)
                .await
                .map_err(ServerSnapshotError::Retryable)?;
            Ok::<_, ServerSnapshotError>((resources, templates))
        }
        .await;

        let result = match result {
            Ok((resources, templates)) => {
                match make_resource_snapshot(
                    &server.server,
                    resources,
                    templates,
                    limits,
                    self.next_resource_snapshot_id.load(Ordering::Relaxed),
                ) {
                    Ok(snapshot) => Ok(Arc::new(snapshot)),
                    Err(error) => Err(ServerSnapshotError::IdentityConflict(error)),
                }
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(snapshot) => {
                if let Some(cached) = current.as_ref()
                    && cached.resources == snapshot.resources
                    && cached.templates == snapshot.templates
                {
                    return Ok(Arc::clone(cached));
                }
                *current = Some(Arc::clone(&snapshot));
                Ok(snapshot)
            }
            Err(ServerSnapshotError::Retryable(_error)) if current.is_some() => {
                self.events.resources_dirty.store(true, Ordering::Release);
                eprintln!(
                    "aifuel: selected MCP server {server_id:?} is unavailable; retaining its last resource list"
                );
                Ok(Arc::clone(current.as_ref().expect("snapshot exists")))
            }
            Err(ServerSnapshotError::Retryable(error)) => {
                self.events.resources_dirty.store(true, Ordering::Release);
                eprintln!("aifuel: selected MCP server {server_id:?} is unavailable");
                Err(ServerSnapshotError::Retryable(error))
            }
            Err(error @ ServerSnapshotError::IdentityConflict(_)) => Err(error),
        }
    }

    pub(super) async fn tool_snapshot(
        &self,
        cancellation: CancellationToken,
        operation_deadline: Instant,
    ) -> Result<Arc<ToolSnapshot>, McpError> {
        if self.servers.is_empty() {
            return Ok(Arc::new(ToolSnapshot::empty_for_scope(
                &self.cursor_scope,
                0,
            )));
        }
        let mut current = tokio::select! {
            result = tokio::time::timeout_at(operation_deadline, self.snapshot.lock()) => {
                result.map_err(|_| McpError::internal_error("MCP server discovery timed out", None))?
            }
            _ = cancellation.cancelled() => {
                return Err(McpError::new(ErrorCode(-32800), "MCP request was cancelled", None));
            }
        };
        let refresh = self.events.tools_dirty.swap(false, Ordering::AcqRel);
        if !refresh && let Some(snapshot) = current.as_ref() {
            return Ok(Arc::clone(snapshot));
        }
        let server_ids = self.servers.keys().cloned().collect::<Vec<_>>();
        let results = join_all(server_ids.iter().cloned().map(|server_id| {
            let cancellation = cancellation.clone();
            async move {
                let result = self
                    .server_snapshot(&server_id, refresh, cancellation, operation_deadline)
                    .await;
                (server_id, result)
            }
        }))
        .await;
        if cancellation.is_cancelled() {
            return Err(McpError::new(
                ErrorCode(-32800),
                "MCP request was cancelled",
                None,
            ));
        }

        let mut snapshots = Vec::new();
        let mut first_error = None;
        for (_, result) in results {
            match result {
                Ok(snapshot) => snapshots.push(snapshot),
                Err(ServerSnapshotError::Retryable(error)) => {
                    first_error.get_or_insert(error);
                }
                Err(ServerSnapshotError::IdentityConflict(error)) => {
                    self.events.tools_dirty.store(true, Ordering::Release);
                    return Err(error);
                }
            }
        }
        if snapshots.is_empty() {
            return Err(first_error.unwrap_or_else(|| {
                McpError::internal_error(
                    "no selected MCP server returned a usable tools/list snapshot",
                    None,
                )
            }));
        }

        let snapshot = match ToolSnapshot::aggregate(
            &self.cursor_scope,
            &snapshots,
            self.next_snapshot_id.fetch_add(1, Ordering::Relaxed),
        ) {
            Ok(snapshot) => Arc::new(snapshot),
            Err(error) => {
                self.events.tools_dirty.store(true, Ordering::Release);
                return Err(error);
            }
        };
        if let Some(cached) = current.as_ref()
            && cached.tools == snapshot.tools
            && cached.routes == snapshot.routes
        {
            return Ok(Arc::clone(cached));
        }
        if current.is_some() {
            self.pending_tool_list_notification
                .store(true, Ordering::Release);
        }
        *current = Some(Arc::clone(&snapshot));
        Ok(snapshot)
    }

    async fn server_snapshot(
        &self,
        server_id: &str,
        refresh: bool,
        cancellation: CancellationToken,
        operation_deadline: Instant,
    ) -> Result<Arc<ServerToolSnapshot>, ServerSnapshotError> {
        let server = self.servers.get(server_id).expect("selected server exists");
        let mut current = tokio::select! {
            result = tokio::time::timeout_at(operation_deadline, server.snapshot.lock()) => {
                result.map_err(|_| ServerSnapshotError::Retryable(McpError::internal_error(
                    "MCP server discovery timed out",
                    None,
                )))?
            }
            _ = cancellation.cancelled() => {
                return Err(ServerSnapshotError::Retryable(McpError::new(
                    ErrorCode(-32800),
                    "MCP request was cancelled",
                    None,
                )));
            }
        };
        if !refresh && let Some(snapshot) = current.as_ref() {
            return Ok(Arc::clone(snapshot));
        }

        let limits = selected_server_limits(&server.server);
        let discovery_deadline = Instant::now()
            + Duration::from_secs(limits.discovery_seconds)
                .min(operation_deadline.saturating_duration_since(Instant::now()));
        let result = async {
            let connection = self
                .connection(server_id, cancellation.clone(), discovery_deadline)
                .await
                .map_err(ServerSnapshotError::Retryable)?;
            let _request_permit = connection
                .try_request_slot()
                .map_err(ServerSnapshotError::Retryable)?;
            connection
                .list_tools(discovery_deadline, cancellation)
                .await
                .map_err(ServerSnapshotError::Retryable)
        }
        .await;

        let result = match result {
            Ok(upstream_tools) => {
                match make_server_snapshot(&server.server, upstream_tools, limits) {
                    Ok(snapshot) => Ok(Arc::new(snapshot)),
                    Err(SnapshotBuildError::IdentityConflict(error)) => {
                        self.events.tools_dirty.store(true, Ordering::Release);
                        return Err(ServerSnapshotError::IdentityConflict(error));
                    }
                    Err(SnapshotBuildError::Invalid(error)) => {
                        Err(ServerSnapshotError::Retryable(error))
                    }
                }
            }
            Err(error) => Err(error),
        };

        match result {
            Ok(snapshot) => {
                if let Some(cached) = current.as_ref()
                    && cached.tools == snapshot.tools
                    && cached.routes == snapshot.routes
                {
                    return Ok(Arc::clone(cached));
                }
                *current = Some(Arc::clone(&snapshot));
                Ok(snapshot)
            }
            Err(ServerSnapshotError::Retryable(_error)) if current.is_some() => {
                self.events.tools_dirty.store(true, Ordering::Release);
                eprintln!(
                    "aifuel: selected MCP server {server_id:?} is unavailable; retaining its last tool list"
                );
                Ok(Arc::clone(current.as_ref().expect("snapshot exists")))
            }
            Err(ServerSnapshotError::Retryable(error)) => {
                self.events.tools_dirty.store(true, Ordering::Release);
                eprintln!("aifuel: selected MCP server {server_id:?} is unavailable");
                Err(ServerSnapshotError::Retryable(error))
            }
            Err(error @ ServerSnapshotError::IdentityConflict(_)) => Err(error),
        }
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
        let state = Arc::new(GatewayState::new(facade, CancellationToken::new()));
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

use super::connection::ConnectedGatewayServer;
use super::process::{ResolvedEnvironment, snapshot_environment};
use super::progress::{GatewayEvents, ProgressRoutes};
use super::snapshot::{ToolSnapshot, make_snapshot};
use aifuel_app::{GatewayLimits, McpGatewayFacade, McpServerDefinition, ServerLimits};
use rmcp::model::{ErrorCode, ErrorData as McpError};
use rmcp::service::{Peer, RoleServer};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub(super) struct GatewayState {
    facade: McpGatewayFacade,
    limits: GatewayLimits,
    process_environments: HashMap<String, ResolvedEnvironment>,
    events: Arc<GatewayEvents>,
    pub(super) progress: Arc<ProgressRoutes>,
    pending_tool_list_notification: AtomicBool,
    request_slots: Arc<Semaphore>,
    connection: Mutex<Option<Arc<ConnectedGatewayServer>>>,
    snapshot: Mutex<Option<Arc<ToolSnapshot>>>,
    next_snapshot_id: AtomicU64,
}

impl GatewayState {
    pub(super) fn new(facade: McpGatewayFacade, overflow: CancellationToken) -> Self {
        let limits = facade.gateway_limits().clone();
        let process_environments = facade
            .selected_servers()
            .iter()
            .filter(|server| matches!(&server.definition, McpServerDefinition::Stdio(_)))
            .map(|server| (server.id.clone(), snapshot_environment(server)))
            .collect();
        let events = Arc::new(GatewayEvents::default());
        let progress = Arc::new(ProgressRoutes::new(
            limits.max_output_buffer_bytes,
            overflow,
        ));
        Self {
            facade,
            request_slots: Arc::new(Semaphore::new(limits.max_concurrent_requests)),
            limits,
            process_environments,
            events,
            progress,
            pending_tool_list_notification: AtomicBool::new(false),
            connection: Mutex::new(None),
            snapshot: Mutex::new(None),
            next_snapshot_id: AtomicU64::new(1),
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

    pub(super) fn selected_limits(&self) -> Option<&ServerLimits> {
        self.facade
            .selected_servers()
            .first()
            .map(|server| match &server.definition {
                McpServerDefinition::Stdio(config) => &config.limits,
                McpServerDefinition::StreamableHttp(config) => &config.limits,
            })
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

            let discovery_deadline = Instant::now()
                + Duration::from_secs(
                    self.selected_limits()
                        .map_or(30, |limits| limits.discovery_seconds),
                );
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

    pub(super) async fn connection(
        &self,
        cancellation: CancellationToken,
        request_deadline: Instant,
    ) -> Result<Option<Arc<ConnectedGatewayServer>>, McpError> {
        let Some(server) = self.facade.selected_servers().first().cloned() else {
            return Ok(None);
        };
        let mut current = tokio::select! {
            result = tokio::time::timeout_at(request_deadline, self.connection.lock()) => {
                result.map_err(|_| McpError::internal_error("MCP server connection timed out", None))?
            }
            _ = cancellation.cancelled() => {
                return Err(McpError::new(ErrorCode(-32800), "MCP request was cancelled", None));
            }
        };
        if let Some(connection) = current.as_ref()
            && !connection.is_closed().await
        {
            return Ok(Some(Arc::clone(connection)));
        }
        if let Some(stale) = current.take() {
            let _ = tokio::time::timeout_at(request_deadline, stale.shutdown()).await;
        }

        let process_environment = self.process_environments.get(&server.id).cloned();
        let connection = ConnectedGatewayServer::connect(
            server,
            process_environment,
            Duration::from_secs(self.limits.output_stall_seconds),
            Arc::clone(&self.events),
            Arc::clone(&self.progress),
            request_deadline,
            cancellation,
        )
        .await?;
        *current = Some(Arc::clone(&connection));
        Ok(Some(connection))
    }

    pub(super) async fn tool_snapshot(
        &self,
        cancellation: CancellationToken,
        operation_deadline: Instant,
    ) -> Result<Arc<ToolSnapshot>, McpError> {
        if self.facade.selected_servers().is_empty() {
            return Ok(Arc::new(ToolSnapshot::empty()));
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
        let server = self.facade.selected_servers()[0].clone();
        let limits = self.selected_limits().cloned().unwrap_or_default();
        let discovery_deadline = Instant::now()
            + Duration::from_secs(limits.discovery_seconds)
                .min(operation_deadline.saturating_duration_since(Instant::now()));
        let result = async {
            let connection = self
                .connection(cancellation.clone(), discovery_deadline)
                .await?
                .ok_or_else(|| McpError::internal_error("no MCP server is selected", None))?;
            let _request_permit = connection.try_request_slot()?;
            connection
                .list_tools(discovery_deadline, cancellation)
                .await
        }
        .await;

        match result {
            Ok(upstream_tools) => {
                let snapshot = Arc::new(make_snapshot(
                    &server,
                    upstream_tools,
                    &limits,
                    self.next_snapshot_id.fetch_add(1, Ordering::Relaxed),
                )?);
                if let Some(current) = current.as_ref() {
                    if current.tools == snapshot.tools {
                        return Ok(Arc::clone(current));
                    }
                    self.pending_tool_list_notification
                        .store(true, Ordering::Release);
                }
                *current = Some(Arc::clone(&snapshot));
                Ok(snapshot)
            }
            Err(_error) if current.is_some() => {
                self.events.tools_dirty.store(true, Ordering::Release);
                eprintln!(
                    "aifuel: selected MCP server is unavailable; retaining its last tool list"
                );
                Ok(Arc::clone(current.as_ref().expect("snapshot exists")))
            }
            Err(error) => Err(error),
        }
    }

    pub(super) async fn shutdown(&self) {
        let deadline = Instant::now()
            + Duration::from_secs(
                self.selected_limits()
                    .map_or(5, |limits| limits.shutdown_seconds),
            );
        let Ok(mut connection) = tokio::time::timeout_at(deadline, self.connection.lock()).await
        else {
            return;
        };
        if let Some(connection) = connection.take() {
            let _ = tokio::time::timeout_at(deadline, connection.shutdown()).await;
        }
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

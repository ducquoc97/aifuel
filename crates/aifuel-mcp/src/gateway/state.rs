use super::process::{LocalProcess, ResolvedEnvironment, SpawnedLocalServer, snapshot_environment};
use super::progress::{GatewayEvents, GatewayUpstreamHandler, ProgressRoutes};
use super::request::{UpstreamRequestError, await_server_result};
use super::snapshot::{ToolSnapshot, make_snapshot};
use aifuel_app::{GatewayLimits, McpGatewayFacade, McpServerDefinition, ServerLimits};
use rmcp::ServiceExt;
use rmcp::model::{
    ClientRequest, ErrorCode, ErrorData as McpError, ListToolsRequest, PaginatedRequestParams,
    ServerResult, Tool,
};
use rmcp::service::{Peer, PeerRequestOptions, RoleClient, RoleServer, RunningService};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

type UpstreamService = RunningService<RoleClient, GatewayUpstreamHandler>;

pub(super) struct GatewayState {
    facade: McpGatewayFacade,
    limits: GatewayLimits,
    process_environments: HashMap<String, ResolvedEnvironment>,
    events: Arc<GatewayEvents>,
    pub(super) progress: Arc<ProgressRoutes>,
    pending_tool_list_notification: AtomicBool,
    request_slots: Arc<Semaphore>,
    connection: Mutex<Option<Arc<LocalMcpConnection>>>,
    snapshot: Mutex<Option<Arc<ToolSnapshot>>>,
    next_snapshot_id: AtomicU64,
}

impl GatewayState {
    pub(super) fn new(facade: McpGatewayFacade, overflow: CancellationToken) -> Self {
        let limits = facade.gateway_limits().clone();
        let process_environments = facade
            .selected_servers()
            .iter()
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
    ) -> Result<Option<Arc<LocalMcpConnection>>, McpError> {
        let Some(server) = self.facade.selected_servers().first().cloned() else {
            return Ok(None);
        };
        if matches!(&server.definition, McpServerDefinition::StreamableHttp(_)) {
            return Err(McpError::internal_error(
                "selected server requires the remote Streamable HTTP transport",
                None,
            ));
        }

        let mut current = tokio::select! {
            result = tokio::time::timeout_at(request_deadline, self.connection.lock()) => {
                result.map_err(|_| McpError::internal_error("local MCP server discovery timed out", None))?
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

        let limits = self.selected_limits().cloned().unwrap_or_default();
        let process_output_budget = Arc::new(Semaphore::new(limits.max_message_bytes + 1));
        let process_environment = self
            .process_environments
            .get(&server.id)
            .cloned()
            .expect("selected server has a startup environment snapshot");
        let spawned = SpawnedLocalServer::spawn(
            &server,
            process_environment,
            process_output_budget,
            limits.max_message_bytes,
            Duration::from_secs(self.limits.output_stall_seconds),
        )
        .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        let SpawnedLocalServer {
            transport,
            mut process,
        } = spawned;
        let handler = GatewayUpstreamHandler {
            events: Arc::clone(&self.events),
            progress: Arc::clone(&self.progress),
        };
        let connect_deadline = Instant::now()
            + Duration::from_secs(limits.connect_seconds)
                .min(request_deadline.saturating_duration_since(Instant::now()));
        let service = tokio::select! {
            result = tokio::time::timeout_at(connect_deadline, handler.serve(transport)) => {
                match result {
                    Ok(Ok(service)) => service,
                    Ok(Err(_)) => {
                        process.shutdown(Duration::from_secs(limits.shutdown_seconds)).await;
                        return Err(McpError::internal_error(
                            "configured local MCP server failed protocol initialization",
                            None,
                        ));
                    }
                    Err(_) => {
                        process.shutdown(Duration::from_secs(limits.shutdown_seconds)).await;
                        return Err(McpError::internal_error(
                            "configured local MCP server initialization timed out",
                            None,
                        ));
                    }
                }
            }
            _ = cancellation.cancelled() => {
                process.shutdown(Duration::from_secs(limits.shutdown_seconds)).await;
                return Err(McpError::new(ErrorCode(-32800), "MCP request was cancelled", None));
            }
        };
        let compatible = service.peer_info().is_some_and(|info| {
            info.protocol_version == rmcp::model::ProtocolVersion::V_2025_11_25
        });
        if !compatible {
            let mut service = service;
            let _ = service.close().await;
            process
                .shutdown(Duration::from_secs(limits.shutdown_seconds))
                .await;
            return Err(McpError::internal_error(
                "configured local MCP server does not support protocol version 2025-11-25",
                None,
            ));
        }

        let connection = Arc::new(LocalMcpConnection {
            peer: service.peer().clone(),
            service: Mutex::new(Some(service)),
            process: Mutex::new(Some(process)),
            limits,
            request_slots: Arc::new(Semaphore::new(
                self.selected_limits()
                    .map_or(8, |limits| limits.max_concurrent_requests),
            )),
        });
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
                result.map_err(|_| McpError::internal_error("local MCP server discovery timed out", None))?
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
                    "aifuel: selected local MCP server is unavailable; retaining its last tool list"
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

pub(super) struct LocalMcpConnection {
    pub(super) peer: Peer<RoleClient>,
    service: Mutex<Option<UpstreamService>>,
    process: Mutex<Option<LocalProcess>>,
    limits: ServerLimits,
    request_slots: Arc<Semaphore>,
}

impl LocalMcpConnection {
    pub(super) fn try_request_slot(&self) -> Result<OwnedSemaphorePermit, McpError> {
        self.request_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| McpError::internal_error("selected local MCP server is busy", None))
    }

    async fn is_closed(&self) -> bool {
        self.service
            .lock()
            .await
            .as_ref()
            .is_none_or(RunningService::is_closed)
    }

    async fn list_tools(
        &self,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> Result<Vec<Tool>, McpError> {
        let mut tools = Vec::new();
        let mut cursor = None;
        let mut cursors = HashSet::new();
        loop {
            let params = PaginatedRequestParams::default().with_cursor(cursor.clone());
            let request = ClientRequest::ListToolsRequest(ListToolsRequest::with_param(params));
            let response = self
                .request(request, deadline, cancellation.clone())
                .await?;
            let ServerResult::ListToolsResult(page) = response else {
                return Err(McpError::internal_error(
                    "local MCP server returned an unexpected tools/list response",
                    None,
                ));
            };
            tools.extend(page.tools);
            if tools.len() > self.limits.max_list_entries {
                return Err(McpError::internal_error(
                    "local MCP server exceeded the configured tool count limit",
                    None,
                ));
            }
            let bytes = serde_json::to_vec(&tools)
                .map_err(|_| McpError::internal_error("could not encode MCP tool list", None))?;
            if bytes.len() > self.limits.max_list_snapshot_bytes {
                return Err(McpError::internal_error(
                    "local MCP server exceeded the configured tool snapshot limit",
                    None,
                ));
            }
            cursor = page.next_cursor;
            let Some(next) = cursor.as_ref() else {
                return Ok(tools);
            };
            if !cursors.insert(next.clone()) {
                return Err(McpError::internal_error(
                    "local MCP server returned a repeated tools/list cursor",
                    None,
                ));
            }
        }
    }

    async fn request(
        &self,
        request: ClientRequest,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> Result<ServerResult, McpError> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(McpError::internal_error(
                "local MCP server discovery timed out",
                None,
            ));
        }
        let mut request_options = PeerRequestOptions::no_options();
        request_options.timeout = Some(remaining);
        let handle = match tokio::time::timeout_at(
            deadline,
            self.peer.send_cancellable_request(request, request_options),
        )
        .await
        {
            Ok(Ok(handle)) => handle,
            Ok(Err(_)) | Err(_) => {
                return Err(McpError::internal_error(
                    "local MCP server discovery timed out or disconnected",
                    None,
                ));
            }
        };
        await_server_result(handle, deadline, cancellation)
            .await
            .map_err(|error| match error {
                UpstreamRequestError::Protocol(error) => error,
                UpstreamRequestError::Cancelled => McpError::new(
                    ErrorCode(-32800),
                    "MCP server discovery was cancelled",
                    None,
                ),
                UpstreamRequestError::TimedOut => {
                    McpError::internal_error("local MCP server discovery timed out", None)
                }
                UpstreamRequestError::Disconnected => {
                    McpError::internal_error("local MCP server disconnected during discovery", None)
                }
            })
    }

    async fn shutdown(&self) {
        let deadline = Instant::now() + Duration::from_secs(self.limits.shutdown_seconds);
        if let Ok(mut service) = tokio::time::timeout_at(deadline, self.service.lock()).await
            && let Some(mut service) = service.take()
        {
            let _ = tokio::time::timeout_at(deadline, service.close()).await;
        }
        if let Ok(mut process) = tokio::time::timeout_at(deadline, self.process.lock()).await
            && let Some(mut process) = process.take()
        {
            process
                .shutdown(deadline.saturating_duration_since(Instant::now()))
                .await;
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

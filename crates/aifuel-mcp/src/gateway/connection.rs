use super::process::{LocalProcess, ResolvedEnvironment, SpawnedLocalServer};
use super::progress::{GatewayEvents, GatewayUpstreamHandler, ProgressRoutes};
use super::remote_endpoint::connect_endpoint;
use super::remote_transport::{RemoteHttpTransport, RemoteSession};
use super::request::{UpstreamRequestError, await_server_result};
use aifuel_app::{McpServerDefinition, SelectedMcpServer, ServerLimits};
use rmcp::ServiceExt;
use rmcp::model::{
    ClientRequest, ErrorCode, ErrorData as McpError, ListToolsRequest, PaginatedRequestParams,
    ServerResult, Tool,
};
use rmcp::service::{Peer, PeerRequestOptions, RoleClient, RunningService};
use rmcp::transport::Transport;
use std::env;
use std::io;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

type UpstreamService = RunningService<RoleClient, GatewayUpstreamHandler>;

enum ConnectionOwner {
    Local(LocalProcess),
    Remote(RemoteSession),
}

/// A startup snapshot of the only remote credential supported by this slice.
///
/// The token is intentionally kept out of `Debug` output and error messages.
pub(super) enum ResolvedRemoteAuthentication {
    Unauthenticated,
    BearerToken(String),
    Missing,
    Empty,
    InvalidEncoding,
}

pub(super) fn snapshot_remote_authentication(
    server: &SelectedMcpServer,
) -> ResolvedRemoteAuthentication {
    let McpServerDefinition::StreamableHttp(config) = &server.definition else {
        return ResolvedRemoteAuthentication::Unauthenticated;
    };
    let Some(auth) = &config.auth else {
        return ResolvedRemoteAuthentication::Unauthenticated;
    };
    match env::var_os(&auth.bearer_token_env) {
        None => ResolvedRemoteAuthentication::Missing,
        Some(value) => match value.to_str() {
            None => ResolvedRemoteAuthentication::InvalidEncoding,
            Some("") => ResolvedRemoteAuthentication::Empty,
            Some(value) => ResolvedRemoteAuthentication::BearerToken(value.to_owned()),
        },
    }
}

pub(super) struct ConnectionContext<'a> {
    pub(super) local_environment: Option<ResolvedEnvironment>,
    pub(super) remote_authentication: &'a ResolvedRemoteAuthentication,
}

impl ConnectionOwner {
    async fn shutdown(&mut self, timeout: Duration) {
        match self {
            Self::Local(process) => process.shutdown(timeout).await,
            Self::Remote(session) => {
                let _ = tokio::time::timeout(timeout, session.shutdown()).await;
            }
        }
    }
}

pub(super) struct ConnectedGatewayServer {
    pub(super) server_id: String,
    pub(super) peer: Peer<RoleClient>,
    pub(super) limits: ServerLimits,
    service: Mutex<Option<UpstreamService>>,
    owner: Mutex<Option<ConnectionOwner>>,
    request_slots: Arc<Semaphore>,
}

impl ConnectedGatewayServer {
    pub(super) async fn connect(
        server: SelectedMcpServer,
        context: ConnectionContext<'_>,
        write_stall: Duration,
        events: Arc<GatewayEvents>,
        progress: Arc<ProgressRoutes>,
        request_deadline: Instant,
        cancellation: CancellationToken,
    ) -> Result<Arc<Self>, McpError> {
        let limits = match &server.definition {
            McpServerDefinition::Stdio(config) => config.limits.clone(),
            McpServerDefinition::StreamableHttp(config) => config.limits.clone(),
        };
        let (service, owner) = match &server.definition {
            McpServerDefinition::Stdio(_) => {
                let environment = context.local_environment.ok_or_else(|| {
                    McpError::internal_error(
                        "configured local MCP server environment was not captured at startup",
                        None,
                    )
                })?;
                let output_budget = Arc::new(Semaphore::new(limits.max_message_bytes + 1));
                let spawned = SpawnedLocalServer::spawn(
                    &server,
                    environment,
                    output_budget,
                    limits.max_message_bytes,
                    write_stall,
                )
                .map_err(|error| McpError::internal_error(error.to_string(), None))?;
                let SpawnedLocalServer { transport, process } = spawned;
                let mut owner = ConnectionOwner::Local(process);
                let service = match initialize_upstream(
                    &server,
                    transport,
                    &limits,
                    Arc::clone(&events),
                    Arc::clone(&progress),
                    request_deadline,
                    cancellation,
                )
                .await
                {
                    Ok(service) => service,
                    Err(error) => {
                        owner
                            .shutdown(Duration::from_secs(limits.shutdown_seconds))
                            .await;
                        return Err(error);
                    }
                };
                (service, owner)
            }
            McpServerDefinition::StreamableHttp(config) => {
                if !config.secret_headers.is_empty() {
                    return Err(McpError::internal_error(
                        "remote MCP named secret headers are not available in this transport slice",
                        None,
                    ));
                }
                let bearer_token = match context.remote_authentication {
                    ResolvedRemoteAuthentication::Unauthenticated => None,
                    ResolvedRemoteAuthentication::BearerToken(token) => Some(token.as_str()),
                    ResolvedRemoteAuthentication::Missing => {
                        return Err(McpError::internal_error(
                            "remote MCP bearer token is missing",
                            None,
                        ));
                    }
                    ResolvedRemoteAuthentication::Empty => {
                        return Err(McpError::internal_error(
                            "remote MCP bearer token is empty",
                            None,
                        ));
                    }
                    ResolvedRemoteAuthentication::InvalidEncoding => {
                        return Err(McpError::internal_error(
                            "remote MCP bearer token is not a valid text credential",
                            None,
                        ));
                    }
                };
                let endpoint = connect_endpoint(
                    &config.url,
                    bearer_token,
                    Duration::from_secs(limits.connect_seconds),
                )
                .await
                .map_err(|error| McpError::internal_error(error, None))?;
                let (transport, session) =
                    RemoteHttpTransport::new(endpoint, server.id.clone(), limits.clone());
                let mut owner = ConnectionOwner::Remote(session);
                let service = match initialize_upstream(
                    &server,
                    transport,
                    &limits,
                    Arc::clone(&events),
                    Arc::clone(&progress),
                    request_deadline,
                    cancellation,
                )
                .await
                {
                    Ok(service) => service,
                    Err(error) => {
                        owner
                            .shutdown(Duration::from_secs(limits.shutdown_seconds))
                            .await;
                        return Err(error);
                    }
                };
                (service, owner)
            }
        };

        Ok(Arc::new(Self {
            server_id: server.id,
            peer: service.peer().clone(),
            service: Mutex::new(Some(service)),
            owner: Mutex::new(Some(owner)),
            request_slots: Arc::new(Semaphore::new(limits.max_concurrent_requests)),
            limits,
        }))
    }

    pub(super) fn try_request_slot(&self) -> Result<OwnedSemaphorePermit, McpError> {
        self.request_slots.clone().try_acquire_owned().map_err(|_| {
            McpError::internal_error(
                format!("selected MCP server {} is busy", self.server_id),
                None,
            )
        })
    }

    pub(super) async fn is_closed(&self) -> bool {
        self.service
            .lock()
            .await
            .as_ref()
            .is_none_or(RunningService::is_closed)
    }

    pub(super) async fn list_tools(
        &self,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> Result<Vec<Tool>, McpError> {
        let mut tools = Vec::new();
        let mut cursor = None;
        let mut cursors = std::collections::HashSet::new();
        loop {
            let params = PaginatedRequestParams::default().with_cursor(cursor.clone());
            let request = ClientRequest::ListToolsRequest(ListToolsRequest::with_param(params));
            let response = self
                .request(request, deadline, cancellation.clone())
                .await?;
            let ServerResult::ListToolsResult(page) = response else {
                return Err(McpError::internal_error(
                    "upstream MCP server returned an unexpected tools/list response",
                    None,
                ));
            };
            tools.extend(page.tools);
            if tools.len() > self.limits.max_list_entries {
                return Err(McpError::internal_error(
                    "upstream MCP server exceeded the configured tool count limit",
                    None,
                ));
            }
            let bytes = serde_json::to_vec(&tools)
                .map_err(|_| McpError::internal_error("could not encode MCP tool list", None))?;
            if bytes.len() > self.limits.max_list_snapshot_bytes {
                return Err(McpError::internal_error(
                    "upstream MCP server exceeded the configured tool snapshot limit",
                    None,
                ));
            }
            cursor = page.next_cursor;
            let Some(next) = cursor.as_ref() else {
                return Ok(tools);
            };
            if !cursors.insert(next.clone()) {
                return Err(McpError::internal_error(
                    "upstream MCP server returned a repeated tools/list cursor",
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
                "MCP server discovery timed out",
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
                    "MCP server discovery timed out or disconnected",
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
                    McpError::internal_error("MCP server discovery timed out", None)
                }
                UpstreamRequestError::Disconnected => {
                    McpError::internal_error("MCP server disconnected during discovery", None)
                }
            })
    }

    pub(super) async fn shutdown(&self) {
        let deadline = Instant::now() + Duration::from_secs(self.limits.shutdown_seconds);
        if let Ok(mut service) = tokio::time::timeout_at(deadline, self.service.lock()).await
            && let Some(mut service) = service.take()
        {
            let _ = tokio::time::timeout_at(deadline, service.close()).await;
        }
        if let Ok(mut owner) = tokio::time::timeout_at(deadline, self.owner.lock()).await
            && let Some(mut owner) = owner.take()
        {
            owner
                .shutdown(deadline.saturating_duration_since(Instant::now()))
                .await;
        }
    }
}

async fn initialize_upstream<T>(
    server: &SelectedMcpServer,
    transport: T,
    limits: &ServerLimits,
    events: Arc<GatewayEvents>,
    progress: Arc<ProgressRoutes>,
    request_deadline: Instant,
    cancellation: CancellationToken,
) -> Result<UpstreamService, McpError>
where
    T: Transport<RoleClient, Error = io::Error> + 'static,
{
    let server_kind = match &server.definition {
        McpServerDefinition::Stdio(_) => "local MCP server",
        McpServerDefinition::StreamableHttp(_) => "remote MCP server",
    };
    let handler = GatewayUpstreamHandler {
        server_id: server.id.clone(),
        events,
        progress,
    };
    let connect_deadline = Instant::now()
        + Duration::from_secs(limits.connect_seconds)
            .min(request_deadline.saturating_duration_since(Instant::now()));
    let service = tokio::select! {
        result = tokio::time::timeout_at(connect_deadline, handler.serve(transport)) => {
            match result {
                Ok(Ok(service)) => service,
                Ok(Err(_)) => return Err(McpError::internal_error(
                    format!("configured {server_kind} failed protocol initialization"),
                    None,
                )),
                Err(_) => return Err(McpError::internal_error(
                    format!("configured {server_kind} initialization timed out"),
                    None,
                )),
            }
        }
        _ = cancellation.cancelled() => {
            return Err(McpError::new(ErrorCode(-32800), "MCP request was cancelled", None));
        }
    };
    let compatible = service
        .peer_info()
        .is_some_and(|info| info.protocol_version == rmcp::model::ProtocolVersion::V_2025_11_25);
    if !compatible {
        let mut service = service;
        let _ = service.close().await;
        return Err(McpError::internal_error(
            format!("configured {server_kind} does not support protocol version 2025-11-25"),
            None,
        ));
    }
    Ok(service)
}

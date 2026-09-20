use super::request::{UpstreamRequestError, await_server_result};
use super::state::GatewayState;
use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, ClientRequest, Content, ErrorCode,
    ErrorData as McpError, Implementation, InitializeRequestParams, InitializeResult,
    ListToolsResult, PaginatedRequestParams, ProtocolVersion, RequestId, ServerCapabilities,
    ServerJsonRpcMessage, ServerResult,
};
use rmcp::service::{PeerRequestOptions, RequestContext, RoleServer};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

pub(super) struct GatewayServerHandler {
    state: Arc<GatewayState>,
}

impl GatewayServerHandler {
    pub(super) fn new(state: Arc<GatewayState>) -> Self {
        Self { state }
    }
}

impl ServerHandler for GatewayServerHandler {
    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, McpError> {
        let protocol_version = negotiate_protocol_version(&request.protocol_version);
        self.state.set_host_peer(context.peer).await;
        Ok(server_info(protocol_version))
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let _request_permit = self.state.try_request_slot()?;
        let cursor = request.and_then(|request| request.cursor);
        let discovery_deadline =
            Instant::now() + Duration::from_secs(self.state.discovery_seconds());
        let snapshot = self
            .state
            .tool_snapshot(context.ct.clone(), discovery_deadline)
            .await?;
        snapshot
            .page(
                cursor.as_deref(),
                context.id,
                self.state.max_message_bytes(),
            )
            .await
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if request.task.is_some() {
            return Err(McpError::invalid_params(
                "task-based tool calls are not supported by this gateway",
                None,
            ));
        }
        let _request_permit = self.state.try_request_slot()?;
        let request_started = Instant::now();
        let snapshot_deadline =
            request_started + Duration::from_secs(self.state.operation_seconds());
        let snapshot = self
            .state
            .tool_snapshot(context.ct.clone(), snapshot_deadline)
            .await?;
        let Some(route) = snapshot.routes.get(request.name.as_ref()).cloned() else {
            return Err(McpError::new(
                ErrorCode::METHOD_NOT_FOUND,
                "unknown gateway tool name",
                None,
            ));
        };
        let operation_deadline = request_started
            + Duration::from_secs(
                self.state
                    .operation_seconds_for(&route.server_id)
                    .unwrap_or(120),
            );
        if operation_deadline <= Instant::now() {
            return Ok(tool_error("external MCP tool call exceeded its deadline"));
        }
        let connection = match self
            .state
            .connection(&route.server_id, context.ct.clone(), operation_deadline)
            .await
        {
            Ok(connection) => connection,
            Err(_) if context.ct.is_cancelled() => {
                return Err(McpError::new(
                    ErrorCode(-32800),
                    "MCP request was cancelled",
                    None,
                ));
            }
            Err(_) => return Ok(tool_error("selected MCP server is unavailable")),
        };
        let _server_permit = connection.try_request_slot()?;
        let host_progress = context.meta.get_progress_token().or_else(|| {
            request
                .meta
                .as_ref()
                .and_then(|meta| meta.get_progress_token())
        });
        let remaining = operation_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(tool_error("external MCP tool call exceeded its deadline"));
        }
        let has_host_progress = host_progress.is_some();
        if has_host_progress {
            self.state.progress.begin_request(&route.server_id).await;
        }

        let mut params = CallToolRequestParams::new(route.upstream_name.clone());
        params.arguments = request.arguments;
        params.meta = request
            .meta
            .clone()
            .or_else(|| (!context.meta.0.is_empty()).then(|| context.meta.clone()));
        let rpc_request = ClientRequest::CallToolRequest(CallToolRequest::new(params));
        let mut request_options = PeerRequestOptions::no_options();
        request_options.timeout = Some(remaining);
        let handle = match tokio::time::timeout_at(
            operation_deadline,
            connection
                .peer
                .send_cancellable_request(rpc_request, request_options),
        )
        .await
        {
            Ok(Ok(handle)) => handle,
            Ok(Err(_)) => {
                if has_host_progress {
                    self.state
                        .progress
                        .cancel_unbound_request(&route.server_id)
                        .await;
                }
                return Ok(tool_error("selected MCP server is unavailable"));
            }
            Err(_) => {
                if has_host_progress {
                    self.state
                        .progress
                        .cancel_unbound_request(&route.server_id)
                        .await;
                }
                return Ok(tool_error("external MCP tool call exceeded its deadline"));
            }
        };

        let upstream_progress = handle.progress_token.clone();
        if let Some(host_progress) = host_progress {
            self.state
                .progress
                .register(
                    &route.server_id,
                    upstream_progress.clone(),
                    context.peer,
                    host_progress,
                )
                .await;
        }
        let result = await_server_result(handle, operation_deadline, context.ct).await;
        if has_host_progress {
            self.state
                .progress
                .remove(&route.server_id, &upstream_progress)
                .await;
        }

        match result {
            Ok(ServerResult::CallToolResult(result)) => {
                if tool_result_bytes(&result, &context.id)? > self.state.max_message_bytes() {
                    return Ok(tool_error(
                        "external MCP tool result exceeds the host message byte limit",
                    ));
                }
                Ok(result)
            }
            Ok(_) => Err(McpError::internal_error(
                "external MCP server returned an unexpected response",
                None,
            )),
            Err(UpstreamRequestError::Protocol(error)) => {
                if rpc_error_bytes(&error, &context.id)? > self.state.max_message_bytes() {
                    return Err(McpError::internal_error(
                        "external MCP error exceeds the host message byte limit",
                        None,
                    ));
                }
                Err(error)
            }
            Err(UpstreamRequestError::Cancelled) => Err(McpError::new(
                ErrorCode(-32800),
                "external MCP tool call was cancelled",
                None,
            )),
            Err(UpstreamRequestError::TimedOut) => Ok(tool_error(
                "external MCP tool call timed out; its outcome may be unknown",
            )),
            Err(UpstreamRequestError::Disconnected) => Ok(tool_error(
                "selected MCP server disconnected; its outcome may be unknown",
            )),
        }
    }

    fn get_info(&self) -> InitializeResult {
        server_info(ProtocolVersion::V_2025_11_25)
    }
}

fn negotiate_protocol_version(requested: &ProtocolVersion) -> ProtocolVersion {
    if requested == &ProtocolVersion::V_2025_06_18 {
        ProtocolVersion::V_2025_06_18
    } else {
        ProtocolVersion::V_2025_11_25
    }
}

fn server_info(protocol_version: ProtocolVersion) -> InitializeResult {
    InitializeResult::new(
        ServerCapabilities::builder()
            .enable_tools()
            .enable_tool_list_changed()
            .build(),
    )
    .with_protocol_version(protocol_version)
    .with_server_info(Implementation::new(
        "aifuel-gateway",
        env!("CARGO_PKG_VERSION"),
    ))
}

fn tool_error(message: &'static str) -> CallToolResult {
    CallToolResult::error(vec![Content::text(message)])
}

fn tool_result_bytes(result: &CallToolResult, request_id: &RequestId) -> Result<usize, McpError> {
    serde_json::to_vec(&ServerJsonRpcMessage::response(
        ServerResult::CallToolResult(result.clone()),
        request_id.clone(),
    ))
    .map(|bytes| bytes.len())
    .map_err(|_| McpError::internal_error("could not encode the MCP tool response", None))
}

fn rpc_error_bytes(error: &McpError, request_id: &RequestId) -> Result<usize, McpError> {
    serde_json::to_vec(&ServerJsonRpcMessage::error(
        error.clone(),
        request_id.clone(),
    ))
    .map(|bytes| bytes.len())
    .map_err(|_| McpError::internal_error("could not encode the MCP error response", None))
}

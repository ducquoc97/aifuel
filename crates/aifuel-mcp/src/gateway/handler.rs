use super::identity::{decode_resource_uri, is_direct_http_uri, resource_uri};
use super::prompts::rewrite_prompt_result;
use super::request::{UpstreamRequestError, await_server_result};
use super::state::GatewayState;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, ClientRequest, CompleteRequestParams,
    CompleteResult, Content, ErrorCode, ErrorData as McpError, GetPromptRequestParams,
    GetPromptResult, Implementation, InitializeRequestParams, InitializeResult, ListPromptsResult,
    ListResourceTemplatesResult, ListResourcesResult, ListToolsResult, PaginatedRequestParams,
    PromptReference, ProtocolVersion, ReadResourceRequestParams, ReadResourceResult, Reference,
    RequestId, ResourceContents, ServerCapabilities, ServerJsonRpcMessage, ServerResult,
    SubscribeRequestParams, UnsubscribeRequestParams,
};
use rmcp::service::{NotificationContext, PeerRequestOptions, RequestContext, RoleServer};
use rmcp::{ServerHandler, Service};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

pub(super) struct GatewayServerHandler {
    state: Arc<GatewayState>,
}

/// rmcp 1.6 models prompt embedded resources with one extra wrapper when it
/// serializes a typed `GetPromptResult`. Keep the protocol wire shape flat by
/// translating only this response through `CustomResult` at the host boundary.
pub(super) struct GatewayServerService {
    handler: GatewayServerHandler,
}

impl GatewayServerService {
    pub(super) fn new(handler: GatewayServerHandler) -> Self {
        Self { handler }
    }
}

impl Service<RoleServer> for GatewayServerService {
    async fn handle_request(
        &self,
        request: rmcp::model::ClientRequest,
        context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::ServerResult, McpError> {
        if let rmcp::model::ClientRequest::GetPromptRequest(request) = request {
            let result = self.handler.get_prompt(request.params, context).await?;
            let value = serde_json::to_value(result).map_err(|_| {
                McpError::internal_error("could not encode the MCP prompt response", None)
            })?;
            return Ok(rmcp::model::ServerResult::CustomResult(
                rmcp::model::CustomResult(flatten_prompt_resources(value)),
            ));
        }
        self.handler.handle_request(request, context).await
    }

    async fn handle_notification(
        &self,
        notification: rmcp::model::ClientNotification,
        context: NotificationContext<RoleServer>,
    ) -> Result<(), McpError> {
        self.handler
            .handle_notification(notification, context)
            .await
    }

    fn get_info(&self) -> InitializeResult {
        ServerHandler::get_info(&self.handler)
    }
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

    async fn list_prompts(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        let _request_permit = self.state.try_request_slot()?;
        let cursor = request.and_then(|request| request.cursor);
        let discovery_deadline =
            Instant::now() + Duration::from_secs(self.state.discovery_seconds());
        let snapshot = self
            .state
            .prompt_snapshot(context.ct.clone(), discovery_deadline)
            .await?;
        snapshot
            .page(
                cursor.as_deref(),
                context.id,
                self.state.max_message_bytes(),
            )
            .await
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResult, McpError> {
        let _request_permit = self.state.try_request_slot()?;
        let request_started = Instant::now();
        let discovery_deadline =
            request_started + Duration::from_secs(self.state.operation_seconds());
        let snapshot = self
            .state
            .prompt_snapshot(context.ct.clone(), discovery_deadline)
            .await?;
        let Some(route) = snapshot.routes.get(&request.name).cloned() else {
            return Err(McpError::new(
                ErrorCode::METHOD_NOT_FOUND,
                "unknown gateway prompt name",
                None,
            ));
        };
        let operation_deadline = request_started
            + Duration::from_secs(
                self.state
                    .operation_seconds_for(&route.server_id)
                    .unwrap_or(120),
            );
        let connection = self
            .state
            .connection(&route.server_id, context.ct.clone(), operation_deadline)
            .await
            .map_err(|_| McpError::internal_error("selected MCP server is unavailable", None))?;
        let _server_permit = connection.try_request_slot()?;
        let mut params = GetPromptRequestParams::new(route.upstream_name);
        params.arguments = request.arguments;
        params.meta = request
            .meta
            .clone()
            .or_else(|| (!context.meta.0.is_empty()).then(|| context.meta.clone()));
        let result = connection
            .get_prompt(params, operation_deadline, context.ct)
            .await?;
        let result = rewrite_prompt_result(&route.server_id, result);
        if prompt_result_bytes(&result, &context.id)? > self.state.max_message_bytes() {
            return Err(McpError::internal_error(
                "external MCP prompt result exceeds the host message byte limit",
                None,
            ));
        }
        Ok(result)
    }

    async fn complete(
        &self,
        mut request: CompleteRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CompleteResult, McpError> {
        let _request_permit = self.state.try_request_slot()?;
        let request_started = Instant::now();
        let discovery_deadline =
            request_started + Duration::from_secs(self.state.operation_seconds());
        let (server_id, upstream_reference) = match &request.r#ref {
            Reference::Prompt(prompt) => {
                let snapshot = self
                    .state
                    .prompt_snapshot(context.ct.clone(), discovery_deadline)
                    .await?;
                let Some(route) = snapshot.routes.get(&prompt.name).cloned() else {
                    return Err(McpError::new(
                        ErrorCode::METHOD_NOT_FOUND,
                        "unknown gateway prompt name for completion",
                        None,
                    ));
                };
                let mut upstream = PromptReference::new(route.upstream_name);
                if let Some(title) = prompt.title.clone() {
                    upstream = upstream.with_title(title);
                }
                (route.server_id, Reference::Prompt(upstream))
            }
            Reference::Resource(resource) => {
                let Ok((server_id, uri)) = decode_resource_uri(&resource.uri) else {
                    return Err(McpError::invalid_params(
                        "resource completion requires a gateway resource URI",
                        None,
                    ));
                };
                (server_id, Reference::for_resource(uri))
            }
        };
        request.r#ref = upstream_reference;
        let operation_deadline = request_started
            + Duration::from_secs(self.state.operation_seconds_for(&server_id).unwrap_or(120));
        let connection = self
            .state
            .connection(&server_id, context.ct.clone(), operation_deadline)
            .await
            .map_err(|_| McpError::internal_error("selected MCP server is unavailable", None))?;
        if !connection.supports_completion() {
            return Err(McpError::new(
                ErrorCode::METHOD_NOT_FOUND,
                "selected MCP server does not support argument completion",
                None,
            ));
        }
        let _server_permit = connection.try_request_slot()?;
        let result = connection
            .complete(request, operation_deadline, context.ct)
            .await?;
        if completion_result_bytes(&result, &context.id)? > self.state.max_message_bytes() {
            return Err(McpError::internal_error(
                "external MCP completion result exceeds the host message byte limit",
                None,
            ));
        }
        Ok(result)
    }

    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let _request_permit = self.state.try_request_slot()?;
        let cursor = request.and_then(|request| request.cursor);
        let discovery_deadline =
            Instant::now() + Duration::from_secs(self.state.discovery_seconds());
        let snapshot = self
            .state
            .resource_snapshot(context.ct.clone(), discovery_deadline)
            .await?;
        snapshot
            .page_resources(
                cursor.as_deref(),
                context.id,
                self.state.max_message_bytes(),
            )
            .await
    }

    async fn list_resource_templates(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        let _request_permit = self.state.try_request_slot()?;
        let cursor = request.and_then(|request| request.cursor);
        let discovery_deadline =
            Instant::now() + Duration::from_secs(self.state.discovery_seconds());
        let snapshot = self
            .state
            .resource_snapshot(context.ct.clone(), discovery_deadline)
            .await?;
        snapshot
            .page_templates(
                cursor.as_deref(),
                context.id,
                self.state.max_message_bytes(),
            )
            .await
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        let _request_permit = self.state.try_request_slot()?;
        let deadline = operation_deadline(&self.state);
        let snapshot = self
            .state
            .resource_snapshot(context.ct.clone(), deadline)
            .await?;
        let route = snapshot.route(&request.uri)?;
        let connection = self
            .state
            .connection(&route.server_id, context.ct.clone(), deadline)
            .await?;
        let _server_permit = connection.try_request_slot()?;
        let mut result = connection
            .read_resource(route.upstream_uri, deadline, context.ct.clone())
            .await?;
        for contents in &mut result.contents {
            rewrite_read_contents(contents, &route.server_id)?;
        }
        if read_result_bytes(&result, &context.id)? > self.state.max_message_bytes() {
            return Err(McpError::internal_error(
                "external MCP resource result exceeds the host message byte limit",
                None,
            ));
        }
        Ok(result)
    }

    async fn subscribe(
        &self,
        request: SubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        let _request_permit = self.state.try_request_slot()?;
        let deadline = operation_deadline(&self.state);
        let snapshot = self
            .state
            .resource_snapshot(context.ct.clone(), deadline)
            .await?;
        let route = snapshot.route(&request.uri)?;
        let connection = self
            .state
            .connection(&route.server_id, context.ct.clone(), deadline)
            .await?;
        let fresh = self
            .state
            .events
            .register_subscription(&route.server_id, &route.upstream_uri, &request.uri)
            .await;
        if fresh
            && let Err(error) = connection
                .subscribe(route.upstream_uri.clone(), deadline, context.ct.clone())
                .await
        {
            let _ = self
                .state
                .events
                .unregister_subscription(&route.server_id, &route.upstream_uri)
                .await;
            return Err(error);
        }
        Ok(())
    }

    async fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        let _request_permit = self.state.try_request_slot()?;
        let deadline = operation_deadline(&self.state);
        let snapshot = self
            .state
            .resource_snapshot(context.ct.clone(), deadline)
            .await?;
        let route = snapshot.route(&request.uri)?;
        if !self
            .state
            .events
            .unregister_subscription(&route.server_id, &route.upstream_uri)
            .await
        {
            return Err(McpError::invalid_params(
                "resource is not subscribed through this gateway",
                None,
            ));
        }
        let connection = self
            .state
            .connection(&route.server_id, context.ct.clone(), deadline)
            .await?;
        if let Err(error) = connection
            .unsubscribe(route.upstream_uri.clone(), deadline, context.ct.clone())
            .await
        {
            let _ = self
                .state
                .events
                .register_subscription(&route.server_id, &route.upstream_uri, &request.uri)
                .await;
            return Err(error);
        }
        Ok(())
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
                let result = rewrite_tool_result(result, &route.server_id)?;
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
            .enable_completions()
            .enable_prompts()
            .enable_prompts_list_changed()
            .enable_tools()
            .enable_tool_list_changed()
            .enable_resources()
            .enable_resources_list_changed()
            .enable_resources_subscribe()
            .build(),
    )
    .with_protocol_version(protocol_version)
    .with_server_info(Implementation::new(
        "aifuel-gateway",
        env!("CARGO_PKG_VERSION"),
    ))
}

fn operation_deadline(state: &GatewayState) -> Instant {
    Instant::now() + Duration::from_secs(state.operation_seconds())
}

fn rewrite_read_contents(contents: &mut ResourceContents, server_id: &str) -> Result<(), McpError> {
    let uri = match contents {
        ResourceContents::TextResourceContents { uri, .. }
        | ResourceContents::BlobResourceContents { uri, .. } => uri,
    };
    *uri = resource_uri(server_id, uri).map_err(|error| McpError::internal_error(error, None))?;
    Ok(())
}

fn rewrite_tool_result(
    mut result: CallToolResult,
    server_id: &str,
) -> Result<CallToolResult, McpError> {
    for content in &mut result.content {
        match &mut content.raw {
            rmcp::model::RawContent::ResourceLink(link) => {
                if !is_direct_http_uri(&link.uri) {
                    link.uri = resource_uri(server_id, &link.uri)
                        .map_err(|error| McpError::internal_error(error, None))?;
                }
            }
            rmcp::model::RawContent::Resource(embedded) => {
                rewrite_embedded_contents(&mut embedded.resource, server_id)?;
            }
            _ => {}
        }
    }
    Ok(result)
}

fn rewrite_embedded_contents(
    contents: &mut ResourceContents,
    server_id: &str,
) -> Result<(), McpError> {
    let uri = match contents {
        ResourceContents::TextResourceContents { uri, .. }
        | ResourceContents::BlobResourceContents { uri, .. } => uri,
    };
    if !is_direct_http_uri(uri) {
        *uri =
            resource_uri(server_id, uri).map_err(|error| McpError::internal_error(error, None))?;
    }
    Ok(())
}

fn read_result_bytes(
    result: &ReadResourceResult,
    request_id: &RequestId,
) -> Result<usize, McpError> {
    serde_json::to_vec(&ServerJsonRpcMessage::response(
        ServerResult::ReadResourceResult(result.clone()),
        request_id.clone(),
    ))
    .map(|bytes| bytes.len())
    .map_err(|_| McpError::internal_error("could not encode the MCP resource response", None))
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

fn flatten_prompt_resources(mut value: serde_json::Value) -> serde_json::Value {
    if let Some(messages) = value
        .get_mut("messages")
        .and_then(|value| value.as_array_mut())
    {
        for message in messages {
            let Some(content) = message.get_mut("content") else {
                continue;
            };
            if content.get("type").and_then(serde_json::Value::as_str) != Some("resource") {
                continue;
            }
            let Some(resource) = content
                .get_mut("resource")
                .and_then(serde_json::Value::as_object_mut)
                .and_then(|resource| resource.remove("resource"))
            else {
                continue;
            };
            content["resource"] = resource;
        }
    }
    value
}

fn prompt_result_bytes(
    result: &GetPromptResult,
    request_id: &RequestId,
) -> Result<usize, McpError> {
    serde_json::to_vec(&ServerJsonRpcMessage::response(
        ServerResult::GetPromptResult(result.clone()),
        request_id.clone(),
    ))
    .map(|bytes| bytes.len())
    .map_err(|_| McpError::internal_error("could not encode the MCP prompt response", None))
}

fn completion_result_bytes(
    result: &CompleteResult,
    request_id: &RequestId,
) -> Result<usize, McpError> {
    serde_json::to_vec(&ServerJsonRpcMessage::response(
        ServerResult::CompleteResult(result.clone()),
        request_id.clone(),
    ))
    .map(|bytes| bytes.len())
    .map_err(|_| McpError::internal_error("could not encode the MCP completion response", None))
}

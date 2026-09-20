use super::process::{LocalProcess, ResolvedEnvironment, SpawnedLocalServer};
use super::progress::{GatewayEvents, GatewayUpstreamHandler, ProgressRoutes};
use super::remote_endpoint::connect_endpoint;
use super::remote_transport::{RemoteHttpTransport, RemoteSession};
use super::request::{UpstreamRequestError, await_server_result};
use aifuel_app::{McpServerDefinition, SelectedMcpServer, ServerLimits};
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::ServiceExt;
use rmcp::model::{
    ClientRequest, CompleteRequest, CompleteRequestParams, CompleteResult, ErrorCode,
    ErrorData as McpError, GetPromptRequest, GetPromptRequestParams, GetPromptResult,
    ListPromptsRequest, ListResourceTemplatesRequest, ListResourcesRequest, ListToolsRequest,
    PaginatedRequestParams, Prompt, ReadResourceRequest, ReadResourceRequestParams,
    ReadResourceResult, Resource, ResourceTemplate, ServerResult, SubscribeRequest,
    SubscribeRequestParams, Tool, UnsubscribeRequest, UnsubscribeRequestParams,
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

pub(super) struct ResolvedRemoteSecretHeaders {
    headers: Vec<(HeaderName, HeaderValue)>,
}

impl ResolvedRemoteSecretHeaders {
    fn unauthenticated() -> Self {
        Self {
            headers: Vec::new(),
        }
    }

    fn headers(&self) -> &[(HeaderName, HeaderValue)] {
        &self.headers
    }
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

pub(super) fn snapshot_remote_secret_headers(
    server: &SelectedMcpServer,
) -> Result<ResolvedRemoteSecretHeaders, &'static str> {
    let McpServerDefinition::StreamableHttp(config) = &server.definition else {
        return Ok(ResolvedRemoteSecretHeaders::unauthenticated());
    };
    if config.secret_headers.is_empty() {
        return Ok(ResolvedRemoteSecretHeaders::unauthenticated());
    }

    let mut headers = Vec::with_capacity(config.secret_headers.len());
    for (name, definition) in &config.secret_headers {
        let value = match env::var_os(&definition.env) {
            None => return Err("remote MCP named secret header credential is missing"),
            Some(value) => match value.to_str() {
                None => {
                    return Err("remote MCP named secret header credential is not valid text");
                }
                Some("") => return Err("remote MCP named secret header credential is empty"),
                Some(value) => HeaderValue::from_str(value).map_err(
                    |_| "remote MCP named secret header credential contains invalid bytes",
                )?,
            },
        };
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| "remote MCP named secret header name is invalid")?;
        headers.push((name, value));
    }
    Ok(ResolvedRemoteSecretHeaders { headers })
}

pub(super) struct ConnectionContext<'a> {
    pub(super) local_environment: Option<ResolvedEnvironment>,
    pub(super) remote_authentication: &'a ResolvedRemoteAuthentication,
    pub(super) remote_secret_headers: &'a Result<ResolvedRemoteSecretHeaders, &'static str>,
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
                let secret_headers = match context.remote_secret_headers {
                    Ok(headers) => headers.headers(),
                    Err(error) => return Err(McpError::internal_error(*error, None)),
                };
                let endpoint = connect_endpoint(
                    &config.url,
                    bearer_token,
                    secret_headers,
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

    pub(super) fn supports_prompts(&self) -> bool {
        self.peer
            .peer_info()
            .is_some_and(|info| info.capabilities.prompts.is_some())
    }

    pub(super) fn supports_completion(&self) -> bool {
        self.peer
            .peer_info()
            .is_some_and(|info| info.capabilities.completions.is_some())
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

    pub(super) async fn list_prompts(
        &self,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> Result<Vec<Prompt>, McpError> {
        let mut prompts = Vec::new();
        let mut cursor = None;
        let mut cursors = std::collections::HashSet::new();
        loop {
            let params = PaginatedRequestParams::default().with_cursor(cursor.clone());
            let request = ClientRequest::ListPromptsRequest(ListPromptsRequest::with_param(params));
            let response = self
                .request(request, deadline, cancellation.clone())
                .await?;
            let ServerResult::ListPromptsResult(page) = response else {
                return Err(McpError::internal_error(
                    "upstream MCP server returned an unexpected prompts/list response",
                    None,
                ));
            };
            prompts.extend(page.prompts);
            if prompts.len() > self.limits.max_list_entries {
                return Err(McpError::internal_error(
                    "upstream MCP server exceeded the configured prompt count limit",
                    None,
                ));
            }
            let bytes = serde_json::to_vec(&prompts)
                .map_err(|_| McpError::internal_error("could not encode MCP prompt list", None))?;
            if bytes.len() > self.limits.max_list_snapshot_bytes {
                return Err(McpError::internal_error(
                    "upstream MCP server exceeded the configured prompt snapshot limit",
                    None,
                ));
            }
            cursor = page.next_cursor;
            let Some(next) = cursor.as_ref() else {
                return Ok(prompts);
            };
            if !cursors.insert(next.clone()) {
                return Err(McpError::internal_error(
                    "upstream MCP server returned a repeated prompts/list cursor",
                    None,
                ));
            }
        }
    }

    pub(super) async fn get_prompt(
        &self,
        params: GetPromptRequestParams,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> Result<GetPromptResult, McpError> {
        let response = self
            .request(
                ClientRequest::GetPromptRequest(GetPromptRequest::new(params)),
                deadline,
                cancellation,
            )
            .await?;
        match response {
            ServerResult::GetPromptResult(result) => Ok(result),
            ServerResult::CustomResult(result) => decode_prompt_result(result.0),
            _ => Err(McpError::internal_error(
                "upstream MCP server returned an unexpected prompts/get response",
                None,
            )),
        }
    }

    pub(super) async fn complete(
        &self,
        params: CompleteRequestParams,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> Result<CompleteResult, McpError> {
        let response = self
            .request(
                ClientRequest::CompleteRequest(CompleteRequest::new(params)),
                deadline,
                cancellation,
            )
            .await?;
        match response {
            ServerResult::CompleteResult(result) => Ok(result),
            _ => Err(McpError::internal_error(
                "upstream MCP server returned an unexpected completion response",
                None,
            )),
        }
    }

    pub(super) async fn list_resources(
        &self,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> Result<Vec<Resource>, McpError> {
        let mut resources = Vec::new();
        let mut cursor = None;
        let mut cursors = std::collections::HashSet::new();
        loop {
            let params = PaginatedRequestParams::default().with_cursor(cursor.clone());
            let request =
                ClientRequest::ListResourcesRequest(ListResourcesRequest::with_param(params));
            let response = self
                .request(request, deadline, cancellation.clone())
                .await?;
            let ServerResult::ListResourcesResult(page) = response else {
                return Err(McpError::internal_error(
                    "upstream MCP server returned an unexpected resources/list response",
                    None,
                ));
            };
            resources.extend(page.resources);
            self.check_list_bounds(&resources, "resource")?;
            cursor = page.next_cursor;
            let Some(next) = cursor.as_ref() else {
                return Ok(resources);
            };
            if !cursors.insert(next.clone()) {
                return Err(McpError::internal_error(
                    "upstream MCP server returned a repeated resources/list cursor",
                    None,
                ));
            }
        }
    }

    pub(super) async fn list_resource_templates(
        &self,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> Result<Vec<ResourceTemplate>, McpError> {
        let mut templates = Vec::new();
        let mut cursor = None;
        let mut cursors = std::collections::HashSet::new();
        loop {
            let params = PaginatedRequestParams::default().with_cursor(cursor.clone());
            let request = ClientRequest::ListResourceTemplatesRequest(
                ListResourceTemplatesRequest::with_param(params),
            );
            let response = self
                .request(request, deadline, cancellation.clone())
                .await?;
            let ServerResult::ListResourceTemplatesResult(page) = response else {
                return Err(McpError::internal_error(
                    "upstream MCP server returned an unexpected resources/templates/list response",
                    None,
                ));
            };
            templates.extend(page.resource_templates);
            self.check_list_bounds(&templates, "resource template")?;
            cursor = page.next_cursor;
            let Some(next) = cursor.as_ref() else {
                return Ok(templates);
            };
            if !cursors.insert(next.clone()) {
                return Err(McpError::internal_error(
                    "upstream MCP server returned a repeated resources/templates/list cursor",
                    None,
                ));
            }
        }
    }

    pub(super) async fn read_resource(
        &self,
        uri: String,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> Result<ReadResourceResult, McpError> {
        let request = ClientRequest::ReadResourceRequest(ReadResourceRequest::new(
            ReadResourceRequestParams::new(uri),
        ));
        let response = self.request(request, deadline, cancellation).await?;
        match response {
            ServerResult::ReadResourceResult(result) => Ok(result),
            _ => Err(McpError::internal_error(
                "upstream MCP server returned an unexpected resources/read response",
                None,
            )),
        }
    }

    pub(super) async fn subscribe(
        &self,
        uri: String,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> Result<(), McpError> {
        let request = ClientRequest::SubscribeRequest(SubscribeRequest::new(
            SubscribeRequestParams::new(uri),
        ));
        let response = self.request(request, deadline, cancellation).await?;
        match response {
            ServerResult::EmptyResult(_) => Ok(()),
            _ => Err(McpError::internal_error(
                "upstream MCP server returned an unexpected resources/subscribe response",
                None,
            )),
        }
    }

    pub(super) async fn unsubscribe(
        &self,
        uri: String,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> Result<(), McpError> {
        let request = ClientRequest::UnsubscribeRequest(UnsubscribeRequest::new(
            UnsubscribeRequestParams::new(uri),
        ));
        let response = self.request(request, deadline, cancellation).await?;
        match response {
            ServerResult::EmptyResult(_) => Ok(()),
            _ => Err(McpError::internal_error(
                "upstream MCP server returned an unexpected resources/unsubscribe response",
                None,
            )),
        }
    }

    fn check_list_bounds<T: serde::Serialize>(
        &self,
        entries: &[T],
        kind: &str,
    ) -> Result<(), McpError> {
        if entries.len() > self.limits.max_list_entries {
            return Err(McpError::internal_error(
                format!("upstream MCP server exceeded the configured {kind} count limit"),
                None,
            ));
        }
        let bytes = serde_json::to_vec(entries)
            .map_err(|_| McpError::internal_error("could not encode MCP resource list", None))?;
        if bytes.len() > self.limits.max_list_snapshot_bytes {
            return Err(McpError::internal_error(
                format!("upstream MCP server exceeded the configured {kind} snapshot limit"),
                None,
            ));
        }
        Ok(())
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

fn decode_prompt_result(mut value: serde_json::Value) -> Result<GetPromptResult, McpError> {
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
            else {
                continue;
            };
            if resource.contains_key("resource") {
                continue;
            }
            let resource = std::mem::take(resource);
            content["resource"] = serde_json::json!({"resource": resource});
        }
    }
    serde_json::from_value(value).map_err(|_| {
        McpError::internal_error(
            "upstream MCP server returned an invalid prompts/get response",
            None,
        )
    })
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

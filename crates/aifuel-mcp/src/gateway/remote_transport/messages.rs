use super::http;
use super::response_sse;
use super::{PROTOCOL_VERSION, RemoteHttpError, RemoteHttpState, request_id_key};
use reqwest::header::CONTENT_TYPE;
use reqwest::{Response, StatusCode};
use rmcp::model::{
    ErrorData as McpError, ErrorData, JsonRpcMessage, RequestId, ServerJsonRpcMessage,
};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub(super) async fn wait_ready(
    state: &Arc<RemoteHttpState>,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<(), RemoteHttpError> {
    loop {
        if state.initialized.load(Ordering::Acquire) {
            return Ok(());
        }
        if state.session_expired.load(Ordering::Acquire) {
            ensure_session_current(state, cancellation, deadline).await?;
            continue;
        }
        let notified = state.ready.notified();
        if state.initialized.load(Ordering::Acquire) {
            return Ok(());
        }
        tokio::select! {
            _ = cancellation.cancelled() => return Err(RemoteHttpError::Cancelled),
            _ = state.cancellation.cancelled() => return Err(RemoteHttpError::Closed),
            _ = tokio::time::sleep_until(deadline) => return Err(RemoteHttpError::TimedOut),
            _ = notified => {}
        }
    }
}

pub(super) async fn ensure_session_current(
    state: &Arc<RemoteHttpState>,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<(), RemoteHttpError> {
    if state.session_expired.load(Ordering::Acquire) {
        let reinit_deadline =
            deadline.min(Instant::now() + Duration::from_secs(state.limits.connect_seconds));
        tokio::select! {
            _ = cancellation.cancelled() => Err(RemoteHttpError::Cancelled),
            result = http::recover_session(state, reinit_deadline) => result,
        }
    } else {
        Ok(())
    }
}

pub(super) async fn cancel_pending_except(state: &RemoteHttpState, keep: Option<&Value>) {
    let keep = keep.map(request_id_key);
    let mut cancelled = Vec::new();
    let pending = state.pending.lock().await;
    for (key, cancellation) in pending.iter() {
        if keep.as_deref() != Some(key.as_str()) {
            cancellation.cancel();
            cancelled.push(key.clone());
        }
    }
    drop(pending);
    state.expired_pending.lock().await.extend(cancelled);
}

pub(super) async fn cancel_upstream_request(
    state: &Arc<RemoteHttpState>,
    request_id: &Value,
) -> Result<(), RemoteHttpError> {
    if state.session_expired.load(Ordering::Acquire) {
        return Ok(());
    }
    let request_key = request_id_key(request_id);
    if !state.pending.lock().await.contains_key(&request_key) {
        return Ok(());
    }
    if !state.cancellations_sent.lock().await.insert(request_key) {
        return Ok(());
    }
    let session_guard = state.session_gate.read().await;
    if state.session_expired.load(Ordering::Acquire) {
        return Ok(());
    }
    let session_id = state.session_id.lock().await.clone();
    let Some(session_id) = session_id else {
        return Ok(());
    };
    let deadline = Instant::now() + Duration::from_secs(state.limits.shutdown_seconds.min(5));
    let cancellation = state.cancellation.child_token();
    let notification = json!({
        "jsonrpc":"2.0",
        "method":"notifications/cancelled",
        "params":{"requestId":request_id,"reason":"request deadline exceeded"}
    });
    let response = http::post(
        state,
        &notification,
        Some(&session_id),
        false,
        &cancellation,
        deadline,
    )
    .await?;
    if response.status() == StatusCode::NOT_FOUND {
        state.session_expired.store(true, Ordering::Release);
        drop(session_guard);
        cancel_pending_except(state, Some(request_id)).await;
        http::recover_session(state, deadline).await?;
        return Ok(());
    }
    drop(session_guard);
    require_accepted(state, response, false, &cancellation, deadline).await
}

pub(super) async fn request_response(
    state: &Arc<RemoteHttpState>,
    response: Response,
    request_id: Option<&Value>,
    cancellation: &CancellationToken,
    deadline: Instant,
    allow_reinitializing: bool,
) -> Result<ServerJsonRpcMessage, RemoteHttpError> {
    match response_content_type(&response) {
        Some("application/json") => {
            let message = read_json_message(state, response, cancellation, deadline).await?;
            validate_response_id(&message, request_id)?;
            Ok(message)
        }
        Some("text/event-stream") => {
            response_sse::read_initialize_response(
                Arc::clone(state),
                response,
                request_id.cloned().ok_or(RemoteHttpError::Message(
                    "remote MCP initialize request has no ID",
                ))?,
                cancellation.clone(),
                deadline,
                allow_reinitializing,
            )
            .await
        }
        _ => Err(RemoteHttpError::Message(
            "remote MCP server returned an unsupported content type",
        )),
    }
}

pub(super) async fn read_json_message(
    state: &RemoteHttpState,
    mut response: Response,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<ServerJsonRpcMessage, RemoteHttpError> {
    let body = read_bounded_body(state, &mut response, cancellation, deadline).await?;
    serde_json::from_slice(&body)
        .map_err(|_| RemoteHttpError::Message("remote MCP server returned malformed JSON-RPC"))
}

async fn read_bounded_body(
    state: &RemoteHttpState,
    response: &mut Response,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<Vec<u8>, RemoteHttpError> {
    let limit = state.limits.max_message_bytes;
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(RemoteHttpError::Message(
            "remote MCP response exceeds the configured byte limit",
        ));
    }
    let mut body = Vec::new();
    loop {
        let chunk = tokio::select! {
            _ = cancellation.cancelled() => return Err(RemoteHttpError::Cancelled),
            _ = state.cancellation.cancelled() => return Err(RemoteHttpError::Closed),
            _ = tokio::time::sleep_until(deadline) => return Err(RemoteHttpError::TimedOut),
            chunk = response.chunk() => chunk.map_err(|_| RemoteHttpError::Message("remote MCP response body could not be read"))?,
        };
        let Some(chunk) = chunk else {
            return Ok(body);
        };
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(RemoteHttpError::Message(
                "remote MCP response exceeds the configured byte limit",
            ));
        }
        body.extend_from_slice(&chunk);
    }
}

pub(super) async fn handle_http_error(
    state: &RemoteHttpState,
    mut response: Response,
    request_id: &Value,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<(), RemoteHttpError> {
    let status = response.status();
    reject_redirect(&response)?;
    if response_content_type(&response) == Some("application/json") {
        let body = read_bounded_body(state, &mut response, cancellation, deadline).await?;
        if let Some(message) = json_rpc_error(&body, request_id) {
            return deliver(state, message, cancellation, Some(deadline)).await;
        }
    }
    Err(RemoteHttpError::HttpStatus(status.as_u16()))
}

fn json_rpc_error(body: &[u8], request_id: &Value) -> Option<ServerJsonRpcMessage> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let response_id = value.get("id");
    if response_id.is_some_and(|id| !id.is_null() && id != request_id) {
        return None;
    }
    let error = serde_json::from_value::<ErrorData>(value.get("error")?.clone()).ok()?;
    let id = serde_json::from_value::<RequestId>(request_id.clone()).ok()?;
    Some(ServerJsonRpcMessage::error(error, id))
}

pub(super) async fn send_error(
    state: &RemoteHttpState,
    request_id: &Value,
    message: &str,
    cancellation: &CancellationToken,
) -> Result<(), RemoteHttpError> {
    let id = serde_json::from_value::<RequestId>(request_id.clone())
        .map_err(|_| RemoteHttpError::Message("remote MCP request ID is invalid"))?;
    let error = McpError::internal_error(message.to_owned(), None);
    deliver(
        state,
        ServerJsonRpcMessage::error(error, id),
        cancellation,
        Some(Instant::now() + Duration::from_secs(state.limits.operation_seconds)),
    )
    .await
}

pub(super) async fn deliver(
    state: &RemoteHttpState,
    message: ServerJsonRpcMessage,
    cancellation: &CancellationToken,
    deadline: Option<Instant>,
) -> Result<(), RemoteHttpError> {
    if let Some(deadline) = deadline {
        tokio::select! {
            _ = cancellation.cancelled() => Err(RemoteHttpError::Cancelled),
            _ = state.cancellation.cancelled() => Err(RemoteHttpError::Closed),
            _ = tokio::time::sleep_until(deadline) => Err(RemoteHttpError::TimedOut),
            result = state.responses.send(message) => result.map_err(|_| RemoteHttpError::Closed),
        }
    } else {
        tokio::select! {
            _ = cancellation.cancelled() => Err(RemoteHttpError::Cancelled),
            _ = state.cancellation.cancelled() => Err(RemoteHttpError::Closed),
            result = state.responses.send(message) => result.map_err(|_| RemoteHttpError::Closed),
        }
    }
}

pub(super) async fn deliver_expected(
    state: &RemoteHttpState,
    message: ServerJsonRpcMessage,
    request_id: &Value,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<(), RemoteHttpError> {
    let response_id = match &message {
        JsonRpcMessage::Response(response) => Some(&response.id),
        JsonRpcMessage::Error(error) => Some(&error.id),
        JsonRpcMessage::Request(_) | JsonRpcMessage::Notification(_) => None,
    };
    if let Some(response_id) = response_id {
        let response_id = serde_json::to_value(response_id)
            .map_err(|_| RemoteHttpError::Message("remote MCP response ID is invalid"))?;
        if &response_id != request_id {
            return Err(RemoteHttpError::Message(
                "remote MCP server returned a response for another request",
            ));
        }
        return deliver(state, message, cancellation, Some(deadline)).await;
    }
    deliver(state, message, cancellation, Some(deadline)).await
}

fn validate_response_id(
    message: &ServerJsonRpcMessage,
    request_id: Option<&Value>,
) -> Result<(), RemoteHttpError> {
    let Some(request_id) = request_id else {
        return Err(RemoteHttpError::Message(
            "remote MCP request has no response ID",
        ));
    };
    let response_id = match message {
        JsonRpcMessage::Response(response) => Some(&response.id),
        JsonRpcMessage::Error(error) => Some(&error.id),
        JsonRpcMessage::Request(_) | JsonRpcMessage::Notification(_) => None,
    };
    let Some(response_id) = response_id else {
        return Err(RemoteHttpError::Message(
            "remote MCP server returned a message instead of a response",
        ));
    };
    let response_id = serde_json::to_value(response_id)
        .map_err(|_| RemoteHttpError::Message("remote MCP response ID is invalid"))?;
    if &response_id != request_id {
        return Err(RemoteHttpError::Message(
            "remote MCP server returned a response for another request",
        ));
    }
    Ok(())
}

pub(super) fn validate_protocol_version(
    message: &ServerJsonRpcMessage,
) -> Result<(), RemoteHttpError> {
    let value = serde_json::to_value(message)
        .map_err(|_| RemoteHttpError::Message("remote MCP initialize response is invalid"))?;
    let version = value
        .pointer("/result/protocolVersion")
        .and_then(Value::as_str);
    if version == Some("2025-11-25") {
        Ok(())
    } else {
        Err(RemoteHttpError::Message(
            "remote MCP server must negotiate protocol version 2025-11-25",
        ))
    }
}

pub(super) fn session_header(response: &Response) -> Result<Option<String>, RemoteHttpError> {
    let Some(value) = response.headers().get("MCP-Session-Id") else {
        return Ok(None);
    };
    let value = value.to_str().map_err(|_| {
        RemoteHttpError::Message("remote MCP server returned an invalid session ID")
    })?;
    if value.is_empty() || !value.bytes().all(|byte| (0x21..=0x7e).contains(&byte)) {
        return Err(RemoteHttpError::Message(
            "remote MCP server returned an invalid session ID",
        ));
    }
    Ok(Some(value.to_owned()))
}

pub(super) fn response_content_type(response: &Response) -> Option<&'static str> {
    let value = response.headers().get(CONTENT_TYPE)?.to_str().ok()?;
    let value = value.split(';').next()?.trim();
    if value.eq_ignore_ascii_case("application/json") {
        Some("application/json")
    } else if value.eq_ignore_ascii_case("text/event-stream") {
        Some("text/event-stream")
    } else {
        None
    }
}

pub(super) fn reject_redirect(response: &Response) -> Result<(), RemoteHttpError> {
    if response.status().is_redirection() {
        Err(RemoteHttpError::Message(
            "remote MCP endpoint returned a redirect; configure its final endpoint",
        ))
    } else {
        Ok(())
    }
}

pub(super) async fn require_accepted(
    state: &RemoteHttpState,
    response: Response,
    expects_response: bool,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<(), RemoteHttpError> {
    if response.status() == StatusCode::ACCEPTED && !expects_response {
        let mut response = response;
        let body = read_bounded_body(state, &mut response, cancellation, deadline).await?;
        if body.is_empty() {
            Ok(())
        } else {
            Err(RemoteHttpError::Message(
                "remote MCP notification response must have an empty body",
            ))
        }
    } else {
        reject_redirect(&response)?;
        if !response.status().is_success() {
            return Err(RemoteHttpError::HttpStatus(response.status().as_u16()));
        }
        Err(RemoteHttpError::Message(
            "remote MCP server did not return HTTP 202 for a notification",
        ))
    }
}

pub(super) async fn shutdown_remote(state: &Arc<RemoteHttpState>) {
    state.cancellation.cancel();
    for cancellation in state.pending.lock().await.values() {
        cancellation.cancel();
    }
    if state.delete_started.swap(true, Ordering::AcqRel) {
        return;
    }
    let Some(session_id) = state.session_id.lock().await.clone() else {
        return;
    };
    let timeout = Duration::from_secs(state.limits.shutdown_seconds);
    let request = state
        .client
        .delete(state.endpoint.clone())
        .header("MCP-Protocol-Version", PROTOCOL_VERSION)
        .header("MCP-Session-Id", session_id)
        .timeout(timeout);
    let _ = tokio::time::timeout(timeout, request.send()).await;
}

pub(super) use super::messages::{
    cancel_pending_except, cancel_upstream_request, deliver, deliver_expected, reject_redirect,
    response_content_type, send_error, shutdown_remote,
};
use super::messages::{
    ensure_session_current, handle_http_error, read_json_message, request_response,
    require_accepted, session_header, validate_protocol_version, wait_ready,
};
use super::response_sse;
use super::{PROTOCOL_VERSION, RemoteHttpError, RemoteHttpState};
use reqwest::header::{ACCEPT, HeaderValue};
use reqwest::{Response, StatusCode};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub(super) async fn send_message(
    state: Arc<RemoteHttpState>,
    body: Value,
    method: Option<&str>,
    request_id: Option<Value>,
    cancellation: CancellationToken,
) -> Result<(), RemoteHttpError> {
    let timeout = match method {
        Some("initialize" | "notifications/initialized") => state.limits.connect_seconds,
        Some("tools/list") => state.limits.discovery_seconds,
        _ => state.limits.operation_seconds,
    };
    let deadline = Instant::now() + Duration::from_secs(timeout);

    if method == Some("initialize") {
        *state.initialize_request.lock().await = Some(body.clone());
        let response = post(&state, &body, None, true, &cancellation, deadline).await?;
        if response.status() == StatusCode::ACCEPTED {
            return Err(RemoteHttpError::Message(
                "remote MCP server accepted initialize without a result",
            ));
        }
        reject_redirect(&response)?;
        if !response.status().is_success() {
            return handle_http_error(
                &state,
                response,
                request_id.as_ref().ok_or(RemoteHttpError::Message(
                    "remote MCP initialize request has no ID",
                ))?,
                &cancellation,
                deadline,
            )
            .await;
        }
        let session = session_header(&response)?;
        *state.session_id.lock().await = session;
        let message = request_response(
            &state,
            response,
            request_id.as_ref(),
            &cancellation,
            deadline,
            false,
        )
        .await?;
        validate_protocol_version(&message)?;
        return deliver(&state, message, &cancellation, Some(deadline)).await;
    }

    if method == Some("notifications/initialized") {
        let session_id = state.session_id.lock().await.clone();
        let _session_guard = state.session_gate.read().await;
        let response = post(
            &state,
            &body,
            session_id.as_deref(),
            false,
            &cancellation,
            deadline,
        )
        .await?;
        if response.status() == StatusCode::NOT_FOUND && session_id.is_some() {
            state.mark_session_expired();
            recover_session(&state, deadline).await?;
            return Ok(());
        }
        require_accepted(&state, response, false, &cancellation, deadline).await?;
        state.initialized.store(true, Ordering::Release);
        state.ready.notify_waiters();
        response_sse::start_common_event_stream(Arc::clone(&state));
        return Ok(());
    }

    wait_ready(&state, &cancellation, deadline).await?;
    let (response, _session_id) = loop {
        ensure_session_current(&state, &cancellation, deadline).await?;
        let session_guard = tokio::select! {
            _ = cancellation.cancelled() => return Err(RemoteHttpError::Cancelled),
            _ = state.cancellation.cancelled() => return Err(RemoteHttpError::Closed),
            _ = tokio::time::sleep_until(deadline) => return Err(RemoteHttpError::TimedOut),
            guard = state.session_gate.read() => guard,
        };
        if state.session_expired.load(Ordering::Acquire) {
            drop(session_guard);
            continue;
        }
        let session_id = state.session_id.lock().await.clone();
        let response = post(
            &state,
            &body,
            session_id.as_deref(),
            false,
            &cancellation,
            deadline,
        )
        .await?;
        if response.status() == StatusCode::NOT_FOUND && session_id.is_some() {
            state.mark_session_expired();
            drop(session_guard);
            cancel_pending_except(&state, request_id.as_ref()).await;
            recover_session(&state, deadline).await?;
            return Err(RemoteHttpError::SessionExpired);
        }
        drop(session_guard);
        break (response, session_id);
    };

    if request_id.is_none() {
        return require_accepted(&state, response, false, &cancellation, deadline).await;
    }
    if response.status() == StatusCode::ACCEPTED {
        return Err(RemoteHttpError::Message(
            "remote MCP server accepted a request without a result",
        ));
    }
    reject_redirect(&response)?;
    if !response.status().is_success() {
        return handle_http_error(
            &state,
            response,
            request_id.as_ref().expect("request id exists"),
            &cancellation,
            deadline,
        )
        .await;
    }
    match response_content_type(&response) {
        Some("application/json") => {
            let message = read_json_message(&state, response, &cancellation, deadline).await?;
            deliver_expected(
                &state,
                message,
                request_id.as_ref().expect("request id exists"),
                &cancellation,
                deadline,
            )
            .await
        }
        Some("text/event-stream") => {
            let message = response_sse::read_request_response(
                Arc::clone(&state),
                response,
                request_id.as_ref().expect("request id exists").clone(),
                cancellation.clone(),
                deadline,
                false,
            )
            .await?;
            deliver_expected(
                &state,
                message,
                request_id.as_ref().expect("request id exists"),
                &cancellation,
                deadline,
            )
            .await
        }
        _ => Err(RemoteHttpError::Message(
            "remote MCP server returned an unsupported content type",
        )),
    }
}

pub(super) async fn post(
    state: &RemoteHttpState,
    body: &Value,
    session_id: Option<&str>,
    initialize: bool,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<Response, RemoteHttpError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(RemoteHttpError::TimedOut);
    }
    let mut request = state
        .client
        .post(state.endpoint.clone())
        .header(ACCEPT, "application/json, text/event-stream")
        .json(body)
        .timeout(remaining);
    if !initialize {
        request = request.header("MCP-Protocol-Version", PROTOCOL_VERSION);
        if let Some(session_id) = session_id {
            request = request.header("MCP-Session-Id", session_id);
        }
    }
    tokio::select! {
        _ = cancellation.cancelled() => Err(RemoteHttpError::Cancelled),
        _ = state.cancellation.cancelled() => Err(RemoteHttpError::Closed),
        _ = tokio::time::sleep_until(deadline) => Err(RemoteHttpError::TimedOut),
        result = request.send() => result.map_err(|_| RemoteHttpError::Message("remote MCP request failed")),
    }
}

async fn get_event_stream(
    state: &RemoteHttpState,
    session_id: Option<&str>,
    last_event_id: Option<&str>,
    cancellation: &CancellationToken,
    deadline: Option<Instant>,
) -> Result<Response, RemoteHttpError> {
    let mut request = state
        .client
        .get(state.endpoint.clone())
        .header(ACCEPT, "text/event-stream")
        .header("MCP-Protocol-Version", PROTOCOL_VERSION);
    if let Some(session_id) = session_id {
        request = request.header("MCP-Session-Id", session_id);
    }
    if let Some(last_event_id) = last_event_id {
        let last_event_id = HeaderValue::from_str(last_event_id)
            .map_err(|_| RemoteHttpError::Message("remote MCP event ID is invalid"))?;
        request = request.header("Last-Event-ID", last_event_id);
    }
    if let Some(deadline) = deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(RemoteHttpError::TimedOut);
        }
        request = request.timeout(remaining);
        tokio::select! {
            _ = cancellation.cancelled() => Err(RemoteHttpError::Cancelled),
            _ = state.cancellation.cancelled() => Err(RemoteHttpError::Closed),
            _ = tokio::time::sleep_until(deadline) => Err(RemoteHttpError::TimedOut),
            result = request.send() => result.map_err(|_| RemoteHttpError::Message("remote MCP event stream request failed")),
        }
    } else {
        tokio::select! {
            _ = cancellation.cancelled() => Err(RemoteHttpError::Cancelled),
            _ = state.cancellation.cancelled() => Err(RemoteHttpError::Closed),
            result = request.send() => result.map_err(|_| RemoteHttpError::Message("remote MCP event stream request failed")),
        }
    }
}

pub(super) async fn open_event_stream(
    state: &Arc<RemoteHttpState>,
    last_event_id: Option<&str>,
    cancellation: &CancellationToken,
    deadline: Option<Instant>,
    allow_reinitializing: bool,
) -> Result<Response, RemoteHttpError> {
    if allow_reinitializing {
        let session_id = state.session_id.lock().await.clone();
        return get_event_stream(
            state,
            session_id.as_deref(),
            last_event_id,
            cancellation,
            deadline,
        )
        .await;
    }
    loop {
        if let Some(deadline) = deadline {
            ensure_session_current(state, cancellation, deadline).await?;
        }
        let guard = if let Some(deadline) = deadline {
            tokio::select! {
                _ = cancellation.cancelled() => return Err(RemoteHttpError::Cancelled),
                _ = state.cancellation.cancelled() => return Err(RemoteHttpError::Closed),
                _ = tokio::time::sleep_until(deadline) => return Err(RemoteHttpError::TimedOut),
                guard = state.session_gate.read() => guard,
            }
        } else {
            tokio::select! {
                _ = cancellation.cancelled() => return Err(RemoteHttpError::Cancelled),
                _ = state.cancellation.cancelled() => return Err(RemoteHttpError::Closed),
                guard = state.session_gate.read() => guard,
            }
        };
        if state.session_expired.load(Ordering::Acquire) {
            drop(guard);
            if let Some(deadline) = deadline {
                ensure_session_current(state, cancellation, deadline).await?;
                continue;
            }
            return Err(RemoteHttpError::SessionExpired);
        }
        let session_id = state.session_id.lock().await.clone();
        let response = get_event_stream(
            state,
            session_id.as_deref(),
            last_event_id,
            cancellation,
            deadline,
        )
        .await;
        drop(guard);
        return response;
    }
}

pub(super) async fn recover_session(
    state: &Arc<RemoteHttpState>,
    deadline: Instant,
) -> Result<(), RemoteHttpError> {
    let _reinitialize_guard = state.reinitialize.lock().await;
    if !state.session_expired.load(Ordering::Acquire) {
        return Ok(());
    }
    state.initialized.store(false, Ordering::Release);
    let result = async {
        let _session_guard = state.session_gate.write().await;
        let original =
            state
                .initialize_request
                .lock()
                .await
                .clone()
                .ok_or(RemoteHttpError::Message(
                    "remote MCP session cannot be reinitialized",
                ))?;
        let mut initialize = original;
        let initialize_id = format!(
            "aifuel-session-{}",
            state.next_initialize_id.fetch_add(1, Ordering::Relaxed)
        );
        initialize["id"] = Value::String(initialize_id.clone());
        let reinit_deadline =
            deadline.min(Instant::now() + Duration::from_secs(state.limits.connect_seconds));
        let response = post(
            state,
            &initialize,
            None,
            true,
            &state.cancellation,
            reinit_deadline,
        )
        .await?;
        reject_redirect(&response)?;
        if !response.status().is_success() || response.status() == StatusCode::ACCEPTED {
            return Err(RemoteHttpError::Message(
                "remote MCP session reinitialization failed",
            ));
        }
        let session = session_header(&response)?;
        *state.session_id.lock().await = session.clone();
        state.session_expired.store(false, Ordering::Release);
        let initialize_message = Box::pin(request_response(
            state,
            response,
            Some(&Value::String(initialize_id)),
            &state.cancellation,
            reinit_deadline,
            true,
        ))
        .await?;
        validate_protocol_version(&initialize_message)?;

        let initialized = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        let response = post(
            state,
            &initialized,
            session.as_deref(),
            false,
            &state.cancellation,
            reinit_deadline,
        )
        .await?;
        reject_redirect(&response)?;
        if response.status() != StatusCode::ACCEPTED {
            return Err(RemoteHttpError::Message(
                "remote MCP server rejected session initialization",
            ));
        }
        Ok(())
    }
    .await;

    match result {
        Ok(()) => {
            state.session_expired.store(false, Ordering::Release);
            state.initialized.store(true, Ordering::Release);
            state.ready.notify_waiters();
            Ok(())
        }
        Err(error) => {
            state.mark_session_expired();
            state.ready.notify_waiters();
            Err(error)
        }
    }
}

use super::http;
use super::{RemoteHttpError, RemoteHttpState};
use crate::gateway::sse::{EventIdSet, SseEvent, SseReadError, SseReader};
use reqwest::{Response, StatusCode};
use rmcp::model::{JsonRpcMessage, ServerJsonRpcMessage};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const RECOVERY_WINDOW: Duration = Duration::from_secs(5 * 60);
const BACKOFF_SECONDS: [u64; 6] = [1, 2, 4, 8, 16, 30];

pub(super) async fn read_initialize_response(
    state: Arc<RemoteHttpState>,
    response: Response,
    request_id: Value,
    cancellation: CancellationToken,
    deadline: Instant,
    allow_reinitializing: bool,
) -> Result<ServerJsonRpcMessage, RemoteHttpError> {
    read_request_response(
        state,
        response,
        request_id,
        cancellation,
        deadline,
        allow_reinitializing,
    )
    .await
}

pub(super) async fn read_request_response(
    state: Arc<RemoteHttpState>,
    response: Response,
    request_id: Value,
    cancellation: CancellationToken,
    deadline: Instant,
    allow_reinitializing: bool,
) -> Result<ServerJsonRpcMessage, RemoteHttpError> {
    let mut reader = SseReader::new(response, state.limits.max_message_bytes);
    let mut event_ids = EventIdSet::new();
    let mut last_event_id = None;
    let mut retry_delay = None;
    let mut retry_attempt: usize = 0;
    let recovery_deadline = Instant::now() + RECOVERY_WINDOW;

    loop {
        match next_event(&state, &mut reader, &cancellation, deadline).await {
            Ok(Some(event)) => {
                if let Some(retry) = event.retry {
                    retry_delay = Some(retry);
                }
                let duplicate = record_event_id(&mut event_ids, &event, &mut last_event_id)?;
                if duplicate {
                    continue;
                }
                if let Some(data) = event.data.filter(|data| !data.is_empty()) {
                    let message = parse_message(&data)?;
                    if is_response_for_request(&message, &request_id)? {
                        return Ok(message);
                    }
                    http::deliver(&state, message, &cancellation, Some(deadline)).await?;
                }
            }
            Ok(None)
            | Err(RemoteHttpError::Message("remote MCP event stream could not be read")) => {
                let Some(last_event_id) = last_event_id.as_deref() else {
                    return Err(RemoteHttpError::Message(
                        "remote MCP response stream ended without a resumable event ID",
                    ));
                };
                let delay = retry_delay.unwrap_or_else(|| backoff(retry_attempt));
                wait_before_retry(&state, &cancellation, deadline, recovery_deadline, delay)
                    .await?;
                retry_attempt += 1;
                let response = loop {
                    match http::open_event_stream(
                        &state,
                        Some(last_event_id),
                        &cancellation,
                        Some(deadline),
                        allow_reinitializing,
                    )
                    .await
                    {
                        Ok(response) if response.status().is_server_error() => {
                            retry_attempt += 1;
                            let delay = retry_delay
                                .take()
                                .unwrap_or_else(|| backoff(retry_attempt - 1));
                            wait_before_retry(
                                &state,
                                &cancellation,
                                deadline,
                                recovery_deadline,
                                delay,
                            )
                            .await?;
                        }
                        Ok(response) => break response,
                        Err(RemoteHttpError::Cancelled | RemoteHttpError::Closed) => {
                            return Err(RemoteHttpError::Cancelled);
                        }
                        Err(error) => {
                            retry_attempt += 1;
                            let delay = retry_delay
                                .take()
                                .unwrap_or_else(|| backoff(retry_attempt - 1));
                            wait_before_retry(
                                &state,
                                &cancellation,
                                deadline,
                                recovery_deadline,
                                delay,
                            )
                            .await?;
                            if !matches!(error, RemoteHttpError::TimedOut) {
                                continue;
                            }
                            return Err(error);
                        }
                    }
                };
                if response.status() == StatusCode::NOT_FOUND {
                    if state.session_id.lock().await.is_some() {
                        if allow_reinitializing {
                            return Err(RemoteHttpError::SessionExpired);
                        }
                        state
                            .session_expired
                            .store(true, std::sync::atomic::Ordering::Release);
                        http::cancel_pending_except(&state, Some(&request_id)).await;
                        http::recover_session(&state, deadline).await?;
                        return Err(RemoteHttpError::SessionExpired);
                    }
                    return Err(RemoteHttpError::HttpStatus(404));
                }
                if response.status() == StatusCode::METHOD_NOT_ALLOWED {
                    return Err(RemoteHttpError::Message(
                        "remote MCP server does not support SSE response resumption",
                    ));
                }
                if response.status().is_redirection() {
                    return Err(RemoteHttpError::Message(
                        "remote MCP endpoint returned a redirect; configure its final endpoint",
                    ));
                }
                if !response.status().is_success() {
                    return Err(RemoteHttpError::HttpStatus(response.status().as_u16()));
                }
                if http::response_content_type(&response) != Some("text/event-stream") {
                    return Err(RemoteHttpError::Message(
                        "remote MCP server returned an unsupported content type for SSE resumption",
                    ));
                }
                reader = SseReader::new(response, state.limits.max_message_bytes);
            }
            Err(error) => return Err(error),
        }
    }
}

pub(super) fn start_common_event_stream(state: Arc<RemoteHttpState>) {
    if state
        .event_listener_started
        .swap(true, std::sync::atomic::Ordering::AcqRel)
    {
        return;
    }
    let cancellation = state.cancellation.child_token();
    tokio::spawn(async move {
        let _ = run_common_event_stream(state, cancellation).await;
    });
}

async fn run_common_event_stream(
    state: Arc<RemoteHttpState>,
    cancellation: CancellationToken,
) -> Result<(), RemoteHttpError> {
    let mut last_event_id = None;
    let mut retry_delay = None;
    let mut retry_attempt: usize = 0;
    let mut recovery_deadline = Instant::now() + RECOVERY_WINDOW;
    let mut event_ids = EventIdSet::new();

    loop {
        let response = match http::open_event_stream(
            &state,
            last_event_id.as_deref(),
            &cancellation,
            None,
            false,
        )
        .await
        {
            Ok(response) => response,
            Err(RemoteHttpError::Cancelled | RemoteHttpError::Closed) => return Ok(()),
            Err(RemoteHttpError::SessionExpired) => {
                http::cancel_pending_except(&state, None).await;
                http::recover_session(&state, recovery_deadline).await?;
                last_event_id = None;
                retry_delay = None;
                retry_attempt = 0;
                recovery_deadline = Instant::now() + RECOVERY_WINDOW;
                event_ids = EventIdSet::new();
                continue;
            }
            Err(_) => {
                retry_delay = None;
                retry_attempt = retry_attempt.saturating_add(1);
                wait_before_retry(
                    &state,
                    &cancellation,
                    recovery_deadline,
                    recovery_deadline,
                    backoff(retry_attempt - 1),
                )
                .await?;
                continue;
            }
        };
        if response.status() == StatusCode::METHOD_NOT_ALLOWED {
            return Ok(());
        }
        if response.status() == StatusCode::NOT_FOUND && state.session_id.lock().await.is_some() {
            state
                .session_expired
                .store(true, std::sync::atomic::Ordering::Release);
            http::cancel_pending_except(&state, None).await;
            http::recover_session(&state, recovery_deadline).await?;
            last_event_id = None;
            retry_delay = None;
            retry_attempt = 0;
            recovery_deadline = Instant::now() + RECOVERY_WINDOW;
            event_ids = EventIdSet::new();
            continue;
        }
        if response.status().is_redirection() {
            return Err(RemoteHttpError::Message(
                "remote MCP endpoint returned a redirect; configure its final endpoint",
            ));
        }
        if !response.status().is_success() {
            retry_attempt = retry_attempt.saturating_add(1);
            wait_before_retry(
                &state,
                &cancellation,
                recovery_deadline,
                recovery_deadline,
                backoff(retry_attempt - 1),
            )
            .await?;
            continue;
        }
        if http::response_content_type(&response) != Some("text/event-stream") {
            return Err(RemoteHttpError::Message(
                "remote MCP server returned an unsupported content type for its event stream",
            ));
        }

        let mut reader = SseReader::new(response, state.limits.max_message_bytes);
        loop {
            match next_common_event(&state, &mut reader, &cancellation).await {
                Ok(Some(event)) => {
                    if let Some(retry) = event.retry {
                        retry_delay = Some(retry);
                    }
                    let duplicate = record_event_id(&mut event_ids, &event, &mut last_event_id)?;
                    if duplicate {
                        continue;
                    }
                    if let Some(data) = event.data.filter(|data| !data.is_empty()) {
                        let message = parse_message(&data)?;
                        if !matches!(
                            &message,
                            JsonRpcMessage::Request(_) | JsonRpcMessage::Notification(_)
                        ) {
                            return Err(RemoteHttpError::Message(
                                "remote MCP GET stream returned an unassociated response",
                            ));
                        }
                        http::deliver(&state, message, &cancellation, None).await?;
                        retry_attempt = 0;
                        recovery_deadline = Instant::now() + RECOVERY_WINDOW;
                    }
                }
                Ok(None)
                | Err(RemoteHttpError::Message("remote MCP event stream could not be read")) => {
                    break;
                }
                Err(RemoteHttpError::Cancelled | RemoteHttpError::Closed) => return Ok(()),
                Err(error) => return Err(error),
            }
        }
        retry_attempt = retry_attempt.saturating_add(1);
        let delay = retry_delay
            .take()
            .unwrap_or_else(|| backoff(retry_attempt - 1));
        wait_before_retry(
            &state,
            &cancellation,
            recovery_deadline,
            recovery_deadline,
            delay,
        )
        .await?;
    }
}

async fn next_event(
    state: &RemoteHttpState,
    reader: &mut SseReader,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<Option<SseEvent>, RemoteHttpError> {
    tokio::select! {
        _ = cancellation.cancelled() => Err(RemoteHttpError::Cancelled),
        _ = state.cancellation.cancelled() => Err(RemoteHttpError::Closed),
        _ = tokio::time::sleep_until(deadline) => Err(RemoteHttpError::TimedOut),
        result = reader.next() => result.map_err(map_sse_error),
    }
}

async fn next_common_event(
    state: &RemoteHttpState,
    reader: &mut SseReader,
    cancellation: &CancellationToken,
) -> Result<Option<SseEvent>, RemoteHttpError> {
    tokio::select! {
        _ = cancellation.cancelled() => Err(RemoteHttpError::Cancelled),
        _ = state.cancellation.cancelled() => Err(RemoteHttpError::Closed),
        result = reader.next() => result.map_err(map_sse_error),
    }
}

fn record_event_id(
    event_ids: &mut EventIdSet,
    event: &SseEvent,
    last_event_id: &mut Option<String>,
) -> Result<bool, RemoteHttpError> {
    if let Some(id) = &event.id {
        *last_event_id = (!id.is_empty()).then(|| id.clone());
        event_ids.insert(id).map_err(map_sse_error)
    } else {
        Ok(false)
    }
}

fn parse_message(data: &[u8]) -> Result<ServerJsonRpcMessage, RemoteHttpError> {
    serde_json::from_slice(data)
        .map_err(|_| RemoteHttpError::Message("remote MCP SSE event contained malformed JSON-RPC"))
}

fn is_response_for_request(
    message: &ServerJsonRpcMessage,
    request_id: &Value,
) -> Result<bool, RemoteHttpError> {
    match &message {
        JsonRpcMessage::Response(response) => {
            check_response_id(&response.id, request_id)?;
            Ok(true)
        }
        JsonRpcMessage::Error(error) => {
            check_response_id(&error.id, request_id)?;
            Ok(true)
        }
        JsonRpcMessage::Request(_) | JsonRpcMessage::Notification(_) => Ok(false),
    }
}

fn check_response_id(
    response_id: &rmcp::model::RequestId,
    request_id: &Value,
) -> Result<(), RemoteHttpError> {
    let response_id = serde_json::to_value(response_id)
        .map_err(|_| RemoteHttpError::Message("remote MCP response ID is invalid"))?;
    if &response_id == request_id {
        Ok(())
    } else {
        Err(RemoteHttpError::Message(
            "remote MCP server returned a response for another request",
        ))
    }
}

async fn wait_before_retry(
    state: &RemoteHttpState,
    cancellation: &CancellationToken,
    deadline: Instant,
    recovery_deadline: Instant,
    delay: Duration,
) -> Result<(), RemoteHttpError> {
    let delay_deadline = Instant::now() + delay;
    let effective_deadline = deadline.min(recovery_deadline);
    if delay_deadline >= effective_deadline {
        tokio::select! {
            _ = cancellation.cancelled() => return Err(RemoteHttpError::Cancelled),
            _ = state.cancellation.cancelled() => return Err(RemoteHttpError::Closed),
            _ = tokio::time::sleep_until(effective_deadline) => return Err(RemoteHttpError::TimedOut),
        }
    }
    tokio::select! {
        _ = cancellation.cancelled() => Err(RemoteHttpError::Cancelled),
        _ = state.cancellation.cancelled() => Err(RemoteHttpError::Closed),
        _ = tokio::time::sleep_until(delay_deadline) => Ok(()),
    }
}

fn backoff(attempt: usize) -> Duration {
    let seconds = BACKOFF_SECONDS[attempt.min(BACKOFF_SECONDS.len() - 1)];
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    let jitter_percent = 80 + nanos % 21;
    Duration::from_millis(seconds * 1000 * u64::from(jitter_percent) / 100)
}

fn map_sse_error(error: SseReadError) -> RemoteHttpError {
    match error {
        SseReadError::ReadFailed => {
            RemoteHttpError::Message("remote MCP event stream could not be read")
        }
        SseReadError::EventTooLarge => {
            RemoteHttpError::Message("remote MCP event exceeds the configured byte limit")
        }
        SseReadError::InvalidEventId => {
            RemoteHttpError::Message("remote MCP event has an invalid ID")
        }
    }
}

use super::http;
use super::response_sse::{
    RECOVERY_WINDOW, backoff, parse_message, record_event_id, wait_before_retry,
};
use super::{RemoteHttpError, RemoteHttpState};
use crate::gateway::sse::{EventIdSet, SseEvent, SseReader};
use reqwest::StatusCode;
use rmcp::model::JsonRpcMessage;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

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
    let mut session_generation = state.session_generation.subscribe();

    'session: loop {
        let response = tokio::select! {
            biased;
            changed = session_generation.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                reset_shared_stream_state(
                    &mut last_event_id,
                    &mut retry_delay,
                    &mut retry_attempt,
                    &mut recovery_deadline,
                    &mut event_ids,
                );
                continue 'session;
            }
            response = http::open_event_stream(
                &state,
                last_event_id.as_deref(),
                &cancellation,
                None,
                false,
            ) => response,
        };
        let response = match response {
            Ok(response) => response,
            Err(RemoteHttpError::Cancelled | RemoteHttpError::Closed) => return Ok(()),
            Err(RemoteHttpError::SessionExpired) => {
                http::cancel_pending_except(&state, None).await;
                http::recover_session(&state, recovery_deadline).await?;
                reset_shared_stream_state(
                    &mut last_event_id,
                    &mut retry_delay,
                    &mut retry_attempt,
                    &mut recovery_deadline,
                    &mut event_ids,
                );
                continue 'session;
            }
            Err(_) => {
                retry_attempt = retry_attempt.saturating_add(1);
                let delay = retry_delay.unwrap_or_else(|| backoff(retry_attempt - 1));
                if !wait_before_common_retry(
                    &state,
                    &cancellation,
                    &mut session_generation,
                    recovery_deadline,
                    delay,
                )
                .await?
                {
                    reset_shared_stream_state(
                        &mut last_event_id,
                        &mut retry_delay,
                        &mut retry_attempt,
                        &mut recovery_deadline,
                        &mut event_ids,
                    );
                }
                continue 'session;
            }
        };
        if response.status() == StatusCode::METHOD_NOT_ALLOWED {
            return Ok(());
        }
        if matches!(
            response.status(),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ) {
            return Err(if response.status() == StatusCode::UNAUTHORIZED {
                RemoteHttpError::AuthenticationFailed
            } else {
                RemoteHttpError::PermissionDenied
            });
        }
        if response.status() == StatusCode::NOT_FOUND && state.session_id.lock().await.is_some() {
            state.mark_session_expired();
            http::cancel_pending_except(&state, None).await;
            http::recover_session(&state, recovery_deadline).await?;
            reset_shared_stream_state(
                &mut last_event_id,
                &mut retry_delay,
                &mut retry_attempt,
                &mut recovery_deadline,
                &mut event_ids,
            );
            continue 'session;
        }
        http::reject_redirect(&response)?;
        if !response.status().is_success() {
            retry_attempt = retry_attempt.saturating_add(1);
            let delay = retry_delay.unwrap_or_else(|| backoff(retry_attempt - 1));
            if !wait_before_common_retry(
                &state,
                &cancellation,
                &mut session_generation,
                recovery_deadline,
                delay,
            )
            .await?
            {
                reset_shared_stream_state(
                    &mut last_event_id,
                    &mut retry_delay,
                    &mut retry_attempt,
                    &mut recovery_deadline,
                    &mut event_ids,
                );
            }
            continue 'session;
        }
        if http::response_content_type(&response) != Some("text/event-stream") {
            return Err(RemoteHttpError::Message(
                "remote MCP server returned an unsupported content type for its event stream",
            ));
        }

        let mut reader = SseReader::new(response, state.limits.max_message_bytes);
        loop {
            let next_event = tokio::select! {
                biased;
                changed = session_generation.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                    reset_shared_stream_state(
                        &mut last_event_id,
                        &mut retry_delay,
                        &mut retry_attempt,
                        &mut recovery_deadline,
                        &mut event_ids,
                    );
                    continue 'session;
                }
                event = next_common_event(&state, &mut reader, &cancellation) => event,
            };
            match next_event {
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
        let delay = retry_delay.unwrap_or_else(|| backoff(retry_attempt - 1));
        if !wait_before_common_retry(
            &state,
            &cancellation,
            &mut session_generation,
            recovery_deadline,
            delay,
        )
        .await?
        {
            reset_shared_stream_state(
                &mut last_event_id,
                &mut retry_delay,
                &mut retry_attempt,
                &mut recovery_deadline,
                &mut event_ids,
            );
        }
    }
}

fn reset_shared_stream_state(
    last_event_id: &mut Option<String>,
    retry_delay: &mut Option<Duration>,
    retry_attempt: &mut usize,
    recovery_deadline: &mut Instant,
    event_ids: &mut EventIdSet,
) {
    *last_event_id = None;
    *retry_delay = None;
    *retry_attempt = 0;
    *recovery_deadline = Instant::now() + RECOVERY_WINDOW;
    *event_ids = EventIdSet::new();
}

async fn wait_before_common_retry(
    state: &RemoteHttpState,
    cancellation: &CancellationToken,
    session_generation: &mut tokio::sync::watch::Receiver<u64>,
    recovery_deadline: Instant,
    delay: Duration,
) -> Result<bool, RemoteHttpError> {
    tokio::select! {
        biased;
        changed = session_generation.changed() => {
            changed.map_err(|_| RemoteHttpError::Closed)?;
            Ok(false)
        }
        result = wait_before_retry(
            state,
            cancellation,
            recovery_deadline,
            recovery_deadline,
            delay,
        ) => {
            result?;
            Ok(true)
        }
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
        result = reader.next() => result.map_err(super::response_sse::map_sse_error),
    }
}

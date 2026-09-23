//! Bounded JSONL framing helpers for the Codex App Server protocol.

use crate::agent_execution::MAX_CAPTURE_BYTES;
use aifuel_core::{AgentRunError, RunCancellationToken};
use serde_json::Value;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::time::timeout;

pub(super) const MAX_FRAME_BYTES: usize = 1024 * 1024;

pub(super) async fn send<W>(
    stdin: &mut W,
    message: Value,
    deadline: Option<Instant>,
) -> Result<(), AgentRunError>
where
    W: AsyncWrite + Unpin,
{
    let mut line = serde_json::to_vec(&message)
        .map_err(|error| AgentRunError::InvalidRequest(error.to_string()))?;
    line.push(b'\n');
    match deadline {
        Some(deadline) => timeout(
            deadline.saturating_duration_since(Instant::now()),
            stdin.write_all(&line),
        )
        .await
        .map_err(|_| AgentRunError::Timeout("Codex App Server communication timed out".to_owned()))?
        .map_err(AgentRunError::Io),
        None => stdin.write_all(&line).await.map_err(AgentRunError::Io),
    }
}

pub(super) async fn expect_successful_response<R>(
    stdout: &mut BufReader<R>,
    id: u64,
    deadline: Instant,
    cancellation: &RunCancellationToken,
) -> Result<Value, AgentRunError>
where
    R: AsyncRead + Unpin,
{
    loop {
        let message = read_message(stdout, Some(deadline), cancellation)
            .await?
            .ok_or_else(|| protocol_error("Codex App Server closed during setup"))?;
        if message.get("id").and_then(Value::as_u64) == Some(id) {
            if !message.get("error").unwrap_or(&Value::Null).is_null() {
                return Err(protocol_error("Codex App Server rejected a setup request"));
            }
            return Ok(message);
        }
    }
}

pub(super) async fn read_message<R>(
    stdout: &mut BufReader<R>,
    deadline: Option<Instant>,
    cancellation: &RunCancellationToken,
) -> Result<Option<Value>, AgentRunError>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    loop {
        if cancellation.is_cancelled() {
            return Err(AgentRunError::Cancelled);
        }
        let poll_duration = deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::from_millis(100))
            .min(Duration::from_millis(100));
        if poll_duration.is_zero() {
            return Err(AgentRunError::Timeout(
                "Codex App Server communication timed out".to_owned(),
            ));
        }
        let read = timeout(
            poll_duration,
            (&mut *stdout)
                .take((MAX_CAPTURE_BYTES as u64).min(MAX_FRAME_BYTES as u64))
                .read_until(b'\n', &mut bytes),
        )
        .await;
        match read {
            Err(_) => {
                if bytes.len() >= MAX_FRAME_BYTES && bytes.last() != Some(&b'\n') {
                    return Err(protocol_error(
                        "Codex App Server response exceeded the frame limit",
                    ));
                }
                continue;
            }
            Ok(Err(error)) => return Err(AgentRunError::Io(error)),
            Ok(Ok(0)) if bytes.is_empty() => return Ok(None),
            Ok(Ok(0)) => {
                return Err(protocol_error(
                    "Codex App Server closed with an incomplete response frame",
                ));
            }
            Ok(Ok(_)) => {}
        }
        if bytes.len() > MAX_FRAME_BYTES {
            return Err(protocol_error(
                "Codex App Server response exceeded the frame limit",
            ));
        }
        if bytes.last() == Some(&b'\n') {
            return serde_json::from_slice(&bytes).map(Some).map_err(|error| {
                protocol_error(&format!("invalid Codex App Server JSON: {error}"))
            });
        }
        if bytes.len() >= MAX_FRAME_BYTES {
            return Err(protocol_error(
                "Codex App Server response exceeded the frame limit",
            ));
        }
    }
}

pub(super) fn protocol_error(message: &str) -> AgentRunError {
    AgentRunError::InvalidRequest(message.to_owned())
}

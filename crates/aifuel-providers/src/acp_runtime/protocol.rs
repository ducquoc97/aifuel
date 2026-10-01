//! Bounded JSONL framing for the Agent Client Protocol.
//!
//! ACP is JSON-RPC 2.0 over line-delimited stdio: every frame is one
//! serialized JSON value terminated by a newline. The reader bounds each
//! frame and polls cancellation between reads so teardown unblocks it
//! promptly, matching the app-server framing seam.

use crate::agent_execution::MAX_CAPTURE_BYTES;
use aifuel_core::{AgentRunError, RunCancellationToken};
use serde_json::Value;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::time::timeout;

/// One frame may not exceed this size.
const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Serialize `message` as one JSONL frame on the agent's stdin.
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
        .map_err(|_| AgentRunError::Timeout("ACP agent communication timed out".to_owned()))?
        .map_err(AgentRunError::Io),
        None => stdin.write_all(&line).await.map_err(AgentRunError::Io),
    }
}

/// Read one JSONL frame from the agent's stdout. `Ok(None)` means the
/// stream ended cleanly at a frame boundary.
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
                "ACP agent communication timed out".to_owned(),
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
                        "the ACP agent's response exceeded the frame limit",
                    ));
                }
                continue;
            }
            Ok(Err(error)) => return Err(AgentRunError::Io(error)),
            Ok(Ok(0)) if bytes.is_empty() => return Ok(None),
            Ok(Ok(0)) => {
                return Err(protocol_error(
                    "the ACP agent closed with an incomplete response frame",
                ));
            }
            Ok(Ok(_)) => {}
        }
        if bytes.len() > MAX_FRAME_BYTES {
            return Err(protocol_error(
                "the ACP agent's response exceeded the frame limit",
            ));
        }
        if bytes.last() == Some(&b'\n') {
            return serde_json::from_slice(&bytes).map(Some).map_err(|error| {
                protocol_error(&format!("invalid ACP JSON from the agent: {error}"))
            });
        }
    }
}

fn protocol_error(message: &str) -> AgentRunError {
    AgentRunError::InvalidRequest(message.to_owned())
}

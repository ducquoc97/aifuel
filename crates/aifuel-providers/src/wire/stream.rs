//! The response-stream driver: feed body chunks through the SSE parser,
//! forward answer deltas to the owner, and decide how the stream ended.
//!
//! The byte source is a [`ChunkStream`] so the driver runs in tests without
//! a live socket. Pull-based reads keep backpressure honest: the transport
//! is only asked for the next chunk once the previous one was processed.

use super::openai_chat;
use super::sse::{SseEvent, SseParser};
use aifuel_core::{
    AgentRunOutputHandler, MAX_ANSWER_BYTES_PER_RUN, RunCancellationToken, TokenUsage,
};
use std::io;
use std::time::{Duration, Instant};

/// A pull-based byte source for a response body. Abstracted so the stream
/// driver is exercised in tests without a live socket.
pub(crate) trait ChunkStream {
    /// The next body chunk, `None` at EOF, or an error mid-stream.
    async fn next_chunk(&mut self) -> io::Result<Option<Vec<u8>>>;
}

impl ChunkStream for reqwest::Response {
    async fn next_chunk(&mut self) -> io::Result<Option<Vec<u8>>> {
        self.chunk()
            .await
            .map(|chunk| chunk.map(|bytes| bytes.to_vec()))
            .map_err(io::Error::other)
    }
}

/// How a response stream terminated.
#[derive(Debug, PartialEq)]
pub(crate) enum StreamEnd {
    /// A terminal completion payload (`[DONE]` or a finish reason) was
    /// observed before EOF.
    Completed,
    /// EOF arrived without a terminal event: a truncated stream, which is a
    /// failure even though bytes were received.
    TruncatedEof,
    /// The transport or the provider reported a mid-stream failure.
    Failed(String),
    /// The caller cancelled the run.
    Cancelled,
    /// The caller's run deadline elapsed.
    DeadlineExceeded,
}

/// The captured answer and end state for one response stream.
#[derive(Debug)]
pub(crate) struct StreamOutcome {
    /// Captured public answer text, bounded at `MAX_ANSWER_BYTES_PER_RUN`.
    pub output: String,
    /// Whether captured output was cut at the bound.
    pub truncated: bool,
    /// The provider-reported model id, when a chunk carried one.
    pub model: Option<String>,
    /// The provider-reported token accounting, when a chunk carried one.
    pub usage: Option<TokenUsage>,
    /// How the stream terminated.
    pub end: StreamEnd,
}

/// Drive one response body to its end: feed chunks through the SSE parser,
/// forward answer deltas to the owner, and report how the stream stopped.
/// Cancellation and the run deadline are checked on a poll tick, and the
/// stream fails after `idle_timeout` with no bytes - independent of the
/// caller's deadline, since a silent peer is a broken run, not a slow one.
pub(crate) async fn drive_stream(
    stream: &mut impl ChunkStream,
    deadline: Option<Instant>,
    idle_timeout: Duration,
    poll_tick: Duration,
    cancellation: &RunCancellationToken,
    output_handler: Option<&dyn AgentRunOutputHandler>,
) -> StreamOutcome {
    let mut parser = SseParser::new();
    let mut accumulator = StreamAccumulator::default();
    let mut ticks = tokio::time::interval(poll_tick);
    let mut idle_deadline = Instant::now() + idle_timeout;
    let end = loop {
        let mut end = None;
        tokio::select! {
            chunk = stream.next_chunk() => match chunk {
                Ok(Some(bytes)) => {
                    idle_deadline = Instant::now() + idle_timeout;
                    if let Err(error) = parser.feed(&bytes) {
                        end = Some(StreamEnd::Failed(format!(
                            "stream decode failed: {error}"
                        )));
                    } else {
                        end = drain_events(&mut parser, &mut accumulator, output_handler);
                    }
                }
                Ok(None) => {
                    end = match parser.finish() {
                        Err(error) => Some(StreamEnd::Failed(format!(
                            "stream decode failed: {error}"
                        ))),
                        // The flush may dispatch a trailing event; EOF is
                        // truncated only if no terminal verdict was seen.
                        Ok(()) => drain_events(&mut parser, &mut accumulator, output_handler).or(
                            Some(if accumulator.terminal {
                                StreamEnd::Completed
                            } else {
                                StreamEnd::TruncatedEof
                            }),
                        ),
                    };
                }
                Err(error) => {
                    end = Some(StreamEnd::Failed(format!(
                        "the response stream failed: {error}"
                    )));
                }
            },
            _ = ticks.tick() => {
                if cancellation.is_cancelled() {
                    end = Some(StreamEnd::Cancelled);
                } else if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    end = Some(StreamEnd::DeadlineExceeded);
                } else if Instant::now() >= idle_deadline {
                    end = Some(StreamEnd::Failed(format!(
                        "the response stream produced no data for {} seconds",
                        idle_timeout.as_secs()
                    )));
                }
            }
        }
        if let Some(end) = end {
            break end;
        }
    };
    StreamOutcome {
        output: accumulator.output,
        truncated: accumulator.truncated,
        model: accumulator.model,
        usage: accumulator.usage,
        end,
    }
}

/// Read a failed response's body for diagnostics, bounded and best-effort:
/// diagnostics are never worth failing over.
pub(crate) async fn read_bounded_body(response: &mut reqwest::Response) -> Option<String> {
    const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
    let mut captured = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(bytes)) => {
                let remaining = MAX_ERROR_BODY_BYTES.saturating_sub(captured.len());
                if remaining == 0 {
                    break;
                }
                captured.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
            }
            Ok(None) | Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&captured).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// Apply every queued event to the accumulator, returning the stream's end
/// state when an event carries one.
fn drain_events(
    parser: &mut SseParser,
    accumulator: &mut StreamAccumulator,
    output_handler: Option<&dyn AgentRunOutputHandler>,
) -> Option<StreamEnd> {
    while let Some(event) = parser.next_event() {
        if let Some(end) = apply_event(&event, accumulator, output_handler) {
            return Some(end);
        }
    }
    None
}

/// What one event means for the run: answer text is forwarded and captured;
/// a terminal or failure verdict ends the stream.
fn apply_event(
    event: &SseEvent,
    accumulator: &mut StreamAccumulator,
    output_handler: Option<&dyn AgentRunOutputHandler>,
) -> Option<StreamEnd> {
    match openai_chat::classify_event(event.event.as_deref(), &event.data) {
        openai_chat::DataVerdict::Delta {
            text,
            terminal,
            model,
            usage,
        } => {
            if model.is_some() {
                accumulator.model = model;
            }
            if usage.is_some() {
                accumulator.usage = usage;
            }
            if let Some(handler) = output_handler {
                handler.on_output(&text);
            }
            append_bounded(accumulator, &text);
            accumulator.terminal |= terminal;
            // A `finish_reason` chunk is not the last event: `include_usage`
            // endpoints send a usage-only chunk before `[DONE]`. Keep reading
            // until the terminator or EOF; only stop early once both the
            // terminal verdict and the accounting have arrived.
            (terminal && accumulator.usage.is_some()).then_some(StreamEnd::Completed)
        }
        openai_chat::DataVerdict::Finish { model, usage } => {
            if model.is_some() {
                accumulator.model = model;
            }
            if usage.is_some() {
                accumulator.usage = usage;
            }
            accumulator.terminal = true;
            // With the accounting already in hand nothing else can arrive
            // that matters; otherwise read on for the trailing usage chunk
            // and `[DONE]`.
            accumulator.usage.is_some().then_some(StreamEnd::Completed)
        }
        openai_chat::DataVerdict::Complete { model, usage } => {
            if model.is_some() {
                accumulator.model = model;
            }
            if usage.is_some() {
                accumulator.usage = usage;
            }
            Some(StreamEnd::Completed)
        }
        openai_chat::DataVerdict::Failed { message } => Some(StreamEnd::Failed(message)),
        openai_chat::DataVerdict::Usage { usage, model } => {
            accumulator.usage = Some(usage);
            if model.is_some() {
                accumulator.model = model;
            }
            None
        }
        openai_chat::DataVerdict::Ignored { model } => {
            if model.is_some() {
                accumulator.model = model;
            }
            None
        }
    }
}

/// The partial run answer as it accumulates.
#[derive(Default)]
struct StreamAccumulator {
    output: String,
    truncated: bool,
    model: Option<String>,
    usage: Option<TokenUsage>,
    /// A `finish_reason` verdict was seen; the usage chunk and `[DONE]` may
    /// still follow, so EOF after this is a complete stream, not truncated.
    terminal: bool,
}

/// Append answer text under the capture bound; past it, deltas still reach
/// the owner but the recorded answer is cut, mirroring `read_bounded`.
fn append_bounded(accumulator: &mut StreamAccumulator, text: &str) {
    let remaining = MAX_ANSWER_BYTES_PER_RUN.saturating_sub(accumulator.output.len());
    if text.len() <= remaining {
        accumulator.output.push_str(text);
        return;
    }
    let mut boundary = remaining;
    while !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    accumulator.output.push_str(&text[..boundary]);
    accumulator.truncated = true;
}

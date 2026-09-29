//! Incremental server-sent-event decoding for Wire Api streams.
//!
//! The parser is fed raw bytes and emits complete events. Chunk boundaries
//! may split UTF-8 sequences and `data:` lines without corrupting output,
//! and a single event is bounded so a broken or hostile peer cannot grow
//! memory without limit. `id:` and `retry:` fields are deliberately
//! ignored: AI Fuel never auto-replays an inference request, so SSE
//! reconnection bookkeeping has no meaning here.

use std::collections::VecDeque;
use std::fmt;

/// The largest one event may grow across its lines and payload, in bytes.
/// Completion chunks are small; an event crossing this bound is a broken or
/// hostile stream, not a slow one.
const DEFAULT_MAX_EVENT_BYTES: usize = 1024 * 1024;

/// The UTF-8 byte-order mark a stream may open with.
const BOM: &[u8] = b"\xef\xbb\xbf";

/// One complete server-sent event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SseEvent {
    /// The `event:` field when the peer set one; `None` is the default
    /// `message` dispatch.
    pub event: Option<String>,
    /// Every `data:` line of the event joined by `\n`.
    pub data: String,
}

/// The failures a stream decode can report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SseError {
    /// One event exceeded the bounded buffer.
    EventTooLarge,
    /// Field content was not valid UTF-8.
    InvalidUtf8,
}

impl fmt::Display for SseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EventTooLarge => f.write_str("a single stream event exceeded the size bound"),
            Self::InvalidUtf8 => f.write_str("stream payload was not valid UTF-8"),
        }
    }
}

impl std::error::Error for SseError {}

/// A byte-fed parser producing one [`SseEvent`] per blank-line-terminated
/// block. Bytes not yet terminated by `\n` stay buffered, so feed sizes are
/// irrelevant to correctness.
pub(crate) struct SseParser {
    /// Bytes of the current line, not yet terminated.
    pending_line: Vec<u8>,
    /// The `data:` payload accumulated for the in-progress event, kept as
    /// bytes so a UTF-8 sequence split across `data:` lines still decodes.
    data: Vec<u8>,
    /// Whether a `data:` line was seen for the in-progress event; an empty
    /// `data:` line still counts toward the `\n` join between lines.
    data_seen: bool,
    /// The `event:` field accumulated for the in-progress event.
    event: Vec<u8>,
    /// Complete events awaiting consumption.
    pending: VecDeque<SseEvent>,
    /// Whether any bytes have been fed; used to skip a leading BOM once.
    started: bool,
    max_event_bytes: usize,
}

impl SseParser {
    /// A parser with the default per-event bound.
    pub(crate) fn new() -> Self {
        Self::with_max_event_bytes(DEFAULT_MAX_EVENT_BYTES)
    }

    /// A parser with an explicit per-event bound, for tests that exercise
    /// the bound without megabytes of payload.
    pub(crate) fn with_max_event_bytes(max_event_bytes: usize) -> Self {
        Self {
            pending_line: Vec::new(),
            data: Vec::new(),
            event: Vec::new(),
            data_seen: false,
            pending: VecDeque::new(),
            started: false,
            max_event_bytes,
        }
    }

    /// Feed raw response bytes. Complete events are queued for
    /// [`Self::next_event`].
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Result<(), SseError> {
        let mut rest = bytes;
        if !self.started {
            self.started = true;
            rest = rest.strip_prefix(BOM).unwrap_or(rest);
        }
        while let Some(newline) = rest.iter().position(|byte| *byte == b'\n') {
            self.pending_line.extend_from_slice(&rest[..newline]);
            rest = &rest[newline + 1..];
            let line = std::mem::take(&mut self.pending_line);
            self.process_line(&line)?;
        }
        self.pending_line.extend_from_slice(rest);
        self.enforce_bound()
    }

    /// Flush state at end of stream: a trailing line without its newline is
    /// processed, then the pending event dispatches as if the stream closed
    /// on a blank line. Whether a terminal payload was observed before EOF
    /// is the caller's question, not the parser's.
    pub(crate) fn finish(&mut self) -> Result<(), SseError> {
        if !self.pending_line.is_empty() {
            let line = std::mem::take(&mut self.pending_line);
            self.process_line(&line)?;
        }
        self.process_line(&[])
    }

    /// Pop the next complete event, in order.
    pub(crate) fn next_event(&mut self) -> Option<SseEvent> {
        self.pending.pop_front()
    }

    /// Process one complete line: accumulate fields or dispatch the event
    /// on a blank line.
    fn process_line(&mut self, line: &[u8]) -> Result<(), SseError> {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            return self.dispatch();
        }
        // A leading ':' is a comment line, which providers use as a
        // keepalive; it carries no event data.
        if line[0] == b':' {
            return Ok(());
        }
        let (field, value) = match line.iter().position(|byte| *byte == b':') {
            Some(colon) => {
                let value = line[colon + 1..]
                    .strip_prefix(b" ")
                    .unwrap_or(&line[colon + 1..]);
                (&line[..colon], value)
            }
            None => (line, &line[..0]),
        };
        match field {
            b"data" => {
                if self.data_seen {
                    self.data.push(b'\n');
                }
                self.data.extend_from_slice(value);
                self.data_seen = true;
            }
            b"event" => self.event.extend_from_slice(value),
            _ => {}
        }
        self.enforce_bound()
    }

    /// Emit the in-progress event. Per the SSE model an event whose data
    /// buffer is empty is dropped, so keepalive comments and event-name-only
    /// blocks never surface.
    fn dispatch(&mut self) -> Result<(), SseError> {
        self.data_seen = false;
        if self.data.is_empty() {
            self.event.clear();
            return Ok(());
        }
        let data =
            String::from_utf8(std::mem::take(&mut self.data)).map_err(|_| SseError::InvalidUtf8)?;
        let event = if self.event.is_empty() {
            None
        } else {
            Some(
                String::from_utf8(std::mem::take(&mut self.event))
                    .map_err(|_| SseError::InvalidUtf8)?,
            )
        };
        self.pending.push_back(SseEvent { event, data });
        Ok(())
    }

    /// The memory bound: unconsumed line bytes plus accumulated field data
    /// for the in-progress event.
    fn enforce_bound(&self) -> Result<(), SseError> {
        if self.data.len() + self.event.len() + self.pending_line.len() > self.max_event_bytes {
            Err(SseError::EventTooLarge)
        } else {
            Ok(())
        }
    }
}

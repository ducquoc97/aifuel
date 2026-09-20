use reqwest::Response;
use std::collections::{HashSet, VecDeque};
use std::time::Duration;

const MAX_EVENT_ID_BYTES: usize = 1024 * 1024;

pub(super) struct SseEvent {
    pub(super) data: Option<Vec<u8>>,
    pub(super) id: Option<String>,
    pub(super) retry: Option<Duration>,
}

pub(super) struct SseReader {
    response: Response,
    buffer: Vec<u8>,
    max_event_bytes: usize,
    event_data: Vec<u8>,
    event_has_data: bool,
    event_id: Option<String>,
    event_retry: Option<Duration>,
    event_bytes: usize,
    skip_lf: bool,
    eof: bool,
}

impl SseReader {
    pub(super) fn new(response: Response, max_event_bytes: usize) -> Self {
        Self {
            response,
            buffer: Vec::new(),
            max_event_bytes,
            event_data: Vec::new(),
            event_has_data: false,
            event_id: None,
            event_retry: None,
            event_bytes: 0,
            skip_lf: false,
            eof: false,
        }
    }

    pub(super) async fn next(&mut self) -> Result<Option<SseEvent>, SseReadError> {
        loop {
            if let Some(line) = self.take_line() {
                if line.is_empty() {
                    if self.event_has_data || self.event_id.is_some() || self.event_retry.is_some()
                    {
                        return Ok(Some(self.finish_event()));
                    }
                    self.event_bytes = 0;
                    continue;
                }
                self.process_line(&line)?;
                continue;
            }

            if self.eof {
                if !self.buffer.is_empty() {
                    let line = std::mem::take(&mut self.buffer);
                    self.process_line(&line)?;
                    continue;
                }
                if self.event_has_data || self.event_id.is_some() || self.event_retry.is_some() {
                    return Ok(Some(self.finish_event()));
                }
                return Ok(None);
            }

            match self.response.chunk().await {
                Ok(Some(chunk)) => {
                    self.buffer.extend_from_slice(&chunk);
                    let first_line_end = self
                        .buffer
                        .iter()
                        .position(|byte| matches!(byte, b'\n' | b'\r'));
                    if first_line_end.unwrap_or(self.buffer.len())
                        > self.max_event_bytes.saturating_add(1)
                    {
                        return Err(SseReadError::EventTooLarge);
                    }
                }
                Ok(None) => self.eof = true,
                Err(_) => return Err(SseReadError::ReadFailed),
            }
        }
    }

    fn take_line(&mut self) -> Option<Vec<u8>> {
        if self.skip_lf {
            if self.buffer.is_empty() {
                return None;
            }
            if self.buffer.first() == Some(&b'\n') {
                self.buffer.remove(0);
            }
            self.skip_lf = false;
        }

        for index in 0..self.buffer.len() {
            match self.buffer[index] {
                b'\n' => {
                    let mut line = self.buffer.drain(..=index).collect::<Vec<_>>();
                    line.pop();
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    return Some(line);
                }
                b'\r' => {
                    let mut line = self.buffer.drain(..=index).collect::<Vec<_>>();
                    line.pop();
                    self.skip_lf = true;
                    return Some(line);
                }
                _ => {}
            }
        }
        None
    }

    fn process_line(&mut self, line: &[u8]) -> Result<(), SseReadError> {
        self.event_bytes = self
            .event_bytes
            .saturating_add(line.len().saturating_add(1));
        if self.event_bytes > self.max_event_bytes {
            return Err(SseReadError::EventTooLarge);
        }
        if line.first() == Some(&b':') {
            return Ok(());
        }
        let colon = line.iter().position(|byte| *byte == b':');
        let (field, value) = colon.map_or((line, &[][..]), |index| {
            let value = &line[index + 1..];
            let value = value.strip_prefix(b" ").unwrap_or(value);
            (&line[..index], value)
        });
        match field {
            b"data" => {
                if self.event_has_data {
                    self.event_data.push(b'\n');
                }
                self.event_data.extend_from_slice(value);
                self.event_has_data = true;
                if self.event_data.len() > self.max_event_bytes {
                    return Err(SseReadError::EventTooLarge);
                }
            }
            b"id" if !value.contains(&0) => {
                let id = std::str::from_utf8(value).map_err(|_| SseReadError::InvalidEventId)?;
                if id.len() > MAX_EVENT_ID_BYTES {
                    return Err(SseReadError::EventTooLarge);
                }
                self.event_id = Some(id.to_owned());
            }
            b"retry" if value.iter().all(u8::is_ascii_digit) && !value.is_empty() => {
                if let Ok(milliseconds) = std::str::from_utf8(value)
                    .unwrap_or_default()
                    .parse::<u64>()
                {
                    self.event_retry = Some(Duration::from_millis(milliseconds));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn finish_event(&mut self) -> SseEvent {
        let data = self
            .event_has_data
            .then(|| std::mem::take(&mut self.event_data));
        self.event_bytes = 0;
        self.event_has_data = false;
        SseEvent {
            data,
            id: self.event_id.take(),
            retry: self.event_retry.take(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) enum SseReadError {
    ReadFailed,
    EventTooLarge,
    InvalidEventId,
}

impl std::fmt::Display for SseReadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::ReadFailed => "remote MCP event stream could not be read",
            Self::EventTooLarge => "remote MCP event exceeds the configured byte limit",
            Self::InvalidEventId => "remote MCP event has an invalid ID",
        })
    }
}

pub(super) struct EventIdSet {
    seen: HashSet<String>,
    oldest: VecDeque<String>,
    bytes: usize,
}

impl EventIdSet {
    pub(super) fn new() -> Self {
        Self {
            seen: HashSet::new(),
            oldest: VecDeque::new(),
            bytes: 0,
        }
    }

    /// Returns true when this ID was already delivered on the current stream.
    pub(super) fn insert(&mut self, id: &str) -> Result<bool, SseReadError> {
        if id.is_empty() {
            return Ok(false);
        }
        if self.seen.contains(id) {
            return Ok(true);
        }
        if id.len() > MAX_EVENT_ID_BYTES {
            return Err(SseReadError::EventTooLarge);
        }
        while self.bytes.saturating_add(id.len()) > MAX_EVENT_ID_BYTES {
            let Some(oldest) = self.oldest.pop_front() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(oldest.len());
            self.seen.remove(&oldest);
        }
        let id = id.to_owned();
        self.bytes = self.bytes.saturating_add(id.len());
        self.oldest.push_back(id.clone());
        self.seen.insert(id);
        Ok(false)
    }
}

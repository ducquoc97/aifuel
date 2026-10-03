//! In-memory request log for the `/v1` surface: a bounded buffer the
//! dashboard's `GET /api/gateway/logs` reads back newest-first. Entries
//! live for the process lifetime only - nothing here persists.

use std::collections::VecDeque;
use std::sync::Mutex;

/// Hard cap on retained entries; the oldest evict first so a busy gateway
/// cannot grow the log without bound.
pub(crate) const MAX_ENTRIES: usize = 500;

/// One completed `/v1` request for the dashboard log view.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct Entry {
    pub(crate) ts_unix: u64,
    pub(crate) model: String,
    pub(crate) integration: Option<String>,
    pub(crate) status: u16,
    pub(crate) stream: bool,
    pub(crate) usage: Option<serde_json::Value>,
    pub(crate) error: Option<String>,
}

/// The ring the shared static wraps. It is its own type so tests exercise
/// eviction and ordering without touching process state - a global deque
/// would interleave parallel test writes.
#[derive(Debug, Default)]
struct Buffer {
    entries: VecDeque<Entry>,
}

impl Buffer {
    fn record(&mut self, entry: Entry) {
        while self.entries.len() >= MAX_ENTRIES {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    /// Newest-first view, limited to `limit` entries and never more than
    /// the buffer holds.
    fn entries(&self, limit: usize) -> Vec<Entry> {
        self.entries
            .iter()
            .rev()
            .take(limit.min(MAX_ENTRIES))
            .cloned()
            .collect()
    }
}

static BUFFER: Mutex<Buffer> = Mutex::new(Buffer {
    entries: VecDeque::new(),
});

/// Record one completed request. Calls are added at each terminal outcome
/// in `chat`, `messages`, `responses`, and `completions`.
pub(crate) fn record(entry: Entry) {
    BUFFER.lock().expect("request log mutex").record(entry);
}

/// Newest-first entries for `GET /api/gateway/logs`, capped at `limit`.
pub(crate) fn entries(limit: usize) -> Vec<Entry> {
    BUFFER.lock().expect("request log mutex").entries(limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(seq: u64) -> Entry {
        Entry {
            ts_unix: seq,
            model: "auto".to_owned(),
            integration: None,
            status: 200,
            stream: false,
            usage: None,
            error: None,
        }
    }

    #[test]
    fn the_log_evicts_the_oldest_entries_past_its_cap() {
        // The cap is the memory contract: an endlessly busy gateway must
        // not grow the log without bound, and eviction drops the stalest
        // entries first so the view stays recent.
        let mut buffer = Buffer::default();
        for seq in 0..(MAX_ENTRIES + 20) as u64 {
            buffer.record(entry(seq));
        }
        let entries = buffer.entries(MAX_ENTRIES);
        assert_eq!(entries.len(), MAX_ENTRIES);
        assert_eq!(entries.first().expect("nonempty").ts_unix, 519);
        assert_eq!(entries.last().expect("nonempty").ts_unix, 20);
    }

    #[test]
    fn entries_reads_newest_first_and_honors_the_limit() {
        // The dashboard shows recent traffic first; `limit` bounds the
        // answer without mutating the buffer.
        let mut buffer = Buffer::default();
        for seq in 0..10_u64 {
            buffer.record(entry(seq));
        }
        let entries = buffer.entries(3);
        let stamps: Vec<u64> = entries.iter().map(|entry| entry.ts_unix).collect();
        assert_eq!(stamps, vec![9, 8, 7]);
        assert_eq!(
            buffer.entries(usize::MAX).len(),
            10,
            "a limit past the cap reads back only what the buffer holds"
        );
    }

    #[test]
    fn an_entry_serializes_the_dashboard_fields() {
        // The admin log view reads these names straight off the wire.
        let entry = Entry {
            integration: Some("codex:cli".to_owned()),
            error: Some("upstream failed".to_owned()),
            ..entry(7)
        };
        let value = serde_json::to_value(&entry).expect("entry serializes");
        assert_eq!(value["ts_unix"], 7);
        assert_eq!(value["model"], "auto");
        assert_eq!(value["integration"], "codex:cli");
        assert_eq!(value["status"], 200);
        assert_eq!(value["stream"], false);
        assert!(value["usage"].is_null());
        assert_eq!(value["error"], "upstream failed");
    }
}

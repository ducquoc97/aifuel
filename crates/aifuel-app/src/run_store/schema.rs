//! Database schema and bootstrap constants for the run history store.
//!
//! The schema is the allowlisted metadata contract: identifiers, provider
//! context, settings selections, timestamps, workspace links, state, stable
//! outcome metadata, and event timeline rows. Free-form content columns exist
//! only for `events.data`, which stays `NULL` unless the owning manager
//! enables content retention.

pub(super) const SCHEMA_VERSION: u32 = 1;

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
    provider TEXT NOT NULL,
    session_id TEXT NOT NULL,
    model TEXT,
    effort TEXT,
    working_directory TEXT NOT NULL,
    updated_at REAL NOT NULL,
    PRIMARY KEY (provider, session_id)
);
CREATE TABLE IF NOT EXISTS runs (
    run_id TEXT PRIMARY KEY,
    owner_pid INTEGER NOT NULL,
    provider TEXT NOT NULL,
    state TEXT NOT NULL,
    status TEXT,
    created_at REAL NOT NULL,
    completed_at REAL,
    working_directory TEXT,
    requested_model TEXT,
    requested_effort TEXT,
    effective_model TEXT,
    effective_effort TEXT,
    external_tools TEXT,
    output_format TEXT,
    access TEXT,
    timeout_seconds INTEGER,
    requested_account TEXT,
    reported_account TEXT,
    resume TEXT,
    session_id TEXT,
    local_session_id TEXT,
    exit_code INTEGER,
    closed_reason TEXT,
    content_available INTEGER NOT NULL DEFAULT 0,
    output_bytes INTEGER NOT NULL DEFAULT 0,
    diagnostics_bytes INTEGER NOT NULL DEFAULT 0,
    output_truncated INTEGER NOT NULL DEFAULT 0,
    diagnostics_truncated INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS runs_completed_at ON runs(completed_at);
CREATE INDEX IF NOT EXISTS runs_native_session ON runs(provider, session_id);
CREATE TABLE IF NOT EXISTS events (
    run_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    kind TEXT NOT NULL,
    created_at REAL NOT NULL,
    data TEXT,
    PRIMARY KEY (run_id, seq)
);
";

pub(super) const TERMINAL_STATES: &str = "'succeeded', 'failed', 'timed_out', 'cancelled'";

/// Event kinds that close a stream, used to keep reconciliation from
/// appending a second terminal event.
pub(super) const TERMINAL_EVENT_KINDS: &str = "'completed', 'failed', 'timed_out', 'cancelled'";

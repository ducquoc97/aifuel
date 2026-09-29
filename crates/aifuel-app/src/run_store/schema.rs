//! Database schema and bootstrap constants for the run history store.
//!
//! The schema is the allowlisted metadata contract: identifiers, provider
//! context, settings selections, timestamps, workspace links, state, stable
//! outcome metadata, and event timeline rows. Free-form content columns exist
//! only for `events.data`, which stays `NULL` unless the owning manager
//! enables content retention.

pub(super) const SCHEMA_VERSION: u32 = 3;

/// The `meta` table is created before any versioned migration so the schema
/// version can be read even on a database that predates the migration.
pub(super) const META_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    provider TEXT NOT NULL,
    integration TEXT NOT NULL,
    session_id TEXT NOT NULL,
    model TEXT,
    effort TEXT,
    working_directory TEXT NOT NULL,
    updated_at REAL NOT NULL,
    PRIMARY KEY (integration, session_id)
);
CREATE TABLE IF NOT EXISTS runs (
    run_id TEXT PRIMARY KEY,
    owner_pid INTEGER NOT NULL,
    provider TEXT NOT NULL,
    integration TEXT NOT NULL,
    state TEXT NOT NULL,
    status TEXT,
    created_at REAL NOT NULL,
    completed_at REAL,
    working_directory TEXT,
    integration_version TEXT,
    platform TEXT,
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
    diagnostics_truncated INTEGER NOT NULL DEFAULT 0,
    usage TEXT
);
CREATE INDEX IF NOT EXISTS runs_completed_at ON runs(completed_at);
CREATE INDEX IF NOT EXISTS runs_native_session ON runs(integration, session_id);
CREATE TABLE IF NOT EXISTS events (
    run_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    kind TEXT NOT NULL,
    created_at REAL NOT NULL,
    data TEXT,
    PRIMARY KEY (run_id, seq)
);
";

/// Schema version 1 stored only the provider key on runs and used it as the
/// session scope. Built-in integration ids equal the catalog provider ids,
/// so the stored value seeds the new `integration` column, and the sessions
/// table is rebuilt with the integration-scoped primary key.
pub(super) const MIGRATION_V1: &str = "
ALTER TABLE runs ADD COLUMN integration TEXT NOT NULL DEFAULT '';
UPDATE runs SET integration = provider WHERE integration = '';
CREATE TABLE sessions_v2 (
    provider TEXT NOT NULL,
    integration TEXT NOT NULL,
    session_id TEXT NOT NULL,
    model TEXT,
    effort TEXT,
    working_directory TEXT NOT NULL,
    updated_at REAL NOT NULL,
    PRIMARY KEY (integration, session_id)
);
INSERT INTO sessions_v2 (provider, integration, session_id, model, effort, working_directory, updated_at)
    SELECT provider, provider, session_id, model, effort, working_directory, updated_at
    FROM sessions;
DROP TABLE sessions;
ALTER TABLE sessions_v2 RENAME TO sessions;
DROP INDEX runs_native_session;
UPDATE meta SET value = '2' WHERE key = 'schema_version';
";

/// Schema version 2 predates persisted token accounting. Usage is terminal
/// outcome metadata the completion write sets once, so a nullable column
/// leaves existing history rows reading as no reported usage.
pub(super) const MIGRATION_V2: &str = "
ALTER TABLE runs ADD COLUMN usage TEXT;
UPDATE meta SET value = '3' WHERE key = 'schema_version';
";

pub(super) const TERMINAL_STATES: &str = "'succeeded', 'failed', 'timed_out', 'cancelled'";

/// Event kinds that close a stream, used to keep reconciliation from
/// appending a second terminal event.
pub(super) const TERMINAL_EVENT_KINDS: &str = "'completed', 'failed', 'timed_out', 'cancelled'";

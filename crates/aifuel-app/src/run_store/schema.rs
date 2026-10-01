//! Database schema and bootstrap constants for the run history store.
//!
//! The schema is the allowlisted metadata contract: identifiers, provider
//! context, settings selections, timestamps, workspace links, state, stable
//! outcome metadata, and event timeline rows. Free-form content columns exist
//! only for `events.data`, which stays `NULL` unless the owning manager
//! enables content retention.

pub(super) const SCHEMA_VERSION: u32 = 7;

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
CREATE TABLE IF NOT EXISTS agent_sessions (
    session_id TEXT PRIMARY KEY,
    integration TEXT NOT NULL,
    model TEXT,
    effort TEXT,
    cwd TEXT NOT NULL,
    status TEXT NOT NULL,
    resume_cursor TEXT,
    owner TEXT,
    external_tools TEXT,
    created_at REAL NOT NULL,
    updated_at REAL NOT NULL
);
CREATE TABLE IF NOT EXISTS session_events (
    session_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    kind TEXT NOT NULL,
    run_id TEXT,
    created_at REAL NOT NULL,
    schema_version INTEGER NOT NULL,
    data TEXT NOT NULL,
    PRIMARY KEY (session_id, seq)
);
CREATE INDEX IF NOT EXISTS session_events_run ON session_events(session_id, run_id, seq);
CREATE TABLE IF NOT EXISTS commands (
    command_id TEXT PRIMARY KEY,
    receipt_json TEXT NOT NULL,
    created_at REAL NOT NULL
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

/// Schema version 3 predates the Session Event Log: `session_events` holds
/// the durable per-session contract event sequence and `agent_sessions` the
/// persisted session read-model projection. Both are pure additions, so the
/// version stamp is all the migration needs; the schema batch that follows
/// creates the tables.
pub(super) const MIGRATION_V3: &str = "
UPDATE meta SET value = '4' WHERE key = 'schema_version';
";

/// Schema version 4 predates the command receipt log: `commands` holds the
/// serialized Receipt recorded for each dispatched command id so retries
/// answer idempotently. A pure addition, so the version stamp is all the
/// migration needs; the schema batch creates the table.
pub(super) const MIGRATION_V4: &str = "
UPDATE meta SET value = '5' WHERE key = 'schema_version';
";

/// Schema version 5 predates Agent Session ownership: `owner` records the
/// process instance that created the session so startup reconciliation and
/// shutdown marking only touch sessions whose owner is gone. Versions
/// before 4 have no `agent_sessions` table at all, so the migration creates
/// it first to keep `ALTER TABLE` valid on every upgrade path; rows written
/// before owner scoping keep `NULL` and reconcile as orphaned.
pub(super) const MIGRATION_V5: &str = "
CREATE TABLE IF NOT EXISTS agent_sessions (
    session_id TEXT PRIMARY KEY,
    integration TEXT NOT NULL,
    model TEXT,
    effort TEXT,
    cwd TEXT NOT NULL,
    status TEXT NOT NULL,
    resume_cursor TEXT,
    created_at REAL NOT NULL,
    updated_at REAL NOT NULL
);
ALTER TABLE agent_sessions ADD COLUMN owner TEXT;
UPDATE meta SET value = '6' WHERE key = 'schema_version';
";

/// Schema version 6 predates persisted session tool enforcement:
/// `external_tools` holds the JSON tool list `session.create` declared so a
/// startup resume can redeclare the same exact set instead of silently
/// continuing on the provider's full tool surface. Versions before 5 have
/// no `agent_sessions` table at all, so the migration creates it first to
/// keep `ALTER TABLE` valid on every upgrade path; rows written before the
/// column existed keep `NULL` and resume with no tool enforcement, the
/// same honest posture they ran under.
pub(super) const MIGRATION_V6: &str = "
CREATE TABLE IF NOT EXISTS agent_sessions (
    session_id TEXT PRIMARY KEY,
    integration TEXT NOT NULL,
    model TEXT,
    effort TEXT,
    cwd TEXT NOT NULL,
    status TEXT NOT NULL,
    resume_cursor TEXT,
    created_at REAL NOT NULL,
    updated_at REAL NOT NULL
);
ALTER TABLE agent_sessions ADD COLUMN external_tools TEXT;
UPDATE meta SET value = '7' WHERE key = 'schema_version';
";

pub(super) const TERMINAL_STATES: &str = "'succeeded', 'failed', 'timed_out', 'cancelled'";

/// Event kinds that close a stream, used to keep reconciliation from
/// appending a second terminal event.
pub(super) const TERMINAL_EVENT_KINDS: &str = "'completed', 'failed', 'timed_out', 'cancelled'";

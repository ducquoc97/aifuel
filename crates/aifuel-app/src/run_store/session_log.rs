//! The Session Event Log: the durable, per-Agent-Session sequence of typed
//! contract events.
//!
//! Each append stamps a monotonic `seq` scoped to the Agent Session across
//! all of its Agent Runs - unlike the per-run `events` stream - plus a
//! Unix-epoch `ts`. The log powers `session.subscribe` replay, the
//! materialized `SessionSnapshot` read model, and shutdown/startup
//! reconciliation.
//!
//! `agent_sessions` is the persisted projection those appends maintain:
//! current status, selection, cwd, and the provider resume cursor. Replay
//! reads the raw `session_events` rows only.

mod agent_sessions;
mod append;
mod replay;
mod snapshot;

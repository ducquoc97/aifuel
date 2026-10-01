//! End-to-end tests for the P0 `AgentRuntime` command surface, driven
//! through `dispatch` plus the consumer event channel against a scripted
//! in-crate adapter and a real Session Event Log store. Replay, restart,
//! and shutdown reconciliation live in `replay.rs`.
//!
//! The suite is split by contract surface: `session` covers the session
//! lifecycle commands, `runs` covers run-scoped commands and the approval
//! channel, and `selection` covers the registry-backed listing and
//! selection commands.

mod support;

#[path = "runtime/checkpoints.rs"]
mod checkpoints;
#[path = "runtime/runs.rs"]
mod runs;
#[path = "runtime/selection.rs"]
mod selection;
#[path = "runtime/session.rs"]
mod session;

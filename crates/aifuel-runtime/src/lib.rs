//! The portable agent runtime facade.
//!
//! [`AgentRuntime`] stitches the agent runtime contract
//! ([`aifuel_core`]'s `AgentCommand`/`AgentEvent`/`Receipt` types) to the
//! compiled provider adapters and the durable Session Event Log in
//! [`aifuel_app`]'s run store. Host Applications embed it in-process and
//! drive every operation through [`AgentRuntime::dispatch`]; live events
//! reach consumers on per-consumer channels registered with
//! [`AgentRuntime::events`].
//!
//! The facade owns the facts only it can author - `session.created`,
//! `session.closed`, and the `answered_by` attribution on
//! `approval.resolved` - while adapters own everything the provider emits.
//! The Session Event Log assigns `seq`/`ts` once per fact, so replay and
//! the `SessionSnapshot` read model stay honest for every consumer.

mod adapter;
mod commands;
mod dispatch;
mod pump;
mod registry;
mod runtime;

pub use adapter::RuntimeAdapter;
pub use dispatch::{CommandOutcome, CommandPayload};
pub use runtime::AgentRuntime;

/// The largest number of log events one `session.subscribe` replays before
/// the runtime falls back to a fresh `SessionSnapshot`. An internal guard,
/// tunable between versions; not part of the versioned contract.
pub const MAX_REPLAY_EVENTS: usize = 256;

/// The largest serialized payload one `session.subscribe` replays before
/// the runtime falls back to a fresh `SessionSnapshot`. Pairs with
/// [`MAX_REPLAY_EVENTS`] as the contract's second replay bound.
pub const MAX_REPLAY_BYTES: usize = 1024 * 1024;

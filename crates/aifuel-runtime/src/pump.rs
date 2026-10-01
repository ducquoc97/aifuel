//! The per-session event pump.
//!
//! One pump thread per live session drains the adapter's pull stream and
//! turns payload kinds into durable [`AgentEvent`]s: each kind is appended
//! to the Session Event Log under the runtime's `inner` lock - so a
//! `session.subscribe` holding the same lock sees a stable head sequence -
//! then broadcast to the session's live subscribers.
//!
//! Fact ownership is split deliberately:
//!
//! - The facade authors `session.created` and `session.closed` inside
//!   dispatch so the command's receipt carries the fact's `seq`; the
//!   adapter's own copies are skipped here. An adapter-emitted
//!   `session.closed` the facade did not initiate still lands, once, so a
//!   provider-side close is never lost.
//! - A resumed session's adapter emits the same fresh-session `idle`
//!   prelude a new session does, but the facade already recorded
//!   `session.status: working` as the continuation fact; the pump drops
//!   that one stale `idle` so the projection stays honest.
//! - `approval.resolved` is adapter-emitted inside the run's causal order;
//!   the pump only rewrites `answered_by` to the consumer id the facade
//!   registered when it accepted the answer.
//! - `checkpoint.created` and `quota.observed` are runtime-owned follow-ups
//!   to `run.completed`: the pump records the run's Checkpoint (write-access
//!   sessions in a git worktree only) and the integration's post-run Quota
//!   Pool observation when the adapter's Monitoring Collection Contract
//!   reports one. Neither can fail the run; their collection runs outside
//!   the shared lock so subprocess and network latency never stalls other
//!   sessions' appends.

use crate::adapter::RuntimeAdapter;
use crate::checkpoints;
use crate::runtime::Inner;
use aifuel_app::RunStore;
use aifuel_core::{
    AccessMode, AgentEvent, AgentEventKind, AgentSessionHandle, RunId, SessionId, SessionStatus,
};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::thread::{self, JoinHandle};

/// Start the drain loop for one live session. The pump ends when the
/// adapter's stream closes or the runtime is dropped. `resumed` marks a
/// startup-reconcile continuation: the adapter's fresh-session `idle`
/// prelude is stale there, so the first one is skipped.
pub(crate) fn spawn(
    store: RunStore,
    inner: Weak<Mutex<Inner>>,
    session_id: SessionId,
    adapter: Arc<dyn RuntimeAdapter>,
    handle: AgentSessionHandle,
    resumed: bool,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name(format!("aifuel-runtime-pump-{session_id}"))
        .spawn(move || drain(store, inner, session_id, adapter, handle, resumed))
}

fn drain(
    store: RunStore,
    inner: Weak<Mutex<Inner>>,
    session_id: SessionId,
    adapter: Arc<dyn RuntimeAdapter>,
    handle: AgentSessionHandle,
    resumed: bool,
) {
    // For a resumed session the adapter replays its fresh-session prelude;
    // the first `idle` would undo the `working` the facade just recorded.
    let mut skip_startup_idle = resumed;
    for mut kind in adapter.events(&handle) {
        let Some(inner) = inner.upgrade() else {
            return;
        };
        let mut guard = inner.lock().expect("runtime mutex");
        match &mut kind {
            // The facade's `session.create` append is the authoritative
            // record; the adapter's copy of the same fact is skipped.
            AgentEventKind::SessionCreated { .. } => continue,
            AgentEventKind::SessionStatus {
                status: SessionStatus::Idle,
            } if skip_startup_idle => {
                skip_startup_idle = false;
                continue;
            }
            AgentEventKind::SessionClosed { .. } => {
                // Only an adapter-initiated close lands: once the facade's
                // `session.close` removes the live entry or records the
                // fact itself, the adapter's copy would duplicate it.
                let Some(live) = guard.live.get_mut(&session_id) else {
                    continue;
                };
                if live.closed {
                    continue;
                }
                live.closed = true;
            }
            AgentEventKind::ApprovalResolved {
                request_id,
                answered_by,
                ..
            } => {
                if let Some(consumer) = guard
                    .answers
                    .remove(&(session_id.clone(), request_id.clone()))
                {
                    *answered_by = consumer.as_str().to_owned();
                }
            }
            _ => {}
        }
        skip_startup_idle = false;
        let completed_run = match &kind {
            AgentEventKind::RunCompleted { run_id, .. } => Some(run_id.clone()),
            _ => None,
        };
        match store.append(&session_id, kind) {
            Ok(event) => broadcast(&guard, &session_id, &event),
            Err(error) => {
                // A dropped fact must be loud even though the drain keeps
                // going: the log may recover on the next append.
                eprintln!("aifuel: session event append failed: {error}");
            }
        }
        // The post-run scope is read under the lock, then the lock is
        // released: checkpoint git plumbing and quota collection run
        // unlocked so their latency never stalls other sessions' appends.
        let scope = completed_run.and_then(|run_id| {
            guard.live.get(&session_id).map(|live| PostRunScope {
                run_id,
                access: live.access,
                cwd: live.cwd.clone(),
                monitoring: live.monitoring,
            })
        });
        drop(guard);
        if let Some(scope) = scope {
            post_run(&store, &inner, &session_id, &adapter, scope);
        }
    }
}

/// What one completed run's post-run follow-ups need to know about the
/// live session, read under the shared lock and carried out of it.
struct PostRunScope {
    run_id: RunId,
    access: AccessMode,
    cwd: PathBuf,
    monitoring: bool,
}

/// The runtime-owned follow-ups to one `run.completed` fact: the run's
/// Checkpoint for write-access sessions in a git worktree, then the
/// integration's post-run Quota Pool observation. Neither is a run fact -
/// they land in the log after `run.completed` and never fail the run.
fn post_run(
    store: &RunStore,
    inner: &Arc<Mutex<Inner>>,
    session_id: &SessionId,
    adapter: &Arc<dyn RuntimeAdapter>,
    scope: PostRunScope,
) {
    if scope.access != AccessMode::ReadOnly
        && let Some(root) = checkpoints::worktree_root(&scope.cwd)
    {
        // The session's previous checkpoint is the diff base, so the
        // diffstat reports this run's own changes; a checkpoint-log read
        // failure skips the checkpoint rather than guessing a baseline.
        match store.session_checkpoints(session_id) {
            Ok(recorded) => {
                let base = recorded.last().map(|checkpoint| &checkpoint.checkpoint_id);
                match checkpoints::create(&root, &scope.run_id, base) {
                    Ok(Some(recorded)) => append_fact(
                        store,
                        inner,
                        session_id,
                        AgentEventKind::CheckpointCreated {
                            run_id: scope.run_id,
                            checkpoint_id: recorded.checkpoint_id,
                            diffstat: recorded.diffstat,
                        },
                    ),
                    // No diff, no Checkpoint - the read_only rule applied
                    // to a write run that happened not to change anything.
                    Ok(None) => {}
                    Err(error) => {
                        eprintln!("aifuel: the run's Checkpoint could not be recorded: {error}")
                    }
                }
            }
            Err(error) => {
                eprintln!("aifuel: the session's Checkpoints could not be read: {error}")
            }
        }
    }
    // `quota.observed` lands only when the session's Integration declares
    // a Monitoring Collection Contract and the adapter's collection
    // produced a real observation: a missing contract or a failed
    // collection reports nothing, never a zero.
    if scope.monitoring
        && let Some(quota) = adapter.quota_observation()
    {
        append_fact(
            store,
            inner,
            session_id,
            AgentEventKind::QuotaObserved {
                integration_id: adapter.integration(),
                quota,
            },
        );
    }
}

/// Append one runtime-authored fact under the shared lock - so a
/// `session.subscribe` cannot slip between the append and the subscriber
/// registration - then deliver it to the session's live subscribers.
fn append_fact(
    store: &RunStore,
    inner: &Arc<Mutex<Inner>>,
    session_id: &SessionId,
    kind: AgentEventKind,
) {
    let guard = inner.lock().expect("runtime mutex");
    match store.append(session_id, kind) {
        Ok(event) => broadcast(&guard, session_id, &event),
        Err(error) => {
            eprintln!("aifuel: session event append failed: {error}");
        }
    }
}

/// Deliver one stamped event to every consumer subscribed to the session.
/// Sends are non-blocking and a disconnected consumer is skipped.
pub(crate) fn broadcast(inner: &Inner, session_id: &SessionId, event: &AgentEvent) {
    let Some(live) = inner.live.get(session_id) else {
        return;
    };
    for consumer_id in &live.subscribers {
        if let Some(consumer) = inner.consumers.get(consumer_id) {
            let _ = consumer.sender.send(event.clone());
        }
    }
}

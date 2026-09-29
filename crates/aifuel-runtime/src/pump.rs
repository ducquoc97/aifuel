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
//! - `approval.resolved` is adapter-emitted inside the run's causal order;
//!   the pump only rewrites `answered_by` to the consumer id the facade
//!   registered when it accepted the answer.

use crate::adapter::RuntimeAdapter;
use crate::runtime::Inner;
use aifuel_app::RunStore;
use aifuel_core::{AgentEvent, AgentEventKind, AgentSessionHandle, SessionId};
use std::sync::{Arc, Mutex, Weak};
use std::thread::{self, JoinHandle};

/// Start the drain loop for one live session. The pump ends when the
/// adapter's stream closes or the runtime is dropped.
pub(crate) fn spawn(
    store: RunStore,
    inner: Weak<Mutex<Inner>>,
    session_id: SessionId,
    adapter: Arc<dyn RuntimeAdapter>,
    handle: AgentSessionHandle,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name(format!("aifuel-runtime-pump-{session_id}"))
        .spawn(move || drain(store, inner, session_id, adapter, handle))
}

fn drain(
    store: RunStore,
    inner: Weak<Mutex<Inner>>,
    session_id: SessionId,
    adapter: Arc<dyn RuntimeAdapter>,
    handle: AgentSessionHandle,
) {
    for mut kind in adapter.events(&handle) {
        let Some(inner) = inner.upgrade() else {
            return;
        };
        let mut inner = inner.lock().expect("runtime mutex");
        match &mut kind {
            // The facade's `session.create` append is the authoritative
            // record; the adapter's copy of the same fact is skipped.
            AgentEventKind::SessionCreated { .. } => continue,
            AgentEventKind::SessionClosed { .. } => {
                // Only an adapter-initiated close lands: once the facade's
                // `session.close` removes the live entry or records the
                // fact itself, the adapter's copy would duplicate it.
                let Some(live) = inner.live.get_mut(&session_id) else {
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
                if let Some(consumer) = inner
                    .answers
                    .remove(&(session_id.clone(), request_id.clone()))
                {
                    *answered_by = consumer;
                }
            }
            _ => {}
        }
        match store.append(&session_id, kind) {
            Ok(event) => broadcast(&inner, &session_id, &event),
            Err(error) => {
                // A dropped fact must be loud even though the drain keeps
                // going: the log may recover on the next append.
                eprintln!("aifuel: session event append failed: {error}");
            }
        }
    }
}

/// Deliver one stamped event to every consumer subscribed to the session.
/// Sends are non-blocking and a disconnected consumer is skipped.
fn broadcast(inner: &Inner, session_id: &SessionId, event: &AgentEvent) {
    let Some(live) = inner.live.get(session_id) else {
        return;
    };
    for consumer_id in &live.subscribers {
        if let Some(consumer) = inner.consumers.get(consumer_id) {
            let _ = consumer.sender.send(event.clone());
        }
    }
}

//! Plumbing shared by the local provider-process adapters.
//!
//! [`cli_adapter`](crate::cli_adapter), [`codex_runtime`](crate::codex_runtime),
//! and [`claude_runtime`](crate::claude_runtime) each own one provider
//! process per Agent Session and enforce the same contract policy on
//! top: discovery evidence inspection, Advertised Model catalog
//! discovery, descriptor assembly ([`descriptors`]), Approval Request
//! policy ([`approvals`]), and the session-map plumbing below. Each
//! shared rule lives here once; the adapters keep only their
//! provider-specific construction and transport.

pub(crate) mod approvals;
pub(crate) mod descriptors;

use crate::credentials::CredentialStore;
use crate::discovery::DiscoveryContext;
use crate::integrations::{EvidenceContext, EvidenceSource, inspect_any};
use crate::model_catalog::{
    ProviderCatalogDiscovery, ProviderCatalogModel, discover_model_catalog,
};
use aifuel_core::{
    AgentAdapter, AgentEventKind, AgentEventStream, AgentRuntimeError, AgentSessionHandle,
    DiscoveryError, DiscoveryState, IntegrationId, ModelDescriptor, ModelSelection, ProviderKey,
    ReceiptCode, SessionId,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// The local facts discovery evidence inspection may consult.
///
/// Constructed by the caller at the runtime boundary; the adapter stores an
/// owned copy and inspects it lazily on the first availability question.
/// Every read is metadata-only: marker paths, environment variable
/// presence, and Credential Store metadata - never secret material.
#[derive(Debug, Clone)]
pub struct AdapterDiscovery {
    /// Home-relative filesystem checks, shared with catalog discovery.
    pub discovery: DiscoveryContext,
    /// The Credential Store rooted at the AI Fuel config directory. Only the
    /// metadata read is used for evidence.
    pub credentials: CredentialStore,
    /// Integration Identities carrying a `providers.json` entry, one arm of
    /// [`EvidenceSource::ConfiguredEndpoint`].
    pub configured: BTreeSet<IntegrationId>,
}

/// Local evidence attached to an adapter: the descriptor's declared
/// sources plus the context to inspect them with.
pub(crate) struct AdapterEvidence {
    sources: Vec<EvidenceSource>,
    context: AdapterDiscovery,
}

impl AdapterEvidence {
    pub(crate) fn new(sources: Vec<EvidenceSource>, context: AdapterDiscovery) -> Self {
        Self { sources, context }
    }
}

/// Inspect the adapter's attached evidence sources for `integration`.
/// `None` means no discovery context was attached, which reads as
/// `unknown` availability rather than absent evidence.
pub(crate) fn discovered(
    evidence: Option<&AdapterEvidence>,
    integration: &IntegrationId,
) -> Option<Result<DiscoveryState, DiscoveryError>> {
    let evidence = evidence?;
    let context = EvidenceContext {
        discovery: &evidence.context.discovery,
        credentials: &evidence.context.credentials,
        configured: &evidence.context.configured,
    };
    Some(inspect_any(&evidence.sources, integration, &context))
}

/// The serving check every adapter method runs first: a request naming
/// another Integration Identity fails rather than silently rerouting to
/// the integration this adapter serves.
pub(crate) fn ensure_serves(
    served: &IntegrationId,
    requested: &IntegrationId,
) -> Result<(), AgentRuntimeError> {
    if requested == served {
        Ok(())
    } else {
        Err(AgentRuntimeError::unsupported(format!(
            "this adapter serves integration {served} only"
        )))
    }
}

/// The advertised catalog, discovered once by the owning adapter.
///
/// The catalog interface is async; it runs on a short-lived runtime on a
/// scoped thread, the same pattern the execution adapter uses. Discovery
/// failures surface as no advertised models, never a guessed list.
pub(crate) fn discover_catalog(provider: ProviderKey) -> Vec<ProviderCatalogModel> {
    let discovery = thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_io()
                    .enable_time()
                    .build()
                    .ok()?;
                Some(runtime.block_on(discover_model_catalog(provider)))
            })
            .join()
    });
    match discovery {
        Ok(Some(ProviderCatalogDiscovery::Available { models, .. })) => models,
        _ => Vec::new(),
    }
}

/// An adapter's live sessions, keyed by contract SessionId.
pub(crate) type SessionMap<S> = Mutex<BTreeMap<SessionId, Arc<S>>>;

/// The operations the adapter-side plumbing drives on a live session,
/// uniform across the three local adapters: read the provider resume
/// cursor, apply a resolved `model.select`, hand out the event receiver
/// once, and tear the session down gracefully or best-effort.
pub(crate) trait LocalSession {
    /// The provider resume cursor the session reported, if any.
    fn resume_cursor(&self) -> Option<String>;
    /// Apply an already-resolved selection; a closed session or an
    /// in-flight run rejects the change.
    fn set_selection(&self, selection: ModelSelection) -> Result<(), AgentRuntimeError>;
    /// The raw event receiver, handed out once.
    fn take_events(&self) -> Option<mpsc::Receiver<AgentEventKind>>;
    /// Graceful teardown behind `stop`: the session's terminal facts land
    /// before this returns.
    fn shutdown(&self);
    /// Best-effort teardown kick behind adapter `Drop`: mark closed,
    /// cancel in-flight work, and kill the provider transport.
    fn close_transport(&self);
}

/// The live session behind `session_id`, or `unknown_session`.
pub(crate) fn live_session<S>(
    sessions: &SessionMap<S>,
    session_id: &SessionId,
) -> Result<Arc<S>, AgentRuntimeError> {
    sessions
        .lock()
        .expect("sessions mutex")
        .get(session_id)
        .cloned()
        .ok_or_else(unknown_session)
}

/// The resume cursor a live session reported, `None` for unknown or
/// already-stopped sessions.
pub(crate) fn resume_cursor<S: LocalSession>(
    sessions: &SessionMap<S>,
    session_id: &SessionId,
) -> Option<String> {
    sessions
        .lock()
        .expect("sessions mutex")
        .get(session_id)
        .and_then(|session| session.resume_cursor())
}

/// Resolve the selection, then apply it to the live session. Only a
/// resolvable selection reaches the session, and a session with a run in
/// flight rejects the change.
pub(crate) fn set_selection<A, S>(
    adapter: &A,
    sessions: &SessionMap<S>,
    handle: &AgentSessionHandle,
    selection: ModelSelection,
) -> Result<ModelDescriptor, AgentRuntimeError>
where
    A: AgentAdapter + ?Sized,
    S: LocalSession,
{
    let descriptor = adapter.resolve(&selection)?;
    live_session(sessions, &handle.session_id)?.set_selection(selection)?;
    Ok(descriptor)
}

/// The session's event receiver as the contract stream; an unknown
/// session or an already-taken receiver yields an empty stream.
pub(crate) fn event_stream<S: LocalSession>(
    sessions: &SessionMap<S>,
    session_id: &SessionId,
) -> AgentEventStream {
    sessions
        .lock()
        .expect("sessions mutex")
        .get(session_id)
        .and_then(|session| session.take_events())
        .map(|events| Box::new(events.into_iter()) as AgentEventStream)
        .unwrap_or_else(|| Box::new(std::iter::empty()))
}

/// Remove the session and shut it down: the run's terminal facts and
/// `session.closed` land in order before the handle is released.
pub(crate) fn stop<S: LocalSession>(
    sessions: &SessionMap<S>,
    session_id: &SessionId,
) -> Result<(), AgentRuntimeError> {
    let session = sessions
        .lock()
        .expect("sessions mutex")
        .remove(session_id)
        .ok_or_else(unknown_session)?;
    session.shutdown();
    Ok(())
}

/// Adapter-drop teardown: every live session gets the best-effort kick
/// so drivers unblock and pending waiters release.
pub(crate) fn close_all<S: LocalSession>(sessions: &SessionMap<S>) {
    let sessions = sessions.lock().expect("sessions mutex");
    for session in sessions.values() {
        session.close_transport();
    }
}

/// The raw event receiver, for tests that need bounded reads the boxed
/// iterator cannot express.
#[cfg(test)]
pub(crate) fn test_events<S: LocalSession>(
    sessions: &SessionMap<S>,
    session_id: &SessionId,
) -> Option<mpsc::Receiver<AgentEventKind>> {
    sessions
        .lock()
        .expect("sessions mutex")
        .get(session_id)
        .and_then(|session| session.take_events())
}

/// Join a finished-or-finishing thread with a bound: a live driver exits
/// well inside it, and a wedged thread is left running rather than
/// blocking teardown forever.
pub(crate) fn join_bounded(handle: Option<JoinHandle<()>>) {
    const THREAD_JOIN_TIMEOUT: Duration = Duration::from_secs(5);
    if let Some(handle) = handle {
        let deadline = Instant::now() + THREAD_JOIN_TIMEOUT;
        while !handle.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if handle.is_finished() {
            let _ = handle.join();
        }
    }
}

fn unknown_session() -> AgentRuntimeError {
    AgentRuntimeError::new(
        ReceiptCode::UnknownSession,
        "the session id is not live in this adapter",
    )
}

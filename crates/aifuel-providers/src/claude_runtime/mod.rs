//! The Claude Code [`AgentAdapter`].
//!
//! One [`ClaudeAdapter`] speaks the `claude` CLI's bidirectional
//! stream-json protocol directly. Each Agent Session owns one `claude`
//! process started with `-p --input-format stream-json
//! --output-format stream-json`; each [`send`](aifuel_core::AgentAdapter::send)
//! writes one `user` frame on its stdin. A per-session driver thread
//! owns the transport, the process lifetime, and every run-scoped
//! emission, so event order follows the run's causal order exactly as
//! the protocol reports it.
//!
//! Declarations never exceed observed evidence:
//!
//! - `streaming` comes from `--include-partial-messages`
//!   `stream_event` text and thinking deltas, with the completed
//!   `assistant` frame as the fallback answer path.
//! - `resume` is the provider's `--resume <session-id>`; the session id
//!   `system`/`init` reports is the resume cursor persisted on the
//!   session handle.
//! - `approvals` answers `can_use_tool` control requests through
//!   `--permission-prompt-tool stdio` and error-replies every other
//!   control subtype, never silently approving.
//! - `effort` maps onto the `--effort` flag; `images` stays `false`
//!   because attachment content blocks are not wired, and `todos`
//!   stays `false` because no provider task-list frame is mapped.
//! - `checkpoints` stays `false`: the runtime owns the git-ref feature.
//! - `models.list` stays empty: Claude exposes no verified catalog
//!   interface, so no model list is fabricated. `resolve` still
//!   validates a requested effort against the spellings `--effort`
//!   accepts.

use aifuel_core::{
    AdapterCapabilities, AgentAdapter, AgentExecutionAdapter, AgentIntegrationInfo,
    AgentRuntimeError, DiscoveryError, DiscoveryState, ExecutionAvailability, IntegrationId,
    ModelDescriptor, ModelSelection, ProviderId, ProviderKey, ReceiptCode, SessionId,
};
use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

mod adapter;
mod descriptors;
mod driver;
mod protocol;
mod session;
#[cfg(test)]
mod tests;

use crate::cli_adapter::AdapterDiscovery;
use crate::integrations::{EvidenceContext, EvidenceSource, builtin_integrations, inspect_any};
use crate::model_catalog::{
    ProviderCatalogDiscovery, ProviderCatalogModel, discover_model_catalog,
};
use session::ClaudeSession;

/// Local evidence attached to the adapter: the descriptor's declared
/// sources plus the context to inspect them with.
struct AdapterEvidence {
    sources: Vec<EvidenceSource>,
    context: AdapterDiscovery,
}

/// An honest [`AgentAdapter`](aifuel_core::AgentAdapter) over the
/// Claude Code stream-json protocol.
///
/// The adapter serves exactly the integration it is built for:
/// requests for any other Integration Identity fail, never silently
/// route to another provider, model, or credential.
pub struct ClaudeAdapter {
    integration: IntegrationId,
    capabilities: AdapterCapabilities,
    evidence: Option<AdapterEvidence>,
    /// Probed once: native inspection spawns bounded provider commands
    /// and must not repeat per call.
    agent_info: OnceLock<AgentIntegrationInfo>,
    /// The provider's Advertised Model catalog, discovered once on
    /// first use. Discovery failures surface as no advertised models,
    /// never a guessed list.
    catalog: OnceLock<Vec<ProviderCatalogModel>>,
    sessions: Mutex<BTreeMap<String, Arc<ClaudeSession>>>,
    next_id: AtomicU64,
    /// The transport factory the session driver asks for a `claude`
    /// I/O pair; production spawns the process, tests inject a scripted
    /// duplex.
    connector: session::Connector,
}

impl Default for ClaudeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeAdapter {
    /// An adapter serving the built-in `claude` integration.
    pub fn new() -> Self {
        Self::build(IntegrationId::from(ProviderKey::Claude))
    }

    /// Serve a different Integration Identity bound to the Claude
    /// executable, for configured integrations the host registers.
    pub fn with_integration(mut self, integration: IntegrationId) -> Self {
        self.integration = integration;
        self
    }

    fn build(integration: IntegrationId) -> Self {
        Self {
            integration,
            capabilities: CAPABILITIES,
            evidence: None,
            agent_info: OnceLock::new(),
            catalog: OnceLock::new(),
            sessions: Mutex::new(BTreeMap::new()),
            next_id: AtomicU64::new(0),
            connector: session::default_connector(),
        }
    }

    /// Attach the integration descriptor's discovery evidence sources
    /// and the local context they inspect.
    pub fn with_discovery(
        mut self,
        sources: Vec<EvidenceSource>,
        context: AdapterDiscovery,
    ) -> Self {
        self.evidence = Some(AdapterEvidence { sources, context });
        self
    }

    /// The configured integration this adapter serves.
    pub fn integration(&self) -> IntegrationId {
        self.integration.clone()
    }

    /// The upstream provider this integration executes against.
    pub fn provider(&self) -> ProviderId {
        ProviderId::from(ProviderKey::Claude)
    }

    /// Readiness to attempt a run through this integration, derived
    /// from the probed native evidence plus attached discovery
    /// sources.
    pub fn availability(&self) -> ExecutionAvailability {
        descriptors::availability_from(self.agent_info(), self.discovered())
    }

    /// The probed integration evidence, cached after the first
    /// inspection because probing spawns bounded provider commands.
    pub fn agent_info(&self) -> &AgentIntegrationInfo {
        self.agent_info
            .get_or_init(|| crate::claude::AGENT_RUN_ADAPTER.agent_info())
    }

    /// The resume cursor a live session continues from: the claude
    /// session id `system`/`init` reported at session start.
    pub fn resume_cursor(&self, session_id: &SessionId) -> Option<String> {
        let sessions = self.sessions.lock().expect("sessions mutex");
        sessions
            .get(session_id.as_str())
            .and_then(|session| session.resume_cursor())
    }

    /// Apply a resolved `model.select` to one session. The selection
    /// is resolved first; only a resolvable selection replaces the
    /// stored one, and a session with a run in flight rejects the
    /// change. The wire `set_model` request must land before the
    /// selection stores.
    ///
    /// This is an inherent method because [`AgentAdapter`] keeps
    /// selection inside session state; the facade resolves then
    /// applies it here.
    pub fn set_selection(
        &self,
        handle: &aifuel_core::AgentSessionHandle,
        selection: ModelSelection,
    ) -> Result<ModelDescriptor, AgentRuntimeError> {
        let descriptor = self.resolve(&selection)?;
        self.session(&handle.session_id)?.set_selection(selection)?;
        Ok(descriptor)
    }

    /// Inspect the attached evidence sources for this integration.
    /// `None` means no discovery context was attached, which reads as
    /// `unknown` availability rather than absent evidence.
    fn discovered(&self) -> Option<Result<DiscoveryState, DiscoveryError>> {
        let evidence = self.evidence.as_ref()?;
        let context = EvidenceContext {
            discovery: &evidence.context.discovery,
            credentials: &evidence.context.credentials,
            configured: &evidence.context.configured,
        };
        Some(inspect_any(
            &evidence.sources,
            &self.integration(),
            &context,
        ))
    }

    /// The advertised catalog, discovered once. Discovery failures
    /// are not retried and surface as no advertised models, never a
    /// guessed list.
    fn catalog_models(&self) -> &[ProviderCatalogModel] {
        self.catalog.get_or_init(|| self.discover_catalog())
    }

    fn discover_catalog(&self) -> Vec<ProviderCatalogModel> {
        // The catalog interface is async; run it on a short-lived
        // runtime on a scoped thread, the same pattern the CLI
        // adapter uses.
        let discovery = thread::scope(|scope| {
            scope
                .spawn(|| {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_io()
                        .enable_time()
                        .build()
                        .ok()?;
                    Some(runtime.block_on(discover_model_catalog(ProviderKey::Claude)))
                })
                .join()
        });
        match discovery {
            Ok(Some(ProviderCatalogDiscovery::Available { models, .. })) => models,
            _ => Vec::new(),
        }
    }

    fn ensure_serves(&self, integration: &IntegrationId) -> Result<(), AgentRuntimeError> {
        if *integration == self.integration() {
            Ok(())
        } else {
            Err(AgentRuntimeError::unsupported(format!(
                "this adapter serves integration {} only",
                self.integration()
            )))
        }
    }

    fn session(&self, session_id: &SessionId) -> Result<Arc<ClaudeSession>, AgentRuntimeError> {
        self.sessions
            .lock()
            .expect("sessions mutex")
            .get(session_id.as_str())
            .cloned()
            .ok_or_else(|| {
                AgentRuntimeError::new(
                    ReceiptCode::UnknownSession,
                    "the session id is not live in this adapter",
                )
            })
    }

    /// The raw event receiver, for tests that need bounded reads the
    /// boxed iterator cannot express.
    #[cfg(test)]
    pub(super) fn test_events(
        &self,
        handle: &aifuel_core::AgentSessionHandle,
    ) -> Option<std::sync::mpsc::Receiver<aifuel_core::AgentEventKind>> {
        self.sessions
            .lock()
            .expect("sessions mutex")
            .get(handle.session_id.as_str())
            .and_then(|session| session.take_events())
    }

    /// An adapter whose sessions speak over the given transport
    /// factory instead of spawning `claude`. Tests inject a scripted
    /// duplex endpoint here and play the provider side of the
    /// protocol.
    #[cfg(test)]
    pub(in crate::claude_runtime) fn with_connector(
        mut self,
        connector: session::Connector,
    ) -> Self {
        self.connector = connector;
        self
    }

    /// Pre-seed the native inspection evidence, for tests that must
    /// not probe a real `claude` binary.
    #[cfg(test)]
    pub(crate) fn with_agent_info(self, info: AgentIntegrationInfo) -> Self {
        let _ = self.agent_info.set(info);
        self
    }
}

impl Drop for ClaudeAdapter {
    /// Dropping the adapter closes every live session: the driver
    /// exits when its `claude` process dies, and pending approval
    /// waiters release with the session state.
    fn drop(&mut self) {
        let sessions = self.sessions.lock().expect("sessions mutex");
        for session in sessions.values() {
            session.close_transport();
        }
    }
}

/// One [`ClaudeAdapter`] for the built-in `claude` integration
/// descriptor, paired with its declared discovery evidence, mirroring
/// [`cli_fallback_adapters`](crate::cli_adapter::cli_fallback_adapters).
/// The runtime registration itself lands with the parent wiring.
pub fn claude_adapter(context: &AdapterDiscovery) -> ClaudeAdapter {
    let adapter = ClaudeAdapter::new();
    match builtin_integrations()
        .iter()
        .find(|descriptor| descriptor.integration.id == adapter.integration())
    {
        Some(descriptor) => adapter.with_discovery(descriptor.sources.clone(), context.clone()),
        None => adapter,
    }
}

/// The honest declarations this adapter can surface. Sessions stream
/// message deltas, resume provider sessions, answer `can_use_tool`
/// permission requests, and carry effort through `--effort`.
/// Checkpoints stay unimplemented: the runtime owns that feature.
/// `images` and `todos` stay false because neither wire shape is
/// mapped.
pub const CAPABILITIES: AdapterCapabilities = AdapterCapabilities {
    streaming: true,
    resume: true,
    approvals: true,
    checkpoints: false,
    effort: true,
    images: false,
    todos: false,
};

/// The access spellings `--permission-mode` carries per session. The
/// mapping matches the existing one-shot `claude` run path: read-only
/// is `plan`, workspace write is `acceptEdits`, and full access is
/// `bypassPermissions`.
pub(crate) fn permission_mode(access: aifuel_core::AccessMode) -> &'static str {
    match access {
        aifuel_core::AccessMode::ReadOnly => "plan",
        aifuel_core::AccessMode::WorkspaceWrite => "acceptEdits",
        aifuel_core::AccessMode::Full => "bypassPermissions",
    }
}

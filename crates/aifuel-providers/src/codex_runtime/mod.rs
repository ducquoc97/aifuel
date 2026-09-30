//! The Codex App Server [`AgentAdapter`](aifuel_core::AgentAdapter).
//!
//! One [`CodexAdapter`] speaks the persistent `codex app-server` JSON-RPC
//! protocol directly. Each Agent Session owns one app-server process
//! holding one Codex thread; each [`send`](aifuel_core::AgentAdapter::send)
//! starts one `turn/*` on it. A per-session driver thread owns the
//! transport, the process lifetime, and every run-scoped emission, so
//! event order follows the run's causal order exactly as the protocol
//! reports it.
//!
//! Declarations never exceed observed evidence:
//!
//! - `streaming` comes from `item/agentMessage/delta` and the reasoning
//!   and plan delta notifications.
//! - `resume` is the provider's `thread/resume`; the thread id is the
//!   resume cursor persisted on the session handle.
//! - `approvals` answers every request kind the existing interaction
//!   normalization covers and rejects anything else at the protocol
//!   layer, never silently approving.
//! - `effort` and `images` map onto native `turn/start` fields (`effort`,
//!   `localImage` inputs); `todos` maps `turn/plan/updated`.
//! - `external_tools` injects the filtered AI Fuel Gateway as the
//!   thread's only `mcp_servers` entry and gates setup on
//!   `mcpServerStatus/list` reporting exactly the selected tools; a
//!   session never runs on a partial or wider tool set.
//! - `checkpoints` stays `false`: the runtime owns the git-ref feature.
//! - Quota stays `None`: token accounting feeds `run.completed` usage
//!   only; Quota Pool observations come from the monitoring contract,
//!   not this adapter.

use aifuel_core::{
    AdapterCapabilities, AgentExecutionAdapter, AgentIntegrationInfo, AgentRuntimeError,
    ExecutionAvailability, IntegrationId, ModelDescriptor, ModelSelection, ProviderId, ProviderKey,
    SessionId,
};
use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, OnceLock};

mod adapter;
mod driver;
mod interactions;
mod mapping;
mod session;
#[cfg(test)]
mod tests;

use crate::integrations::{EvidenceSource, builtin_integrations};
use crate::local_adapter::descriptors;
use crate::local_adapter::{self, AdapterDiscovery, AdapterEvidence, SessionMap};
use crate::model_catalog::ProviderCatalogModel;
use session::CodexSession;

/// An honest [`AgentAdapter`](aifuel_core::AgentAdapter) over the Codex
/// App Server protocol.
///
/// The adapter serves exactly the integration it is built for: requests
/// for any other Integration Identity fail, never silently route to
/// another provider, model, or credential.
pub struct CodexAdapter {
    integration: IntegrationId,
    capabilities: AdapterCapabilities,
    evidence: Option<AdapterEvidence>,
    /// Probed once: native inspection spawns bounded provider commands
    /// and must not repeat per call.
    agent_info: OnceLock<AgentIntegrationInfo>,
    /// The provider's Advertised Model catalog, discovered once on first
    /// use. Discovery failures surface as no advertised models, never a
    /// guessed list.
    catalog: OnceLock<Vec<ProviderCatalogModel>>,
    sessions: SessionMap<CodexSession>,
    next_id: AtomicU64,
    /// The transport factory the session driver asks for an app-server
    /// I/O pair; production spawns the process, tests inject a duplex.
    connector: session::Connector,
}

impl Default for CodexAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexAdapter {
    /// An adapter serving the built-in `codex` integration.
    pub fn new() -> Self {
        Self::build(IntegrationId::from(ProviderKey::Codex))
    }

    /// Serve a different Integration Identity bound to the Codex
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
            sessions: SessionMap::new(BTreeMap::new()),
            next_id: AtomicU64::new(0),
            connector: session::default_connector(),
        }
    }

    /// Attach the integration descriptor's discovery evidence sources and
    /// the local context they inspect.
    pub fn with_discovery(
        mut self,
        sources: Vec<EvidenceSource>,
        context: AdapterDiscovery,
    ) -> Self {
        self.evidence = Some(AdapterEvidence::new(sources, context));
        self
    }

    /// The configured integration this adapter serves.
    pub fn integration(&self) -> IntegrationId {
        self.integration.clone()
    }

    /// The upstream provider this integration executes against.
    pub fn provider(&self) -> ProviderId {
        ProviderId::from(ProviderKey::Codex)
    }

    /// Readiness to attempt a run through this integration, derived from
    /// the probed native evidence plus attached discovery sources.
    pub fn availability(&self) -> ExecutionAvailability {
        descriptors::availability_from(
            self.agent_info(),
            local_adapter::discovered(self.evidence.as_ref(), &self.integration()),
        )
    }

    /// The probed integration evidence, cached after the first
    /// inspection because probing spawns bounded provider commands.
    pub fn agent_info(&self) -> &AgentIntegrationInfo {
        self.agent_info
            .get_or_init(|| crate::codex::AGENT_RUN_ADAPTER.agent_info())
    }

    /// The resume cursor a live session continues from: the Codex thread
    /// id `thread/start` or `thread/resume` reported at session start.
    pub fn resume_cursor(&self, session_id: &SessionId) -> Option<String> {
        local_adapter::resume_cursor(&self.sessions, session_id)
    }

    /// Apply a resolved `model.select` to one session. The selection is
    /// resolved first; only a resolvable selection replaces the stored
    /// one, and a session with a run in flight rejects the change.
    ///
    /// This is an inherent method because [`AgentAdapter`](aifuel_core::AgentAdapter)
    /// keeps
    /// selection inside session state; the facade resolves then applies
    /// it here.
    pub fn set_selection(
        &self,
        handle: &aifuel_core::AgentSessionHandle,
        selection: ModelSelection,
    ) -> Result<ModelDescriptor, AgentRuntimeError> {
        local_adapter::set_selection(self, &self.sessions, handle, selection)
    }

    /// The advertised catalog, discovered once. Discovery failures are
    /// not retried and surface as no advertised models, never a
    /// guessed list.
    fn catalog_models(&self) -> &[ProviderCatalogModel] {
        self.catalog
            .get_or_init(|| local_adapter::discover_catalog(ProviderKey::Codex))
    }

    fn ensure_serves(&self, integration: &IntegrationId) -> Result<(), AgentRuntimeError> {
        local_adapter::ensure_serves(&self.integration(), integration)
    }

    fn session(&self, session_id: &SessionId) -> Result<Arc<CodexSession>, AgentRuntimeError> {
        local_adapter::live_session(&self.sessions, session_id)
    }

    /// The raw event receiver, for tests that need bounded reads the
    /// boxed iterator cannot express.
    #[cfg(test)]
    pub(super) fn test_events(
        &self,
        handle: &aifuel_core::AgentSessionHandle,
    ) -> Option<std::sync::mpsc::Receiver<aifuel_core::AgentEventKind>> {
        local_adapter::test_events(&self.sessions, &handle.session_id)
    }

    /// An adapter whose sessions speak over the given transport factory
    /// instead of spawning `codex app-server`. Tests inject a duplex
    /// endpoint here and play the server side of the protocol.
    #[cfg(test)]
    pub(in crate::codex_runtime) fn with_connector(
        mut self,
        connector: session::Connector,
    ) -> Self {
        self.connector = connector;
        self
    }

    /// Pre-seed the probed integration evidence so tests never spawn
    /// provider binaries.
    #[cfg(test)]
    pub(super) fn with_agent_info(self, info: AgentIntegrationInfo) -> Self {
        let _ = self.agent_info.set(info);
        self
    }

    /// Pre-seed the advertised catalog so tests never spawn provider
    /// binaries.
    #[cfg(test)]
    pub(super) fn with_catalog(self, models: Vec<ProviderCatalogModel>) -> Self {
        let _ = self.catalog.set(models);
        self
    }
}

impl Drop for CodexAdapter {
    /// Dropping the adapter closes every live session: the driver exits
    /// when its app-server process dies, and pending approval waiters
    /// release with the session state.
    fn drop(&mut self) {
        local_adapter::close_all(&self.sessions);
    }
}

/// One [`CodexAdapter`] for the built-in `codex` integration descriptor,
/// paired with its declared discovery evidence, mirroring
/// [`cli_fallback_adapters`](crate::cli_adapter::cli_fallback_adapters).
/// The runtime registration itself lands with the parent wiring.
pub fn codex_adapter(context: &AdapterDiscovery) -> CodexAdapter {
    let adapter = CodexAdapter::new();
    match builtin_integrations()
        .iter()
        .find(|descriptor| descriptor.integration.id == adapter.integration())
    {
        Some(descriptor) => adapter.with_discovery(descriptor.sources.clone(), context.clone()),
        None => adapter,
    }
}

/// The honest declarations this adapter can surface. Sessions stream
/// message deltas, resume provider threads, answer every supported
/// approval kind, and report todos and effort the protocol carries.
/// External tools are enforced by the managed Gateway MCP server the
/// handshake registers and polls ready. Checkpoints stay unimplemented:
/// the runtime owns that feature.
pub const CAPABILITIES: AdapterCapabilities = AdapterCapabilities {
    streaming: true,
    resume: true,
    approvals: true,
    checkpoints: false,
    effort: true,
    images: true,
    todos: true,
    external_tools: true,
};

/// The access spellings `turn/start` carries per turn: the Codex
/// `SandboxPolicy` object and the approval policy the host declared.
pub(crate) fn sandbox_policy(access: aifuel_core::AccessMode) -> serde_json::Value {
    let policy = match access {
        aifuel_core::AccessMode::ReadOnly => "readOnly",
        aifuel_core::AccessMode::WorkspaceWrite => "workspaceWrite",
        aifuel_core::AccessMode::Full => "dangerFullAccess",
    };
    serde_json::json!({"type": policy})
}

/// The thread-level sandbox spelling `thread/start` and `thread/resume`
/// carry, matching the existing app-server run path.
pub(crate) fn thread_sandbox(access: aifuel_core::AccessMode) -> &'static str {
    match access {
        aifuel_core::AccessMode::ReadOnly => "read-only",
        aifuel_core::AccessMode::WorkspaceWrite => "workspace-write",
        aifuel_core::AccessMode::Full => "danger-full-access",
    }
}

/// Approval Requests stay opt-in at every access level: the provider
/// asks, the host answers, and `Full`'s danger-full-access sandbox makes
/// the provider auto-approve rather than this adapter.
pub(crate) const APPROVAL_POLICY: &str = "on-request";

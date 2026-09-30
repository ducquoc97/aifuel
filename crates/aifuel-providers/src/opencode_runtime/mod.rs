//! The OpenCode serve [`AgentAdapter`](aifuel_core::AgentAdapter).
//!
//! One [`OpenCodeAdapter`] drives the provider's headless HTTP server.
//! Each Agent Session owns one `opencode serve --hostname 127.0.0.1
//! --port <n>` process and talks to it over its documented API surface:
//! `POST /session`, `GET /session/{id}`, `POST /session/{id}/message`,
//! `POST /session/{id}/abort`,
//! `POST /session/{id}/permissions/{permissionID}`, `GET /provider`, and
//! the `GET /event` server-sent-event stream. A per-session driver
//! thread owns the process lifetime, the event stream, and every
//! run-scoped emission, so event order follows the run's causal order.
//! Every request carries the session's `?directory=` so the server
//! routes it to the right project instance.
//!
//! Declarations never exceed observed evidence:
//!
//! - `streaming` comes from `message.part.updated` text and reasoning
//!   part events on the `/event` stream.
//! - `resume` reattaches a persisted OpenCode session id through
//!   `GET /session/{id}`; the session id is the resume cursor.
//! - `approvals` answers `permission.updated` asks through the
//!   permissions endpoint with the provider's `once`/`always`/`reject`
//!   vocabulary; nothing is auto-approved.
//! - `images` maps image and file attachments onto `file` prompt parts.
//! - `todos` maps `todo.updated` payloads.
//! - `effort` stays `false`: OpenCode models expose a `reasoning`
//!   capability flag, not a selectable effort ladder.
//! - `checkpoints` stays `false`: the runtime owns the git-ref feature;
//!   OpenCode's `revert` is a provider session operation, not the
//!   contract's workspace Checkpoint.
//! - Quota stays `None`: token accounting feeds `run.completed` usage
//!   only.

use aifuel_core::{
    AdapterCapabilities, AgentIntegrationInfo, AgentRuntimeError, ExecutionAvailability,
    IntegrationId, ModelDescriptor, ModelSelection, ProviderId, SessionId,
};
use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, OnceLock};

mod adapter;
mod catalog;
mod driver;
mod protocol;
mod serve;
mod session;
#[cfg(test)]
mod tests;

use crate::agent_execution::{ExecutionCapabilities, inspect_agent};
use crate::integrations::{EvidenceSource, builtin_integrations};
use crate::local_adapter::descriptors;
use crate::local_adapter::{self, AdapterDiscovery, AdapterEvidence, SessionMap};
use crate::model_catalog::ProviderCatalogModel;
use session::OpenCodeSession;

/// The provider and integration identity this adapter serves by
/// default.
pub const PROVIDER_ID: &str = "opencode";

/// An honest [`AgentAdapter`](aifuel_core::AgentAdapter) over the
/// `opencode serve` HTTP API.
///
/// The adapter serves exactly the integration it is built for: requests
/// for any other Integration Identity fail, never silently route to
/// another provider, model, or credential.
pub struct OpenCodeAdapter {
    integration: IntegrationId,
    capabilities: AdapterCapabilities,
    evidence: Option<AdapterEvidence>,
    /// Probed once: native inspection spawns bounded provider commands
    /// and must not repeat per call.
    agent_info: OnceLock<AgentIntegrationInfo>,
    /// The provider's Advertised Model catalog plus the connected
    /// provider ids `/provider` reported, discovered once on first use.
    /// Discovery failures surface as no advertised models, never a
    /// guessed list.
    catalog: OnceLock<catalog::CatalogResult>,
    sessions: SessionMap<OpenCodeSession>,
    next_id: AtomicU64,
    /// The transport factory the session driver asks for a serve
    /// process; production spawns `opencode serve`, tests inject a fake
    /// HTTP server.
    connector: serve::Connector,
}

impl Default for OpenCodeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenCodeAdapter {
    /// An adapter serving the built-in `opencode` integration.
    pub fn new() -> Self {
        Self::build(IntegrationId::new(PROVIDER_ID))
    }

    /// Serve a different Integration Identity bound to the OpenCode
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
            connector: serve::default_connector(),
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
        ProviderId::new(PROVIDER_ID)
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
        self.agent_info.get_or_init(|| {
            inspect_agent(
                ProviderId::new(PROVIDER_ID),
                "opencode",
                Some(&["--version"]),
                None,
                declared().evidence(),
            )
        })
    }

    /// The resume cursor a live session continues from: the OpenCode
    /// session id the server created or `GET /session/{id}` verified.
    pub fn resume_cursor(&self, session_id: &SessionId) -> Option<String> {
        local_adapter::resume_cursor(&self.sessions, session_id)
    }

    /// Apply a resolved `model.select` to one session. The selection is
    /// resolved first; only a resolvable selection replaces the stored
    /// one, and a session with a run in flight rejects the change.
    ///
    /// This is an inherent method because [`AgentAdapter`](aifuel_core::AgentAdapter)
    /// keeps selection inside session state; the facade resolves then
    /// applies it here.
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
        &self.catalog_result().models
    }

    /// Provider ids `/provider` reported as connected; models on a
    /// connected provider are the only entitlement evidence the serve
    /// API exposes.
    fn entitlements(&self) -> BTreeMap<String, aifuel_core::CapabilityState> {
        let connected = &self.catalog_result().connected;
        self.catalog_models()
            .iter()
            .map(|model| {
                let state = if model
                    .model_id
                    .split_once('/')
                    .is_some_and(|(provider, _)| connected.contains(provider))
                {
                    aifuel_core::CapabilityState::Supported
                } else {
                    aifuel_core::CapabilityState::Unknown
                };
                (model.model_id.clone(), state)
            })
            .collect()
    }

    fn catalog_result(&self) -> &catalog::CatalogResult {
        self.catalog.get_or_init(catalog::discover)
    }

    fn ensure_serves(&self, integration: &IntegrationId) -> Result<(), AgentRuntimeError> {
        local_adapter::ensure_serves(&self.integration(), integration)
    }

    fn session(&self, session_id: &SessionId) -> Result<Arc<OpenCodeSession>, AgentRuntimeError> {
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

    /// An adapter whose sessions talk to the given serve endpoint
    /// instead of spawning `opencode serve`. Tests inject a fake HTTP
    /// server here and script the API surface.
    #[cfg(test)]
    pub(in crate::opencode_runtime) fn with_connector(
        mut self,
        connector: serve::Connector,
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
    pub(super) fn with_catalog(self, result: catalog::CatalogResult) -> Self {
        let _ = self.catalog.set(result);
        self
    }
}

impl Drop for OpenCodeAdapter {
    /// Dropping the adapter closes every live session: the driver exits
    /// when its serve process dies, and pending approval waiters
    /// release with the session state.
    fn drop(&mut self) {
        local_adapter::close_all(&self.sessions);
    }
}

/// One [`OpenCodeAdapter`] for the built-in `opencode` integration
/// descriptor, paired with its declared discovery evidence, mirroring
/// [`cli_fallback_adapters`](crate::cli_adapter::cli_fallback_adapters).
/// The runtime registration itself lands with the parent wiring.
pub fn opencode_adapter(context: &AdapterDiscovery) -> OpenCodeAdapter {
    let adapter = OpenCodeAdapter::new();
    match builtin_integrations()
        .iter()
        .find(|descriptor| descriptor.integration.id == adapter.integration())
    {
        Some(descriptor) => adapter.with_discovery(descriptor.sources.clone(), context.clone()),
        None => adapter,
    }
}

/// The honest declarations this adapter can surface. Sessions stream
/// message deltas over SSE, resume persisted provider sessions, answer
/// permission asks, and report tool, todo, and usage events the API
/// carries. Effort stays undeclared: the provider's `reasoning`
/// capability is a model flag, not a selectable effort. Checkpoints
/// stay unimplemented: the runtime owns that feature.
pub const CAPABILITIES: AdapterCapabilities = AdapterCapabilities {
    streaming: true,
    resume: true,
    approvals: true,
    checkpoints: false,
    effort: false,
    images: true,
    todos: true,
};

/// The declared execution evidence `list_agents` reports for this
/// adapter. Read-only is enforced by routing the session's prompts to
/// OpenCode's `plan` agent; approval requests are surfaced rather than
/// auto-approved; the catalog is the provider's own `/provider` list.
pub(crate) fn declared() -> ExecutionCapabilities {
    ExecutionCapabilities::new(
        /* supports_resume */ true, /* supports_account_selection */ false,
        /* supports_workspace_write */ true, /* supports_jsonl */ false,
    )
    .with_model_catalog()
    .with_streaming()
    .with_read_only()
    .with_permission_approval()
}

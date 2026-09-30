//! The Agent Client Protocol [`AgentAdapter`](aifuel_core::AgentAdapter).
//!
//! One [`AcpAdapter`] speaks ACP v1: JSON-RPC 2.0 over line-delimited
//! stdio. Each Agent Session owns one agent process spawned as
//! `program args` - `cursor-agent acp` for the built-in Cursor
//! integration, any other descriptor-configured binary for further
//! ACP-speaking agents - holding one provider session created by
//! `session/new` or `session/load`. Each
//! [`send`](aifuel_core::AgentAdapter::send) starts one `session/prompt`
//! on it; `session/update` notifications stream the run and
//! `session/request_permission` becomes an Approval Request. A
//! per-session driver thread owns the transport, the process lifetime,
//! and every run-scoped emission, so event order follows the run's
//! causal order exactly as the protocol reports it.
//!
//! One adapter covers every ACP-speaking agent: the protocol is
//! provider-neutral and spawn specifics stay in the descriptor.
//!
//! Declarations never exceed observed evidence:
//!
//! - `streaming` comes from `agent_message_chunk` and
//!   `agent_thought_chunk` `session/update` notifications.
//! - `resume` is the protocol's `session/load`; the ACP `sessionId` is
//!   the resume cursor persisted on the session handle. An agent that
//!   does not advertise `loadSession` fails a resumed `start` honestly.
//! - `approvals` answers `session/request_permission` through the
//!   shared permission policy: `accept` maps only to a real
//!   `allow_once` option, never to `allow_always`; unsupported or
//!   runless asks are answered `cancelled` or rejected at the protocol
//!   layer, never silently approved.
//! - `images` maps image attachments onto `image` content blocks when
//!   the agent advertises `promptCapabilities.image`, degrading to a
//!   `resource_link` block otherwise; other attachments are
//!   `resource_link` blocks, which every ACP agent accepts.
//! - `todos` maps `plan` notifications.
//! - `effort` stays `false`: ACP has no effort selection.
//! - `checkpoints` stays `false`: the runtime owns the git-ref feature.
//! - The `fs/read_text_file` and `fs/write_text_file` client services
//!   are advertised and served against the session workspace boundary;
//!   writes additionally require a non-read-only session. `terminal`
//!   is not implemented and stays unadvertised.
//! - Quota stays `None`: `usage_update` token accounting feeds
//!   `run.completed` usage only.

use aifuel_core::{
    AdapterCapabilities, AgentIntegrationInfo, AgentRuntimeError, ExecutionAvailability,
    IntegrationId, ModelDescriptor, ModelSelection, ProviderId, SessionId,
};
use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, OnceLock};

mod adapter;
mod driver;
mod interactions;
mod mapping;
mod protocol;
mod session;
#[cfg(test)]
mod tests;

use crate::agent_execution::{ExecutionCapabilities, inspect_agent};
use crate::integrations::{EvidenceSource, builtin_integrations};
use crate::local_adapter::descriptors;
use crate::local_adapter::{self, AdapterDiscovery, AdapterEvidence, SessionMap};
use session::AcpSession;

/// The provider and integration identity the built-in Cursor
/// integration carries.
pub const CURSOR_ID: &str = "cursor";
/// The executable the Cursor integration spawns.
pub const CURSOR_PROGRAM: &str = "cursor-agent";
/// The argv the Cursor integration spawns it with: ACP server mode.
pub const CURSOR_ARGS: &[&str] = &["acp"];

/// An honest [`AgentAdapter`](aifuel_core::AgentAdapter) over the Agent
/// Client Protocol.
///
/// The adapter serves exactly the integration it is built for: requests
/// for any other Integration Identity fail, never silently route to
/// another provider, model, or credential. The spawn program and argv
/// are the only per-agent specifics; the protocol implementation is
/// shared.
pub struct AcpAdapter {
    integration: IntegrationId,
    provider: ProviderId,
    /// The agent executable and argv every session spawn uses.
    program: String,
    args: Vec<String>,
    capabilities: AdapterCapabilities,
    evidence: Option<AdapterEvidence>,
    /// Probed once: native inspection spawns bounded provider commands
    /// and must not repeat per call.
    agent_info: OnceLock<AgentIntegrationInfo>,
    sessions: SessionMap<AcpSession>,
    next_id: AtomicU64,
    /// The transport factory the session driver asks for one agent
    /// stdio pair; production spawns `program args`, tests inject a
    /// duplex.
    connector: session::Connector,
}

impl AcpAdapter {
    /// An adapter serving the built-in `cursor` integration:
    /// `cursor-agent acp`.
    pub fn new() -> Self {
        Self::for_agent(
            IntegrationId::new(CURSOR_ID),
            CURSOR_PROGRAM.to_owned(),
            CURSOR_ARGS.iter().map(|arg| arg.to_string()).collect(),
        )
    }

    /// An adapter serving one ACP-speaking agent behind `integration`.
    /// `program` and `args` are the full spawn invocation - the only
    /// per-agent specifics the adapter needs.
    pub fn for_agent(integration: IntegrationId, program: String, args: Vec<String>) -> Self {
        Self {
            provider: ProviderId::new(integration.as_str()),
            integration,
            program,
            args,
            capabilities: CAPABILITIES,
            evidence: None,
            agent_info: OnceLock::new(),
            sessions: SessionMap::new(BTreeMap::new()),
            next_id: AtomicU64::new(0),
            connector: session::process_connector(),
        }
    }

    /// Serve a different Integration Identity bound to the same agent
    /// executable, for configured integrations the host registers.
    pub fn with_integration(mut self, integration: IntegrationId) -> Self {
        self.provider = ProviderId::new(integration.as_str());
        self.integration = integration;
        self
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
        self.provider.clone()
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
    /// Authentication is not probed: ACP agents report their methods in
    /// the `initialize` response, and `authenticate` runs there.
    pub fn agent_info(&self) -> &AgentIntegrationInfo {
        self.agent_info.get_or_init(|| {
            inspect_agent(
                self.provider.clone(),
                &self.program,
                Some(&["--version"]),
                None,
                declared().evidence(),
            )
        })
    }

    /// The resume cursor a live session continues from: the ACP
    /// `sessionId` `session/new` or `session/load` reported.
    pub fn resume_cursor(&self, session_id: &SessionId) -> Option<String> {
        local_adapter::resume_cursor(&self.sessions, session_id)
    }

    /// Apply a resolved `model.select` to one session. The selection is
    /// resolved first; only a resolvable selection reaches the session,
    /// and a session with a run in flight rejects the change.
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

    fn ensure_serves(&self, integration: &IntegrationId) -> Result<(), AgentRuntimeError> {
        local_adapter::ensure_serves(&self.integration(), integration)
    }

    fn session(&self, session_id: &SessionId) -> Result<Arc<AcpSession>, AgentRuntimeError> {
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
    /// instead of spawning the agent binary. Tests inject a duplex
    /// endpoint here and play the agent side of the protocol.
    #[cfg(test)]
    pub(in crate::acp_runtime) fn with_connector(mut self, connector: session::Connector) -> Self {
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
}

impl Default for AcpAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AcpAdapter {
    /// Dropping the adapter closes every live session: the driver exits
    /// when its agent process dies, and pending approval waiters release
    /// with the session state.
    fn drop(&mut self) {
        local_adapter::close_all(&self.sessions);
    }
}

/// One [`AcpAdapter`] for the built-in `cursor` integration descriptor,
/// paired with its declared discovery evidence, mirroring
/// [`cli_fallback_adapters`](crate::cli_adapter::cli_fallback_adapters).
/// The runtime registration itself lands with the parent wiring.
pub fn acp_adapter(context: &AdapterDiscovery) -> AcpAdapter {
    let adapter = AcpAdapter::new();
    match builtin_integrations()
        .iter()
        .find(|descriptor| descriptor.integration.id == adapter.integration())
    {
        Some(descriptor) => adapter.with_discovery(descriptor.sources.clone(), context.clone()),
        None => adapter,
    }
}

/// The honest declarations this adapter can surface. Sessions stream
/// message and thought deltas, resume provider sessions through
/// `session/load`, answer permission asks, and report tool, todo, and
/// usage events the protocol carries. Effort stays undeclared: ACP has
/// no effort selection. Checkpoints stay unimplemented: the runtime
/// owns that feature.
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
/// adapter. Read-only is enforced by suppressing `accept` on permission
/// asks and refusing `fs/write_text_file`; approval requests are
/// surfaced rather than auto-approved.
pub(crate) fn declared() -> ExecutionCapabilities {
    ExecutionCapabilities::new(
        /* supports_resume */ true, /* supports_account_selection */ false,
        /* supports_workspace_write */ true, /* supports_jsonl */ true,
    )
    .with_streaming()
    .with_read_only()
    .with_permission_approval()
}

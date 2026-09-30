//! The facade-side adapter surface.
//!
//! The contract [`AgentAdapter`] covers provider protocol behavior. The
//! facade additionally needs a small set of session-state operations the
//! contract trait deliberately leaves adapter-side: which integration the
//! adapter serves, the probed evidence behind `integrations.list`, the
//! provider resume cursor recorded at shutdown, and applying a resolved
//! `model.select`. [`RuntimeAdapter`] adds exactly those, so the facade and
//! its registry stay typed over the object-safe contract while the
//! concrete adapter set grows behind it.

use aifuel_core::{
    AgentAdapter, AgentIntegrationInfo, AgentRuntimeError, AgentSessionHandle, IntegrationId,
    ModelDescriptor, ModelSelection, ProviderId, QuotaSummary, SessionId,
};
use aifuel_providers::{ClaudeAdapter, CliAdapter, CodexAdapter};

/// One serving adapter behind the facade: the contract [`AgentAdapter`]
/// plus the session-state operations the facade needs for P0.
///
/// Every implementation serves exactly one Integration Identity; requests
/// naming another integration fail rather than silently reroute.
pub trait RuntimeAdapter: AgentAdapter {
    /// The Integration Identity this adapter serves.
    fn integration(&self) -> IntegrationId;

    /// The upstream provider the served integration binds to.
    fn provider(&self) -> ProviderId;

    /// The adapter's probed integration evidence, feeding the status field
    /// of `integrations.list` summaries. Probing is expected to be cached
    /// adapter-side; the facade calls it at most once per summary.
    fn agent_info(&self) -> AgentIntegrationInfo;

    /// The resume cursor a live session has reported, where the adapter
    /// declares the `resume` capability. `None` means either the adapter
    /// cannot resume or the session has not reported one yet; the facade
    /// records only `Some` cursors at shutdown.
    fn resume_cursor(&self, session_id: &SessionId) -> Option<String>;

    /// Apply an already-resolved `model.select` to a live session.
    /// Implementations keep their own readiness checks: a closed session or
    /// an in-flight Agent Run rejects the change with `invalid_state`.
    fn set_selection(
        &self,
        handle: &AgentSessionHandle,
        selection: ModelSelection,
    ) -> Result<ModelDescriptor, AgentRuntimeError>;

    /// Collect one Quota Pool observation through the integration's
    /// Monitoring Collection Contract, called once per completed Agent Run
    /// on a session whose Integration declares a contract - the pump
    /// resolves that from the registry, so this never fires contract-free.
    /// `None` means the collection produced no usable observation;
    /// collection failures stay adapter-side diagnostics and never fail
    /// the run. The default reports no observation, which is correct for
    /// the CLI adapters: their integrations declare no contract.
    fn quota_observation(&self) -> Option<QuotaSummary> {
        None
    }
}

impl RuntimeAdapter for CliAdapter {
    fn integration(&self) -> IntegrationId {
        CliAdapter::integration(self)
    }

    fn provider(&self) -> ProviderId {
        CliAdapter::provider(self)
    }

    fn agent_info(&self) -> AgentIntegrationInfo {
        CliAdapter::agent_info(self).clone()
    }

    fn resume_cursor(&self, session_id: &SessionId) -> Option<String> {
        CliAdapter::resume_cursor(self, session_id)
    }

    fn set_selection(
        &self,
        handle: &AgentSessionHandle,
        selection: ModelSelection,
    ) -> Result<ModelDescriptor, AgentRuntimeError> {
        CliAdapter::set_selection(self, handle, selection)
    }
}

impl RuntimeAdapter for CodexAdapter {
    fn integration(&self) -> IntegrationId {
        CodexAdapter::integration(self)
    }

    fn provider(&self) -> ProviderId {
        CodexAdapter::provider(self)
    }

    fn agent_info(&self) -> AgentIntegrationInfo {
        CodexAdapter::agent_info(self).clone()
    }

    fn resume_cursor(&self, session_id: &SessionId) -> Option<String> {
        CodexAdapter::resume_cursor(self, session_id)
    }

    fn set_selection(
        &self,
        handle: &AgentSessionHandle,
        selection: ModelSelection,
    ) -> Result<ModelDescriptor, AgentRuntimeError> {
        CodexAdapter::set_selection(self, handle, selection)
    }
}

impl RuntimeAdapter for ClaudeAdapter {
    fn integration(&self) -> IntegrationId {
        ClaudeAdapter::integration(self)
    }

    fn provider(&self) -> ProviderId {
        ClaudeAdapter::provider(self)
    }

    fn agent_info(&self) -> AgentIntegrationInfo {
        ClaudeAdapter::agent_info(self).clone()
    }

    fn resume_cursor(&self, session_id: &SessionId) -> Option<String> {
        ClaudeAdapter::resume_cursor(self, session_id)
    }

    fn set_selection(
        &self,
        handle: &AgentSessionHandle,
        selection: ModelSelection,
    ) -> Result<ModelDescriptor, AgentRuntimeError> {
        ClaudeAdapter::set_selection(self, handle, selection)
    }
}

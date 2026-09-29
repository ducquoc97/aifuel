//! The facade-side adapter surface.
//!
//! The contract [`AgentAdapter`] covers provider protocol behavior. The
//! facade additionally needs a small set of session-state operations the
//! contract trait deliberately leaves adapter-side: which integration the
//! adapter serves, the probed evidence behind `integrations.list`, the
//! provider resume cursor recorded at shutdown, and applying a resolved
//! `model.select`. [`RuntimeAdapter`] adds exactly those, so the facade and
//! its registry stay typed over the object-safe contract while P0 ships
//! only [`CliAdapter`] behind it.

use aifuel_core::{
    AgentAdapter, AgentIntegrationInfo, AgentRuntimeError, AgentSessionHandle, IntegrationId,
    ModelDescriptor, ModelSelection, ProviderId, SessionId,
};
use aifuel_providers::CliAdapter;

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

    /// The provider resume cursor a live session has reported, where the
    /// adapter declares the `resume` capability. `None` means either the
    /// adapter cannot resume or the session has not reported one yet; the
    /// facade records only `Some` cursors at shutdown.
    fn provider_session(&self, session_id: &SessionId) -> Option<String>;

    /// Apply an already-resolved `model.select` to a live session.
    /// Implementations keep their own readiness checks: a closed session or
    /// an in-flight Agent Run rejects the change with `invalid_state`.
    fn set_selection(
        &self,
        handle: &AgentSessionHandle,
        selection: ModelSelection,
    ) -> Result<ModelDescriptor, AgentRuntimeError>;
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

    fn provider_session(&self, session_id: &SessionId) -> Option<String> {
        CliAdapter::provider_session(self, session_id)
    }

    fn set_selection(
        &self,
        handle: &AgentSessionHandle,
        selection: ModelSelection,
    ) -> Result<ModelDescriptor, AgentRuntimeError> {
        CliAdapter::set_selection(self, handle, selection)
    }
}

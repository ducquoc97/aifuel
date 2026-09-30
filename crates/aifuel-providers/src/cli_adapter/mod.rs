//! The CLI fallback [`AgentAdapter`](aifuel_core::AgentAdapter).
//!
//! One [`CliAdapter`] wraps one compiled [`AgentExecutionAdapter`] - the
//! one-shot CLI or App Server machinery a provider already declares. A
//! session is an adapter-side run loop over one working directory: `send`
//! validates the request, spawns a worker thread, and streams typed event
//! kinds; `cancel` and `answer` drive the run's cancellation token and
//! pending interactions; `stop` closes the session.
//!
//! Declarations never exceed observed evidence:
//!
//! - `streaming` flows only where the execution adapter reports live output
//!   through [`AgentRunOutputHandler`](aifuel_core::AgentRunOutputHandler);
//!   other providers still deliver their parsed public answer, as one
//!   terminal `message.delta`.
//! - `approvals` exists only where a declared interaction capability exists
//!   (Codex's App Server path today); the generic CLI path does not invent a
//!   pending-input channel.
//! - `resume` follows each provider's declared Resume capability and only
//!   fires once a run reports a provider-native session id.
//! - `checkpoints` stays `false`: no git-ref machinery is wired yet.
//! - Quota stays `None`: CLI integrations carry no monitoring contract, and
//!   missing headroom is never fabricated as zero.

use aifuel_core::{
    AdapterCapabilities, AgentCapability, AgentExecutionAdapter, AgentIntegrationInfo,
    AgentRuntimeError, AgentSessionHandle, CapabilityState, ExecutionAvailability, IntegrationId,
    ModelDescriptor, ModelSelection, ProviderId, ProviderKey, SessionId,
};
use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, OnceLock};

mod adapter;
mod descriptors;
mod interactions;
mod session;
#[cfg(test)]
mod tests;

use descriptors::{availability_from, execution_capabilities};
pub use descriptors::{integration_summary, quota_summary};

use crate::agent_run_adapters;
use crate::integrations::{EvidenceSource, builtin_integrations};
pub use crate::local_adapter::AdapterDiscovery;
use crate::local_adapter::{self, AdapterEvidence, SessionMap};
use crate::model_catalog::ProviderCatalogModel;
use session::CliSession;

// Test fakes glob-import this module's surface; these names keep their
// construction paths stable without weighing down the shipped imports.
#[cfg(test)]
use crate::credentials::CredentialStore;
#[cfg(test)]
use crate::discovery::DiscoveryContext;
#[cfg(test)]
use crate::integrations::EvidenceContext;
#[cfg(test)]
use aifuel_core::AgentAdapter;

/// The execution adapter a `CliAdapter` serves. Built-in adapters are
/// `'static` table entries; tests and embedders wrap owned implementations.
#[derive(Clone)]
enum Execution {
    Owned(Arc<dyn AgentExecutionAdapter>),
    Builtin(&'static dyn AgentExecutionAdapter),
}

impl Execution {
    fn adapter(&self) -> &dyn AgentExecutionAdapter {
        match self {
            Self::Owned(adapter) => adapter.as_ref(),
            Self::Builtin(adapter) => *adapter,
        }
    }
}

/// An honest [`AgentAdapter`](aifuel_core::AgentAdapter) over one compiled
/// CLI execution adapter.
///
/// The adapter serves exactly the integration its execution adapter names:
/// requests for any other Integration Identity fail, never silently route
/// to another provider, model, or credential.
pub struct CliAdapter {
    execution: Execution,
    capabilities: AdapterCapabilities,
    supports_jsonl: bool,
    supports_resume: bool,
    supports_interaction: bool,
    supports_catalog: bool,
    evidence: Option<AdapterEvidence>,
    /// Probed once: native inspection spawns bounded provider commands and
    /// must not repeat per call.
    agent_info: OnceLock<AgentIntegrationInfo>,
    /// The provider's Advertised Model catalog, discovered once on first
    /// use. Providers that declare no catalog keep an empty list.
    catalog: OnceLock<Vec<ProviderCatalogModel>>,
    sessions: SessionMap<CliSession>,
    next_id: AtomicU64,
}

impl CliAdapter {
    /// Wrap one owned execution adapter (embedders and tests).
    pub fn new(execution: Arc<dyn AgentExecutionAdapter>) -> Self {
        Self::build(Execution::Owned(execution))
    }

    /// Wrap one compiled built-in execution adapter.
    pub fn builtin(execution: &'static dyn AgentExecutionAdapter) -> Self {
        Self::build(Execution::Builtin(execution))
    }

    fn build(execution: Execution) -> Self {
        let adapter = execution.adapter();
        let capabilities = execution_capabilities(adapter);
        let declared = adapter.declared_agent_capabilities();
        let supported = |capability| {
            declared
                .get(&capability)
                .is_some_and(|evidence| evidence.state == CapabilityState::Supported)
        };
        Self {
            execution,
            capabilities,
            supports_jsonl: supported(AgentCapability::StructuredOutput),
            supports_resume: supported(AgentCapability::Resume),
            supports_interaction: capabilities.approvals,
            supports_catalog: supported(AgentCapability::ModelCatalog),
            evidence: None,
            agent_info: OnceLock::new(),
            catalog: OnceLock::new(),
            sessions: SessionMap::new(BTreeMap::new()),
            next_id: AtomicU64::new(0),
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
        self.execution.adapter().integration()
    }

    /// The upstream provider this integration executes against.
    pub fn provider(&self) -> ProviderId {
        self.execution.adapter().provider()
    }

    /// Readiness to attempt a run through this integration, derived from
    /// the adapter's probed evidence plus attached discovery sources.
    pub fn availability(&self) -> ExecutionAvailability {
        availability_from(
            self.agent_info(),
            local_adapter::discovered(self.evidence.as_ref(), &self.integration()),
        )
    }

    /// The adapter's probed integration evidence, cached after the first
    /// inspection because probing spawns bounded provider commands.
    pub fn agent_info(&self) -> &AgentIntegrationInfo {
        self.agent_info
            .get_or_init(|| self.execution.adapter().agent_info())
    }

    /// The resume cursor a live session continues from: the provider-native
    /// session id the last run reported, or the persisted cursor startup
    /// reconcile seeded. The facade persists it so a runtime restart can
    /// attempt provider-side continuation where `resume` is declared.
    pub fn resume_cursor(&self, session_id: &SessionId) -> Option<String> {
        local_adapter::resume_cursor(&self.sessions, session_id)
    }

    /// Apply a resolved `model.select` to one session. The selection is
    /// resolved first; only a resolvable selection replaces the stored one,
    /// and a session with a run in flight rejects the change.
    ///
    /// This is an inherent method because [`AgentAdapter`](aifuel_core::AgentAdapter)
    /// keeps selection
    /// inside session state; the facade resolves then applies it here.
    pub fn set_selection(
        &self,
        handle: &AgentSessionHandle,
        selection: ModelSelection,
    ) -> Result<ModelDescriptor, AgentRuntimeError> {
        local_adapter::set_selection(self, &self.sessions, handle, selection)
    }

    /// The advertised catalog, discovered once. Providers that declare no
    /// catalog capability keep an empty list; discovery failures are not
    /// retried and surface as no advertised models, never a guessed list.
    fn catalog_models(&self) -> &[ProviderCatalogModel] {
        self.catalog.get_or_init(|| self.discover_catalog())
    }

    fn discover_catalog(&self) -> Vec<ProviderCatalogModel> {
        if !self.supports_catalog {
            return Vec::new();
        }
        let Ok(provider) = self.provider().as_str().parse::<ProviderKey>() else {
            return Vec::new();
        };
        local_adapter::discover_catalog(provider)
    }

    fn ensure_serves(&self, integration: &IntegrationId) -> Result<(), AgentRuntimeError> {
        local_adapter::ensure_serves(&self.integration(), integration)
    }

    fn session(&self, session_id: &SessionId) -> Result<Arc<CliSession>, AgentRuntimeError> {
        local_adapter::live_session(&self.sessions, session_id)
    }

    /// The raw event receiver, for tests that need bounded reads the boxed
    /// iterator cannot express.
    #[cfg(test)]
    pub(super) fn test_events(
        &self,
        handle: &AgentSessionHandle,
    ) -> Option<std::sync::mpsc::Receiver<aifuel_core::AgentEventKind>> {
        local_adapter::test_events(&self.sessions, &handle.session_id)
    }
}

impl Drop for CliAdapter {
    /// Dropping the adapter cancels in-flight runs. Workers keep their own
    /// session reference and unwind as the provider process exits.
    fn drop(&mut self) {
        local_adapter::close_all(&self.sessions);
    }
}

/// One [`CliAdapter`] per compiled CLI execution adapter, in registry
/// order, each paired with its built-in descriptor's discovery evidence.
///
/// `context` carries the caller's home directory, Credential Store handle,
/// and the set of `providers.json` integration ids; the adapters clone it.
pub fn cli_fallback_adapters(context: &AdapterDiscovery) -> Vec<CliAdapter> {
    let descriptors = builtin_integrations();
    agent_run_adapters()
        .iter()
        .map(|execution| {
            let adapter = CliAdapter::builtin(*execution);
            match descriptors
                .iter()
                .find(|descriptor| descriptor.integration.id == execution.integration())
            {
                Some(descriptor) => {
                    adapter.with_discovery(descriptor.sources.clone(), context.clone())
                }
                None => adapter,
            }
        })
        .collect()
}

//! The CLI fallback [`AgentAdapter`].
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
    AdapterCapabilities, AgentAdapter, AgentCapability, AgentExecutionAdapter,
    AgentIntegrationInfo, AgentRuntimeError, AgentSessionHandle, CapabilityState, DiscoveryError,
    DiscoveryState, ExecutionAvailability, IntegrationId, ModelDescriptor, ModelSelection,
    ProviderId, ProviderKey, ReceiptCode, SessionId,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

mod adapter;
mod descriptors;
mod interactions;
mod session;
#[cfg(test)]
mod tests;

pub use descriptors::{
    auth_binding_kind, availability_from, execution_capabilities, integration_summary,
    model_descriptors, quota_summary,
};

use crate::agent_run_adapters;
use crate::credentials::CredentialStore;
use crate::discovery::DiscoveryContext;
use crate::integrations::{EvidenceContext, EvidenceSource, builtin_integrations, inspect_any};
use crate::model_catalog::{
    ProviderCatalogDiscovery, ProviderCatalogModel, discover_model_catalog,
};
use session::CliSession;

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

/// Local evidence attached to an adapter: the descriptor's declared sources
/// plus the context to inspect them with.
struct AdapterEvidence {
    sources: Vec<EvidenceSource>,
    context: AdapterDiscovery,
}

/// An honest [`AgentAdapter`] over one compiled CLI execution adapter.
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
    sessions: Mutex<BTreeMap<String, Arc<CliSession>>>,
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
            sessions: Mutex::new(BTreeMap::new()),
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
        self.evidence = Some(AdapterEvidence { sources, context });
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
        availability_from(self.agent_info(), self.discovered())
    }

    /// The adapter's probed integration evidence, cached after the first
    /// inspection because probing spawns bounded provider commands.
    pub fn agent_info(&self) -> &AgentIntegrationInfo {
        self.agent_info
            .get_or_init(|| self.execution.adapter().agent_info())
    }

    /// The provider-native session id the last run reported for a session,
    /// when the provider protocol exposes one. The facade persists it so a
    /// runtime restart can attempt provider-side continuation where
    /// `resume` is declared.
    pub fn provider_session(&self, session_id: &SessionId) -> Option<String> {
        let sessions = self.sessions.lock().expect("sessions mutex");
        sessions.get(session_id.as_str()).and_then(|session| {
            session
                .state
                .lock()
                .expect("session state mutex")
                .provider_session
                .clone()
        })
    }

    /// Apply a resolved `model.select` to one session. The selection is
    /// resolved first; only a resolvable selection replaces the stored one,
    /// and a session with a run in flight rejects the change.
    ///
    /// This is an inherent method because [`AgentAdapter`] keeps selection
    /// inside session state; the facade resolves then applies it here.
    pub fn set_selection(
        &self,
        handle: &AgentSessionHandle,
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
        // The catalog interface is async; run it on a short-lived runtime
        // on a scoped thread, the same pattern the execution adapter uses.
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

    fn session(&self, session_id: &SessionId) -> Result<Arc<CliSession>, AgentRuntimeError> {
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

    /// The raw event receiver, for tests that need bounded reads the boxed
    /// iterator cannot express.
    #[cfg(test)]
    pub(super) fn test_events(
        &self,
        handle: &AgentSessionHandle,
    ) -> Option<std::sync::mpsc::Receiver<aifuel_core::AgentEventKind>> {
        self.sessions
            .lock()
            .expect("sessions mutex")
            .get(handle.session_id.as_str())
            .and_then(|session| session.take_events())
    }
}

impl Drop for CliAdapter {
    /// Dropping the adapter cancels in-flight runs. Workers keep their own
    /// session reference and unwind as the provider process exits.
    fn drop(&mut self) {
        let sessions = self.sessions.lock().expect("sessions mutex");
        for session in sessions.values() {
            let mut state = session.state.lock().expect("session state mutex");
            state.closed = true;
            if let Some(active) = &state.active_run {
                active.cancellation.cancel();
            }
        }
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

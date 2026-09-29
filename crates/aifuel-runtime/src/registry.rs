//! The facade's adapter registry: which compiled adapter serves which
//! Provider Integration, plus the descriptors and discovery evidence behind
//! `integrations.list` and `models.list`.
//!
//! Resolution is by exact Integration Identity only. A descriptor with no
//! serving adapter still lists - its capabilities stay all-false rather
//! than faking support - while `models.list` and `session.create` against
//! it fail `unsupported`.

use crate::adapter::RuntimeAdapter;
use aifuel_core::{
    AdapterCapabilities, AgentRuntimeError, IntegrationId, IntegrationSummary, ModelDescriptor,
    ReceiptCode,
};
use aifuel_providers::{
    AdapterDiscovery, EvidenceContext, IntegrationDescriptor, integration_summary,
};
use std::sync::Arc;

/// The capabilities an integration reports when no adapter serves it:
/// every flag false, per the honest-declaration rule.
const UNSERVED_CAPABILITIES: AdapterCapabilities = AdapterCapabilities {
    streaming: false,
    resume: false,
    approvals: false,
    checkpoints: false,
    effort: false,
    images: false,
    todos: false,
};

/// The runtime's compiled adapter set over the registered Integration
/// descriptors.
pub(crate) struct Registry {
    adapters: Vec<Arc<dyn RuntimeAdapter>>,
    descriptors: Vec<IntegrationDescriptor>,
    discovery: AdapterDiscovery,
}

impl Registry {
    pub(crate) fn new(
        adapters: Vec<Arc<dyn RuntimeAdapter>>,
        descriptors: Vec<IntegrationDescriptor>,
        discovery: AdapterDiscovery,
    ) -> Self {
        Self {
            adapters,
            descriptors,
            discovery,
        }
    }

    /// The adapter serving one Integration Identity, when one is registered.
    pub(crate) fn adapter_for(
        &self,
        integration: &IntegrationId,
    ) -> Option<Arc<dyn RuntimeAdapter>> {
        self.adapters
            .iter()
            .find(|adapter| adapter.integration() == *integration)
            .cloned()
    }

    /// The descriptor for one Integration Identity, whether or not an
    /// adapter serves it.
    pub(crate) fn descriptor_for(
        &self,
        integration: &IntegrationId,
    ) -> Option<&IntegrationDescriptor> {
        self.descriptors
            .iter()
            .find(|descriptor| descriptor.id() == integration)
    }

    /// The `integrations.list` payload: one summary per registered
    /// descriptor, with capabilities and probed evidence from the serving
    /// adapter where one exists.
    pub(crate) fn integration_summaries(&self) -> Vec<IntegrationSummary> {
        let context = EvidenceContext {
            discovery: &self.discovery.discovery,
            credentials: &self.discovery.credentials,
            configured: &self.discovery.configured,
        };
        self.descriptors
            .iter()
            .map(|descriptor| {
                let adapter = self.adapter_for(descriptor.id());
                let info = adapter.as_ref().map(|adapter| adapter.agent_info());
                let capabilities = adapter
                    .as_ref()
                    .map(|adapter| adapter.capabilities())
                    .unwrap_or(UNSERVED_CAPABILITIES);
                integration_summary(descriptor, info.as_ref(), capabilities, &context)
            })
            .collect()
    }

    /// The `models.list` payload for one integration: the serving adapter's
    /// merged descriptors. An unknown id is `invalid_selection`; a known
    /// integration with no serving adapter is `unsupported`.
    pub(crate) fn model_descriptors(
        &self,
        integration: &IntegrationId,
    ) -> Result<Vec<ModelDescriptor>, AgentRuntimeError> {
        let descriptor = self.descriptor_for(integration).ok_or_else(|| {
            AgentRuntimeError::new(
                ReceiptCode::InvalidSelection,
                format!("no Provider Integration is registered as {integration}"),
            )
        })?;
        let adapter = self.adapter_for(integration).ok_or_else(|| {
            AgentRuntimeError::unsupported(format!("no adapter serves integration {integration}"))
        })?;
        adapter.list_models(&descriptor.integration)
    }
}

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
    AdapterDiscovery, EvidenceContext, InstanceDescriptor, IntegrationDescriptor,
    integration_summary,
};
use std::collections::BTreeMap;
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
    external_tools: false,
};

/// The runtime's compiled adapter set over the registered Integration
/// descriptors and Provider Integration instances.
pub(crate) struct Registry {
    adapters: Vec<Arc<dyn RuntimeAdapter>>,
    descriptors: Vec<IntegrationDescriptor>,
    discovery: AdapterDiscovery,
    /// Provider Integration instances keyed by selector id. An instance id
    /// resolves to its base integration's descriptor and adapter; the
    /// instance itself carries only the environment spec and credential
    /// binding the session start resolves.
    instances: BTreeMap<IntegrationId, InstanceDescriptor>,
}

impl Registry {
    pub(crate) fn new(
        adapters: Vec<Arc<dyn RuntimeAdapter>>,
        descriptors: Vec<IntegrationDescriptor>,
        discovery: AdapterDiscovery,
        instances: Vec<InstanceDescriptor>,
    ) -> Self {
        Self {
            adapters,
            descriptors,
            discovery,
            instances: instances
                .into_iter()
                .map(|instance| (instance.id.clone(), instance))
                .collect(),
        }
    }

    /// Resolve one Integration Identity to its serving descriptor and the
    /// instance overlay it carries, when any. An instance id yields its
    /// base integration's descriptor plus the instance; a base integration
    /// id yields the descriptor alone.
    ///
    /// Instance ids are exact identities only: a bare provider name never
    /// resolves to an instance, so `claude` keeps meaning the `claude`
    /// integration rather than a configuration user's `claude.work`.
    pub(crate) fn serving(
        &self,
        integration: &IntegrationId,
    ) -> Option<(&IntegrationDescriptor, Option<&InstanceDescriptor>)> {
        if let Some(instance) = self.instances.get(integration) {
            let base = self
                .descriptors
                .iter()
                .find(|descriptor| *descriptor.id() == instance.integration);
            return base.map(|descriptor| (descriptor, Some(instance)));
        }
        self.descriptor_for(integration)
            .map(|descriptor| (descriptor, None))
    }

    /// The Credential Store environment and credential references resolve
    /// against. Execution-time reads only; listing paths never call it.
    pub(crate) fn credentials(&self) -> &aifuel_providers::CredentialStore {
        &self.discovery.credentials
    }

    /// The adapter serving one Integration Identity - the base
    /// integration's adapter when `integration` is an instance id, since
    /// instances never widen or narrow the serving adapter.
    pub(crate) fn adapter_for(
        &self,
        integration: &IntegrationId,
    ) -> Option<Arc<dyn RuntimeAdapter>> {
        let (descriptor, _) = self.serving(integration)?;
        self.adapters
            .iter()
            .find(|adapter| adapter.integration() == *descriptor.id())
            .cloned()
    }

    /// Every registered adapter, in first-match resolution order.
    pub(crate) fn adapters(&self) -> Vec<Arc<dyn RuntimeAdapter>> {
        self.adapters.clone()
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
        let (descriptor, _) = self.serving(integration).ok_or_else(|| {
            AgentRuntimeError::new(
                ReceiptCode::InvalidSelection,
                format!("no Provider Integration or Instance is registered as {integration}"),
            )
        })?;
        let adapter = self.adapter_for(integration).ok_or_else(|| {
            AgentRuntimeError::unsupported(format!("no adapter serves integration {integration}"))
        })?;
        adapter.list_models(&descriptor.integration)
    }
}

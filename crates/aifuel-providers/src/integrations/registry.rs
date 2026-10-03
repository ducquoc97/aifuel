//! The owned runtime registry of Provider Integrations.
//!
//! Built at startup from compiled built-in descriptors plus validated
//! `providers.json` entries (spec "Registry and Migration"). Config entries
//! select compiled capabilities; they cannot inject behavior. Policies:
//!
//! - A duplicate [`IntegrationId`] is a hard error; nothing is silently
//!   renamed.
//! - Built-in integration ids are reserved: a config entry cannot redefine
//!   them, so exactly which definition wins is never ambiguous.
//! - Credential References held by built-ins are reserved: a config entry
//!   cannot reference one, so an overridden endpoint can never inherit a
//!   built-in's Managed Credential (spec "Security and Trust Boundary").
//! - Ordering is deterministic: built-ins in pinned catalog order, then
//!   config entries in file order.

use super::chains::ChainDescriptor;
use super::config::ConfigError;
use super::evidence::{EvidenceContext, EvidenceSource, inspect_any};
use super::instances::InstanceDescriptor;
use crate::credentials::CredentialStore;
use crate::discovery::DiscoveryContext;
use aifuel_core::{
    CredentialRef, DiscoveryError, DiscoveryState, ExecutionConfig, Integration, IntegrationId,
    ProviderId,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// Where a registered Provider Integration was defined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrationOrigin {
    /// Compiled into this build; the id is reserved against config
    /// redefinition.
    Builtin,
    /// Loaded from `providers.json` in the AI Fuel config directory.
    Configured,
}

/// One registered Provider Integration: the configured binding plus the
/// registry metadata the runtime needs - provenance and the local evidence
/// sources Provider Discovery inspects for it.
///
/// This is the registry's descriptor type; it owns its strings and needs no
/// leaked `'static` allocations (spec "Capability indexes ... are derived
/// views over the runtime registry").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrationDescriptor {
    /// The configured binding of provider identity to execution config.
    pub integration: Integration,
    /// Whether this descriptor is compiled in or came from `providers.json`.
    pub origin: IntegrationOrigin,
    /// The local presence sources discovery inspects, in precedence order.
    /// Every source is local and side-effect-free.
    pub sources: Vec<EvidenceSource>,
}

impl IntegrationDescriptor {
    /// A built-in descriptor backed by compiled behavior.
    pub fn builtin(integration: Integration, sources: Vec<EvidenceSource>) -> Self {
        Self {
            integration,
            origin: IntegrationOrigin::Builtin,
            sources,
        }
    }

    /// A descriptor loaded from `providers.json`.
    pub fn configured(integration: Integration, sources: Vec<EvidenceSource>) -> Self {
        Self {
            integration,
            origin: IntegrationOrigin::Configured,
            sources,
        }
    }

    /// The opaque Integration Identity of this descriptor.
    pub fn id(&self) -> &IntegrationId {
        &self.integration.id
    }

    /// The upstream provider this integration binds to.
    pub fn provider(&self) -> &ProviderId {
        &self.integration.provider
    }

    /// Inspect this integration's evidence sources for local presence.
    /// The first present source wins; errors are recorded but do not stop
    /// the remaining sources.
    pub fn discover(
        &self,
        context: &EvidenceContext<'_>,
    ) -> Result<DiscoveryState, DiscoveryError> {
        inspect_any(&self.sources, &self.integration.id, context)
    }
}

/// The runtime registry over built-in and configured Provider Integrations,
/// plus the named Provider Integration instances defined over them.
///
/// `entries` is a `BTreeMap` so lookups are deterministic; `order` preserves
/// the spec's presentation order (built-ins in catalog order, then config
/// entries in file order); `configured` names the ids `providers.json`
/// defined, which is itself one of the discovery evidence kinds.
/// `instances` holds the instance overlays keyed by their selector id.
/// `chains` holds the named fallback chains `--chain` selects, keyed by
/// chain name; `optimizer` is the file-level token-optimization plan.
#[derive(Debug)]
pub struct IntegrationRegistry {
    entries: BTreeMap<IntegrationId, IntegrationDescriptor>,
    order: Vec<IntegrationId>,
    configured: BTreeSet<IntegrationId>,
    instances: BTreeMap<IntegrationId, InstanceDescriptor>,
    chains: BTreeMap<String, ChainDescriptor>,
    optimizer: aifuel_core::OptimizePlan,
    /// The Credential References built-ins hold, kept for
    /// [`Self::check_instance`], which applies the same reservation to
    /// instances added after build.
    builtin_credentials: BTreeSet<CredentialRef>,
}

impl IntegrationRegistry {
    /// Build the runtime registry from compiled built-ins and validated
    /// config entries and instances.
    ///
    /// A config entry may carry an individual [`ConfigError`] (one malformed
    /// entry in an otherwise parseable file); the first one fails the build
    /// so a bad file can never produce a silently partial registry.
    pub fn build(
        builtins: impl IntoIterator<Item = IntegrationDescriptor>,
        config_entries: impl IntoIterator<Item = Result<IntegrationDescriptor, ConfigError>>,
        config_instances: impl IntoIterator<Item = Result<InstanceDescriptor, ConfigError>>,
        config_chains: impl IntoIterator<Item = Result<ChainDescriptor, ConfigError>>,
        optimizer: aifuel_core::OptimizePlan,
    ) -> Result<Self, RegistryError> {
        let mut entries = BTreeMap::new();
        let mut order = Vec::new();
        let mut configured = BTreeSet::new();
        let mut builtin_ids = BTreeSet::new();
        let mut builtin_credentials = BTreeSet::new();

        for descriptor in builtins {
            let id = descriptor.integration.id.clone();
            if entries.contains_key(&id) {
                return Err(RegistryError::DuplicateIntegrationId(id));
            }
            builtin_ids.insert(id.clone());
            builtin_credentials.extend(
                credential_refs(&descriptor.integration)
                    .into_iter()
                    .cloned(),
            );
            order.push(id.clone());
            entries.insert(id, descriptor);
        }

        for entry in config_entries {
            let descriptor = entry.map_err(RegistryError::Config)?;
            let id = descriptor.integration.id.clone();
            if builtin_ids.contains(&id) {
                return Err(RegistryError::ReservedIntegrationId(id));
            }
            if entries.contains_key(&id) {
                return Err(RegistryError::DuplicateIntegrationId(id));
            }
            for credential in credential_refs(&descriptor.integration) {
                if builtin_credentials.contains(credential) {
                    return Err(RegistryError::ReservedCredential {
                        integration: id,
                        credential: credential.clone(),
                    });
                }
            }
            configured.insert(id.clone());
            order.push(id.clone());
            entries.insert(id, descriptor);
        }

        let mut instances = BTreeMap::new();
        for entry in config_instances {
            let instance = entry.map_err(RegistryError::Config)?;
            check_instance(&entries, &instances, &builtin_credentials, &instance)?;
            instances.insert(instance.id.clone(), instance);
        }

        let mut chains = BTreeMap::new();
        for entry in config_chains {
            let chain = entry.map_err(RegistryError::Config)?;
            check_chain(&entries, &instances, &chain)?;
            if chains.insert(chain.name.clone(), chain).is_some() {
                unreachable!("chain names are BTreeMap keys and cannot repeat");
            }
        }

        Ok(Self {
            entries,
            order,
            configured,
            instances,
            chains,
            optimizer,
            builtin_credentials,
        })
    }

    /// Resolve a caller's selection to one registered integration.
    ///
    /// An exact Integration Identity match wins. Otherwise the selector is
    /// treated as a bare [`ProviderId`] and resolves only when exactly one
    /// registered integration uses it; several matches are
    /// [`ResolveError::Ambiguous`], never a silent pick (spec: ambiguity is
    /// an error, not a guess - silently choosing subscription versus billed
    /// execution is a billing decision AI Fuel must not make). The rule is
    /// shared with run-time adapter resolution through
    /// [`aifuel_core::match_selector`].
    pub fn resolve(&self, selector: &str) -> Result<&IntegrationDescriptor, ResolveError> {
        match aifuel_core::match_selector(
            selector,
            self.entries
                .values()
                .map(|descriptor| (descriptor.id(), descriptor.provider())),
        ) {
            aifuel_core::SelectorMatch::Exact(id) | aifuel_core::SelectorMatch::Unique(id) => {
                Ok(&self.entries[&id])
            }
            aifuel_core::SelectorMatch::Ambiguous {
                provider,
                candidates,
            } => Err(ResolveError::Ambiguous {
                provider,
                candidates,
            }),
            aifuel_core::SelectorMatch::Unknown => Err(ResolveError::Unknown {
                selector: selector.to_owned(),
            }),
        }
    }

    /// Look up one integration by exact Integration Identity.
    pub fn get(&self, id: &IntegrationId) -> Option<&IntegrationDescriptor> {
        self.entries.get(id)
    }

    /// Look up one Provider Integration instance by its exact selector id.
    pub fn instance(&self, id: &IntegrationId) -> Option<&InstanceDescriptor> {
        self.instances.get(id)
    }

    /// Iterate the registered instances in id order.
    pub fn instances(&self) -> impl Iterator<Item = &InstanceDescriptor> {
        self.instances.values()
    }

    /// Look up one fallback chain by exact name.
    pub fn chain(&self, name: &str) -> Option<&ChainDescriptor> {
        self.chains.get(name)
    }

    /// Iterate the registered fallback chains in name order.
    pub fn chains(&self) -> impl Iterator<Item = &ChainDescriptor> {
        self.chains.values()
    }

    /// The file-level token-optimization plan `providers.json` declared;
    /// the inert default when it declared none.
    pub fn optimizer(&self) -> &aifuel_core::OptimizePlan {
        &self.optimizer
    }

    /// Resolve one Integration Identity to the integration that serves it
    /// and the instance overlay it carries, when any. An instance id yields
    /// its base integration plus the instance; a base integration id yields
    /// the integration itself.
    ///
    /// Instances are reached only by their exact id: [`Self::resolve`] never
    /// returns one, because an instance's id is not a provider name and a
    /// stored `claude` selection must keep meaning `claude`.
    pub fn serving(
        &self,
        id: &IntegrationId,
    ) -> Option<(&IntegrationDescriptor, Option<&InstanceDescriptor>)> {
        if let Some(instance) = self.instances.get(id) {
            // Build validated the base exists, so indexing cannot miss.
            let base = &self.entries[&instance.integration];
            return Some((base, Some(instance)));
        }
        self.entries.get(id).map(|descriptor| (descriptor, None))
    }

    /// Validate a prospective instance against this registry the same way
    /// [`Self::build`] validated the loaded ones. `aifuel instance add`
    /// calls this before writing so `providers.json` never gains an entry
    /// the registry would reject.
    pub fn check_instance(&self, instance: &InstanceDescriptor) -> Result<(), RegistryError> {
        check_instance(
            &self.entries,
            &self.instances,
            &self.builtin_credentials,
            instance,
        )
    }

    /// Iterate descriptors in deterministic order: built-ins first in
    /// catalog order, then config entries in file order.
    pub fn list(&self) -> impl Iterator<Item = &IntegrationDescriptor> {
        self.order.iter().map(|id| &self.entries[id])
    }

    /// The number of registered integrations.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The Integration Identities defined by `providers.json`. Presence in
    /// this set is one arm of [`EvidenceSource::ConfiguredEndpoint`].
    pub fn configured_ids(&self) -> &BTreeSet<IntegrationId> {
        &self.configured
    }

    /// Assemble the read-only facts evidence inspection may consult: the
    /// home-relative discovery context, the Credential Store (metadata reads
    /// only), and this registry's configured ids.
    pub fn evidence_context<'a>(
        &'a self,
        discovery: &'a DiscoveryContext,
        credentials: &'a CredentialStore,
    ) -> EvidenceContext<'a> {
        EvidenceContext {
            discovery,
            credentials,
            configured: &self.configured,
        }
    }
}

/// The registry-level checks every instance must pass, shared between
/// [`IntegrationRegistry::build`] and [`IntegrationRegistry::check_instance`]:
/// the base integration exists, the id collides with no claimed identity,
/// no built-in-held credential is referenced, a `credential` binding has a
/// slot to fill, and the id does not shadow a selector users already type.
fn check_instance(
    entries: &BTreeMap<IntegrationId, IntegrationDescriptor>,
    instances: &BTreeMap<IntegrationId, InstanceDescriptor>,
    builtin_credentials: &BTreeSet<CredentialRef>,
    instance: &InstanceDescriptor,
) -> Result<(), RegistryError> {
    let id = &instance.id;
    if entries.contains_key(id) || instances.contains_key(id) {
        return Err(RegistryError::DuplicateInstanceId(id.clone()));
    }
    let Some(base) = entries.get(&instance.integration) else {
        return Err(RegistryError::InvalidInstance {
            id: id.clone(),
            reason: format!(
                "base integration '{}' is not a registered integration",
                instance.integration
            ),
        });
    };
    for credential in instance.credential_refs() {
        if builtin_credentials.contains(credential) {
            return Err(RegistryError::ReservedCredential {
                integration: id.clone(),
                credential: credential.clone(),
            });
        }
    }
    // `credential` fills the base binding's credential slot. An HTTP
    // integration declaring `auth: none` has no slot, and a CLI execution
    // arm carries no Authentication Binding at all, so the binding is
    // meaningful only as the destination `auth set-key` targets.
    if instance.credential.is_some()
        && matches!(
            base.integration.execution,
            ExecutionConfig::Http { ref auth, .. }
                if matches!(auth, aifuel_core::AuthBinding::None)
        )
    {
        return Err(RegistryError::InvalidInstance {
            id: id.clone(),
            reason: format!(
                "credential cannot be bound: integration '{}' declares auth 'none'",
                instance.integration
            ),
        });
    }
    // An instance id that resolves as a provider selector to other
    // integrations would silently claim selections users already type: an
    // exact id beats provider names, so `aifuel run --provider <name>`
    // would suddenly spawn this instance. A match on the instance's own
    // base is consistent - the provider name and the instance id then lead
    // to the same serving integration.
    match aifuel_core::match_selector(
        id.as_str(),
        entries
            .values()
            .map(|descriptor| (descriptor.id(), descriptor.provider())),
    ) {
        aifuel_core::SelectorMatch::Unknown => {}
        aifuel_core::SelectorMatch::Unique(existing) if existing == instance.integration => {}
        _ => {
            return Err(RegistryError::InvalidInstance {
                id: id.clone(),
                reason: format!(
                    "instance id '{id}' collides with a provider or integration selector already in use; pick an id that names only this instance"
                ),
            });
        }
    }
    Ok(())
}

/// The registry-level check every chain must pass: each step's
/// `integration` names a registered Integration Identity - a base
/// integration or an instance selector id. An unknown reference is an
/// explicit error: a chain that names a typo must not silently skip it
/// at run time.
fn check_chain(
    entries: &BTreeMap<IntegrationId, IntegrationDescriptor>,
    instances: &BTreeMap<IntegrationId, InstanceDescriptor>,
    chain: &ChainDescriptor,
) -> Result<(), RegistryError> {
    for (index, step) in chain.steps.iter().enumerate() {
        if !entries.contains_key(&step.integration) && !instances.contains_key(&step.integration) {
            return Err(RegistryError::InvalidChain {
                name: chain.name.clone(),
                reason: format!(
                    "steps[{index}].integration '{}' is not a registered integration or instance",
                    step.integration
                ),
            });
        }
    }
    Ok(())
}

/// The Credential References an integration binds, from its execution auth
/// and its monitoring credential. Used to reserve built-in-held references
/// against config smuggling.
fn credential_refs(integration: &Integration) -> Vec<&CredentialRef> {
    let mut refs = Vec::new();
    if let ExecutionConfig::Http { auth, .. } = &integration.execution
        && let Some(reference) = auth.credential_ref()
    {
        refs.push(reference);
    }
    if let Some(monitoring) = &integration.monitoring
        && let Some(credential) = &monitoring.credential
    {
        refs.push(credential);
    }
    refs
}

/// The failures that can stop registry construction.
#[derive(Debug)]
pub enum RegistryError {
    /// A config entry failed validation at the load boundary.
    Config(ConfigError),
    /// Two entries share one Integration Identity. The offending entry is
    /// rejected; nothing is silently renamed.
    DuplicateIntegrationId(IntegrationId),
    /// A config entry used an Integration Identity reserved by a built-in.
    ReservedIntegrationId(IntegrationId),
    /// A config entry referenced a Credential Reference a built-in holds.
    /// Built-in-held references are bound to the built-in's own endpoint; a
    /// config endpoint must never inherit one, because that would send
    /// managed credential material to a different host.
    ReservedCredential {
        integration: IntegrationId,
        credential: CredentialRef,
    },
    /// Two `instances` entries share a selector id, or one reuses an
    /// Integration Identity.
    DuplicateInstanceId(IntegrationId),
    /// An `instances` entry is structurally valid but contradicts the
    /// registry: unknown base integration, a `credential` binding with no
    /// slot to fill, or an id that shadows an existing selector.
    InvalidInstance { id: IntegrationId, reason: String },
    /// A `chains` entry is structurally valid but names an integration
    /// the registry does not know.
    InvalidChain { name: String, reason: String },
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(error) => fmt::Display::fmt(error, f),
            Self::DuplicateIntegrationId(id) => {
                write!(f, "duplicate integration id '{id}' in providers config")
            }
            Self::ReservedIntegrationId(id) => write!(
                f,
                "integration id '{id}' is reserved by a built-in integration; config cannot redefine it"
            ),
            Self::ReservedCredential {
                integration,
                credential,
            } => write!(
                f,
                "integration '{integration}' cannot reference managed credential '{credential}': the reference is held by a built-in integration and bound to its endpoint"
            ),
            Self::DuplicateInstanceId(id) => write!(
                f,
                "instance id '{id}' collides with an integration id or another instance in providers config"
            ),
            Self::InvalidInstance { id, reason } => {
                write!(f, "instance '{id}': {reason}")
            }
            Self::InvalidChain { name, reason } => {
                write!(f, "chain '{name}': {reason}")
            }
        }
    }
}

impl std::error::Error for RegistryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Config(error) => Some(error),
            _ => None,
        }
    }
}

/// The ways a selection can fail to name one registered integration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// The selector is neither a registered Integration Identity nor a
    /// provider id in use.
    Unknown { selector: String },
    /// A bare provider id maps to several integrations. Choosing between a
    /// subscription path and a billed path is never guessed.
    Ambiguous {
        provider: ProviderId,
        candidates: Vec<IntegrationId>,
    },
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown { selector } => write!(
                f,
                "'{selector}' is not a registered integration id or unambiguous provider id"
            ),
            Self::Ambiguous {
                provider,
                candidates,
            } => {
                let ids = candidates
                    .iter()
                    .map(IntegrationId::as_str)
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(
                    f,
                    "provider '{provider}' maps to multiple integrations ({ids}); select an integration id explicitly"
                )
            }
        }
    }
}

impl std::error::Error for ResolveError {}

#[cfg(test)]
mod tests;

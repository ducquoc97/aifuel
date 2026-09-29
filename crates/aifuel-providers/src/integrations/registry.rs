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

use super::config::ConfigError;
use super::evidence::{EvidenceContext, EvidenceSource, inspect_any};
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

/// The runtime registry over built-in and configured Provider Integrations.
///
/// `entries` is a `BTreeMap` so lookups are deterministic; `order` preserves
/// the spec's presentation order (built-ins in catalog order, then config
/// entries in file order); `configured` names the ids `providers.json`
/// defined, which is itself one of the discovery evidence kinds.
#[derive(Debug)]
pub struct IntegrationRegistry {
    entries: BTreeMap<IntegrationId, IntegrationDescriptor>,
    order: Vec<IntegrationId>,
    configured: BTreeSet<IntegrationId>,
}

impl IntegrationRegistry {
    /// Build the runtime registry from compiled built-ins and validated
    /// config entries.
    ///
    /// A config entry may carry an individual [`ConfigError`] (one malformed
    /// entry in an otherwise parseable file); the first one fails the build
    /// so a bad file can never produce a silently partial registry.
    pub fn build(
        builtins: impl IntoIterator<Item = IntegrationDescriptor>,
        config_entries: impl IntoIterator<Item = Result<IntegrationDescriptor, ConfigError>>,
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

        Ok(Self {
            entries,
            order,
            configured,
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

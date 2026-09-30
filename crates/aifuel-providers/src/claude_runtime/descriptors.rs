//! Descriptor assembly for the Claude adapter.
//!
//! `models.list` and `resolve` merge the same evidence kinds the CLI
//! adapter merges: native presence and authentication probes, local
//! credential-source discovery, and the provider's Advertised Model
//! catalog. The policy mirrors
//! [`cli_adapter`](crate::cli_adapter)'s descriptor layer, duplicated
//! here because that module's internals are private to it; unknown
//! stays unknown, absent stays absent, and quota stays `None` without
//! a real observation.

use crate::model_catalog::ProviderCatalogModel;
use aifuel_core::{
    AgentAuthenticationState, AgentIntegrationInfo, AgentPresenceState, CapabilityState,
    DiscoveryError, DiscoveryState, Effort, ExecutionAvailability, ModelDescriptor, ProviderId,
    QuotaSummary,
};
use std::collections::BTreeMap;

/// Readiness to attempt a run through this integration, from the
/// strongest evidence first: a missing executable, a native
/// authentication probe, then local credential-source discovery.
///
/// `discovered` is `None` when the caller attached no discovery
/// evidence, which reads as `unknown`, never as absent. A presence
/// probe that could not run also stays `unknown`: `ready` is claimed
/// only on positive evidence.
pub(super) fn availability_from(
    info: &AgentIntegrationInfo,
    discovered: Option<Result<DiscoveryState, DiscoveryError>>,
) -> ExecutionAvailability {
    if info.native_presence.state == AgentPresenceState::Absent {
        return ExecutionAvailability::Unsupported;
    }
    match info.native_authentication.state {
        // A successful auth probe implies the executable ran; it is the
        // strongest readiness signal the CLI path has.
        AgentAuthenticationState::Authenticated => return ExecutionAvailability::Ready,
        AgentAuthenticationState::Unauthenticated => return ExecutionAvailability::NeedsAuth,
        AgentAuthenticationState::Unknown => {}
    }
    if info.native_presence.state == AgentPresenceState::Unknown {
        return ExecutionAvailability::Unknown;
    }
    match discovered {
        Some(Ok(DiscoveryState::Present)) => ExecutionAvailability::Ready,
        Some(Ok(DiscoveryState::Absent)) => ExecutionAvailability::NeedsAuth,
        Some(Err(_)) | None => ExecutionAvailability::Unknown,
    }
}

/// One model descriptor for a catalog entry, merging entitlement and
/// availability evidence the caller supplies.
pub(super) fn catalog_descriptor(
    provider: &ProviderId,
    model: &ProviderCatalogModel,
    entitled: CapabilityState,
    availability: ExecutionAvailability,
    quota: Option<QuotaSummary>,
) -> ModelDescriptor {
    ModelDescriptor {
        provider: provider.clone(),
        model: model.model_id.clone(),
        label: model
            .display_label
            .clone()
            .unwrap_or_else(|| model.model_id.clone()),
        efforts: selectable_efforts(model.supported_efforts.as_deref()),
        advertised: true,
        entitled,
        availability,
        quota,
    }
}

/// Merge Advertised Model catalog entries with Account Entitlement,
/// Execution Availability, and one integration-level Quota Pool
/// observation into `models.list` descriptors.
///
/// `entitlements` keys model ids to their observed entitlement; models
/// without an entry stay `unknown`. `quota` attaches to every model
/// only when a real monitoring observation supplies it. Unadvertised
/// models never appear here: the catalog is the only advertisement
/// evidence, and Claude's stays empty until a verified catalog
/// interface exists.
pub(super) fn model_descriptors(
    provider: ProviderId,
    advertised: &[ProviderCatalogModel],
    entitlements: &BTreeMap<String, CapabilityState>,
    availability: ExecutionAvailability,
    quota: Option<QuotaSummary>,
) -> Vec<ModelDescriptor> {
    advertised
        .iter()
        .map(|model| {
            catalog_descriptor(
                &provider,
                model,
                entitlements
                    .get(&model.model_id)
                    .copied()
                    .unwrap_or(CapabilityState::Unknown),
                availability,
                quota,
            )
        })
        .collect()
}

/// The effort spellings a catalog advertises, reduced to the
/// contract's closed set in report order. Spellings outside the set
/// are not selectable through `model.select`, so they are not offered.
pub(super) fn selectable_efforts(supported: Option<&[String]>) -> Vec<Effort> {
    let mut efforts = Vec::new();
    for spelling in supported.into_iter().flatten() {
        if let Some(effort) = Effort::parse(spelling)
            && !efforts.contains(&effort)
        {
            efforts.push(effort);
        }
    }
    efforts
}

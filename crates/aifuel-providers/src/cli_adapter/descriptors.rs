//! Descriptor assembly for the agent runtime contract.
//!
//! `integrations.list` and `models.list` merge the same evidence kinds here:
//! provider-declared capabilities, native presence and authentication
//! probes, local credential-source discovery, and monitoring observations.
//! Nothing here promotes evidence: unknown stays unknown, absent stays
//! absent, and quota stays `None` without a real observation.

use crate::integrations::{EvidenceContext, IntegrationDescriptor, inspect_any};
use crate::model_catalog::ProviderCatalogModel;
use aifuel_core::{
    AdapterCapabilities, AgentAuthenticationState, AgentCapability, AgentExecutionAdapter,
    AgentIntegrationInfo, AgentPresenceState, AuthBinding, CapabilityState, DiscoveryError,
    DiscoveryState, Effort, ExecutionAvailability, ExecutionConfig, IntegrationAuthKind,
    IntegrationStatus, IntegrationSummary, ModelDescriptor, ObservationState, ProviderId,
    QuotaSummary, StatusObservation,
};
use std::collections::BTreeMap;

/// The honest [`AdapterCapabilities`] one execution adapter can surface,
/// mapped from its declared AgentCapability evidence.
///
/// Undeclared and `unknown` capabilities both read as `false`: a host hides
/// the affordance rather than discover it was faked. `checkpoints`, `images`,
/// and `todos` stay `false` because the CLI execution machinery exposes none
/// of those paths.
pub fn execution_capabilities(execution: &dyn AgentExecutionAdapter) -> AdapterCapabilities {
    let declared = execution.declared_agent_capabilities();
    let supported = |capability: AgentCapability| {
        declared
            .get(&capability)
            .is_some_and(|evidence| evidence.state == CapabilityState::Supported)
    };
    AdapterCapabilities {
        streaming: supported(AgentCapability::Streaming),
        resume: supported(AgentCapability::Resume),
        // An approvals path exists only where a declared interaction
        // capability exists: the CLI fallback can wait on and answer
        // provider interactions through the run's interaction handler.
        approvals: supported(AgentCapability::PermissionApproval)
            || supported(AgentCapability::OrdinaryInput),
        checkpoints: false,
        effort: supported(AgentCapability::Effort),
        images: false,
        todos: false,
    }
}

/// The Authentication Binding kind one integration's execution applies,
/// reported as the contract's opaque category.
///
/// `Cli` carries no auth fields because the provider CLI owns its
/// credential; every `Http` execution carries an explicit binding.
pub fn auth_binding_kind(execution: &ExecutionConfig) -> IntegrationAuthKind {
    match execution {
        ExecutionConfig::Cli { .. } => IntegrationAuthKind::ProviderCli,
        ExecutionConfig::Http { auth, .. } => match auth {
            AuthBinding::None => IntegrationAuthKind::None,
            AuthBinding::ApiKey { .. } | AuthBinding::OAuth { .. } => IntegrationAuthKind::Managed,
        },
    }
}

/// Readiness to attempt a run through one integration, from the strongest
/// evidence first: a missing executable, a native authentication probe,
/// then local credential-source discovery.
///
/// `discovered` is `None` when the caller attached no discovery evidence,
/// which reads as `unknown`, never as absent. A presence probe that could
/// not run also stays `unknown`: `ready` is only claimed on positive
/// evidence.
pub fn availability_from(
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

/// Assemble the `integrations.list` entry for one registered descriptor.
///
/// `info` is the serving execution adapter's inspected evidence, or `None`
/// when no adapter serves the integration. `capabilities` is the adapter's
/// [`execution_capabilities`] result, all-false when nothing can serve it.
/// Only presence metadata feeds the payload; credential material and
/// Credential References never appear in it.
pub fn integration_summary(
    descriptor: &IntegrationDescriptor,
    info: Option<&AgentIntegrationInfo>,
    capabilities: AdapterCapabilities,
    context: &EvidenceContext<'_>,
) -> IntegrationSummary {
    let auth = auth_binding_kind(&descriptor.integration.execution);
    let status = integration_status(descriptor, info, auth, context);
    IntegrationSummary {
        integration_id: descriptor.integration.id.clone(),
        provider: descriptor.integration.provider.clone(),
        label: descriptor.integration.name.clone(),
        auth,
        status,
        capabilities,
    }
}

fn integration_status(
    descriptor: &IntegrationDescriptor,
    info: Option<&AgentIntegrationInfo>,
    auth: IntegrationAuthKind,
    context: &EvidenceContext<'_>,
) -> IntegrationStatus {
    match &descriptor.integration.execution {
        ExecutionConfig::Cli { .. } => {
            let Some(info) = info else {
                return IntegrationStatus::Unavailable;
            };
            let discovered = inspect_any(&descriptor.sources, descriptor.id(), context);
            match availability_from(info, Some(discovered)) {
                ExecutionAvailability::Ready => IntegrationStatus::Ready,
                ExecutionAvailability::NeedsAuth => IntegrationStatus::NeedsAuth,
                ExecutionAvailability::Unknown => IntegrationStatus::Degraded,
                ExecutionAvailability::Unsupported => IntegrationStatus::Unavailable,
            }
        }
        // HTTP integrations have no native executable to probe; readiness is
        // the credential and endpoint evidence alone.
        ExecutionConfig::Http { .. } => {
            if info.is_none() {
                return IntegrationStatus::Unavailable;
            }
            match inspect_any(&descriptor.sources, descriptor.id(), context) {
                Ok(DiscoveryState::Present) => IntegrationStatus::Ready,
                Ok(DiscoveryState::Absent) if auth == IntegrationAuthKind::Managed => {
                    IntegrationStatus::NeedsAuth
                }
                Ok(DiscoveryState::Absent) | Err(_) => IntegrationStatus::Degraded,
            }
        }
    }
}

/// One model descriptor for a catalog entry, merging entitlement and
/// availability evidence the caller supplies.
pub(crate) fn catalog_descriptor(
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
/// Execution Availability, and one integration-level Quota Pool observation
/// into `models.list` descriptors.
///
/// `entitlements` keys model ids to their observed entitlement; models
/// without an entry stay `unknown`. `quota` attaches to every model only
/// when a real monitoring observation supplies it. Unadvertised models never
/// appear here: the catalog is the only advertisement evidence.
pub fn model_descriptors(
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

/// The compact Quota Pool summary a monitoring observation reports, or
/// `None` when the observation holds no usable values. Missing headroom
/// stays `None`; it is never reported as zero.
pub fn quota_summary(observation: &StatusObservation) -> Option<QuotaSummary> {
    if observation.state != ObservationState::Observed {
        return None;
    }
    // A reset timestamp alone is scheduling metadata, not headroom
    // evidence; only an observed percentage makes a summary.
    if observation.remaining_percent.is_none() && observation.used_percent.is_none() {
        return None;
    }
    Some(QuotaSummary {
        remaining_pct: observation.remaining_percent,
        resets_at: observation.resets_at,
        depleted: observation.remaining_percent == Some(0.0)
            || observation.used_percent == Some(100.0),
    })
}

/// The effort spellings a catalog advertises, reduced to the contract's
/// closed set in report order. Spellings outside the set are not selectable
/// through `model.select`, so they are not offered.
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

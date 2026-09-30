//! `integrations.list` and `models.list` payload assembly tests.

use super::super::descriptors;
use super::*;
use crate::IntegrationDescriptor;
use aifuel_core::{
    AgentAuthenticationState, AuthBinding, Effort, EndpointConfig, ExecutionConfig,
    IntegrationAuthKind, IntegrationStatus, ObservationState, QuotaSummary, StatusObservation,
    WireApi,
};
use std::collections::{BTreeMap, BTreeSet};

/// `models.list` is honest when the provider advertises nothing: an empty
/// list, never a guessed model.
#[test]
fn list_models_returns_only_advertised_evidence() {
    let adapter = adapter(vec![]);
    let models = adapter
        .list_models(&integration(ProviderKey::Claude))
        .expect("list_models works");
    assert!(models.is_empty(), "no catalog means no advertised models");
}

/// The merge keeps the semantic layers distinct: advertised, entitled,
/// availability, and quota are independent evidence kinds.
#[test]
fn model_descriptors_merge_without_promotion() {
    let advertised = vec![
        ProviderCatalogModel {
            model_id: "gpt-5.3".to_owned(),
            display_label: Some("GPT-5.3".to_owned()),
            default_effort: Some("medium".to_owned()),
            supported_efforts: Some(vec![
                "low".to_owned(),
                "medium".to_owned(),
                "high".to_owned(),
            ]),
        },
        ProviderCatalogModel {
            model_id: "gpt-5.3-mini".to_owned(),
            display_label: None,
            default_effort: None,
            supported_efforts: None,
        },
    ];
    let entitlements = BTreeMap::from([("gpt-5.3".to_owned(), CapabilityState::Supported)]);
    let quota = Some(QuotaSummary {
        remaining_pct: Some(42.0),
        resets_at: Some(1_700_000_000.0),
        depleted: false,
    });
    let models = descriptors::model_descriptors(
        ProviderId::new("codex"),
        &advertised,
        &entitlements,
        ExecutionAvailability::NeedsAuth,
        quota,
    );
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].label, "GPT-5.3");
    assert_eq!(
        models[0].efforts,
        vec![Effort::Low, Effort::Medium, Effort::High]
    );
    assert!(models[0].advertised);
    assert_eq!(models[0].entitled, CapabilityState::Supported);
    assert_eq!(models[0].availability, ExecutionAvailability::NeedsAuth);
    assert_eq!(models[0].quota, quota);
    // No entitlement evidence stays unknown; no effort data means no
    // selectable efforts.
    assert_eq!(models[1].entitled, CapabilityState::Unknown);
    assert!(models[1].efforts.is_empty());
    assert_eq!(models[1].label, "gpt-5.3-mini");
}

/// `integrations.list` reports the auth binding kind and observed status
/// without any credential material.
#[test]
fn integration_summary_reports_binding_kind_and_status() {
    let home = std::env::temp_dir().join(format!("aifuel-cli-summary-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let discovery = DiscoveryContext::new(&home);
    let credentials = CredentialStore::new(home.join("aifuel"));
    let configured = BTreeSet::new();
    let context = EvidenceContext {
        discovery: &discovery,
        credentials: &credentials,
        configured: &configured,
    };
    let capabilities = execution_capabilities(&FakeExecution::new(ProviderKey::Claude));

    let cli = IntegrationDescriptor::builtin(integration(ProviderKey::Claude), vec![]);
    let info = FakeExecution::new(ProviderKey::Claude)
        .with_authentication(AgentAuthenticationState::Unauthenticated)
        .agent_info();
    let summary = integration_summary(&cli, Some(&info), capabilities, &context);
    assert_eq!(summary.auth, IntegrationAuthKind::ProviderCli);
    assert_eq!(summary.status, IntegrationStatus::NeedsAuth);
    assert_eq!(summary.integration_id, IntegrationId::new("claude"));

    let http = IntegrationDescriptor::builtin(
        Integration {
            id: IntegrationId::new("ollama:local"),
            provider: ProviderId::new("ollama"),
            name: "Ollama".to_owned(),
            execution: ExecutionConfig::Http {
                endpoint: EndpointConfig {
                    base_url: "http://127.0.0.1:11434".to_owned(),
                    extra_headers: BTreeMap::new(),
                    request_timeout_seconds: None,
                },
                protocol: WireApi::OpenAiChat,
                auth: AuthBinding::None,
            },
            monitoring: None,
        },
        vec![EvidenceSource::ConfiguredEndpoint {
            marker_directories: vec![".ollama".to_owned()],
        }],
    );
    let info = FakeExecution::new(ProviderKey::Claude).agent_info();
    let summary = integration_summary(&http, Some(&info), capabilities, &context);
    assert_eq!(summary.auth, IntegrationAuthKind::None);
    // No marker and no credential requirement: readiness is unestablished,
    // not auth-blocked.
    assert_eq!(summary.status, IntegrationStatus::Degraded);

    let missing = integration_summary(&http, None, capabilities, &context);
    assert_eq!(missing.status, IntegrationStatus::Unavailable);
}

/// Quota attaches only from a real observation; uncollected or empty
/// observations produce `None`, never a fabricated zero.
#[test]
fn quota_summary_requires_observed_values() {
    let observation = |state, remaining| StatusObservation {
        id: "obs".to_owned(),
        provider_id: "claude".to_owned(),
        account_id: None,
        quota_pool_id: "claude:usage".to_owned(),
        label: "Usage".to_owned(),
        used_percent: None,
        remaining_percent: remaining,
        resets_at: Some(1_700_000_000.0),
        state,
        integration_id: None,
        observed_at: None,
        collected_at: 0.0,
        freshness: aifuel_core::FreshnessState::Fresh,
        provenance: aifuel_core::Provenance::ProviderApi,
    };
    assert_eq!(
        quota_summary(&observation(ObservationState::Unavailable, None)),
        None
    );
    assert_eq!(
        quota_summary(&observation(ObservationState::Observed, None)),
        None,
        "an observation with no values is no summary"
    );
    let summary = quota_summary(&observation(ObservationState::Observed, Some(0.0)))
        .expect("a real observation summarizes");
    assert!(summary.depleted, "zero remaining is exhaustion");
    assert_eq!(summary.remaining_pct, Some(0.0));
}

/// Selectable efforts reduce catalog spellings to the contract set and keep
/// report order; unknown spellings are not offered.
#[test]
fn selectable_efforts_filters_to_the_contract_set() {
    assert_eq!(
        descriptors::selectable_efforts(Some(&[
            "high".to_owned(),
            "maximum".to_owned(),
            "low".to_owned()
        ])),
        vec![Effort::High, Effort::Low]
    );
    assert!(descriptors::selectable_efforts(None).is_empty());
}

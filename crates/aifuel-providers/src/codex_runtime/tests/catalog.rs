//! Model selection surface: `resolve` and `models.list` report the
//! injected Advertised Model catalog merged with the probed evidence,
//! and unselectable effort values fail before any provider work starts.

use super::*;
use crate::model_catalog::ProviderCatalogModel;
use aifuel_core::{AgentAdapter, CapabilityState, Effort, ExecutionAvailability, ReceiptCode};

fn advertised() -> Vec<ProviderCatalogModel> {
    vec![
        ProviderCatalogModel {
            model_id: "codex-test-model".to_owned(),
            display_label: Some("Codex Test".to_owned()),
            default_effort: Some("medium".to_owned()),
            supported_efforts: Some(vec!["low".to_owned(), "medium".to_owned()]),
        },
        ProviderCatalogModel {
            model_id: "codex-fast".to_owned(),
            display_label: None,
            default_effort: None,
            supported_efforts: None,
        },
    ]
}

#[test]
fn resolve_merges_catalog_label_and_effort_evidence() {
    let (adapter, _servers) = duplex_adapter_with_catalog(advertised());
    let integration = integration();
    let descriptor = adapter
        .resolve(&ModelSelection {
            integration_id: integration.id.clone(),
            model: "codex-test-model".to_owned(),
            effort: Some(Effort::Medium),
        })
        .expect("advertised model resolves");
    assert_eq!(descriptor.model, "codex-test-model");
    assert_eq!(descriptor.label, "Codex Test");
    assert_eq!(
        descriptor.efforts,
        vec![Effort::Low, Effort::Medium],
        "catalog effort spellings reduce to the contract set"
    );
    assert!(descriptor.advertised);
    // The app-server carries no entitlement or quota evidence.
    assert_eq!(descriptor.entitled, CapabilityState::Unknown);
    assert!(descriptor.quota.is_none());
    // Presence is proven but authentication was never probed and no
    // discovery evidence is attached: readiness stays unknown.
    assert_eq!(descriptor.availability, ExecutionAvailability::Unknown);
}

#[test]
fn resolve_marks_unadvertised_models_and_rejects_unselectable_effort() {
    let (adapter, _servers) = duplex_adapter_with_catalog(advertised());
    let integration = integration();
    let descriptor = adapter
        .resolve(&ModelSelection {
            integration_id: integration.id.clone(),
            model: "not-in-catalog".to_owned(),
            effort: None,
        })
        .expect("an unadvertised model still resolves");
    assert!(!descriptor.advertised);
    assert!(descriptor.efforts.is_empty());

    let error = adapter
        .resolve(&ModelSelection {
            integration_id: integration.id.clone(),
            model: "codex-test-model".to_owned(),
            effort: Some(Effort::Max),
        })
        .expect_err("max is not selectable for this model");
    assert_eq!(error.code, ReceiptCode::InvalidSelection);
}

#[test]
fn list_models_reports_only_advertised_entries() {
    let (adapter, _servers) = duplex_adapter_with_catalog(advertised());
    let integration = integration();
    let models = adapter.list_models(&integration).expect("list_models");
    assert_eq!(
        models
            .iter()
            .map(|model| model.model.as_str())
            .collect::<Vec<_>>(),
        vec!["codex-test-model", "codex-fast"]
    );
    // The model with no reported effort data offers none.
    assert_eq!(models[0].efforts, vec![Effort::Low, Effort::Medium]);
    assert!(models[1].efforts.is_empty());
}

#[test]
fn a_foreign_integration_is_never_served() {
    let (adapter, _servers) = duplex_adapter();
    let mut foreign = integration();
    foreign.id = IntegrationId::new("codex-other");
    let error = adapter
        .list_models(&foreign)
        .expect_err("foreign integrations are not served");
    assert_eq!(error.code, ReceiptCode::Unsupported);
    let error = adapter
        .resolve(&ModelSelection {
            integration_id: foreign.id.clone(),
            model: "codex-test-model".to_owned(),
            effort: None,
        })
        .expect_err("foreign selections are not resolved");
    assert_eq!(error.code, ReceiptCode::Unsupported);
}

#[test]
fn start_rejects_unselectable_effort_before_any_transport() {
    let (adapter, servers) = duplex_adapter_with_catalog(advertised());
    let mut options = options(AccessMode::ReadOnly);
    options.selection.effort = Some(Effort::Max);
    let error = adapter
        .start(&integration(), options)
        .expect_err("unselectable effort fails start");
    assert_eq!(error.code, ReceiptCode::InvalidSelection);
    // The selection failed before the connector ran: no transport was
    // requested, so no fake server appears on the channel.
    assert!(
        servers.recv_timeout(Duration::from_millis(200)).is_err(),
        "a rejected selection never spawns the provider"
    );
}

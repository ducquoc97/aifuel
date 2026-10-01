//! Evidence-driven behavior: capabilities, availability, selection
//! resolution, and the compiled fallback set.

use super::*;
use aifuel_core::{
    AgentAuthenticationState, Effort, ExecutionAvailability, IntegrationId, ModelSelection,
    ReceiptCode,
};
use std::collections::BTreeSet;
use std::sync::Arc;

/// Capabilities come from declared evidence only: supported declarations
/// flip the matching flag and nothing else.
#[test]
fn capabilities_follow_declared_evidence() {
    let plain = CliAdapter::new(Arc::new(FakeExecution::new(ProviderKey::Claude)));
    assert_eq!(
        plain.capabilities(),
        aifuel_core::AdapterCapabilities {
            streaming: false,
            resume: false,
            approvals: false,
            checkpoints: false,
            effort: false,
            images: false,
            todos: false,
            external_tools: false,
        }
    );

    let codex_like = CliAdapter::new(Arc::new(FakeExecution::declaring(
        ProviderKey::Codex,
        &[
            AgentCapability::Streaming,
            AgentCapability::Resume,
            AgentCapability::PermissionApproval,
            AgentCapability::Effort,
        ],
    )));
    let capabilities = codex_like.capabilities();
    assert!(capabilities.streaming);
    assert!(capabilities.resume);
    assert!(capabilities.approvals);
    assert!(!capabilities.checkpoints);
    assert!(capabilities.effort);
}

/// Availability honors the auth probe first, then credential evidence.
#[test]
fn availability_maps_probe_and_evidence() {
    let adapter = CliAdapter::new(Arc::new(
        FakeExecution::new(ProviderKey::Claude)
            .with_authentication(AgentAuthenticationState::Unauthenticated),
    ));
    assert_eq!(adapter.availability(), ExecutionAvailability::NeedsAuth);

    let ready = CliAdapter::new(Arc::new(
        FakeExecution::new(ProviderKey::Claude)
            .with_authentication(AgentAuthenticationState::Authenticated),
    ));
    assert_eq!(ready.availability(), ExecutionAvailability::Ready);
}

/// `resolve` answers honestly for unadvertised models, rejects effort the
/// model does not offer, and refuses selections for other integrations.
#[test]
fn resolve_describes_and_rejects_honestly() {
    let adapter = adapter(vec![]);
    let selection = ModelSelection {
        integration_id: IntegrationId::new("claude"),
        model: "claude-opus-4".to_owned(),
        effort: None,
    };
    let descriptor = adapter.resolve(&selection).expect("the model resolves");
    assert!(!descriptor.advertised, "no catalog advertises this model");
    assert_eq!(descriptor.entitled, CapabilityState::Unknown);
    assert_eq!(
        descriptor.availability,
        ExecutionAvailability::Unknown,
        "an adapter without evidence reports unknown"
    );
    assert_eq!(descriptor.quota, None);

    let effort = ModelSelection {
        effort: Some(Effort::High),
        ..selection.clone()
    };
    assert_eq!(
        adapter
            .resolve(&effort)
            .expect_err("unselectable effort fails")
            .code,
        ReceiptCode::InvalidSelection
    );

    let foreign = ModelSelection {
        integration_id: IntegrationId::new("codex"),
        ..selection
    };
    assert_eq!(
        adapter
            .resolve(&foreign)
            .expect_err("foreign integrations are rejected")
            .code,
        ReceiptCode::Unsupported
    );
}

/// The fallback adapter set covers every compiled CLI adapter in registry
/// order and restates only their declared capabilities.
#[test]
fn cli_fallback_adapters_cover_the_compiled_set() {
    let home = std::env::temp_dir().join(format!("aifuel-cli-fallback-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let context = AdapterDiscovery {
        discovery: DiscoveryContext::new(&home),
        credentials: CredentialStore::new(home.join("aifuel")),
        configured: BTreeSet::new(),
    };
    let adapters = cli_fallback_adapters(&context);
    let compiled = agent_run_adapters();
    assert_eq!(adapters.len(), compiled.len());
    for (adapter, execution) in adapters.iter().zip(compiled.iter()) {
        assert_eq!(adapter.integration(), execution.integration());
        let capabilities = adapter.capabilities();
        let declared = execution.declared_agent_capabilities();
        let supported = |capability| {
            declared
                .get(&capability)
                .is_some_and(|evidence| evidence.state == CapabilityState::Supported)
        };
        assert_eq!(
            capabilities.streaming,
            supported(AgentCapability::Streaming)
        );
        assert_eq!(capabilities.resume, supported(AgentCapability::Resume));
        // External tool enforcement follows the wrapped executor's declared
        // evidence like every other capability - an executor that can
        // enforce an exact selection must not read as unable.
        assert_eq!(
            capabilities.external_tools,
            supported(AgentCapability::ExternalMcpTools)
        );
        assert!(!capabilities.checkpoints);
    }
}

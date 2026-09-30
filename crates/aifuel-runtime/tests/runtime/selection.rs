//! Registry-backed listing commands plus `model.select` and the
//! readiness gate: `session.create` and `model.select` resolve the
//! selection against the adapter and reject `needs_auth`/`unsupported`
//! availability before any provider call.

use crate::support::{
    FakeAdapter, consumer, create, created_session, fake_runtime, next_id, receipt_code, selection,
    test_dir,
};
use aifuel_core::{
    AgentCommand, CapabilityState, ExecutionAvailability, ModelDescriptor, ModelSelection,
    ProviderId, ReceiptCode,
};
use aifuel_runtime::CommandPayload;

/// One canned descriptor whose availability `models.list` reports and the
/// readiness gate consults.
fn unavailable_model(model: &str, availability: ExecutionAvailability) -> ModelDescriptor {
    ModelDescriptor {
        provider: ProviderId::new(crate::support::FAKE_PROVIDER),
        model: model.to_owned(),
        label: format!("Fake {model}"),
        efforts: vec![],
        advertised: true,
        entitled: CapabilityState::Unknown,
        availability,
        quota: None,
    }
}

#[test]
fn integrations_and_models_list_report_honest_descriptors() {
    let dir = test_dir("catalog");
    let (_store, runtime) = fake_runtime(
        &dir,
        FakeAdapter::default_capabilities(),
        vec![
            FakeAdapter::model("fake-a", &[aifuel_core::Effort::High]),
            FakeAdapter::model("fake-b", &[]),
        ],
        vec![],
    );
    let c1 = consumer("c1");
    let integrations = runtime.dispatch(
        AgentCommand::IntegrationsList {
            command_id: next_id(),
        },
        &c1,
    );
    let integrations = match integrations.payload {
        CommandPayload::Integrations(summaries) => summaries,
        _ => panic!("integrations.list carries its payload"),
    };
    assert_eq!(integrations.len(), 1);
    assert_eq!(
        integrations[0].integration_id.as_str(),
        crate::support::FAKE_INTEGRATION
    );
    assert!(
        integrations[0].capabilities.streaming,
        "the summary carries the adapter's declared capabilities"
    );

    let models = runtime.dispatch(
        AgentCommand::ModelsList {
            command_id: next_id(),
            integration_id: aifuel_core::IntegrationId::new(crate::support::FAKE_INTEGRATION),
        },
        &c1,
    );
    let models = match models.payload {
        CommandPayload::Models(models) => models,
        _ => panic!("models.list carries its payload"),
    };
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].model, "fake-a");
    assert_eq!(models[0].efforts, vec![aifuel_core::Effort::High]);
    assert_eq!(models[1].model, "fake-b");

    // A foreign integration reports `invalid_selection` honestly.
    let foreign = runtime.dispatch(
        AgentCommand::ModelsList {
            command_id: next_id(),
            integration_id: aifuel_core::IntegrationId::new("no-such-integration"),
        },
        &c1,
    );
    assert_eq!(receipt_code(&foreign), ReceiptCode::InvalidSelection);
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn open_builds_the_compiled_registry() {
    // `open` resolves the real provider catalog; every builtin integration
    // is listed with a presence/auth verdict and no panic.
    let dir = test_dir("open");
    let path = dir.join("aifuel.db");
    let runtime = aifuel_runtime::AgentRuntime::open(&path).expect("the runtime opens");
    let c1 = consumer("c1");
    let outcome = runtime.dispatch(
        AgentCommand::IntegrationsList {
            command_id: next_id(),
        },
        &c1,
    );
    let integrations = match outcome.payload {
        CommandPayload::Integrations(summaries) => summaries,
        _ => panic!("integrations.list carries its payload"),
    };
    assert!(
        !integrations.is_empty(),
        "the compiled adapter registry lists the built-in integrations"
    );
    assert!(
        integrations
            .iter()
            .any(|summary| summary.integration_id.as_str() == "claude"),
        "the builtin claude descriptor is listed"
    );
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn model_select_updates_the_persisted_selection() {
    let dir = test_dir("select");
    let (store, runtime) = fake_runtime(
        &dir,
        FakeAdapter::default_capabilities(),
        vec![
            FakeAdapter::model("fake-a", &[]),
            FakeAdapter::model("fake-b", &[aifuel_core::Effort::Low]),
        ],
        vec![],
    );
    let c1 = consumer("c1");
    let session_id = created_session(&runtime.dispatch(create(&dir, "fake-a"), &c1));
    let outcome = runtime.dispatch(
        AgentCommand::ModelSelect {
            command_id: next_id(),
            session_id: session_id.clone(),
            selection: ModelSelection {
                effort: Some(aifuel_core::Effort::Low),
                ..selection("fake-b")
            },
        },
        &c1,
    );
    assert!(outcome.receipt.ok);
    let session = store
        .agent_session(&session_id)
        .expect("session reads")
        .expect("session exists");
    assert_eq!(session.model.as_deref(), Some("fake-b"));
    assert_eq!(session.effort, Some(aifuel_core::Effort::Low));

    // An effort the model does not advertise reports `invalid_selection`
    // and leaves the selection unchanged.
    let outcome = runtime.dispatch(
        AgentCommand::ModelSelect {
            command_id: next_id(),
            session_id: session_id.clone(),
            selection: ModelSelection {
                effort: Some(aifuel_core::Effort::High),
                ..selection("fake-b")
            },
        },
        &c1,
    );
    assert_eq!(receipt_code(&outcome), ReceiptCode::InvalidSelection);
    let session = store
        .agent_session(&session_id)
        .expect("session reads")
        .expect("session exists");
    assert_eq!(session.model.as_deref(), Some("fake-b"));
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn readiness_gate_rejects_unavailable_selections() {
    // The registry's availability verdict is enforced before a provider
    // call happens: `needs_auth`/`unsupported` answer `invalid_selection`
    // on both `session.create` and `model.select`, while `unknown` passes
    // since an unprobed provider is not yet known broken.
    let dir = test_dir("readiness");
    let (_store, runtime) = fake_runtime(
        &dir,
        FakeAdapter::default_capabilities(),
        vec![
            FakeAdapter::model("fake-ready", &[]),
            unavailable_model("fake-auth", ExecutionAvailability::NeedsAuth),
            unavailable_model("fake-broken", ExecutionAvailability::Unsupported),
            unavailable_model("fake-unknown", ExecutionAvailability::Unknown),
        ],
        vec![],
    );
    let c1 = consumer("c1");

    for model in ["fake-auth", "fake-broken"] {
        let outcome = runtime.dispatch(create(&dir, model), &c1);
        assert_eq!(
            receipt_code(&outcome),
            ReceiptCode::InvalidSelection,
            "session.create rejects a {model} selection"
        );
    }
    let unknown = runtime.dispatch(create(&dir, "fake-unknown"), &c1);
    assert!(
        unknown.receipt.ok,
        "an unprobed selection proceeds: {:?}",
        unknown.receipt.outcome
    );

    // `model.select` applies the same gate on a live session.
    let session_id = created_session(&runtime.dispatch(create(&dir, "fake-ready"), &c1));
    for model in ["fake-auth", "fake-broken"] {
        let outcome = runtime.dispatch(
            AgentCommand::ModelSelect {
                command_id: next_id(),
                session_id: session_id.clone(),
                selection: selection(model),
            },
            &c1,
        );
        assert_eq!(
            receipt_code(&outcome),
            ReceiptCode::InvalidSelection,
            "model.select rejects a {model} selection"
        );
    }
    let outcome = runtime.dispatch(
        AgentCommand::ModelSelect {
            command_id: next_id(),
            session_id,
            selection: selection("fake-unknown"),
        },
        &c1,
    );
    assert!(
        outcome.receipt.ok,
        "model.select proceeds on an unknown availability"
    );
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

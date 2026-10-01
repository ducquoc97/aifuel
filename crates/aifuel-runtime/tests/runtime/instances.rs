//! Provider Integration instances through the facade: an instance id on
//! `session.create` serves the base integration's adapter with the
//! instance's resolved environment, persists the instance id on the
//! session row and its facts, and re-resolves the environment on a
//! reconcile resume - removed instances or credentials refuse rather than
//! resuming under a different configuration.

use crate::support::{
    FakeAdapter, FakeScript, collect_until, consumer, created_session, fake_descriptor,
    fake_discovery, next_id, receipt_code, run_start, subscribe, test_dir,
};
use aifuel_app::RunStore;
use aifuel_core::{
    AccessMode, AgentCommand, AgentEventKind, CredentialRef, IntegrationId, ModelSelection,
    ReceiptCode, SessionStatus,
};
use aifuel_providers::{CredentialStore, InstanceDescriptor, InstanceEnvSource};
use aifuel_runtime::AgentRuntime;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// The instance selector the tests use, over the fake integration.
const INSTANCE_ID: &str = "fake-cli.work";

fn instance(env: BTreeMap<String, InstanceEnvSource>) -> InstanceDescriptor {
    InstanceDescriptor {
        id: IntegrationId::new(INSTANCE_ID),
        integration: IntegrationId::new(crate::support::FAKE_INTEGRATION),
        env,
        credential: None,
    }
}

/// A runtime serving the fake adapter plus one instance over it, with the
/// Credential Store rooted at the test directory.
fn instance_runtime(
    dir: &std::path::Path,
    adapter: Arc<FakeAdapter>,
    instances: Vec<InstanceDescriptor>,
) -> (RunStore, AgentRuntime) {
    std::fs::create_dir_all(dir).expect("test dir creates");
    let store = RunStore::open(dir.join("aifuel.db")).expect("run store opens");
    let runtime = AgentRuntime::with_adapters(
        store.clone(),
        vec![adapter],
        vec![fake_descriptor()],
        instances,
        fake_discovery(dir),
    )
    .expect("runtime opens");
    (store, runtime)
}

fn create_instance(dir: &std::path::Path, model: &str) -> AgentCommand {
    AgentCommand::SessionCreate {
        command_id: next_id(),
        cwd: dir.to_path_buf(),
        selection: ModelSelection {
            integration_id: IntegrationId::new(INSTANCE_ID),
            model: model.to_owned(),
            effort: None,
        },
        access: AccessMode::WorkspaceWrite,
        resume_cursor: None,
        external_tools: Vec::new(),
    }
}

fn select(session_id: &aifuel_core::SessionId, integration: &str, model: &str) -> AgentCommand {
    AgentCommand::ModelSelect {
        command_id: next_id(),
        session_id: session_id.clone(),
        selection: ModelSelection {
            integration_id: IntegrationId::new(integration),
            model: model.to_owned(),
            effort: None,
        },
    }
}

#[test]
fn an_instance_session_serves_the_base_adapter_with_the_resolved_env() {
    let dir = test_dir("instance-create");
    // `fake_discovery` roots its Credential Store at `dir`; seeding the
    // store there puts the material where the facade resolves it.
    CredentialStore::new(&dir)
        .set_api_key(&CredentialRef::new("work-key"), "sk-work-secret")
        .expect("credential stores");

    let adapter = Arc::new(FakeAdapter::new(
        FakeAdapter::default_capabilities(),
        vec![FakeAdapter::model("fake-a", &[])],
    ));
    let overlay = BTreeMap::from([
        (
            "FAKE_CONFIG_DIR".to_owned(),
            InstanceEnvSource::Literal("/instance/cfg".to_owned()),
        ),
        (
            "FAKE_API_KEY".to_owned(),
            InstanceEnvSource::Credential(CredentialRef::new("work-key")),
        ),
    ]);
    let (_store, runtime) = instance_runtime(&dir, adapter.clone(), vec![instance(overlay)]);
    let c1 = consumer("c1");
    let outcome = runtime.dispatch(create_instance(&dir, "fake-a"), &c1);
    let session_id = created_session(&outcome);

    // The adapter saw the resolved overlay - literal plus credential
    // material - and the session row keeps the instance id the host sent.
    assert_eq!(
        adapter.recorded_envs(),
        vec![BTreeMap::from([
            ("FAKE_CONFIG_DIR".to_owned(), "/instance/cfg".to_owned()),
            ("FAKE_API_KEY".to_owned(), "sk-work-secret".to_owned()),
        ])],
        "the resolved overlay reaches the adapter start"
    );
    let persisted = _store
        .agent_session(&session_id)
        .expect("session reads")
        .expect("session persists");
    assert_eq!(persisted.integration.as_str(), INSTANCE_ID);

    // The `session.created` fact names the instance id, not the base.
    runtime.dispatch(subscribe(&session_id, 0), &c1);
    let events = runtime.events(&c1).expect("channel");
    let replayed = collect_until(&events, |event| {
        matches!(event.kind, AgentEventKind::SessionStatus { .. })
    });
    let created = replayed
        .iter()
        .find_map(|event| match &event.kind {
            AgentEventKind::SessionCreated { integration_id, .. } => Some(integration_id),
            _ => None,
        })
        .expect("session.created replays");
    assert_eq!(created.as_str(), INSTANCE_ID);

    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_instance_env_failure_rejects_before_any_adapter_start() {
    let dir = test_dir("instance-missing-credential");
    let adapter = Arc::new(FakeAdapter::new(
        FakeAdapter::default_capabilities(),
        vec![FakeAdapter::model("fake-a", &[])],
    ));
    let overlay = BTreeMap::from([(
        "FAKE_API_KEY".to_owned(),
        InstanceEnvSource::Credential(CredentialRef::new("absent")),
    )]);
    let (_store, runtime) = instance_runtime(&dir, adapter.clone(), vec![instance(overlay)]);
    let c1 = consumer("c1");
    let outcome = runtime.dispatch(create_instance(&dir, "fake-a"), &c1);
    assert_eq!(receipt_code(&outcome), ReceiptCode::InvalidSelection);
    assert!(
        adapter.recorded_envs().is_empty(),
        "a credential failure never reaches the adapter"
    );
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unknown_instance_ids_and_model_select_identity_rules_hold() {
    let dir = test_dir("instance-selection");
    let adapter = Arc::new(FakeAdapter::new(
        FakeAdapter::default_capabilities(),
        vec![
            FakeAdapter::model("fake-a", &[]),
            FakeAdapter::model("fake-b", &[]),
        ],
    ));
    let (_store, runtime) = instance_runtime(
        &dir,
        adapter.clone(),
        vec![
            instance(BTreeMap::new()),
            InstanceDescriptor {
                id: IntegrationId::new("fake-cli.other"),
                ..instance(BTreeMap::new())
            },
        ],
    );
    let c1 = consumer("c1");

    // An id that is neither integration nor instance fails selection.
    let outcome = runtime.dispatch(
        AgentCommand::SessionCreate {
            command_id: next_id(),
            cwd: dir.clone(),
            selection: ModelSelection {
                integration_id: IntegrationId::new("nobody.home"),
                model: "fake-a".to_owned(),
                effort: None,
            },
            access: AccessMode::WorkspaceWrite,
            resume_cursor: None,
            external_tools: Vec::new(),
        },
        &c1,
    );
    assert_eq!(receipt_code(&outcome), ReceiptCode::InvalidSelection);

    // `models.list` through the instance id reports the base's catalog.
    let outcome = runtime.dispatch(
        AgentCommand::ModelsList {
            command_id: next_id(),
            integration_id: IntegrationId::new(INSTANCE_ID),
        },
        &c1,
    );
    match &outcome.payload {
        aifuel_runtime::CommandPayload::Models(models) => {
            assert_eq!(models.len(), 2);
        }
        _ => panic!("expected a models payload: {:?}", outcome.receipt.outcome),
    }

    let session_id = created_session(&runtime.dispatch(create_instance(&dir, "fake-a"), &c1));

    // The same instance id reselects fine.
    let outcome = runtime.dispatch(select(&session_id, INSTANCE_ID, "fake-b"), &c1);
    assert!(outcome.receipt.ok, "{:?}", outcome.receipt.outcome);

    // The base id and a sibling instance id are different identities: a
    // mid-session swap would trade the environment the provider process
    // started under, so both are `invalid_selection`.
    for identity in [
        crate::support::FAKE_INTEGRATION,
        "fake-cli.other",
        "nobody.home",
    ] {
        let outcome = runtime.dispatch(select(&session_id, identity, "fake-a"), &c1);
        assert_eq!(
            receipt_code(&outcome),
            ReceiptCode::InvalidSelection,
            "{identity} must not rebind a {INSTANCE_ID} session"
        );
    }
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_reconcile_resume_re_resolves_the_instance_environment() {
    let dir = test_dir("instance-resume");
    CredentialStore::new(&dir)
        .set_api_key(&CredentialRef::new("work-key"), "sk-work-secret")
        .expect("credential stores");
    let overlay = || {
        BTreeMap::from([(
            "FAKE_API_KEY".to_owned(),
            InstanceEnvSource::Credential(CredentialRef::new("work-key")),
        )])
    };

    // Interrupt an in-flight instance session: the blocked run's cursor is
    // persisted by shutdown and the orphaned row is resume-eligible.
    let session_id;
    {
        let adapter = Arc::new(
            FakeAdapter::new(
                FakeAdapter::default_capabilities(),
                vec![FakeAdapter::model("fake-a", &[])],
            )
            .with_scripts(vec![FakeScript::Block {
                cursor: Some("provider-cursor-1"),
            }]),
        );
        let (store, runtime) = instance_runtime(&dir, adapter, vec![instance(overlay())]);
        let c1 = consumer("c1");
        session_id = created_session(&runtime.dispatch(create_instance(&dir, "fake-a"), &c1));
        assert!(
            runtime
                .dispatch(run_start(&session_id, "work"), &c1)
                .receipt
                .ok
        );
        for _ in 0..200 {
            let working = store
                .agent_session(&session_id)
                .expect("session reads")
                .is_some_and(|session| session.status == SessionStatus::Working);
            if working {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        runtime.shutdown();
    }

    // Reopen with the instance still defined: the resume resolves the same
    // overlay, so the adapter's second `start` sees the credential again.
    let resuming = Arc::new(FakeAdapter::new(
        FakeAdapter::default_capabilities(),
        vec![FakeAdapter::model("fake-a", &[])],
    ));
    let (_store, runtime) = instance_runtime(&dir, resuming.clone(), vec![instance(overlay())]);
    assert_eq!(
        resuming.recorded_envs(),
        vec![BTreeMap::from([(
            "FAKE_API_KEY".to_owned(),
            "sk-work-secret".to_owned()
        )])],
        "the resumed session re-resolves the instance env"
    );
    let persisted = _store
        .agent_session(&session_id)
        .expect("session reads")
        .expect("session persists");
    assert_eq!(persisted.status, SessionStatus::Working);
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_reconcile_resume_refuses_a_removed_instance_or_credential() {
    for drop_credential in [false, true] {
        let dir = test_dir(if drop_credential {
            "instance-resume-no-credential"
        } else {
            "instance-resume-no-instance"
        });
        CredentialStore::new(&dir)
            .set_api_key(&CredentialRef::new("work-key"), "sk-work-secret")
            .expect("credential stores");
        let overlay = || {
            BTreeMap::from([(
                "FAKE_API_KEY".to_owned(),
                InstanceEnvSource::Credential(CredentialRef::new("work-key")),
            )])
        };
        let session_id;
        {
            let adapter = Arc::new(
                FakeAdapter::new(
                    FakeAdapter::default_capabilities(),
                    vec![FakeAdapter::model("fake-a", &[])],
                )
                .with_scripts(vec![FakeScript::Block {
                    cursor: Some("provider-cursor-1"),
                }]),
            );
            let (store, runtime) = instance_runtime(&dir, adapter, vec![instance(overlay())]);
            let c1 = consumer("c1");
            session_id = created_session(&runtime.dispatch(create_instance(&dir, "fake-a"), &c1));
            assert!(
                runtime
                    .dispatch(run_start(&session_id, "work"), &c1)
                    .receipt
                    .ok
            );
            for _ in 0..200 {
                let working = store
                    .agent_session(&session_id)
                    .expect("session reads")
                    .is_some_and(|session| session.status == SessionStatus::Working);
                if working {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            runtime.shutdown();
        }
        if drop_credential {
            CredentialStore::new(&dir)
                .remove(&CredentialRef::new("work-key"))
                .expect("credential removes");
        }
        // Reopen missing the piece the session needs: the session stays
        // interrupted and the adapter is never asked to start.
        let resuming = Arc::new(FakeAdapter::new(
            FakeAdapter::default_capabilities(),
            vec![FakeAdapter::model("fake-a", &[])],
        ));
        let instances = if drop_credential {
            vec![instance(overlay())]
        } else {
            Vec::new()
        };
        let (_store, runtime) = instance_runtime(&dir, resuming.clone(), instances);
        assert!(
            resuming.recorded_envs().is_empty(),
            "a session whose instance cannot be re-resolved never resumes"
        );
        let persisted = _store
            .agent_session(&session_id)
            .expect("session reads")
            .expect("session persists");
        assert_eq!(persisted.status, SessionStatus::Interrupted);
        runtime.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}

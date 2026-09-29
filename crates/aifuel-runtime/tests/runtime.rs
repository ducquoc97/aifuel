//! End-to-end tests for the P0 `AgentRuntime` command surface, driven
//! through `dispatch` plus the consumer event channel against a scripted
//! in-crate adapter and a real Session Event Log store. Replay, restart,
//! and shutdown reconciliation live in `replay.rs`.

mod support;

use aifuel_core::{
    AccessMode, AgentCommand, AgentEventKind, ApprovalDecision, ApprovalKind, ApprovalOption,
    ApprovalRequest, Effort, IntegrationId, ModelSelection, ReceiptCode, RunId, SessionId,
    SessionStatus,
};
use aifuel_runtime::{AgentRuntime, CommandPayload};
use support::{
    FAKE_INTEGRATION, FakeAdapter, FakeScript, collect_run, collect_until, create, created_session,
    fake_descriptor, fake_runtime, is_completed, next_id, receipt_code, receipt_seq,
    receipt_snapshot, run_start, runtime_at, selection, subscribe, test_dir,
};

#[test]
fn session_create_subscribe_run_events_completed() {
    let dir = test_dir("flow");
    let (_store, runtime) = fake_runtime(
        &dir,
        FakeAdapter::default_capabilities(),
        vec![FakeAdapter::model("fake-a", &[])],
        vec![FakeScript::Complete {
            deltas: vec!["hello".to_owned(), " world".to_owned()],
            cursor: Some("native-9"),
        }],
    );

    let outcome = runtime.dispatch(create(&dir, "fake-a"), "consumer-a");
    let session_id = created_session(&outcome);
    assert_eq!(
        receipt_seq(&outcome.receipt),
        1,
        "session.created is the log's first fact"
    );

    let events = runtime.events("consumer-a").expect("consumer channel");
    let outcome = runtime.dispatch(subscribe(&session_id, 0), "consumer-a");
    assert!(outcome.receipt.ok);
    let snapshot = receipt_snapshot(&outcome.receipt);
    assert_eq!(snapshot.status, SessionStatus::Idle);
    assert_eq!(snapshot.selection.model, "fake-a");

    let outcome = runtime.dispatch(run_start(&session_id, "hi"), "consumer-a");
    assert!(outcome.receipt.ok, "run.start succeeds on a live session");

    let collected = collect_run(&events);
    let kinds: Vec<&AgentEventKind> = collected.iter().map(|event| &event.kind).collect();
    // The channel carries the replayed `session.created` then every live
    // fact in causal order.
    assert!(matches!(kinds[0], AgentEventKind::SessionCreated { .. }));
    assert!(matches!(
        kinds[1],
        AgentEventKind::SessionStatus {
            status: SessionStatus::Idle
        }
    ));
    assert!(matches!(kinds[2], AgentEventKind::RunStarted { .. }));
    assert!(matches!(
        kinds[3],
        AgentEventKind::SessionStatus {
            status: SessionStatus::Working
        }
    ));
    let deltas: Vec<&str> = collected
        .iter()
        .filter_map(|event| match &event.kind {
            AgentEventKind::MessageDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, ["hello", " world"]);
    assert!(
        collected.iter().any(|event| matches!(
            &event.kind,
            AgentEventKind::RunCompleted {
                outcome: aifuel_core::RunOutcome::Success,
                ..
            }
        )),
        "the run completes with success"
    );
    // Every event is stamped by the log in order for this session.
    for (index, event) in collected.iter().enumerate() {
        assert_eq!(event.seq, index as u64 + 1);
        assert_eq!(event.session_id, session_id);
    }
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn approval_answer_round_trip_and_second_answer_loses() {
    let dir = test_dir("approval");
    let request = ApprovalRequest {
        kind: ApprovalKind::ToolPermission,
        title: "run command".to_owned(),
        detail: "rm -rf build/".to_owned(),
        options: vec![
            ApprovalOption {
                id: "accept".to_owned(),
                label: "Accept".to_owned(),
            },
            ApprovalOption {
                id: "decline".to_owned(),
                label: "Decline".to_owned(),
            },
        ],
        requires_confirm: false,
    };
    let mut capabilities = FakeAdapter::default_capabilities();
    capabilities.approvals = true;
    let (_store, runtime) = fake_runtime(
        &dir,
        capabilities,
        vec![FakeAdapter::model("fake-a", &[])],
        vec![FakeScript::Approval {
            request: request.clone(),
        }],
    );
    let session_id = created_session(&runtime.dispatch(create(&dir, "fake-a"), "c1"));
    let events = runtime.events("c1").expect("channel");
    runtime.dispatch(subscribe(&session_id, 0), "c1");
    runtime.dispatch(run_start(&session_id, "edit"), "c1");

    let collected = collect_until(&events, |event| {
        matches!(event.kind, AgentEventKind::ApprovalRequested { .. })
    });
    let request_id = collected
        .iter()
        .find_map(|event| match &event.kind {
            AgentEventKind::ApprovalRequested { request_id, .. } => Some(request_id.clone()),
            _ => None,
        })
        .expect("the approval request carries its id");

    // The first answer wins and is attributed to the answering consumer.
    let outcome = runtime.dispatch(
        AgentCommand::ApprovalAnswer {
            command_id: next_id(),
            session_id: session_id.clone(),
            request_id: request_id.clone(),
            decision: ApprovalDecision::OptionId("accept".to_owned()),
        },
        "consumer-a",
    );
    assert!(outcome.receipt.ok, "the first answer resolves");

    let collected = collect_until(&events, |event| {
        matches!(event.kind, AgentEventKind::ApprovalResolved { .. })
    });
    match &collected.last().expect("resolved event").kind {
        AgentEventKind::ApprovalResolved {
            decision,
            answered_by,
            request_id: resolved_id,
        } => {
            assert_eq!(resolved_id, &request_id);
            assert_eq!(decision, &ApprovalDecision::OptionId("accept".to_owned()));
            assert_eq!(
                answered_by, "consumer-a",
                "the pump attributes the real consumer, not the adapter placeholder"
            );
        }
        other => panic!("expected approval.resolved: {other:?}"),
    }
    collect_run(&events);

    // A second answer to the resolved request loses with already_resolved.
    let outcome = runtime.dispatch(
        AgentCommand::ApprovalAnswer {
            command_id: next_id(),
            session_id: session_id.clone(),
            request_id: request_id.clone(),
            decision: ApprovalDecision::OptionId("decline".to_owned()),
        },
        "consumer-b",
    );
    assert_eq!(receipt_code(&outcome), ReceiptCode::AlreadyResolved);
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn integrations_and_models_list_report_honest_descriptors() {
    let dir = test_dir("list");
    let adapter = std::sync::Arc::new(FakeAdapter::new(
        FakeAdapter::default_capabilities(),
        vec![
            FakeAdapter::model("fake-a", &[Effort::Low, Effort::High]),
            FakeAdapter::model("fake-b", &[]),
        ],
    ));
    // A registered integration with no serving adapter lists with
    // all-false capabilities rather than disappearing.
    let ghost = aifuel_providers::IntegrationDescriptor::builtin(
        aifuel_core::Integration {
            id: IntegrationId::new("ghost"),
            provider: aifuel_core::ProviderId::new("ghost"),
            name: "Ghost".to_owned(),
            execution: aifuel_core::ExecutionConfig::Cli {
                adapter: aifuel_core::CliAdapterId::new("ghost"),
            },
            monitoring: None,
        },
        Vec::new(),
    );
    let (_store, runtime) = runtime_at(&dir, vec![adapter], vec![fake_descriptor(), ghost]);

    let outcome = runtime.dispatch(
        AgentCommand::IntegrationsList {
            command_id: next_id(),
        },
        "c1",
    );
    assert!(outcome.receipt.ok);
    let CommandPayload::Integrations(summaries) = &outcome.payload else {
        panic!("integrations.list carries the integrations payload");
    };
    assert_eq!(summaries.len(), 2);
    let fake = summaries
        .iter()
        .find(|summary| summary.integration_id.as_str() == FAKE_INTEGRATION)
        .expect("the fake integration lists");
    assert_eq!(fake.status.as_str(), "ready");
    assert!(fake.capabilities.streaming);
    assert!(fake.capabilities.resume);
    let ghost = summaries
        .iter()
        .find(|summary| summary.integration_id.as_str() == "ghost")
        .expect("the unserved integration still lists");
    assert!(!ghost.capabilities.streaming);
    assert!(!ghost.capabilities.resume);
    assert_eq!(ghost.status.as_str(), "unavailable");

    let outcome = runtime.dispatch(
        AgentCommand::ModelsList {
            command_id: next_id(),
            integration_id: IntegrationId::new(FAKE_INTEGRATION),
        },
        "c1",
    );
    let CommandPayload::Models(models) = &outcome.payload else {
        panic!("models.list carries the models payload");
    };
    assert_eq!(models.len(), 2);
    assert!(models[0].advertised);
    assert_eq!(models[0].availability.as_str(), "ready");

    // Unknown and unserved integrations fail with honest codes.
    let outcome = runtime.dispatch(
        AgentCommand::ModelsList {
            command_id: next_id(),
            integration_id: IntegrationId::new("nope"),
        },
        "c1",
    );
    assert_eq!(receipt_code(&outcome), ReceiptCode::InvalidSelection);
    let outcome = runtime.dispatch(
        AgentCommand::ModelsList {
            command_id: next_id(),
            integration_id: IntegrationId::new("ghost"),
        },
        "c1",
    );
    assert_eq!(receipt_code(&outcome), ReceiptCode::Unsupported);
    let outcome = runtime.dispatch(
        AgentCommand::SessionCreate {
            command_id: next_id(),
            cwd: dir.clone(),
            selection: ModelSelection {
                integration_id: IntegrationId::new("ghost"),
                model: "m".to_owned(),
                effort: None,
            },
            access: AccessMode::WorkspaceWrite,
        },
        "c1",
    );
    assert_eq!(receipt_code(&outcome), ReceiptCode::Unsupported);
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn session_close_records_the_fact_and_blocks_runs() {
    let dir = test_dir("close");
    let (store, runtime) = fake_runtime(
        &dir,
        FakeAdapter::default_capabilities(),
        vec![FakeAdapter::model("fake-a", &[])],
        vec![],
    );
    let session_id = created_session(&runtime.dispatch(create(&dir, "fake-a"), "c1"));

    let outcome = runtime.dispatch(
        AgentCommand::SessionClose {
            command_id: next_id(),
            session_id: session_id.clone(),
        },
        "c1",
    );
    assert!(outcome.receipt.ok);
    assert!(receipt_seq(&outcome.receipt) >= 2);

    let session = store
        .agent_session(&session_id)
        .expect("session reads")
        .expect("session exists");
    assert_eq!(session.status, SessionStatus::Closed);
    let outcome = runtime.dispatch(run_start(&session_id, "hi"), "c1");
    assert_eq!(receipt_code(&outcome), ReceiptCode::InvalidState);
    // Closing twice reports the state honestly rather than erroring blindly.
    let outcome = runtime.dispatch(
        AgentCommand::SessionClose {
            command_id: next_id(),
            session_id: session_id.clone(),
        },
        "c1",
    );
    assert_eq!(receipt_code(&outcome), ReceiptCode::InvalidState);
    // The log's last fact is session.closed.
    let page = store
        .replay(&session_id, 0, 100, usize::MAX)
        .expect("replay reads");
    assert!(matches!(
        page.events.last().map(|event| &event.kind),
        Some(AgentEventKind::SessionClosed { .. })
    ));
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn session_scoped_commands_report_unknown_session() {
    let dir = test_dir("unknown");
    let (_store, runtime) = fake_runtime(
        &dir,
        FakeAdapter::default_capabilities(),
        vec![FakeAdapter::model("fake-a", &[])],
        vec![],
    );
    let session_id = SessionId::new("no-such-session");
    for command in [
        subscribe(&session_id, 0),
        run_start(&session_id, "hi"),
        AgentCommand::RunCancel {
            command_id: next_id(),
            session_id: session_id.clone(),
            run_id: RunId::new("run-1"),
        },
        AgentCommand::ApprovalAnswer {
            command_id: next_id(),
            session_id: session_id.clone(),
            request_id: aifuel_core::RequestId::new("req-1"),
            decision: ApprovalDecision::OptionId("accept".to_owned()),
        },
        AgentCommand::ModelSelect {
            command_id: next_id(),
            session_id: session_id.clone(),
            selection: selection("fake-a"),
        },
        AgentCommand::SessionClose {
            command_id: next_id(),
            session_id: session_id.clone(),
        },
    ] {
        let outcome = runtime.dispatch(command, "c1");
        assert_eq!(receipt_code(&outcome), ReceiptCode::UnknownSession);
    }
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn model_select_updates_the_persisted_selection() {
    let dir = test_dir("model-select");
    let (store, runtime) = fake_runtime(
        &dir,
        FakeAdapter::default_capabilities(),
        vec![
            FakeAdapter::model("fake-a", &[Effort::Low, Effort::High]),
            FakeAdapter::model("fake-b", &[Effort::Low]),
        ],
        vec![],
    );
    let session_id = created_session(&runtime.dispatch(create(&dir, "fake-a"), "c1"));

    let outcome = runtime.dispatch(
        AgentCommand::ModelSelect {
            command_id: next_id(),
            session_id: session_id.clone(),
            selection: ModelSelection {
                integration_id: IntegrationId::new(FAKE_INTEGRATION),
                model: "fake-b".to_owned(),
                effort: Some(Effort::Low),
            },
        },
        "c1",
    );
    assert!(outcome.receipt.ok);
    let session = store
        .agent_session(&session_id)
        .expect("session reads")
        .expect("session exists");
    assert_eq!(session.model.as_deref(), Some("fake-b"));
    assert_eq!(session.effort, Some(Effort::Low));

    // An effort the descriptor does not offer fails invalid_selection.
    let outcome = runtime.dispatch(
        AgentCommand::ModelSelect {
            command_id: next_id(),
            session_id: session_id.clone(),
            selection: ModelSelection {
                integration_id: IntegrationId::new(FAKE_INTEGRATION),
                model: "fake-b".to_owned(),
                effort: Some(Effort::Max),
            },
        },
        "c1",
    );
    assert_eq!(receipt_code(&outcome), ReceiptCode::InvalidSelection);
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn run_cancel_reports_cancelled_completion() {
    let dir = test_dir("cancel");
    let (_store, runtime) = fake_runtime(
        &dir,
        FakeAdapter::default_capabilities(),
        vec![FakeAdapter::model("fake-a", &[])],
        vec![FakeScript::Block { cursor: None }],
    );
    let session_id = created_session(&runtime.dispatch(create(&dir, "fake-a"), "c1"));
    let events = runtime.events("c1").expect("channel");
    runtime.dispatch(subscribe(&session_id, 0), "c1");
    runtime.dispatch(run_start(&session_id, "work"), "c1");
    let collected = collect_until(&events, |event| {
        matches!(event.kind, AgentEventKind::RunStarted { .. })
    });
    let run_id = collected
        .iter()
        .find_map(|event| match &event.kind {
            AgentEventKind::RunStarted { run_id, .. } => Some(run_id.clone()),
            _ => None,
        })
        .expect("run.started carries the run id");

    let outcome = runtime.dispatch(
        AgentCommand::RunCancel {
            command_id: next_id(),
            session_id: session_id.clone(),
            run_id: run_id.clone(),
        },
        "c1",
    );
    assert!(outcome.receipt.ok);
    let collected = collect_run(&events);
    assert!(
        collected.iter().any(|event| matches!(
            &event.kind,
            AgentEventKind::RunCompleted {
                outcome: aifuel_core::RunOutcome::Cancelled,
                ..
            }
        )),
        "the cancelled run reports its outcome"
    );
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn two_consumers_each_receive_the_session_events() {
    let dir = test_dir("multi-consumer");
    let (_store, runtime) = fake_runtime(
        &dir,
        FakeAdapter::default_capabilities(),
        vec![FakeAdapter::model("fake-a", &[])],
        vec![FakeScript::Complete {
            deltas: vec!["hi".to_owned()],
            cursor: None,
        }],
    );
    let session_id = created_session(&runtime.dispatch(create(&dir, "fake-a"), "c1"));
    let first = runtime.events("consumer-a").expect("channel");
    let second = runtime.events("consumer-b").expect("channel");
    runtime.dispatch(subscribe(&session_id, 0), "consumer-a");
    runtime.dispatch(subscribe(&session_id, 0), "consumer-b");
    runtime.dispatch(run_start(&session_id, "go"), "consumer-a");

    let first = collect_run(&first);
    let second = collect_run(&second);
    assert!(
        first.iter().any(is_completed) && second.iter().any(is_completed),
        "both subscribed consumers receive the run's events"
    );
    // The event log serializes one sequence for both consumers.
    let seqs: Vec<u64> = first.iter().map(|event| event.seq).collect();
    let other: Vec<u64> = second.iter().map(|event| event.seq).collect();
    assert_eq!(seqs, other);
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn open_builds_the_compiled_registry() {
    let dir = test_dir("open");
    let runtime = AgentRuntime::open(dir.join("aifuel.db")).expect("runtime opens");
    let outcome = runtime.dispatch(
        AgentCommand::IntegrationsList {
            command_id: next_id(),
        },
        "c1",
    );
    let CommandPayload::Integrations(summaries) = &outcome.payload else {
        panic!("integrations.list carries the integrations payload");
    };
    assert!(
        !summaries.is_empty(),
        "the compiled adapter registry lists the built-in integrations"
    );
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

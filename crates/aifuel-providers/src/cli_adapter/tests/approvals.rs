//! Approval and provider-question round trips through `approval.answer`.

use super::*;
use aifuel_core::{
    AccessMode, AgentEventKind, AgentInteractionKind, AgentInteractionResponse, ApprovalDecision,
    ApprovalKind, PermissionApprovalDecision, ReceiptCode, RunOutcome,
};
use std::sync::Arc;

/// Permission approvals raise `approval.requested`; the decision maps onto
/// the provider-native response before the run resumes, and a second answer
/// loses with `already_resolved`.
#[test]
fn approval_request_round_trips_through_answer() {
    let adapter = CliAdapter::new(Arc::new(
        FakeExecution::declaring(ProviderKey::Codex, &[AgentCapability::PermissionApproval])
            .with_runs(vec![FakeRun::Interaction {
                kind: AgentInteractionKind::CommandApproval,
                description: "write requested",
                questions: Vec::new(),
                requires_expanded_access: false,
                expect: Box::new(|response| {
                    matches!(
                        response,
                        AgentInteractionResponse::Permission(PermissionApprovalDecision::Accept)
                    )
                }),
            }]),
    ));
    let handle = adapter
        .start(
            &integration(ProviderKey::Codex),
            options(ProviderKey::Codex, AccessMode::WorkspaceWrite),
        )
        .expect("session starts");
    let events = test_events(&adapter, &handle);
    adapter
        .send(&handle, input("go"))
        .expect("send is accepted");

    let request_id = loop {
        match recv(&events) {
            AgentEventKind::ApprovalRequested {
                request_id,
                request,
                ..
            } => {
                assert_eq!(request.kind, ApprovalKind::ToolPermission);
                assert_eq!(
                    request
                        .options
                        .iter()
                        .map(|option| option.id.as_str())
                        .collect::<Vec<_>>(),
                    ["accept", "decline", "cancel"],
                    "a writable run offers the full decision set"
                );
                break request_id;
            }
            AgentEventKind::RunCompleted { .. } => panic!("the run must wait on the approval"),
            _ => {}
        }
    };

    adapter
        .answer(
            &handle,
            request_id.clone(),
            ApprovalDecision::OptionId("accept".to_owned()),
        )
        .expect("the first answer is accepted");
    let duplicate = adapter.answer(
        &handle,
        request_id,
        ApprovalDecision::OptionId("decline".to_owned()),
    );
    assert_eq!(
        duplicate.expect_err("the second answer loses").code,
        ReceiptCode::AlreadyResolved
    );

    let kinds = through_run_completed(&events);
    let resolved = kinds.iter().position(|kind| {
        matches!(
            kind,
            AgentEventKind::ApprovalResolved { decision, .. }
                if *decision == ApprovalDecision::OptionId("accept".to_owned())
        )
    });
    assert!(
        resolved.is_some(),
        "the resolved fact lands before completion: {kinds:?}"
    );
    assert!(matches!(
        kinds.last(),
        Some(AgentEventKind::RunCompleted {
            outcome: RunOutcome::Success,
            ..
        })
    ));
}

/// A permission-profile request offers only `decline`: the profile payload
/// cannot be constructed from a boolean accept.
#[test]
fn permission_profile_requests_offer_decline_only() {
    let adapter = CliAdapter::new(Arc::new(
        FakeExecution::declaring(ProviderKey::Codex, &[AgentCapability::PermissionApproval])
            .with_runs(vec![FakeRun::Interaction {
                kind: AgentInteractionKind::PermissionProfileApproval,
                description: "access requested",
                questions: Vec::new(),
                requires_expanded_access: true,
                expect: Box::new(|response| {
                    matches!(
                        response,
                        AgentInteractionResponse::PermissionProfile { scope, .. }
                            if scope == "turn"
                    )
                }),
            }]),
    ));
    let handle = adapter
        .start(
            &integration(ProviderKey::Codex),
            options(ProviderKey::Codex, AccessMode::WorkspaceWrite),
        )
        .expect("session starts");
    let events = test_events(&adapter, &handle);
    adapter
        .send(&handle, input("go"))
        .expect("send is accepted");
    let request_id = loop {
        match recv(&events) {
            AgentEventKind::ApprovalRequested {
                request_id,
                request,
                ..
            } => {
                assert_eq!(
                    request
                        .options
                        .iter()
                        .map(|option| option.id.as_str())
                        .collect::<Vec<_>>(),
                    ["decline"]
                );
                break request_id;
            }
            AgentEventKind::RunCompleted { .. } => panic!("the run waits on the request"),
            _ => {}
        }
    };
    adapter
        .answer(
            &handle,
            request_id,
            ApprovalDecision::OptionId("decline".to_owned()),
        )
        .expect("decline resolves the request");
    assert!(matches!(
        through_run_completed(&events).last(),
        Some(AgentEventKind::RunCompleted {
            outcome: RunOutcome::Success,
            ..
        })
    ));
}

/// A declared `answers` decision maps straight onto the provider's
/// question-id-keyed map - the shape a multi-question ask needs, which
/// free text cannot express.
#[test]
fn answers_decision_maps_onto_the_declared_questions() {
    let adapter = CliAdapter::new(Arc::new(
        FakeExecution::declaring(ProviderKey::Codex, &[AgentCapability::OrdinaryInput]).with_runs(
            vec![FakeRun::Interaction {
                kind: AgentInteractionKind::OrdinaryInput,
                description: "provider asked",
                questions: vec![("branch", "Which branch"), ("target", "Which target")],
                requires_expanded_access: false,
                expect: Box::new(|response| {
                    matches!(
                        response,
                        AgentInteractionResponse::Answers(answers)
                            if answers.get("branch") == Some(&vec!["main".to_owned()])
                                && answers.get("target") == Some(&vec!["workspace".to_owned()])
                    )
                }),
            }],
        ),
    ));
    let handle = adapter
        .start(
            &integration(ProviderKey::Codex),
            options(ProviderKey::Codex, AccessMode::WorkspaceWrite),
        )
        .expect("session starts");
    let events = test_events(&adapter, &handle);
    adapter
        .send(&handle, input("go"))
        .expect("send is accepted");
    let request_id = loop {
        match recv(&events) {
            AgentEventKind::ApprovalRequested {
                request_id,
                request,
                ..
            } => {
                assert_eq!(request.kind, ApprovalKind::Question);
                break request_id;
            }
            AgentEventKind::RunCompleted { .. } => panic!("the run waits on the question"),
            _ => {}
        }
    };
    adapter
        .answer(
            &handle,
            request_id,
            ApprovalDecision::Answers(std::collections::BTreeMap::from([
                ("branch".to_owned(), vec!["main".to_owned()]),
                ("target".to_owned(), vec!["workspace".to_owned()]),
            ])),
        )
        .expect("the answers map resolves the ask");
    assert!(matches!(
        through_run_completed(&events).last(),
        Some(AgentEventKind::RunCompleted {
            outcome: RunOutcome::Success,
            ..
        })
    ));
}

/// An `elicitation` decision returns the content JSON verbatim, the
/// shape the MCP `requestedSchema` asked for.
#[test]
fn elicitation_decision_returns_verbatim_content() {
    let adapter = CliAdapter::new(Arc::new(
        FakeExecution::declaring(ProviderKey::Codex, &[AgentCapability::OrdinaryInput]).with_runs(
            vec![FakeRun::Interaction {
                kind: AgentInteractionKind::McpElicitation,
                description: "provider elicited",
                questions: Vec::new(),
                requires_expanded_access: false,
                expect: Box::new(|response| {
                    matches!(
                        response,
                        AgentInteractionResponse::Elicitation(content)
                            if content == &serde_json::json!({"mode": "fast"})
                    )
                }),
            }],
        ),
    ));
    let handle = adapter
        .start(
            &integration(ProviderKey::Codex),
            options(ProviderKey::Codex, AccessMode::WorkspaceWrite),
        )
        .expect("session starts");
    let events = test_events(&adapter, &handle);
    adapter
        .send(&handle, input("go"))
        .expect("send is accepted");
    let request_id = loop {
        match recv(&events) {
            AgentEventKind::ApprovalRequested { request_id, .. } => break request_id,
            AgentEventKind::RunCompleted { .. } => panic!("the run waits on the ask"),
            _ => {}
        }
    };
    adapter
        .answer(
            &handle,
            request_id,
            ApprovalDecision::Elicitation(serde_json::json!({"mode": "fast"})),
        )
        .expect("the elicitation content resolves the ask");
    assert!(matches!(
        through_run_completed(&events).last(),
        Some(AgentEventKind::RunCompleted {
            outcome: RunOutcome::Success,
            ..
        })
    ));
}

/// Answers naming a question the ask never declared are rejected rather
/// than forwarded, and the ask stays pending for a valid answer.
#[test]
fn answers_for_unasked_questions_are_rejected() {
    let adapter = CliAdapter::new(Arc::new(
        FakeExecution::declaring(ProviderKey::Codex, &[AgentCapability::OrdinaryInput]).with_runs(
            vec![FakeRun::Interaction {
                kind: AgentInteractionKind::OrdinaryInput,
                description: "provider asked",
                questions: vec![("branch", "Which branch")],
                requires_expanded_access: false,
                expect: Box::new(|response| {
                    matches!(
                        response,
                        AgentInteractionResponse::Answers(answers)
                            if answers.get("branch") == Some(&vec!["main".to_owned()])
                    )
                }),
            }],
        ),
    ));
    let handle = adapter
        .start(
            &integration(ProviderKey::Codex),
            options(ProviderKey::Codex, AccessMode::WorkspaceWrite),
        )
        .expect("session starts");
    let events = test_events(&adapter, &handle);
    adapter
        .send(&handle, input("go"))
        .expect("send is accepted");
    let request_id = loop {
        match recv(&events) {
            AgentEventKind::ApprovalRequested { request_id, .. } => break request_id,
            AgentEventKind::RunCompleted { .. } => panic!("the run waits on the question"),
            _ => {}
        }
    };
    let error = adapter
        .answer(
            &handle,
            request_id.clone(),
            ApprovalDecision::Answers(std::collections::BTreeMap::from([(
                "unasked".to_owned(),
                vec!["sneaky".to_owned()],
            )])),
        )
        .expect_err("answers for unasked questions are rejected");
    assert_eq!(error.code, ReceiptCode::InvalidState);
    adapter
        .answer(
            &handle,
            request_id,
            ApprovalDecision::Answers(std::collections::BTreeMap::from([(
                "branch".to_owned(),
                vec!["main".to_owned()],
            )])),
        )
        .expect("the declared answer resolves the ask");
    assert!(matches!(
        through_run_completed(&events).last(),
        Some(AgentEventKind::RunCompleted {
            outcome: RunOutcome::Success,
            ..
        })
    ));
}

/// Ordinary-input questions take free text, mapped onto the provider's
/// question-id-keyed answers shape.
#[test]
fn ordinary_input_takes_text_and_maps_onto_answers() {
    let adapter = CliAdapter::new(Arc::new(
        FakeExecution::declaring(ProviderKey::Codex, &[AgentCapability::OrdinaryInput]).with_runs(
            vec![FakeRun::Interaction {
                kind: AgentInteractionKind::OrdinaryInput,
                description: "provider asked",
                questions: vec![("target", "Choose a target")],
                requires_expanded_access: false,
                expect: Box::new(|response| {
                    matches!(
                        response,
                        AgentInteractionResponse::Answers(answers)
                            if answers.get("target") == Some(&vec!["workspace".to_owned()])
                    )
                }),
            }],
        ),
    ));
    let handle = adapter
        .start(
            &integration(ProviderKey::Codex),
            options(ProviderKey::Codex, AccessMode::WorkspaceWrite),
        )
        .expect("session starts");
    let events = test_events(&adapter, &handle);
    adapter
        .send(&handle, input("go"))
        .expect("send is accepted");
    let request_id = loop {
        match recv(&events) {
            AgentEventKind::ApprovalRequested {
                request_id,
                request,
                ..
            } => {
                assert_eq!(request.kind, ApprovalKind::Question);
                break request_id;
            }
            AgentEventKind::RunCompleted { .. } => panic!("the run waits on the question"),
            _ => {}
        }
    };
    adapter
        .answer(
            &handle,
            request_id,
            ApprovalDecision::Text("workspace".to_owned()),
        )
        .expect("text resolves the question");
    assert!(matches!(
        through_run_completed(&events).last(),
        Some(AgentEventKind::RunCompleted {
            outcome: RunOutcome::Success,
            ..
        })
    ));
}

use super::*;

#[test]
fn managed_cli_run_prints_the_local_approval_command() {
    let manager = aifuel_app::RunManager::new(Vec::<Arc<dyn AgentExecutionAdapter>>::new());
    let pending = PendingRunInput {
        input_id: "input-1".to_owned(),
        run_id: "run-77".to_owned(),
        kind: RunInputKind::Permission,
        interaction_kind: AgentInteractionKind::CommandApproval,
        description: "run shell command".to_owned(),
        native_method: Some("item/commandExecution/requestApproval".to_owned()),
        questions: Vec::new(),
        question_ids: Vec::new(),
        parameters: None,
        requires_expanded_access: false,
    };
    let run = ManagedRun {
        schema_version: aifuel_core::RUN_MANAGEMENT_SCHEMA_VERSION,
        run_id: "run-77".to_owned(),
        state: RunState::WaitingForApproval,
        provider: ProviderKey::Codex,
        requested_model: None,
        requested_effort: None,
        external_tools: None,
        created_at: 0.0,
        completed_at: None,
        content_available: false,
        pending_input: Some(pending.clone()),
    };
    let mut input = io::Cursor::new(Vec::new());
    let mut diagnostics = Vec::new();

    let action = handle_pending_input(
        &manager,
        &run,
        &pending,
        &mut input,
        &mut diagnostics,
        false,
    )
    .expect("a local command approval can be routed to the owner");

    assert!(matches!(action, PendingInputAction::WaitingForApproval));
    assert_eq!(
        String::from_utf8(diagnostics).expect("approval instructions are UTF-8"),
        "Permission requested: run shell command\nApprove or reject it with: aifuel approve --run run-77 --input input-1 --decision accept|decline|cancel\n"
    );
}

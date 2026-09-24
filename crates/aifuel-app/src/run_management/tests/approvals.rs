use super::*;

fn pending_profile_record(run_id: &str) -> Arc<RunRecord> {
    pending_approval_record(
        run_id,
        AccessMode::ReadOnly,
        AgentInteractionKind::PermissionProfileApproval,
        false,
    )
}

fn pending_approval_record(
    run_id: &str,
    access: AccessMode,
    interaction_kind: AgentInteractionKind,
    requires_expanded_access: bool,
) -> Arc<RunRecord> {
    let mut request = request();
    request.access = access;
    let record = Arc::new(RunRecord::new(run_id.to_owned(), &request, None, None));
    record.metadata.lock().expect("run metadata mutex").state = RunState::WaitingForApproval;
    *record.pending_input.lock().expect("pending input mutex") = Some(PendingRunInput {
        input_id: "pending-approval".to_owned(),
        run_id: run_id.to_owned(),
        kind: RunInputKind::Permission,
        interaction_kind,
        description: "request permission".to_owned(),
        native_method: None,
        questions: Vec::new(),
        question_ids: Vec::new(),
        parameters: None,
        requires_expanded_access,
    });
    record
}

#[cfg(unix)]
fn command_approval_request() -> AgentInteractionRequest {
    AgentInteractionRequest {
        request_id: serde_json::json!(1),
        method: "item/commandExecution/requestApproval".to_owned(),
        kind: AgentInteractionKind::CommandApproval,
        description: "approve command".to_owned(),
        questions: Vec::new(),
        parameters: serde_json::json!({"command":"ls"}),
        requires_expanded_access: false,
    }
}

#[cfg(unix)]
fn insert_interaction_record(
    manager: &RunManager,
    run_id: &str,
    access: AccessMode,
    deadline: Option<Instant>,
) -> Arc<RunRecord> {
    let mut request = request();
    request.access = access;
    let record = Arc::new(RunRecord::new(run_id.to_owned(), &request, deadline, None));
    manager
        .inner
        .records
        .lock()
        .expect("run records mutex")
        .insert(run_id.to_owned(), Arc::clone(&record));
    record
}

#[cfg(unix)]
fn spawn_interaction_wait(
    manager: &RunManager,
    record: Arc<RunRecord>,
) -> thread::JoinHandle<Result<AgentInteractionResponse, AgentRunError>> {
    let manager = manager.clone();
    thread::spawn(move || {
        manager.wait_for_provider_interaction(
            &record,
            command_approval_request(),
            &record.cancellation,
        )
    })
}

#[cfg(unix)]
fn wait_for_pending_input(record: &RunRecord) -> PendingRunInput {
    for _ in 0..1000 {
        if let Some(pending) = record
            .pending_input
            .lock()
            .expect("pending input mutex")
            .clone()
        {
            return pending;
        }
        thread::sleep(Duration::from_millis(1));
    }
    panic!("run did not publish a pending input");
}

#[cfg(unix)]
fn pending_approval_count(directory: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("pending-") && name.ends_with(".json"))
        })
        .count()
}

#[cfg(unix)]
fn wait_for_pending_mapping(directory: &Path) {
    for _ in 0..1000 {
        if pending_approval_count(directory) > 0 {
            return;
        }
        thread::sleep(Duration::from_millis(1));
    }
    panic!("run did not publish a pending approval mapping");
}

#[test]
fn permission_profile_approval_cannot_expand_run_permissions() {
    let manager = RunManager::new(Vec::<Arc<ProbeAdapter>>::new());
    let accept_record = pending_profile_record("profile-accept-run");
    manager
        .inner
        .records
        .lock()
        .expect("run records mutex")
        .insert(accept_record.run_id.clone(), Arc::clone(&accept_record));

    let error = manager
        .approve_locally(
            &accept_record.run_id,
            "pending-approval",
            PermissionApprovalDecision::Accept,
        )
        .expect_err("permission profile acceptance could expand run access");
    assert_eq!(error.code, RunManagementErrorCode::PolicyDenied);
    assert!(
        accept_record
            .pending_input
            .lock()
            .expect("pending input mutex")
            .is_some()
    );
    assert!(
        accept_record
            .interaction_response
            .lock()
            .expect("interaction response mutex")
            .is_none()
    );

    let decline_record = pending_profile_record("profile-decline-run");
    manager
        .inner
        .records
        .lock()
        .expect("run records mutex")
        .insert(decline_record.run_id.clone(), Arc::clone(&decline_record));
    manager
        .approve_locally(
            &decline_record.run_id,
            "pending-approval",
            PermissionApprovalDecision::Decline,
        )
        .expect("decline with an empty permission profile");
    assert!(matches!(
        decline_record
            .interaction_response
            .lock()
            .expect("interaction response mutex")
            .as_ref(),
        Some(AgentInteractionResponse::PermissionProfile { permissions, scope })
            if permissions == &serde_json::json!({}) && scope == "turn"
    ));

    let cancel_record = pending_profile_record("profile-cancel-run");
    manager
        .inner
        .records
        .lock()
        .expect("run records mutex")
        .insert(cancel_record.run_id.clone(), Arc::clone(&cancel_record));
    let cancelled = manager
        .approve_locally(
            &cancel_record.run_id,
            "pending-approval",
            PermissionApprovalDecision::Cancel,
        )
        .expect("cancel the run");
    assert_eq!(cancelled.state, RunState::Cancelling);
    assert!(cancel_record.cancellation.is_cancelled());
    assert!(cancelled.pending_input.is_none());
    manager.shutdown();
}

#[test]
fn command_and_file_approvals_cannot_exceed_run_sandbox() {
    let manager = RunManager::new(Vec::<Arc<ProbeAdapter>>::new());
    let insert_record = |record: &Arc<RunRecord>| {
        manager
            .inner
            .records
            .lock()
            .expect("run records mutex")
            .insert(record.run_id.clone(), Arc::clone(record));
    };

    let readonly_command = pending_approval_record(
        "readonly-command",
        AccessMode::ReadOnly,
        AgentInteractionKind::CommandApproval,
        false,
    );
    insert_record(&readonly_command);
    let error = manager
        .approve_locally(
            &readonly_command.run_id,
            "pending-approval",
            PermissionApprovalDecision::Accept,
        )
        .expect_err("read-only command accept must be denied");
    assert_eq!(error.code, RunManagementErrorCode::PolicyDenied);

    let additional_permissions = pending_approval_record(
        "workspace-command-extra",
        AccessMode::WorkspaceWrite,
        AgentInteractionKind::CommandApproval,
        true,
    );
    insert_record(&additional_permissions);
    let error = manager
        .approve_locally(
            &additional_permissions.run_id,
            "pending-approval",
            PermissionApprovalDecision::Accept,
        )
        .expect_err("additional permissions exceed the run sandbox");
    assert_eq!(error.code, RunManagementErrorCode::PolicyDenied);

    let readonly_file_change = pending_approval_record(
        "readonly-file-change",
        AccessMode::ReadOnly,
        AgentInteractionKind::FileChangeApproval,
        false,
    );
    insert_record(&readonly_file_change);
    let error = manager
        .approve_locally(
            &readonly_file_change.run_id,
            "pending-approval",
            PermissionApprovalDecision::Accept,
        )
        .expect_err("read-only file-change accept must be denied");
    assert_eq!(error.code, RunManagementErrorCode::PolicyDenied);

    let file_change_extra_root = pending_approval_record(
        "workspace-file-change-extra",
        AccessMode::WorkspaceWrite,
        AgentInteractionKind::FileChangeApproval,
        true,
    );
    insert_record(&file_change_extra_root);
    let error = manager
        .approve_locally(
            &file_change_extra_root.run_id,
            "pending-approval",
            PermissionApprovalDecision::Accept,
        )
        .expect_err("file-change grant root exceeds the run sandbox");
    assert_eq!(error.code, RunManagementErrorCode::PolicyDenied);

    let workspace_command = pending_approval_record(
        "workspace-command-base-sandbox",
        AccessMode::WorkspaceWrite,
        AgentInteractionKind::CommandApproval,
        false,
    );
    insert_record(&workspace_command);
    manager
        .approve_locally(
            &workspace_command.run_id,
            "pending-approval",
            PermissionApprovalDecision::Accept,
        )
        .expect("workspace write may accept within its base sandbox");
    manager.shutdown();
}

#[cfg(unix)]
#[test]
fn approval_endpoint_is_lazy_and_pending_mapping_is_removed_after_answer() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "aifuel-lazy-approval-{}-{nonce}",
        std::process::id()
    ));
    let manager = RunManager::new(Vec::<Arc<ProbeAdapter>>::new())
        .with_local_approval_channel(&directory)
        .expect("configure local approval directory");
    assert!(
        !directory.exists(),
        "configuration alone must not start IPC"
    );
    assert!(
        manager
            .inner
            .approval_server
            .lock()
            .expect("approval server mutex")
            .is_none()
    );

    let record = insert_interaction_record(&manager, "lazy-run", AccessMode::WorkspaceWrite, None);
    let interaction = spawn_interaction_wait(&manager, Arc::clone(&record));
    let pending = wait_for_pending_input(&record);
    wait_for_pending_mapping(&directory);
    assert_eq!(pending_approval_count(&directory), 1);

    crate::approval_ipc::submit_local_approval(
        &directory,
        &record.run_id,
        &pending.input_id,
        LocalApprovalDecision::Accept,
    )
    .expect("deliver answer through the exact pending mapping");
    assert!(matches!(
        interaction.join().expect("provider waiter joins"),
        Ok(AgentInteractionResponse::Permission(
            PermissionApprovalDecision::Accept
        ))
    ));
    assert_eq!(pending_approval_count(&directory), 0);

    manager.shutdown();
    assert_eq!(
        std::fs::read_dir(&directory)
            .expect("owner directory remains")
            .count(),
        0,
        "owner shutdown removes its record"
    );
    std::fs::remove_dir(directory).expect("remove test directory");
}

#[cfg(unix)]
#[test]
fn failed_pending_mapping_registration_clears_waiting_input() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let invalid_directory = std::env::temp_dir().join(format!(
        "aifuel-approval-file-{}-{nonce}",
        std::process::id()
    ));
    std::fs::write(&invalid_directory, b"not a directory").expect("create invalid path");
    let manager = RunManager::new(Vec::<Arc<ProbeAdapter>>::new())
        .with_local_approval_channel(&invalid_directory)
        .expect("configure local approval path");
    let record = insert_interaction_record(
        &manager,
        "registration-failure-run",
        AccessMode::ReadOnly,
        None,
    );
    let error = manager
        .wait_for_provider_interaction(&record, command_approval_request(), &record.cancellation)
        .expect_err("a file cannot host approval records");
    assert!(matches!(error, AgentRunError::InvalidRequest(_)));
    assert!(
        record
            .pending_input
            .lock()
            .expect("pending input mutex")
            .is_none()
    );
    assert_eq!(
        record.metadata.lock().expect("run metadata mutex").state,
        RunState::Running
    );
    assert!(
        manager
            .inner
            .approval_server
            .lock()
            .expect("approval server mutex")
            .is_none()
    );
    manager.shutdown();
    std::fs::remove_file(invalid_directory).expect("remove invalid path");
}

#[cfg(unix)]
#[test]
fn pending_approval_mapping_is_removed_on_cancellation_and_timeout() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let cancellation_directory = std::env::temp_dir().join(format!(
        "aifuel-approval-cancel-{}-{nonce}",
        std::process::id()
    ));
    let manager = RunManager::new(Vec::<Arc<ProbeAdapter>>::new())
        .with_local_approval_channel(&cancellation_directory)
        .expect("configure cancellation approval directory");
    let record = insert_interaction_record(&manager, "cancel-run", AccessMode::ReadOnly, None);
    let interaction = spawn_interaction_wait(&manager, Arc::clone(&record));
    wait_for_pending_input(&record);
    wait_for_pending_mapping(&cancellation_directory);
    assert_eq!(pending_approval_count(&cancellation_directory), 1);
    manager
        .cancel_run(&record.run_id)
        .expect("cancel pending approval");
    assert!(matches!(
        interaction.join().expect("cancelled waiter joins"),
        Err(AgentRunError::Cancelled)
    ));
    assert_eq!(pending_approval_count(&cancellation_directory), 0);
    manager.shutdown();
    std::fs::remove_dir(cancellation_directory).expect("remove cancellation directory");

    let timeout_directory = std::env::temp_dir().join(format!(
        "aifuel-approval-timeout-{}-{nonce}",
        std::process::id()
    ));
    let manager = RunManager::new(Vec::<Arc<ProbeAdapter>>::new())
        .with_local_approval_channel(&timeout_directory)
        .expect("configure timeout approval directory");
    let record = insert_interaction_record(
        &manager,
        "timeout-run",
        AccessMode::ReadOnly,
        Some(Instant::now() + Duration::from_millis(300)),
    );
    let interaction = spawn_interaction_wait(&manager, Arc::clone(&record));
    wait_for_pending_input(&record);
    wait_for_pending_mapping(&timeout_directory);
    assert_eq!(pending_approval_count(&timeout_directory), 1);
    assert!(matches!(
        interaction.join().expect("timed out waiter joins"),
        Err(AgentRunError::Timeout(_))
    ));
    assert_eq!(pending_approval_count(&timeout_directory), 0);
    manager.shutdown();
    std::fs::remove_dir(timeout_directory).expect("remove timeout directory");
}

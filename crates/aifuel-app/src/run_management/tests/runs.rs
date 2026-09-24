use super::*;

#[test]
fn start_resolve_and_result_share_the_owner_local_contract() {
    let wait = Arc::new(AtomicBool::new(false));
    let adapter = Arc::new(ProbeAdapter {
        provider: ProviderKey::Claude,
        wait,
    });
    let manager = RunManager::new(vec![adapter]);

    let resolved = manager.resolve_run(&request()).expect("request resolves");
    assert_eq!(resolved.provider, ProviderKey::Claude);
    assert_eq!(resolved.requested_model.as_deref(), Some("model-a"));
    assert!(
        !serde_json::to_string(&resolved)
            .expect("resolved DTO serializes")
            .contains("hello")
    );

    let started = manager.start_run(request()).expect("run starts");
    let terminal = wait_for_terminal(&manager, &started.run_id);
    assert_eq!(terminal.state, RunState::Succeeded);
    assert_eq!(terminal.provider, ProviderKey::Claude);
    assert!(terminal.completed_at.is_some());

    let result = manager
        .get_result(&started.run_id)
        .expect("result is available");
    assert_eq!(result.status, Some(RunStatus::Succeeded));
    assert_eq!(result.output.as_deref(), Some("hello"));
    assert_eq!(result.session_id.as_deref(), Some("native-session"));
    let events = manager
        .read_events(&started.run_id, None, None)
        .expect("events are readable");
    assert!(events.terminal);
    assert!(!events.gap);
    assert!(
        events
            .events
            .windows(2)
            .all(|events| events[0].sequence < events[1].sequence)
    );
    manager.shutdown();
}

#[test]
fn provider_questions_are_typed_and_parameters_remain_separate() {
    let manager = RunManager::new(vec![Arc::new(InputAdapter)]);
    let started = manager.start_run(request()).expect("run starts");
    let mut pending = None;
    for _ in 0..100 {
        pending = manager
            .get_run(&started.run_id)
            .expect("run remains owner-local")
            .pending_input;
        if pending.is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    let pending = pending.expect("native input is published");
    assert_eq!(
        pending.questions,
        vec![aifuel_core::AgentInputQuestion {
            id: "target".to_owned(),
            text: "Which target?".to_owned(),
        }]
    );
    assert_eq!(pending.question_ids, ["target"]);
    assert_eq!(
        pending.parameters,
        Some(serde_json::json!({"diagnostic":"retained separately"}))
    );

    manager
        .answer_input(&started.run_id, &pending.input_id, "workspace")
        .expect("typed input answer resumes the run");
    assert_eq!(
        wait_for_terminal(&manager, &started.run_id).state,
        RunState::Succeeded
    );
    manager.shutdown();
}

#[test]
fn read_only_external_tools_require_an_exact_local_allowlist() {
    let adapter = Arc::new(ProbeAdapter {
        provider: ProviderKey::Claude,
        wait: Arc::new(AtomicBool::new(false)),
    });
    let mut selected = request();
    selected.external_tools = Some(vec!["docs__search".to_owned()]);

    let default_manager = RunManager::new(vec![Arc::clone(&adapter)])
        .with_execution_policy(&ExecutionPolicy::default());
    let denied = default_manager
        .resolve_run(&selected)
        .expect_err("read-only MCP tools are disabled by default");
    assert_eq!(denied.code, RunManagementErrorCode::PolicyDenied);

    let policy = ExecutionPolicy {
        allowed_read_only_external_tools: vec!["docs__search".to_owned()],
        ..ExecutionPolicy::default()
    };
    let allowed_manager =
        RunManager::new(vec![Arc::clone(&adapter)]).with_execution_policy(&policy);
    let resolved = allowed_manager
        .resolve_run(&selected)
        .expect("an exact read-only allowlist admits the selected tool");
    assert_eq!(resolved.external_tools, selected.external_tools);

    let mut duplicate = selected;
    duplicate.external_tools = Some(vec!["docs__search".to_owned(), "docs__search".to_owned()]);
    let duplicate_error = allowed_manager
        .resolve_run(&duplicate)
        .expect_err("duplicate tool names must fail before execution");
    assert_eq!(duplicate_error.code, RunManagementErrorCode::InvalidRequest);

    let mut workspace_write = request();
    workspace_write.access = AccessMode::WorkspaceWrite;
    workspace_write.external_tools = Some(vec!["docs__search".to_owned()]);
    let write_policy_manager =
        RunManager::new(vec![adapter]).with_execution_policy(&ExecutionPolicy::default());
    write_policy_manager
        .resolve_run(&workspace_write)
        .expect("workspace-write runs use explicit per-run tool selection");
}

#[test]
fn streams_ordered_output_and_preserves_partial_output_on_cancel() {
    let wait = Arc::new(AtomicBool::new(true));
    let manager = RunManager::new(vec![Arc::new(StreamingAdapter {
        wait: Arc::clone(&wait),
    })]);
    let started = manager.start_run(request()).expect("run starts");

    let mut active_result = None;
    for _ in 0..100 {
        let result = manager
            .get_result(&started.run_id)
            .expect("active result is available");
        if result.output.is_some() {
            active_result = Some(result);
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    let active_result = active_result
        .unwrap_or_else(|| panic!("output deltas were not exposed while the run was active"));
    assert_eq!(active_result.output.as_deref(), Some("first second"));
    assert_eq!(active_result.output_bytes, "first second".len());
    assert!(active_result.content_available);
    assert_eq!(active_result.state, RunState::Running);

    let events = manager
        .read_events(&started.run_id, None, None)
        .expect("event stream is readable");
    let output = events
        .events
        .iter()
        .filter(|event| event.kind == RunEventKind::Output)
        .map(|event| event.data.as_deref().unwrap_or_default())
        .collect::<Vec<_>>();
    assert_eq!(output, ["first ", "second"]);

    manager
        .cancel_run(&started.run_id)
        .expect("run cancellation is accepted");
    let terminal = wait_for_terminal(&manager, &started.run_id);
    assert_eq!(terminal.state, RunState::Cancelled);
    let result = manager
        .get_result(&started.run_id)
        .expect("cancelled result is available");
    assert_eq!(result.output.as_deref(), Some("first second"));
    assert_eq!(result.output_bytes, "first second".len());
    assert!(!result.output_truncated);
    manager.shutdown();
}

#[test]
fn bounded_stream_retains_eight_mib_and_reports_total_observed_bytes() {
    let observed_bytes = MAX_ANSWER_BYTES_PER_RUN + 123;
    let manager = RunManager::new(vec![Arc::new(OversizedOutputAdapter {
        bytes: observed_bytes,
    })]);
    let started = manager.start_run(request()).expect("run starts");
    let terminal = wait_for_terminal(&manager, &started.run_id);
    assert_eq!(terminal.state, RunState::TimedOut);

    let result = manager
        .get_result(&started.run_id)
        .expect("timed out result is available");
    assert_eq!(
        result.output.as_ref().map(String::len),
        Some(MAX_ANSWER_BYTES_PER_RUN)
    );
    assert_eq!(result.output_bytes, observed_bytes);
    assert!(result.output_truncated);

    let events = manager
        .read_events(&started.run_id, None, None)
        .expect("bounded event stream is readable");
    assert!(events.gap);
    assert!(
        events
            .events
            .iter()
            .all(|event| event_size(event) <= MAX_EVENT_BYTES_PER_RUN)
    );
    manager.shutdown();
}

#[test]
fn restricted_roots_are_checked_before_provider_execution() {
    let wait = Arc::new(AtomicBool::new(false));
    let adapter = Arc::new(ProbeAdapter {
        provider: ProviderKey::Claude,
        wait,
    });
    let manager = RunManager::new(vec![adapter]).with_allowed_roots(Vec::new());
    let mut request = request();
    request.working_directory = Some(std::env::temp_dir());
    let error = manager
        .resolve_run(&request)
        .expect_err("empty MCP roots deny repository paths");
    assert_eq!(error.code, RunManagementErrorCode::PolicyDenied);
    manager.shutdown();
}

#[test]
fn workspace_write_runs_conflict_across_manager_instances() {
    let workspace = std::env::temp_dir();
    let wait = Arc::new(AtomicBool::new(true));
    let first = RunManager::new(vec![Arc::new(ProbeAdapter {
        provider: ProviderKey::Claude,
        wait: Arc::clone(&wait),
    })]);
    let second = RunManager::new(vec![Arc::new(ProbeAdapter {
        provider: ProviderKey::Claude,
        wait: Arc::clone(&wait),
    })]);
    let mut request = request();
    request.access = AccessMode::WorkspaceWrite;
    request.working_directory = Some(workspace);
    let started = first
        .start_run(request.clone())
        .expect("first write starts");
    let error = second
        .start_run(request)
        .expect_err("overlapping workspace writes must conflict");
    assert_eq!(error.code, RunManagementErrorCode::WriteConflict);
    first
        .cancel_run(&started.run_id)
        .expect("first write cancels");
    wait.store(false, Ordering::Release);
    first.shutdown();
    second.shutdown();
}

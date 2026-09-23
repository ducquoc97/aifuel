use super::*;

#[test]
fn session_selection_lookup_returns_only_the_stored_provider_model_and_effort() {
    let manager = RunManager::new(vec![Arc::new(ProbeAdapter {
        provider: ProviderKey::Claude,
        wait: Arc::new(AtomicBool::new(false)),
    })]);
    let mut request = request();
    request.model = Some("chosen-model".to_owned());
    request.effort = Some("high".to_owned());
    let run = manager.start_run(request).expect("run starts");
    assert_eq!(
        wait_for_terminal(&manager, &run.run_id).state,
        RunState::Succeeded
    );

    let selection = manager
        .session_selection("native-session")
        .expect("stored selection is available");
    assert_eq!(selection.session_id, "native-session");
    assert_eq!(selection.provider, ProviderKey::Claude);
    assert_eq!(selection.requested_model.as_deref(), Some("chosen-model"));
    assert_eq!(selection.requested_effort.as_deref(), Some("high"));
    assert!(
        !serde_json::to_string(&selection)
            .expect("session selection should serialize")
            .contains("hello")
    );
    manager.shutdown();
}

#[test]
fn session_selection_lookup_keeps_native_model_and_effort_defaults_unset() {
    let manager = RunManager::new(vec![Arc::new(ProbeAdapter {
        provider: ProviderKey::Claude,
        wait: Arc::new(AtomicBool::new(false)),
    })]);
    let mut request = request();
    request.model = None;
    request.effort = None;
    let run = manager.start_run(request).expect("run starts");
    assert_eq!(
        wait_for_terminal(&manager, &run.run_id).state,
        RunState::Succeeded
    );

    let selection = manager
        .session_selection("native-session")
        .expect("stored selection is available");
    assert_eq!(selection.requested_model, None);
    assert_eq!(selection.requested_effort, None);
    manager.shutdown();
}

#[test]
fn input_deadline_clears_pending_state_and_absent_deadline_waits_for_owner() {
    let manager = RunManager::new(vec![Arc::new(InputAdapter)]);
    let mut timed_request = request();
    timed_request.timeout = Some(Duration::from_millis(300));
    let timed = manager.start_run(timed_request).expect("timed run starts");
    let mut timed_is_waiting = false;
    for _ in 0..100 {
        if manager
            .get_run(&timed.run_id)
            .expect("timed run remains available")
            .state
            == RunState::WaitingForInput
        {
            timed_is_waiting = true;
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    assert!(timed_is_waiting, "provider should block waiting for input");
    let mut terminal = manager
        .get_run(&timed.run_id)
        .expect("timed run remains available");
    for _ in 0..250 {
        if terminal.state.is_terminal() {
            break;
        }
        thread::sleep(Duration::from_millis(2));
        terminal = manager
            .get_run(&timed.run_id)
            .expect("timed run remains available");
    }
    assert_eq!(terminal.state, RunState::TimedOut);
    assert!(terminal.pending_input.is_none());
    assert_eq!(
        manager
            .get_result(&timed.run_id)
            .expect("timed run result is available")
            .status,
        Some(RunStatus::Timeout)
    );

    let waiting = manager
        .start_run(request())
        .expect("run without a deadline starts");
    let mut is_waiting = false;
    for _ in 0..100 {
        if manager
            .get_run(&waiting.run_id)
            .expect("waiting run remains available")
            .state
            == RunState::WaitingForInput
        {
            is_waiting = true;
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    assert!(is_waiting, "provider should reach its input request");
    thread::sleep(Duration::from_millis(150));
    let still_waiting = manager
        .get_run(&waiting.run_id)
        .expect("run without a deadline remains available");
    assert_eq!(still_waiting.state, RunState::WaitingForInput);
    let pending = still_waiting
        .pending_input
        .expect("owner response remains pending without a deadline");
    manager
        .answer_input(&waiting.run_id, &pending.input_id, "workspace")
        .expect("ordinary input resumes the provider");
    assert_eq!(
        wait_for_terminal(&manager, &waiting.run_id).state,
        RunState::Succeeded
    );
    manager.shutdown();
}
#[test]
fn same_provider_session_can_resume_with_current_run_policy() {
    let wait = Arc::new(AtomicBool::new(false));
    let adapter = Arc::new(ProbeAdapter {
        provider: ProviderKey::Claude,
        wait,
    });
    let manager = RunManager::new(vec![adapter]);
    let first = manager.start_run(request()).expect("first run starts");
    wait_for_terminal(&manager, &first.run_id);
    let resumed = manager
        .resume_session("native-session", request())
        .expect("same-provider session resumes");
    let terminal = wait_for_terminal(&manager, &resumed.run_id);
    assert_eq!(terminal.provider, ProviderKey::Claude);
    assert_ne!(first.run_id, resumed.run_id);
    manager.shutdown();
}

#[test]
fn native_session_metadata_survives_owner_restart_without_prompt_content() {
    let path = std::env::temp_dir().join(format!(
        "aifuel-session-test-{}-{}.json",
        std::process::id(),
        now()
    ));
    let wait = Arc::new(AtomicBool::new(false));
    {
        let manager = RunManager::new(vec![Arc::new(ProbeAdapter {
            provider: ProviderKey::Claude,
            wait: Arc::clone(&wait),
        })])
        .with_session_store(&path)
        .expect("session store should load");
        let first = manager.start_run(request()).expect("first run starts");
        wait_for_terminal(&manager, &first.run_id);
    }
    let resumed = RunManager::new(vec![Arc::new(ProbeAdapter {
        provider: ProviderKey::Claude,
        wait,
    })])
    .with_session_store(&path)
    .expect("session store should reload")
    .resume_session("native-session", request())
    .expect("persisted same-provider session resumes");
    assert!(!resumed.run_id.is_empty());
    let _ = std::fs::remove_file(path);
}

#[test]
fn persistent_content_is_written_only_when_a_content_store_is_attached() {
    let directory = std::env::temp_dir().join(format!(
        "aifuel-content-test-{}-{}",
        std::process::id(),
        now()
    ));
    let manager = RunManager::new(vec![Arc::new(ProbeAdapter {
        provider: ProviderKey::Claude,
        wait: Arc::new(AtomicBool::new(false)),
    })])
    .with_content_store(&directory)
    .expect("content store should open");
    let started = manager.start_run(request()).expect("run starts");
    wait_for_terminal(&manager, &started.run_id);
    let result = manager
        .get_result(&started.run_id)
        .expect("result is available");
    assert!(result.content_available);
    assert!(
        std::fs::read_dir(&directory)
            .expect("content directory exists")
            .next()
            .is_some()
    );
    manager.shutdown();
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn ordinary_input_waits_and_resumes_while_permissions_stay_local_only() {
    let adapter = Arc::new(ProbeAdapter {
        provider: ProviderKey::Claude,
        wait: Arc::new(AtomicBool::new(true)),
    });
    let manager = RunManager::new(vec![adapter]);
    let started = manager.start_run(request()).expect("run starts");
    let ordinary = manager
        .request_input(&started.run_id, RunInputKind::Ordinary, "choose a target")
        .expect("ordinary input waits");
    assert_eq!(
        manager.get_run(&started.run_id).unwrap().state,
        RunState::WaitingForInput
    );
    manager
        .answer_input(&started.run_id, &ordinary.input_id, "workspace")
        .expect("ordinary input resumes");
    let approval = manager
        .request_input(&started.run_id, RunInputKind::Permission, "write file")
        .expect("permission request waits");
    let error = manager
        .answer_input(&started.run_id, &approval.input_id, "allow")
        .expect_err("MCP cannot approve permissions");
    assert_eq!(error.code, RunManagementErrorCode::UnsupportedCapability);
    manager.shutdown();
}

#[test]
fn worker_start_does_not_overwrite_pending_owner_input() {
    let manager = RunManager::new(Vec::<Arc<dyn AgentExecutionAdapter>>::new());
    let record = Arc::new(RunRecord::new(
        "run-worker-start-input".to_owned(),
        &request(),
        None,
        None,
    ));
    record.push_event(RunEventKind::Started, None);
    manager
        .inner
        .records
        .lock()
        .expect("run records mutex")
        .insert(record.run_id.clone(), Arc::clone(&record));
    let pending = manager
        .request_input(&record.run_id, RunInputKind::Ordinary, "choose a target")
        .expect("owner input is accepted during startup");

    assert!(super::super::workers::mark_worker_running(
        &manager, &record
    ));
    let snapshot = record.snapshot();
    assert_eq!(snapshot.state, RunState::WaitingForInput);
    assert_eq!(snapshot.pending_input, Some(pending));
    assert_eq!(
        record
            .events
            .lock()
            .expect("run events mutex")
            .events
            .iter()
            .map(|event| event.kind)
            .collect::<Vec<_>>(),
        [RunEventKind::Started, RunEventKind::WaitingForInput]
    );
}

#[test]
fn cancellation_is_idempotent_and_terminal_state_is_immutable() {
    let wait = Arc::new(AtomicBool::new(true));
    let adapter = Arc::new(ProbeAdapter {
        provider: ProviderKey::Claude,
        wait: Arc::clone(&wait),
    });
    let manager = RunManager::new(vec![adapter]);
    let started = manager.start_run(request()).expect("run starts");
    let cancelling = manager
        .cancel_run(&started.run_id)
        .expect("cancel is accepted");
    assert!(matches!(
        cancelling.state,
        RunState::Cancelling | RunState::Cancelled
    ));
    let terminal = wait_for_terminal(&manager, &started.run_id);
    assert_eq!(terminal.state, RunState::Cancelled);
    let repeated = manager
        .cancel_run(&started.run_id)
        .expect("repeated cancellation is idempotent");
    assert_eq!(repeated.state, RunState::Cancelled);
    wait.store(false, Ordering::Release);
    manager.shutdown();
}

#[test]
fn dropping_the_owner_cancels_and_joins_active_runs() {
    let wait = Arc::new(AtomicBool::new(true));
    {
        let adapter = Arc::new(ProbeAdapter {
            provider: ProviderKey::Claude,
            wait,
        });
        let manager = RunManager::new(vec![adapter]);
        manager.start_run(request()).expect("run starts");
    }
}

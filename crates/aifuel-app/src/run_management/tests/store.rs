use super::super::helpers::encode_cursor;
use super::*;
use crate::test_support::store_path;

fn probe_manager(path: &std::path::Path) -> RunManager {
    let store = crate::RunStore::open(path).expect("run store opens");
    RunManager::new(vec![Arc::new(ProbeAdapter {
        provider: ProviderKey::Claude,
        wait: Arc::new(AtomicBool::new(false)),
    })])
    .with_run_store(store)
}

#[test]
fn terminal_runs_remain_readable_after_the_owner_drops() {
    let path = store_path("history");
    let run_id;
    {
        let manager = probe_manager(&path);
        let run = manager.start_run(request()).expect("run starts");
        run_id = run.run_id.clone();
        assert_eq!(
            run.platform.as_deref(),
            Some(crate::run_management::host_platform().as_str()),
            "the host platform is stamped when the run is accepted"
        );
        assert_eq!(
            wait_for_terminal(&manager, &run_id).state,
            RunState::Succeeded
        );
        manager.shutdown();
    }

    let next = probe_manager(&path);
    assert_eq!(next.get_run(&run_id).unwrap().state, RunState::Succeeded);
    let result = next.get_result(&run_id).expect("persisted result reads");
    assert_eq!(result.status, Some(RunStatus::Succeeded));
    assert_eq!(result.exit_code, Some(0));
    assert_eq!(result.session_id.as_deref(), Some("native-session"));
    assert_eq!(
        result.usage,
        Some(aifuel_core::TokenUsage {
            input_tokens: Some(7),
            output_tokens: Some(3),
        }),
        "token accounting survives owner restart through persisted history"
    );
    assert_eq!(
        result.output, None,
        "answers never persist without the content opt-in"
    );
    assert!(
        !result.content_available,
        "owner-held memory content must not read as available after exit"
    );

    let events = next
        .read_events(&run_id, None, None)
        .expect("persisted events read");
    assert!(events.terminal);
    assert!(!events.gap);
    assert_eq!(
        events
            .events
            .iter()
            .map(|event| event.kind)
            .collect::<Vec<_>>(),
        [
            RunEventKind::Started,
            RunEventKind::Running,
            RunEventKind::Output,
            RunEventKind::Completed,
        ]
    );
    assert!(
        events.events.iter().all(|event| event.data.is_none()),
        "event payloads stay out of the database without retention opt-in"
    );
    assert!(events.next_cursor.is_none());
    next.shutdown();
    let _ = std::fs::remove_file(path);
}

#[test]
fn persisted_event_pages_keep_cursor_contract() {
    let path = store_path("cursor");
    let run_id;
    {
        let store = crate::RunStore::open(&path).expect("run store opens");
        let manager = RunManager::new(vec![Arc::new(StreamingAdapter {
            wait: Arc::new(AtomicBool::new(false)),
        })])
        .with_run_store(store);
        let mut request = request();
        request.integration = ProviderKey::Claude.into();
        let run = manager.start_run(request).expect("run starts");
        run_id = run.run_id.clone();
        wait_for_terminal(&manager, &run_id);
        manager.shutdown();
    }

    let next = probe_manager(&path);
    let mut cursor = None;
    let mut sequences = Vec::new();
    loop {
        let page = next
            .read_events(&run_id, cursor.as_deref(), Some(128))
            .expect("page reads");
        sequences.extend(page.events.iter().map(|event| event.sequence));
        match page.next_cursor {
            Some(next_cursor) => cursor = Some(next_cursor),
            None => break,
        }
    }
    let total = sequences.len();
    assert!(total >= 2, "output chunks should produce several events");
    assert_eq!(
        sequences,
        (1..=total as u64).collect::<Vec<_>>(),
        "persisted pages preserve exact sequence ordering"
    );

    // A cursor for a different run stays rejected against persisted streams.
    let foreign = encode_cursor("other-run", 0, 0);
    let error = next
        .read_events(&run_id, Some(&foreign), None)
        .expect_err("foreign cursors fail");
    assert_eq!(error.code, RunManagementErrorCode::InvalidCursor);

    // A cursor minted by a different owner for the same run is rejected too:
    // cursor validity is bound to the owning connection.
    let foreign = encode_cursor(&run_id, u64::MAX, 1);
    let error = next
        .read_events(&run_id, Some(&foreign), None)
        .expect_err("foreign-owner cursors fail");
    assert_eq!(error.code, RunManagementErrorCode::InvalidCursor);
    next.shutdown();
    let _ = std::fs::remove_file(path);
}

#[test]
fn persisted_sessions_resume_across_owners() {
    let path = store_path("resume");
    {
        let manager = probe_manager(&path);
        let run = manager.start_run(request()).expect("run starts");
        wait_for_terminal(&manager, &run.run_id);
        manager.shutdown();
    }

    // A new owner with only the database attached resolves the native session.
    let resumed = probe_manager(&path)
        .resume_session("native-session", request())
        .expect("persisted session resumes a new run");
    assert_eq!(resumed.provider, ProviderId::from(ProviderKey::Claude));
    let _ = std::fs::remove_file(path);
}

#[test]
fn resume_rejects_while_the_session_has_an_active_run() {
    let path = store_path("resume-active");
    let wait = Arc::new(AtomicBool::new(true));
    let store = crate::RunStore::open(&path).expect("run store opens");
    let manager = RunManager::new(vec![Arc::new(ProbeAdapter {
        provider: ProviderKey::Claude,
        wait: Arc::clone(&wait),
    })])
    .with_run_store(store);

    // Seed the session association, then hold a resumed run open. The session
    // id must not admit a second concurrent run, read-only ones included.
    let seeded = manager.start_run(request()).expect("run starts");
    wait.store(false, Ordering::Release);
    wait_for_terminal(&manager, &seeded.run_id);
    wait.store(true, Ordering::Release);

    let first = manager
        .resume_session("native-session", request())
        .expect("the first resume starts");
    let error = manager
        .resume_session("native-session", request())
        .expect_err("a second concurrent resume is rejected");
    assert_eq!(error.code, RunManagementErrorCode::SessionUnavailable);

    // Once the holder finishes, the session is resumable again.
    wait.store(false, Ordering::Release);
    wait_for_terminal(&manager, &first.run_id);
    let second = manager
        .resume_session("native-session", request())
        .expect("resume works after the holder completes");
    wait_for_terminal(&manager, &second.run_id);
    manager.shutdown();
    let _ = std::fs::remove_file(path);
}

#[test]
fn failed_runs_keep_a_stable_closed_reason_in_history() {
    let path = store_path("closed-reason");
    let run_id;
    {
        // FailingAdapter fails the run with InvalidRequest immediately.
        let manager = RunManager::new(vec![Arc::new(FailingAdapter)])
            .with_run_store(crate::RunStore::open(&path).expect("run store opens"));
        let run = manager.start_run(request()).expect("run starts");
        run_id = run.run_id.clone();
        wait_for_terminal(&manager, &run_id);
        let result = manager.get_result(&run_id).expect("result reads");
        assert_eq!(result.status, Some(RunStatus::Failed));
        assert_eq!(result.closed_reason.as_deref(), Some("invalid_request"));
        manager.shutdown();
    }

    // The stable category survives on the stored row for the next owner.
    let next = probe_manager(&path);
    let result = next.get_result(&run_id).expect("persisted result reads");
    assert_eq!(result.closed_reason.as_deref(), Some("invalid_request"));
    next.shutdown();
    let _ = std::fs::remove_file(path);
}

#[test]
fn live_runs_stay_owner_scoped_in_shared_databases() {
    let path = store_path("isolation");
    let wait = Arc::new(AtomicBool::new(true));
    let store = crate::RunStore::open(&path).expect("run store opens");
    let owner = RunManager::new(vec![Arc::new(ProbeAdapter {
        provider: ProviderKey::Claude,
        wait: Arc::clone(&wait),
    })])
    .with_run_store(store);
    let live = owner.start_run(request()).expect("run starts");

    // A second owner on the same database must not observe the live run.
    let other = probe_manager(&path);
    let error = other
        .get_run(&live.run_id)
        .expect_err("in-flight runs stay owner-scoped");
    assert_eq!(error.code, RunManagementErrorCode::RunNotFound);
    assert_eq!(
        other
            .read_events(&live.run_id, None, None)
            .expect_err("live events stay owner-scoped")
            .code,
        RunManagementErrorCode::RunNotFound
    );
    wait.store(false, Ordering::Release);
    owner.shutdown();
    other.shutdown();
    let _ = std::fs::remove_file(path);
}

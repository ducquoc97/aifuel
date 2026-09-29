//! Session lifecycle and run event ordering tests.

use super::*;
use aifuel_core::{
    AccessMode, AgentEventKind, Attachment, AttachmentKind, ReceiptCode, RunOutcome, RunStatus,
    SessionId, UserInput,
};
use std::path::PathBuf;
use std::sync::Arc;

/// One run produces the ordered kinds the contract needs: started, output,
/// completed, then the session returns to idle.
#[test]
fn send_emits_a_honest_run_sequence() {
    let adapter = adapter(vec![FakeRun::Complete {
        status: RunStatus::Succeeded,
        output: "the answer",
        session_id: None,
    }]);
    let handle = adapter
        .start(
            &integration(ProviderKey::Claude),
            options(ProviderKey::Claude, AccessMode::ReadOnly),
        )
        .expect("session starts");
    let events = test_events(&adapter, &handle);

    let run = adapter
        .send(&handle, input("hello"))
        .expect("send is accepted");
    let kinds = through_idle(&events);

    assert!(
        matches!(kinds[0], AgentEventKind::SessionCreated { .. }),
        "created first: {kinds:?}"
    );
    let position = |probe: &dyn Fn(&AgentEventKind) -> bool| {
        kinds
            .iter()
            .position(probe)
            .unwrap_or_else(|| panic!("event missing in {kinds:?}"))
    };
    let started = position(&|kind| matches!(kind, AgentEventKind::RunStarted { .. }));
    let delta = position(&|kind| matches!(kind, AgentEventKind::MessageDelta { .. }));
    let completed = position(&|kind| matches!(kind, AgentEventKind::MessageCompleted { .. }));
    let finished = position(&|kind| {
        matches!(
            kind,
            AgentEventKind::RunCompleted {
                outcome: RunOutcome::Success,
                ..
            }
        )
    });
    assert!(
        started < delta && delta < completed && completed < finished,
        "run events stay in causal order: {kinds:?}"
    );
    match &kinds[started] {
        AgentEventKind::RunStarted { run_id, .. } => assert_eq!(*run_id, run),
        _ => unreachable!(),
    }

    // Sequential runs are allowed once the first is terminal.
    assert!(adapter.send(&handle, input("again")).is_ok());
}

/// Streamed provider deltas become `message.delta` events and the fallback
/// terminal delta does not duplicate the streamed answer.
#[test]
fn streamed_output_becomes_deltas_without_duplication() {
    let adapter = adapter(vec![FakeRun::Streamed]);
    let handle = adapter
        .start(
            &integration(ProviderKey::Claude),
            options(ProviderKey::Claude, AccessMode::ReadOnly),
        )
        .expect("session starts");
    let events = test_events(&adapter, &handle);
    adapter
        .send(&handle, input("go"))
        .expect("send is accepted");
    let kinds = through_run_completed(&events);
    let deltas: Vec<_> = kinds
        .iter()
        .filter_map(|kind| match kind {
            AgentEventKind::MessageDelta { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, ["streamed ", "answer"]);
}

/// `send` rejects a second run while one is in flight; `cancel` unwinds it
/// and the stream reports the cancelled outcome.
#[test]
fn one_run_per_session_and_cancel_reports_cancelled() {
    let adapter = adapter(vec![FakeRun::Blocking]);
    let handle = adapter
        .start(
            &integration(ProviderKey::Claude),
            options(ProviderKey::Claude, AccessMode::ReadOnly),
        )
        .expect("session starts");
    let events = test_events(&adapter, &handle);
    let run = adapter
        .send(&handle, input("work"))
        .expect("send is accepted");

    let second = adapter.send(&handle, input("more"));
    assert_eq!(
        second
            .expect_err("a second run is rejected while one is in flight")
            .code,
        ReceiptCode::InvalidState
    );

    adapter
        .cancel(&handle, run.clone())
        .expect("cancel is accepted");
    let kinds = through_run_completed(&events);
    match kinds.last() {
        Some(AgentEventKind::RunCompleted {
            run_id, outcome, ..
        }) => {
            assert_eq!(*run_id, run);
            assert_eq!(*outcome, RunOutcome::Cancelled);
        }
        other => panic!("run.completed is the terminal event: {other:?}"),
    }
}

/// A failed run emits the error fact then `run.completed: failed`, and the
/// session returns to idle so the caller can send again.
#[test]
fn failed_run_emits_error_then_failed() {
    let adapter = adapter(vec![FakeRun::Complete {
        status: RunStatus::Failed,
        output: "",
        session_id: None,
    }]);
    let handle = adapter
        .start(
            &integration(ProviderKey::Claude),
            options(ProviderKey::Claude, AccessMode::ReadOnly),
        )
        .expect("session starts");
    let events = test_events(&adapter, &handle);
    adapter
        .send(&handle, input("boom"))
        .expect("send is accepted");
    let kinds = through_run_completed(&events);
    let error = kinds
        .iter()
        .find_map(|kind| match kind {
            AgentEventKind::Error {
                code, retryable, ..
            } => Some((*code, *retryable)),
            _ => None,
        })
        .expect("an error fact precedes the failed completion");
    assert_eq!(error, (ReceiptCode::ProviderError, false));
    assert!(matches!(
        kinds.last(),
        Some(AgentEventKind::RunCompleted {
            outcome: RunOutcome::Failed,
            ..
        })
    ));
}

/// Attachments are rejected rather than silently dropped, and an empty
/// prompt never opens a run.
#[test]
fn send_rejects_attachments_and_empty_input() {
    let adapter = adapter(vec![]);
    let handle = adapter
        .start(
            &integration(ProviderKey::Claude),
            options(ProviderKey::Claude, AccessMode::ReadOnly),
        )
        .expect("session starts");
    let attachment = adapter.send(
        &handle,
        UserInput {
            text: "look".to_owned(),
            attachments: vec![Attachment {
                kind: AttachmentKind::Image,
                path: PathBuf::from("/tmp/image.png"),
            }],
        },
    );
    assert_eq!(
        attachment.expect_err("attachments are not carried").code,
        ReceiptCode::Unsupported
    );
    let empty = adapter.send(&handle, input("   "));
    assert_eq!(
        empty.expect_err("empty input is rejected").code,
        ReceiptCode::InvalidState
    );
}

/// A session id that is not live is `unknown_session`; stopped sessions are
/// closed to further sends.
#[test]
fn unknown_and_closed_sessions_are_explicit() {
    let adapter = adapter(vec![]);
    let missing = adapter.send(
        &AgentSessionHandle {
            session_id: SessionId::new("missing"),
            provider_session: None,
        },
        input("hello"),
    );
    assert_eq!(
        missing.expect_err("unknown sessions error").code,
        ReceiptCode::UnknownSession
    );

    let handle = adapter
        .start(
            &integration(ProviderKey::Claude),
            options(ProviderKey::Claude, AccessMode::ReadOnly),
        )
        .expect("session starts");
    adapter.stop(handle.clone()).expect("stop closes");
    let closed = adapter.send(&handle, input("hello"));
    assert_eq!(
        closed.expect_err("stopped sessions do not send").code,
        ReceiptCode::UnknownSession
    );
}

/// Resume threads the provider-reported session id into the next run only
/// where the adapter declares the capability.
#[test]
fn reported_provider_session_feeds_resume() {
    let execution = Arc::new(
        FakeExecution::declaring(ProviderKey::Claude, &[AgentCapability::Resume]).with_runs(vec![
            FakeRun::Complete {
                status: RunStatus::Succeeded,
                output: "first",
                session_id: Some("native-1"),
            },
            FakeRun::Complete {
                status: RunStatus::Succeeded,
                output: "second",
                session_id: Some("native-2"),
            },
        ]),
    );
    let adapter = CliAdapter::new(execution.clone());
    let handle = adapter
        .start(
            &integration(ProviderKey::Claude),
            options(ProviderKey::Claude, AccessMode::ReadOnly),
        )
        .expect("session starts");
    let events = test_events(&adapter, &handle);
    for text in ["one", "two"] {
        adapter
            .send(&handle, input(text))
            .expect("send is accepted");
        through_run_completed(&events);
    }
    let requests = execution.requests.lock().expect("requests mutex");
    assert_eq!(requests[0].resume, None, "nothing resumes the first run");
    assert_eq!(
        requests[1].resume.as_deref(),
        Some("native-1"),
        "the second run resumes the reported provider session"
    );
    drop(requests);
    assert_eq!(
        adapter.provider_session(&handle.session_id).as_deref(),
        Some("native-2"),
        "the facade can persist the newest provider session id"
    );
}

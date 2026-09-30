//! Session lifecycle: create, resume, capability guards, and close.

use super::*;
use aifuel_core::{AgentAdapter, ReceiptCode};

#[test]
fn start_creates_session_and_reports_the_resume_cursor() {
    let fake = FakeServe::start();
    fake.set_create_session(json!({"id": "ses_42"}));
    fake.set_get_session(200, json!({"id": "ses_42"}));
    let adapter = adapter_with(&fake);
    let options = options(AccessMode::WorkspaceWrite);
    let (handle, events) = start_session(&adapter, &fake, options);

    assert_eq!(handle.provider_session.as_deref(), Some("ses_42"));
    assert_eq!(
        adapter.resume_cursor(&handle.session_id).as_deref(),
        Some("ses_42")
    );
    expect_prelude(&events);
    assert!(requested(&fake, "/session"), "POST /session was issued");
}

#[test]
fn resume_reattaches_a_persisted_session() {
    let fake = FakeServe::start();
    fake.set_get_session(200, json!({"id": "ses_old"}));
    let adapter = adapter_with(&fake);
    let mut options = options(AccessMode::WorkspaceWrite);
    options.resume_cursor = Some("ses_old".to_owned());
    let (handle, _events) = start_session(&adapter, &fake, options);

    assert_eq!(handle.provider_session.as_deref(), Some("ses_old"));
    let posts = fake
        .requests()
        .iter()
        .filter(|request| request.method == "POST" && request.path == "/session")
        .count();
    assert_eq!(posts, 0, "a resume never creates a session");
    assert!(
        fake.requests()
            .iter()
            .any(|request| request.method == "GET" && request.path == "/session/ses_old"),
        "the resume verified the provider session"
    );
}

#[test]
fn resume_of_a_missing_session_fails_start() {
    let fake = FakeServe::start();
    fake.set_get_session(404, json!({"error": "not found"}));
    let adapter = adapter_with(&fake);
    let mut options = options(AccessMode::WorkspaceWrite);
    options.resume_cursor = Some("ses_gone".to_owned());
    fake.push_event(connected());

    let error = adapter
        .start(&integration(), options)
        .expect_err("a stale resume cursor cannot start");
    assert_eq!(error.code, ReceiptCode::ProviderError);
}

#[test]
fn the_adapter_serves_only_its_own_integration() {
    let fake = FakeServe::start();
    let adapter = adapter_with(&fake);
    let selection = ModelSelection {
        integration_id: IntegrationId::new("other"),
        model: "prov/model".to_owned(),
        effort: None,
    };
    let error = adapter
        .resolve(&selection)
        .expect_err("another integration is not served");
    assert_eq!(error.code, ReceiptCode::Unsupported);
}

#[test]
fn resolve_validates_the_provider_model_spelling() {
    let fake = FakeServe::start();
    let adapter = adapter_with(&fake);
    let mut selection = ModelSelection {
        integration_id: integration().id.clone(),
        model: "noslash".to_owned(),
        effort: None,
    };
    let error = adapter
        .resolve(&selection)
        .expect_err("a bare model name cannot name an opencode model");
    assert_eq!(error.code, ReceiptCode::InvalidSelection);

    selection.model = "prov/model".to_owned();
    let descriptor = adapter
        .resolve(&selection)
        .expect("provider/model resolves");
    assert_eq!(descriptor.model, "prov/model");
    assert!(
        !descriptor.advertised,
        "an empty catalog advertises nothing"
    );
}

#[test]
fn effort_is_rejected_because_opencode_exposes_no_effort_ladder() {
    let fake = FakeServe::start();
    let adapter = adapter_with(&fake);
    let selection = ModelSelection {
        integration_id: integration().id.clone(),
        model: "prov/model".to_owned(),
        effort: Some(aifuel_core::Effort::High),
    };
    let error = adapter
        .resolve(&selection)
        .expect_err("effort is not selectable");
    assert_eq!(error.code, ReceiptCode::InvalidSelection);
}

#[test]
fn stop_emits_session_closed() {
    let fake = FakeServe::start();
    let adapter = adapter_with(&fake);
    let (handle, events) = start_session(&adapter, &fake, options(AccessMode::WorkspaceWrite));
    expect_prelude(&events);
    adapter.stop(handle).expect("stop succeeds");
    let kinds = through_closed(&events);
    assert!(matches!(
        kinds.last(),
        Some(AgentEventKind::SessionClosed { .. })
    ));
}

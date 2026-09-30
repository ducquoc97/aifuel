//! Session lifecycle: the setup handshake, authentication, resume, and
//! model selection. Every test runs a scripted agent on the duplex
//! transport so the assertions see the exact wire exchange.

use super::*;
use aifuel_core::AgentAdapter;

#[test]
fn start_runs_the_handshake_and_reports_the_provider_session() {
    let (adapter, agents) = duplex_adapter();
    let script = serve(agents, |mut agent| async move {
        let init = agent.next_method("initialize").await;
        assert_eq!(init["params"]["protocolVersion"], 1);
        assert_eq!(
            init["params"]["clientCapabilities"]["fs"]["readTextFile"],
            true
        );
        agent
            .respond(
                &init,
                json!({
                    "protocolVersion": 1,
                    "agentCapabilities": {"loadSession": true},
                    "authMethods": [],
                }),
            )
            .await;
        let _ready = agent.next_method("initialized").await;
        let session = agent.next_method("session/new").await;
        let expected = std::fs::canonicalize("/tmp").expect("cwd canonicalizes");
        assert_eq!(
            session["params"]["cwd"].as_str(),
            Some(expected.to_string_lossy().as_ref()),
            "the session request carries the canonical workspace"
        );
        agent
            .respond(&session, json!({"sessionId": "sess-1"}))
            .await;
        agent.park().await;
    });
    let handle = adapter
        .start(&integration(), options(AccessMode::WorkspaceWrite))
        .expect("the session starts");
    // The ACP sessionId is the provider resume cursor, on the handle
    // and through `resume_cursor`.
    assert_eq!(handle.provider_session.as_deref(), Some("sess-1"));
    assert_eq!(
        adapter.resume_cursor(&handle.session_id).as_deref(),
        Some("sess-1")
    );
    let events = test_events(&adapter, &handle);
    expect_prelude(&events);
    adapter.stop(handle).expect("stop");
    assert!(matches!(
        recv(&events),
        AgentEventKind::SessionClosed { .. }
    ));
    script.join().expect("the agent script completes");
}

#[test]
fn a_single_advertised_auth_method_authenticates() {
    let (adapter, agents) = duplex_adapter();
    let script = serve(agents, |mut agent| async move {
        agent
            .initialize(json!({
                "protocolVersion": 1,
                "agentCapabilities": {"loadSession": true},
                "authMethods": [{"id": "cursor_login", "name": "Cursor login"}],
            }))
            .await;
        let auth = agent.next_method("authenticate").await;
        assert_eq!(auth["params"]["methodId"], "cursor_login");
        agent.respond(&auth, Value::Null).await;
        let _ready = agent.next_method("initialized").await;
        let session = agent.next_method("session/new").await;
        agent
            .respond(&session, json!({"sessionId": "sess-auth"}))
            .await;
        agent.park().await;
    });
    let handle = adapter
        .start(&integration(), options(AccessMode::WorkspaceWrite))
        .expect("the session starts");
    adapter.stop(handle).expect("stop");
    script.join().expect("the agent script completes");
}

#[test]
fn a_failed_authenticate_fails_start() {
    let (adapter, agents) = duplex_adapter();
    let script = serve(agents, |mut agent| async move {
        agent
            .initialize(json!({
                "protocolVersion": 1,
                "agentCapabilities": {},
                "authMethods": [{"id": "cursor_login", "name": "Cursor login"}],
            }))
            .await;
        let auth = agent.next_method("authenticate").await;
        agent
            .write(json!({
                "id": auth["id"],
                "error": {"code": -32000, "message": "auth rejected"},
            }))
            .await;
        agent.park().await;
    });
    let error = adapter
        .start(&integration(), options(AccessMode::WorkspaceWrite))
        .expect_err("a rejected authenticate fails start");
    assert!(
        error.message.contains("authenticate"),
        "the failure names the step: {}",
        error.message
    );
    script.join().expect("the agent script completes");
}

#[test]
fn resume_uses_session_load() {
    let (adapter, agents) = duplex_adapter();
    let script = serve(agents, |mut agent| async move {
        let session = agent
            .handshake_result(
                json!({"loadSession": true}),
                json!({"sessionId": "sess-loaded"}),
            )
            .await;
        assert_eq!(session["method"].as_str(), Some("session/load"));
        assert_eq!(session["params"]["sessionId"].as_str(), Some("sess-stored"));
        agent.park().await;
    });
    let mut options = options(AccessMode::WorkspaceWrite);
    options.resume_cursor = Some("sess-stored".to_owned());
    let handle = adapter
        .start(&integration(), options)
        .expect("the resumed session starts");
    assert_eq!(handle.provider_session.as_deref(), Some("sess-loaded"));
    adapter.stop(handle).expect("stop");
    script.join().expect("the agent script completes");
}

#[test]
fn resume_fails_when_load_session_is_not_advertised() {
    let (adapter, agents) = duplex_adapter();
    let script = serve(agents, |mut agent| async move {
        agent
            .initialize(json!({
                "protocolVersion": 1,
                "agentCapabilities": {"loadSession": false},
                "authMethods": [],
            }))
            .await;
        let _ready = agent.next_method("initialized").await;
        // No `session/load` may be attempted: the driver exits and the
        // transport ends.
        assert!(
            agent.next_client_opt().await.is_none(),
            "no session request follows a resume the agent cannot honor"
        );
    });
    let mut options = options(AccessMode::WorkspaceWrite);
    options.resume_cursor = Some("sess-stored".to_owned());
    let error = adapter
        .start(&integration(), options)
        .expect_err("resume without loadSession fails");
    assert!(
        error.message.contains("session/load"),
        "the failure names the missing capability: {}",
        error.message
    );
    script.join().expect("the agent script completes");
}

#[test]
fn a_rejected_resume_cursor_fails_start_instead_of_reopening() {
    let (adapter, agents) = duplex_adapter();
    let script = serve(agents, |mut agent| async move {
        agent
            .initialize(json!({
                "protocolVersion": 1,
                "agentCapabilities": {"loadSession": true},
                "authMethods": [],
            }))
            .await;
        let _ready = agent.next_method("initialized").await;
        let load = agent.next_method("session/load").await;
        agent
            .write(json!({
                "id": load["id"],
                "error": {"code": -32000, "message": "the session no longer exists"},
            }))
            .await;
        // A stale cursor must not fall back to `session/new`.
        assert!(
            agent.next_client_opt().await.is_none(),
            "a stale resume never silently opens a fresh session"
        );
    });
    let mut options = options(AccessMode::WorkspaceWrite);
    options.resume_cursor = Some("sess-stale".to_owned());
    let error = adapter
        .start(&integration(), options)
        .expect_err("a stale resume cursor fails start");
    assert!(
        error.message.contains("session/load"),
        "the failure names the rejected call: {}",
        error.message
    );
    script.join().expect("the agent script completes");
}

/// A `configOptions` select carrying `category: "model"`, as
/// `cursor-agent acp` advertises it.
fn model_selector(current: &str, values: &[&str]) -> Value {
    json!([{
        "id": "model",
        "category": "model",
        "type": "select",
        "name": "Model",
        "currentValue": current,
        "options": values
            .iter()
            .map(|value| json!({"value": value, "name": value}))
            .collect::<Vec<_>>(),
    }])
}

#[test]
fn a_requested_model_applies_through_set_config_option() {
    let (adapter, agents) = duplex_adapter();
    let script = serve(agents, |mut agent| async move {
        agent
            .handshake_result(
                json!({"loadSession": true}),
                json!({
                    "sessionId": "sess-model",
                    "configOptions": model_selector("auto", &["auto", "opus"]),
                }),
            )
            .await;
        let set = agent.next_method("session/set_config_option").await;
        assert_eq!(set["params"]["configId"], "model");
        assert_eq!(set["params"]["value"], "opus");
        agent
            .respond(
                &set,
                json!({"configOptions": model_selector("opus", &["auto", "opus"])}),
            )
            .await;
        agent.park().await;
    });
    let mut options = options(AccessMode::WorkspaceWrite);
    options.selection.model = "opus".to_owned();
    let handle = adapter
        .start(&integration(), options)
        .expect("the session starts with the requested model");
    adapter.stop(handle).expect("stop");
    script.join().expect("the agent script completes");
}

#[test]
fn a_model_without_an_advertised_selector_fails_start() {
    let (adapter, agents) = duplex_adapter();
    let script = serve(agents, |mut agent| async move {
        agent
            .handshake_result(json!({"loadSession": true}), json!({"sessionId": "sess"}))
            .await;
        assert!(agent.next_client_opt().await.is_none());
    });
    let mut options = options(AccessMode::WorkspaceWrite);
    options.selection.model = "opus".to_owned();
    let error = adapter
        .start(&integration(), options)
        .expect_err("a model with no selector cannot be honored");
    assert!(
        error.message.contains("no model selector"),
        "the failure is honest: {}",
        error.message
    );
    script.join().expect("the agent script completes");
}

#[test]
fn a_model_outside_the_selector_fails_start() {
    let (adapter, agents) = duplex_adapter();
    let script = serve(agents, |mut agent| async move {
        agent
            .handshake_result(
                json!({"loadSession": true}),
                json!({
                    "sessionId": "sess-model",
                    "configOptions": model_selector("auto", &["auto"]),
                }),
            )
            .await;
        assert!(
            agent.next_client_opt().await.is_none(),
            "no set_config_option follows an unselectable model"
        );
    });
    let mut options = options(AccessMode::WorkspaceWrite);
    options.selection.model = "opus".to_owned();
    let error = adapter
        .start(&integration(), options)
        .expect_err("an unlisted model fails start");
    assert!(
        error.message.contains("not a selectable option"),
        "the failure is honest: {}",
        error.message
    );
    script.join().expect("the agent script completes");
}

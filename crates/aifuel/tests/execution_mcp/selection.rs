use serde_json::{Value, json};
use std::fs;

use crate::support::TestDirectory;
use crate::support_protocol::{call_tools, write_execution_config};

#[test]
fn resolve_run_applies_explicit_profile_and_global_precedence_with_sources() {
    let directory = TestDirectory::new("execution-selection");
    write_execution_config(
        &directory,
        json!({
            "schema_version": 1,
            "defaults": {
                "provider": "codex",
                "model": "global-model",
                "effort": "global-effort",
                "access": "read-only",
                "overall_deadline_seconds": 90
            },
            "profiles": {
                "work": {
                    "provider": "codex",
                    "model": "profile-model",
                    "effort": "profile-effort",
                    "access": "workspace-write",
                    "overall_deadline_seconds": 45
                }
            },
            "policy": {"allowed_roots": []}
        }),
    );
    let responses = call_tools(
        &directory,
        &[
            json!({"name":"resolve_run","arguments":{"prompt":"default"}}),
            json!({"name":"resolve_run","arguments":{"profile":"work","prompt":"profile"}}),
            json!({"name":"resolve_run","arguments":{"profile":"work","provider":"codex","model":"explicit-model","effort":"explicit-effort","access":"read-only","timeout_seconds":30,"prompt":"explicit"}}),
        ],
        None,
    );

    let default = structured(&responses[0]);
    assert_eq!(default["provider"], "codex");
    assert_eq!(default["requested_model"], "global-model");
    assert_eq!(default["requested_effort"], "global-effort");
    assert_eq!(default["timeout_seconds"], 90);
    assert_eq!(default["selection_sources"]["provider"], "global_default");
    assert_eq!(default["selection_sources"]["model"], "global_default");

    let profile = structured(&responses[1]);
    assert_eq!(profile["provider"], "codex");
    assert_eq!(profile["requested_model"], "profile-model");
    assert_eq!(profile["requested_effort"], "profile-effort");
    assert_eq!(profile["timeout_seconds"], 45);
    assert_eq!(
        profile["selection_sources"]["provider"],
        json!({"profile":"work"})
    );
    assert_eq!(
        profile["selection_sources"]["model"],
        json!({"profile":"work"})
    );

    let explicit = structured(&responses[2]);
    assert_eq!(explicit["provider"], "codex");
    assert_eq!(explicit["requested_model"], "explicit-model");
    assert_eq!(explicit["requested_effort"], "explicit-effort");
    assert_eq!(explicit["timeout_seconds"], 30);
    assert_eq!(explicit["selection_sources"]["provider"], "explicit");
    assert_eq!(explicit["selection_sources"]["model"], "explicit");
    assert_eq!(explicit["selection_sources"]["access"], "explicit");
}

#[test]
fn resolve_run_reports_actionable_missing_provider_and_model() {
    let without_defaults = TestDirectory::new("execution-no-provider");
    write_execution_config(
        &without_defaults,
        json!({"schema_version":1,"policy":{"allowed_roots":[]}}),
    );
    let provider_error = call_tools(
        &without_defaults,
        &[json!({"name":"resolve_run","arguments":{"prompt":"hello"}})],
        None,
    );
    let message = structured(&provider_error[0])["message"]
        .as_str()
        .expect("validation error should explain the missing provider");
    assert!(message.contains("provider selection is required"));
    assert!(message.contains("provider or profile"));

    let without_model = TestDirectory::new("execution-no-model");
    write_execution_config(
        &without_model,
        json!({"schema_version":1,"defaults":{"provider":"codex"},"policy":{"allowed_roots":[]}}),
    );
    let model_error = call_tools(
        &without_model,
        &[json!({"name":"resolve_run","arguments":{"prompt":"hello"}})],
        None,
    );
    let message = structured(&model_error[0])["message"]
        .as_str()
        .expect("validation error should explain the missing model");
    assert!(message.contains("model selection is required"));
    assert!(message.contains("select a profile/default"));
}

#[test]
fn resume_ignores_global_selection_defaults_and_preserves_stored_values() {
    let directory = TestDirectory::new("execution-resume");
    write_execution_config(
        &directory,
        json!({
            "schema_version": 1,
            "defaults": {"provider":"gemini","model":"global-model","effort":"global-effort"},
            "profiles": {
                "work":{"provider":"codex","model":"profile-model","effort":"profile-effort"},
                "other":{"provider":"gemini","model":"other-model","effort":"low"}
            },
            "policy": {"allowed_roots": [directory.path().to_string_lossy()]}
        }),
    );
    seed_session(&directory, "codex", "stored-model", "stored-effort");

    let inferred_provider = call_tools(
        &directory,
        &[
            json!({"name":"resume_session","arguments":{"session_id":"stored-session","prompt":"resume"}}),
        ],
        Some(directory.path()),
    );
    let resumed = structured(&inferred_provider[0]);
    assert_eq!(resumed["provider"], "codex");
    assert_eq!(resumed["requested_model"], "stored-model");
    assert_eq!(resumed["requested_effort"], "stored-effort");

    let responses = call_tools(
        &directory,
        &[
            json!({"name":"resume_session","arguments":{"session_id":"stored-session","provider":"codex","prompt":"resume"}}),
        ],
        Some(directory.path()),
    );
    let resumed = structured(&responses[0]);
    assert_eq!(resumed["provider"], "codex");
    assert_eq!(resumed["requested_model"], "stored-model");
    assert_eq!(resumed["requested_effort"], "stored-effort");

    let explicit_conflict = call_tools(
        &directory,
        &[
            json!({"name":"resume_session","arguments":{"session_id":"stored-session","provider":"gemini","prompt":"conflict"}}),
        ],
        Some(directory.path()),
    );
    assert!(
        explicit_conflict[0]["result"]["isError"]
            .as_bool()
            .unwrap_or(false)
    );
    assert!(
        structured(&explicit_conflict[0])["message"]
            .as_str()
            .is_some_and(|message| message.contains("codex") && message.contains("gemini"))
    );

    let profile_conflict = call_tools(
        &directory,
        &[
            json!({"name":"resume_session","arguments":{"session_id":"stored-session","profile":"other","prompt":"profile conflict"}}),
        ],
        Some(directory.path()),
    );
    assert!(
        profile_conflict[0]["result"]["isError"]
            .as_bool()
            .unwrap_or(false)
    );
    assert!(
        structured(&profile_conflict[0])["message"]
            .as_str()
            .is_some_and(|message| message.contains("codex") && message.contains("gemini"))
    );

    let profile_resume = call_tools(
        &directory,
        &[
            json!({"name":"resume_session","arguments":{"session_id":"stored-session","profile":"work","prompt":"resume profile"}}),
        ],
        Some(directory.path()),
    );
    let resumed = structured(&profile_resume[0]);
    assert_eq!(resumed["provider"], "codex");
    assert_eq!(resumed["requested_model"], "profile-model");
    assert_eq!(resumed["requested_effort"], "profile-effort");

    let explicit_resume = call_tools(
        &directory,
        &[
            json!({"name":"resume_session","arguments":{"session_id":"stored-session","provider":"codex","model":"explicit-resume-model","effort":"low","prompt":"resume explicit"}}),
        ],
        Some(directory.path()),
    );
    let resumed = structured(&explicit_resume[0]);
    assert_eq!(resumed["requested_model"], "explicit-resume-model");
    assert_eq!(resumed["requested_effort"], "low");
}

fn structured(response: &Value) -> &Value {
    &response["result"]["structuredContent"]
}

fn seed_session(directory: &TestDirectory, provider: &str, model: &str, effort: &str) {
    let path = config_directory(directory).join("agent-sessions.json");
    fs::create_dir_all(path.parent().expect("session store has a parent")).unwrap();
    fs::write(
        path,
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "sessions": {
                "stored-session": {
                    "provider": provider,
                    "model": model,
                    "effort": effort,
                    "working_directory": directory.path()
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
}

fn config_directory(directory: &TestDirectory) -> std::path::PathBuf {
    directory.path().join(".config/aifuel")
}

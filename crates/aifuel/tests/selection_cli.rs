use std::fs;
use std::process::Command;

#[allow(dead_code)]
mod support;

use support::{TestDirectory, install_fake_codex_app_server, path_with};

#[test]
fn run_applies_a_named_profile_to_omitted_model_and_access_values() {
    let directory = TestDirectory::new("selection-cli-profile");
    let config_dir = directory.path().join("aifuel");
    fs::create_dir_all(&config_dir).expect("config directory should exist");
    fs::write(
        config_dir.join("execution.json"),
        r#"{
          "schema_version": 1,
          "profiles": {
            "review": {
              "model": "profile-model",
              "access": "workspace-write"
            }
          },
          "policy": {"allowed_roots": [], "retain_content": false}
        }"#,
    )
    .expect("selection config should be writable");
    let log_path = install_fake_codex_app_server(directory.path());

    let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args([
            "run",
            "--provider",
            "codex",
            "--profile",
            "review",
            "--prompt",
            "hello",
        ])
        .env("PATH", path_with(directory.path()))
        .env("HOME", directory.path())
        .env("USERPROFILE", directory.path())
        .env("APPDATA", directory.path())
        .env("XDG_CONFIG_HOME", directory.path())
        .env("AIFUEL_CODEX_FIXTURE_LOG", &log_path)
        .output()
        .expect("aifuel should start");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let requests: Vec<serde_json::Value> = fs::read_to_string(log_path)
        .expect("App Server requests should be logged")
        .lines()
        .map(|line| serde_json::from_str(line).expect("each request should be JSON"))
        .collect();
    assert_eq!(requests[2]["params"]["model"], "profile-model");
    assert_eq!(requests[2]["params"]["sandbox"], "workspace-write");
}

#[test]
fn resume_uses_stored_model_and_effort_instead_of_changed_global_defaults() {
    let directory = TestDirectory::new("selection-cli-resume-defaults");
    let config_dir = directory.path().join("aifuel");
    fs::create_dir_all(&config_dir).expect("config directory should exist");
    fs::write(
        config_dir.join("execution.json"),
        serde_json::json!({
            "schema_version": 1,
            "defaults": {
                "provider": "codex",
                "model": "new-global-model",
                "effort": "new-global-effort"
            },
            "policy": {
                "allowed_roots": [directory.path()],
                "retain_content": false
            }
        })
        .to_string(),
    )
    .expect("selection config should be writable");
    fs::write(
        config_dir.join("agent-sessions.json"),
        serde_json::json!({
            "schema_version": 1,
            "sessions": {
                "session-77": {
                    "provider": "codex",
                    "model": "stored-session-model",
                    "effort": "stored-session-effort",
                    "working_directory": directory.path()
                }
            }
        })
        .to_string(),
    )
    .expect("managed session metadata should be writable");
    let log_path = install_fake_codex_app_server(directory.path());

    let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args([
            "run",
            "--provider",
            "codex",
            "--resume",
            "session-77",
            "--prompt",
            "continue the session",
        ])
        .env("PATH", path_with(directory.path()))
        .env("HOME", directory.path())
        .env("USERPROFILE", directory.path())
        .env("APPDATA", directory.path())
        .env("XDG_CONFIG_HOME", directory.path())
        .env("AIFUEL_CODEX_FIXTURE_LOG", &log_path)
        .output()
        .expect("aifuel should start");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let requests: Vec<serde_json::Value> = fs::read_to_string(log_path)
        .expect("App Server requests should be logged")
        .lines()
        .map(|line| serde_json::from_str(line).expect("each request should be JSON"))
        .collect();
    assert_eq!(requests[2]["method"], "thread/resume");
    assert_eq!(requests[2]["params"]["model"], "stored-session-model");
    assert_eq!(requests[3]["params"]["model"], "stored-session-model");
    assert_eq!(requests[3]["params"]["effort"], "stored-session-effort");
}

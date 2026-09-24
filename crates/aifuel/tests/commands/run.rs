use std::fs;
use std::process::Command;

use crate::support::{
    TestDirectory, install_fake_codex_app_server, install_fake_command, path_with,
};

#[test]
fn run_rejects_prompt_only_read_only_without_verified_provider_enforcement() {
    for (provider, executable) in [
        ("claude", "claude"),
        ("copilot", "copilot"),
        ("gemini", "gemini"),
        ("antigravity", "agy"),
    ] {
        let directory = TestDirectory::new(&format!("{provider}-prompt-read-only"));
        install_fake_command(directory.path(), executable);

        let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
            .args([
                "run",
                "--provider",
                provider,
                "--model",
                "test-model",
                "--prompt",
                "hello",
                "--access",
                "read-only",
            ])
            .env("PATH", path_with(directory.path()))
            .env("HOME", directory.path())
            .env("USERPROFILE", directory.path())
            .env("APPDATA", directory.path())
            .env("XDG_CONFIG_HOME", directory.path().join(".config"))
            .output()
            .expect("aifuel should start");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{provider}: {stderr}");
        assert!(
            stderr.contains(&format!("{provider} cannot enforce read-only access")),
            "expected a read-only enforcement error for {provider}, got {stderr:?}"
        );
        assert!(stdout.is_empty(), "{provider} must not launch: {stdout:?}");
    }
}

#[test]
fn supported_codex_run_emits_a_structured_result_when_json_is_requested() {
    let directory = TestDirectory::new("json-run");
    let log_path = install_fake_codex_app_server(directory.path());

    let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args([
            "run",
            "--provider",
            "codex",
            "--model",
            "test-model",
            "--prompt",
            "hello",
            "--access",
            "read-only",
            "--output",
            "json",
        ])
        .env("PATH", path_with(directory.path()))
        .env("HOME", directory.path())
        .env("USERPROFILE", directory.path())
        .env("APPDATA", directory.path())
        .env("XDG_CONFIG_HOME", directory.path().join(".config"))
        .env("AIFUEL_CODEX_FIXTURE_LOG", &log_path)
        .output()
        .expect("aifuel should start");

    assert!(output.status.success());
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("run should emit JSON");
    assert_eq!(value["provider"], "codex");
    assert_eq!(value["requested_model"], "test-model");
    assert!(value["effective_model"].is_null());
    assert_eq!(value["state"], "succeeded");
    assert_eq!(value["status"], "succeeded");
    assert!(value["error"].is_null());
    assert!(
        value["output"]
            .as_str()
            .expect("provider output should be text")
            .contains("fake codex app-server response")
    );
}

#[test]
fn run_uses_the_selected_codex_integration() {
    let directory = TestDirectory::new("codex-run");
    let log_path = install_fake_codex_app_server(directory.path());

    let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args([
            "run",
            "--provider",
            "codex",
            "--model",
            "test-model",
            "--prompt",
            "hello",
            "--access",
            "read-only",
        ])
        .env("PATH", path_with(directory.path()))
        .env("HOME", directory.path())
        .env("USERPROFILE", directory.path())
        .env("APPDATA", directory.path())
        .env("XDG_CONFIG_HOME", directory.path().join(".config"))
        .env("AIFUEL_CODEX_FIXTURE_LOG", &log_path)
        .output()
        .expect("aifuel should start");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "fake codex app-server response"
    );
    let requests: Vec<serde_json::Value> = fs::read_to_string(log_path)
        .expect("App Server requests should be logged")
        .lines()
        .map(|line| serde_json::from_str(line).expect("each request should be JSON"))
        .collect();
    assert_eq!(requests[0]["method"], "initialize");
    assert_eq!(requests[1]["method"], "initialized");
    assert_eq!(requests[2]["method"], "thread/start");
    assert_eq!(requests[2]["params"]["model"], "test-model");
    assert_eq!(requests[2]["params"]["sandbox"], "read-only");
    assert_eq!(requests[3]["method"], "turn/start");
    assert_eq!(requests[3]["params"]["input"][0]["text"], "hello");
}

#[test]
fn model_catalog_refresh_reports_unsupported_provider_interfaces() {
    let directory = TestDirectory::new("model-refresh-unsupported");
    let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args(["model", "refresh", "--provider", "gemini"])
        .env("HOME", directory.path())
        .env("USERPROFILE", directory.path())
        .env("APPDATA", directory.path())
        .env("XDG_CONFIG_HOME", directory.path().join(".config"))
        .output()
        .expect("model refresh should start");

    assert_eq!(output.status.code(), Some(3));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("gemini: unsupported"));
    assert!(stdout.contains("no model list was inferred"));
}

#[test]
fn model_catalog_list_reports_a_missing_cache_without_fabricating_models() {
    let directory = TestDirectory::new("model-list-empty");
    let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args(["model", "list", "--provider", "codex"])
        .env("HOME", directory.path())
        .env("USERPROFILE", directory.path())
        .env("APPDATA", directory.path())
        .env("XDG_CONFIG_HOME", directory.path().join(".config"))
        .output()
        .expect("model list should start");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "No cached model catalog evidence for codex."
    );
}

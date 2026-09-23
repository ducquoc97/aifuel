use std::fs;
use std::process::Command;

use crate::support::{TestDirectory, install_fake_command, path_with};

#[test]
fn unverified_workspace_write_modes_are_rejected_before_launch() {
    for provider in ["claude", "gemini", "antigravity"] {
        let directory = TestDirectory::new(&format!("{provider}-unverified-write"));
        install_fake_command(directory.path(), provider);
        let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
            .args([
                "run",
                "--provider",
                provider,
                "--model",
                "test-model",
                "--access",
                "workspace-write",
                "--prompt",
                "hello",
            ])
            .env("PATH", path_with(directory.path()))
            .env("HOME", directory.path())
            .env("USERPROFILE", directory.path())
            .env("APPDATA", directory.path())
            .env("XDG_CONFIG_HOME", directory.path().join(".config"))
            .output()
            .expect("aifuel should start");

        assert_eq!(output.status.code(), Some(2), "{provider}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("cannot enforce workspace-write"),
            "{provider}"
        );
    }
}

#[test]
fn project_runs_outside_configured_execution_roots_are_rejected_by_the_manager() {
    let directory = TestDirectory::new("run-policy-denied");
    let executable_dir = directory.path().join("bin");
    let allowed_root = directory.path().join("allowed");
    let denied_root = directory.path().join("denied");
    let config = directory.path().join(".config/aifuel");
    fs::create_dir_all(&executable_dir).expect("bin directory should exist");
    fs::create_dir_all(&allowed_root).expect("allowed root should exist");
    fs::create_dir_all(&denied_root).expect("denied root should exist");
    fs::create_dir_all(&config).expect("AI Fuel config directory should exist");
    install_fake_command(&executable_dir, "gemini");
    fs::write(
        config.join("execution.json"),
        serde_json::json!({
            "schema_version": 1,
            "policy": {
                "allowed_roots": [allowed_root],
                "retain_content": false
            }
        })
        .to_string(),
    )
    .expect("execution policy should be writable");

    let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args([
            "run",
            "--provider",
            "gemini",
            "--model",
            "test-model",
            "--working-directory",
            denied_root.to_str().expect("test path is UTF-8"),
            "--prompt",
            "hello",
        ])
        .env("PATH", path_with(&executable_dir))
        .env("HOME", directory.path())
        .env("USERPROFILE", directory.path())
        .env("APPDATA", directory.path())
        .env("XDG_CONFIG_HOME", directory.path().join(".config"))
        .output()
        .expect("aifuel should start");

    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("outside the configured execution roots")
    );
    assert!(
        output.stdout.is_empty(),
        "denied requests must not launch providers"
    );
}

#[test]
fn copilot_rejects_unverified_write_and_jsonl_capabilities() {
    for (flag, value, message) in [
        (
            "--access",
            "workspace-write",
            "cannot enforce workspace-write access",
        ),
        ("--output", "jsonl", "cannot provide verified JSONL output"),
    ] {
        let directory = TestDirectory::new(&format!("copilot-unsupported-{value}"));
        install_fake_command(directory.path(), "copilot");

        let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
            .args([
                "run",
                "--provider",
                "copilot",
                "--model",
                "test-model",
                flag,
                value,
                "--prompt",
                "hello",
            ])
            .env("PATH", path_with(directory.path()))
            .env("HOME", directory.path())
            .env("USERPROFILE", directory.path())
            .env("APPDATA", directory.path())
            .output()
            .expect("aifuel should start");

        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains(message));
    }
}

#[test]
fn requested_effort_is_rejected_until_the_provider_reports_a_verified_value() {
    let directory = TestDirectory::new("unknown-effort");
    install_fake_command(directory.path(), "gemini");
    let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args([
            "run",
            "--provider",
            "gemini",
            "--model",
            "test-model",
            "--effort",
            "high",
            "--prompt",
            "hello",
        ])
        .env("PATH", path_with(directory.path()))
        .env("HOME", directory.path())
        .env("USERPROFILE", directory.path())
        .env("APPDATA", directory.path())
        .env("XDG_CONFIG_HOME", directory.path().join(".config"))
        .output()
        .expect("aifuel should start");

    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("cannot report a verified effort setting")
    );
}

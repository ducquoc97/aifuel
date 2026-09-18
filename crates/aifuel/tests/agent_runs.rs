use std::fs;
use std::process::{Command, Stdio};

#[allow(dead_code)]
mod support;

use support::{TestDirectory, install_fake_command, path_with};

#[test]
fn unsupported_provider_does_not_fall_back_to_an_available_cli() {
    let directory = TestDirectory::new("no-agent-fallback");
    install_fake_command(directory.path(), "gemini");

    let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args(["run", "--provider", "antigravity", "--prompt", "hello"])
        .env("PATH", path_with(directory.path()))
        .output()
        .expect("aifuel should start");

    assert_eq!(output.status.code(), Some(3));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("provider antigravity has no verified agent integration")
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn account_selection_remains_explicitly_unsupported_for_each_current_adapter() {
    for provider in ["claude", "codex", "copilot", "gemini"] {
        let directory = TestDirectory::new(&format!("{provider}-account"));
        install_fake_command(directory.path(), provider);

        let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
            .args([
                "run",
                "--provider",
                provider,
                "--account",
                "requested-account",
                "--prompt",
                "hello",
            ])
            .env("PATH", path_with(directory.path()))
            .output()
            .expect("aifuel should start");

        assert_eq!(output.status.code(), Some(2), "{provider}");
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("does not expose provider account selection"),
            "{provider}"
        );
    }
}

#[test]
fn each_adapter_keeps_its_model_access_and_output_argument_contract() {
    let cases = [
        (
            "claude",
            "--permission-mode acceptEdits",
            "--output-format stream-json",
        ),
        ("codex", "--sandbox workspace-write", "--json"),
        (
            "gemini",
            "--approval-mode auto_edit",
            "--output-format stream-json",
        ),
    ];
    for (provider, access, output_format) in cases {
        let directory = TestDirectory::new(&format!("{provider}-options"));
        install_fake_command(directory.path(), provider);

        let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
            .args([
                "run",
                "--provider",
                provider,
                "--model",
                "selected-model",
                "--access",
                "workspace-write",
                "--output",
                "jsonl",
                "--prompt",
                "hello",
            ])
            .env("PATH", path_with(directory.path()))
            .output()
            .expect("aifuel should start");

        assert!(
            output.status.success(),
            "{provider}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("--model selected-model"), "{provider}");
        assert!(stdout.contains(access), "{provider}");
        assert!(stdout.contains(output_format), "{provider}");
    }

    let directory = TestDirectory::new("copilot-json");
    install_fake_command(directory.path(), "copilot");
    let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args([
            "run",
            "--provider",
            "copilot",
            "--model",
            "selected-model",
            "--prompt",
            "hello",
            "--output",
            "json",
        ])
        .env("PATH", path_with(directory.path()))
        .output()
        .expect("aifuel should start");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--model selected-model"));
    assert!(stdout.contains("--plan"));
    assert!(stdout.contains("--output-format json"));
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
                flag,
                value,
                "--prompt",
                "hello",
            ])
            .env("PATH", path_with(directory.path()))
            .output()
            .expect("aifuel should start");

        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains(message));
    }
}

#[test]
fn provider_failure_keeps_stdout_stderr_and_provider_exit_code() {
    let directory = TestDirectory::new("provider-failure");
    install_failing_command(directory.path(), "gemini");

    let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args([
            "run",
            "--provider",
            "gemini",
            "--prompt",
            "hello",
            "--output",
            "json",
        ])
        .env("PATH", path_with(directory.path()))
        .output()
        .expect("aifuel should start");

    assert_eq!(output.status.code(), Some(4));
    let result: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("run should return its structured result");
    assert_eq!(result["status"], "failed");
    assert_eq!(result["exit_code"], 17);
    assert_eq!(result["output"], "partial provider output\n");
    assert_eq!(result["diagnostics"], "provider failure detail\n");
    assert_eq!(result["session_id"], serde_json::Value::Null);
    assert_eq!(result["effective_model"], serde_json::Value::Null);
}

#[cfg(unix)]
#[test]
fn provider_process_timeout_returns_captured_output_and_timeout_status() {
    let directory = TestDirectory::new("provider-timeout");
    install_slow_run_command(directory.path(), "gemini");

    let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args([
            "run",
            "--provider",
            "gemini",
            "--prompt",
            "hello",
            "--output",
            "json",
            "--timeout",
            "1s",
        ])
        .env("PATH", path_with(directory.path()))
        .output()
        .expect("aifuel should start");

    assert_eq!(output.status.code(), Some(5));
    let result: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("timed out run should return its result");
    assert_eq!(result["status"], "timeout");
    assert_eq!(result["timed_out"], true);
    assert!(
        result["output"]
            .as_str()
            .unwrap()
            .contains("partial provider output")
    );
}

#[cfg(unix)]
#[test]
fn stdin_prompt_and_working_directory_reach_the_selected_cli() {
    use std::io::Write;
    use std::os::unix::fs::symlink;

    let directory = TestDirectory::new("stdin-working-directory");
    let physical_working_directory = directory.path().join("workspace");
    let working_directory = directory.path().join("workspace-alias");
    let home = directory.path().join("home");
    let bin = directory.path().join("bin");
    fs::create_dir_all(&physical_working_directory).expect("workspace should be creatable");
    symlink(&physical_working_directory, &working_directory)
        .expect("working-directory alias should be creatable");
    fs::create_dir_all(&home).expect("temporary home should be creatable");
    fs::create_dir_all(&bin).expect("bin should be creatable");
    install_cwd_echo_command(&bin, "gemini");

    let mut child = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args([
            "run",
            "--provider",
            "gemini",
            "--working-directory",
            working_directory.to_str().expect("test path is UTF-8"),
        ])
        .env("PATH", path_with(&bin))
        .env("HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("aifuel should start");
    child
        .stdin
        .take()
        .expect("stdin should be piped")
        .write_all(b"prompt from stdin")
        .expect("stdin prompt should be written");
    let output = child
        .wait_with_output()
        .expect("aifuel should finish the selected run");

    assert!(
        output.status.success(),
        "aifuel run should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let expected_working_directory =
        fs::canonicalize(&working_directory).expect("working-directory alias should resolve");
    assert_ne!(working_directory, expected_working_directory);
    let expected_prefix = format!("{}\n", expected_working_directory.display());
    assert!(
        stdout.starts_with(&expected_prefix),
        "expected child cwd prefix {expected_prefix:?}, got {stdout:?}"
    );
    assert!(stdout.contains("prompt from stdin"));
    assert!(!home.join(".config").exists());
}

#[cfg(unix)]
fn install_failing_command(directory: &std::path::Path, command_name: &str) {
    use std::os::unix::fs::PermissionsExt;

    let path = directory.join(command_name);
    fs::write(
        &path,
        "#!/bin/sh\nif [ \"$1\" = \"--help\" ]; then printf '%s\\n' '--prompt --approval-mode --output-format'; exit 0; fi\nprintf 'partial provider output\\n'\nprintf 'provider failure detail\\n' >&2\nexit 17\n",
    )
    .expect("fake failing executable should be writable");
    let mut permissions = fs::metadata(&path)
        .expect("fake executable should exist")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("fake executable should be executable");
}

#[cfg(windows)]
fn install_failing_command(directory: &std::path::Path, command_name: &str) {
    fs::write(
        directory.join(format!("{command_name}.cmd")),
        "@echo off\nif \"%~1\"==\"--help\" (echo --prompt --approval-mode --output-format & exit /b 0)\necho partial provider output\necho provider failure detail 1>&2\nexit /b 17\n",
    )
    .expect("fake failing executable should be writable");
}

#[cfg(unix)]
fn install_slow_run_command(directory: &std::path::Path, command_name: &str) {
    use std::os::unix::fs::PermissionsExt;

    let path = directory.join(command_name);
    fs::write(
        &path,
        "#!/bin/sh\nif [ \"$1\" = \"--help\" ]; then printf '%s\\n' '--prompt --approval-mode --output-format'; exit 0; fi\nprintf 'partial provider output\\n'\nexec sleep 30\n",
    )
    .expect("slow fake executable should be writable");
    let mut permissions = fs::metadata(&path)
        .expect("slow fake executable should exist")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("slow fake executable should be executable");
}

#[cfg(unix)]
fn install_cwd_echo_command(directory: &std::path::Path, command_name: &str) {
    use std::os::unix::fs::PermissionsExt;

    let path = directory.join(command_name);
    fs::write(
        &path,
        "#!/bin/sh\nif [ \"$1\" = \"--help\" ]; then printf '%s\\n' '--prompt --approval-mode --output-format'; exit 0; fi\npwd -P\nprintf '%s\\n' \"$*\"\n",
    )
    .expect("fake cwd executable should be writable");
    let mut permissions = fs::metadata(&path)
        .expect("fake executable should exist")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("fake executable should be executable");
}

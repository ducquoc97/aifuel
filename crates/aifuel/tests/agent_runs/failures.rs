use std::fs;
use std::process::Command;

#[cfg(unix)]
use std::process::Stdio;

use crate::support::{TestDirectory, ai_fuel_config_dir, path_with};

#[test]
fn provider_failure_keeps_stdout_stderr_and_provider_exit_code() {
    let directory = TestDirectory::new("provider-failure");
    install_failing_command(directory.path(), "gemini");

    let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args([
            "run",
            "--provider",
            "gemini",
            "--model",
            "test-model",
            "--prompt",
            "hello",
            "--output",
            "json",
        ])
        .env("PATH", path_with(directory.path()))
        .env("HOME", directory.path())
        .env("USERPROFILE", directory.path())
        .env("APPDATA", directory.path())
        .env("XDG_CONFIG_HOME", directory.path().join(".config"))
        .output()
        .expect("aifuel should start");

    assert_eq!(output.status.code(), Some(4));
    let result: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("run should return its structured result");
    assert_eq!(result["status"], "failed");
    assert_eq!(result["state"], "failed");
    assert_eq!(result["exit_code"], 17);
    let line_ending = if cfg!(windows) { "\r\n" } else { "\n" };
    assert_eq!(
        result["output"],
        format!("partial provider output{line_ending}")
    );
    assert_eq!(
        result["diagnostics"],
        format!("provider failure detail{line_ending}")
    );
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
            "--model",
            "test-model",
            "--prompt",
            "hello",
            "--output",
            "json",
            "--timeout",
            "1s",
        ])
        .env("PATH", path_with(directory.path()))
        .env("HOME", directory.path())
        .env("USERPROFILE", directory.path())
        .env("APPDATA", directory.path())
        .env("XDG_CONFIG_HOME", directory.path().join(".config"))
        .output()
        .expect("aifuel should start");

    assert_eq!(output.status.code(), Some(5));
    let result: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("timed out run should return its result");
    assert_eq!(result["status"], "timeout");
    assert_eq!(result["state"], "timed_out");
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
    let config = ai_fuel_config_dir(&home);
    fs::create_dir_all(&config).expect("AI Fuel config directory should exist");
    fs::write(
        config.join("execution.json"),
        serde_json::json!({
            "schema_version": 1,
            "policy": {
                "allowed_roots": [physical_working_directory],
                "retain_content": false
            }
        })
        .to_string(),
    )
    .expect("execution policy should be writable");
    install_cwd_echo_command(&bin, "gemini");

    let mut child = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args([
            "run",
            "--provider",
            "gemini",
            "--model",
            "test-model",
            "--working-directory",
            working_directory.to_str().expect("test path is UTF-8"),
        ])
        .env("PATH", path_with(&bin))
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("APPDATA", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
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
    assert!(!config.join("agent-sessions.json").exists());
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
        "@echo off\nif \"%~1\"==\"--help\" (echo --prompt --approval-mode --output-format & exit /b 0)\necho partial provider output\n>&2 echo provider failure detail\nexit /b 17\n",
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

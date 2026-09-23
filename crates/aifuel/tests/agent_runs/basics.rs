use std::fs;
use std::process::Command;

#[cfg(unix)]
use std::{
    io::{BufRead, BufReader, Read},
    process::Stdio,
};

use crate::support::{
    TestDirectory, install_fake_codex_app_server, install_fake_command, path_with,
};

#[test]
fn antigravity_run_uses_the_agy_cli() {
    let directory = TestDirectory::new("antigravity-run");
    install_fake_command(directory.path(), "agy");

    let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args([
            "run",
            "--provider",
            "antigravity",
            "--model",
            "agy-model",
            "--prompt",
            "translate to Vietnamese: hello",
        ])
        .env("PATH", path_with(directory.path()))
        .env("HOME", directory.path())
        .env("USERPROFILE", directory.path())
        .env("APPDATA", directory.path())
        .env("XDG_CONFIG_HOME", directory.path().join(".config"))
        .output()
        .expect("aifuel should start");

    assert!(
        output.status.success(),
        "Antigravity Agent Run should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("fake agy response: --print")
            && stdout.contains("translate to Vietnamese: hello"),
        "expected agy print-mode invocation, got {stdout:?}"
    );
}

#[test]
fn noninteractive_run_requires_an_explicit_or_configured_model() {
    let directory = TestDirectory::new("noninteractive-model-required");
    install_fake_command(directory.path(), "gemini");
    let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args(["run", "--provider", "gemini", "--prompt", "hello"])
        .env("PATH", path_with(directory.path()))
        .env("HOME", directory.path())
        .env("USERPROFILE", directory.path())
        .env("APPDATA", directory.path())
        .env("XDG_CONFIG_HOME", directory.path().join(".config"))
        .output()
        .expect("aifuel should start");

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("run requires --model"));
    assert!(
        output.stdout.is_empty(),
        "provider must not launch implicitly"
    );
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
                "--model",
                "test-model",
                "--account",
                "requested-account",
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
            String::from_utf8_lossy(&output.stderr)
                .contains("does not expose provider account selection"),
            "{provider}"
        );
    }
}

#[test]
fn each_adapter_keeps_its_model_access_and_output_argument_contract() {
    let directory = TestDirectory::new("codex-options");
    let log_path = install_fake_codex_app_server(directory.path());
    let output = Command::new(env!("CARGO_BIN_EXE_aifuel"))
        .args([
            "run",
            "--provider",
            "codex",
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
    let stdout = String::from_utf8_lossy(&output.stdout);
    let result_event: serde_json::Value = serde_json::from_str(
        stdout
            .lines()
            .last()
            .expect("JSONL output ends with a result event"),
    )
    .expect("the final JSONL record should be a run result");
    assert_eq!(result_event["type"], "run_result");
    assert_eq!(result_event["result"]["provider"], "codex");
    assert_eq!(result_event["result"]["state"], "succeeded");
    assert_eq!(
        result_event["result"]["output"],
        "fake codex app-server response"
    );
    let requests: Vec<serde_json::Value> = fs::read_to_string(log_path)
        .expect("App Server requests should be logged")
        .lines()
        .map(|line| serde_json::from_str(line).expect("each request should be JSON"))
        .collect();
    assert_eq!(requests[2]["params"]["model"], "selected-model");
    assert_eq!(requests[2]["params"]["sandbox"], "workspace-write");

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
        .env("HOME", directory.path())
        .env("USERPROFILE", directory.path())
        .env("APPDATA", directory.path())
        .env("XDG_CONFIG_HOME", directory.path().join(".config"))
        .output()
        .expect("aifuel should start");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--model selected-model"));
    assert!(stdout.contains("--plan"));
    assert!(stdout.contains("--output-format json"));
}

#[cfg(unix)]
#[test]
fn codex_cli_run_uses_the_local_approval_command_and_resumes() {
    let directory = TestDirectory::new("codex-local-approval");
    let log_path = install_fake_codex_app_server(directory.path());
    let binary = env!("CARGO_BIN_EXE_aifuel");
    let mut child = Command::new(binary)
        .args([
            "run",
            "--provider",
            "codex",
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
        .env("AIFUEL_CODEX_FIXTURE_LOG", &log_path)
        .env("AIFUEL_CODEX_FIXTURE_APPROVAL", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("managed Codex run should start");
    let mut stderr = BufReader::new(child.stderr.take().expect("stderr should be piped"));
    let approval_line = loop {
        let mut line = String::new();
        let count = stderr
            .read_line(&mut line)
            .expect("approval instructions should be readable");
        assert_ne!(count, 0, "run should print the local approval command");
        if line.starts_with("Approve or reject it with: ") {
            break line;
        }
    };
    let command = approval_line
        .strip_prefix("Approve or reject it with: ")
        .expect("approval instruction should use its command prefix");
    let command: Vec<&str> = command.split_whitespace().collect();
    let run_id = command[3];
    let input_id = command[5];
    assert_eq!(command[0], "aifuel");
    assert_eq!(command[1], "approve");

    let approval = Command::new(binary)
        .args([
            "approve",
            "--run",
            run_id,
            "--input",
            input_id,
            "--decision",
            "accept",
        ])
        .env("HOME", directory.path())
        .env("USERPROFILE", directory.path())
        .env("APPDATA", directory.path())
        .env("XDG_CONFIG_HOME", directory.path().join(".config"))
        .output()
        .expect("local approval command should start");
    assert!(
        approval.status.success(),
        "{}",
        String::from_utf8_lossy(&approval.stderr)
    );

    let status = child.wait().expect("approved run should finish");
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .expect("stdout should be piped")
        .read_to_string(&mut stdout)
        .expect("run output should be readable");
    let mut remaining_stderr = String::new();
    stderr
        .read_to_string(&mut remaining_stderr)
        .expect("remaining diagnostics should be readable");

    assert!(status.success(), "{approval_line}{remaining_stderr}");
    assert_eq!(stdout, "fake codex app-server response");
    let requests: Vec<serde_json::Value> = fs::read_to_string(log_path)
        .expect("App Server requests should be logged")
        .lines()
        .map(|line| serde_json::from_str(line).expect("each request should be JSON"))
        .collect();
    assert_eq!(requests[4]["id"], "approval-1");
    assert_eq!(requests[4]["result"]["decision"], "accept");
}

use super::*;
use crate::agent_execution::parse_public_output;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("test clock should be after the unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "aifuel-cli-{label}-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("test directory should be creatable");
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn empty_arguments(_request: &RunRequest) -> Result<Vec<String>, AgentRunError> {
    Ok(Vec::new())
}

fn install_fake_cli(directory: &Path, unix_body: &str, windows_body: &str) -> &'static str {
    #[cfg(unix)]
    let executable = {
        use std::os::unix::fs::PermissionsExt;

        let executable = directory.join("fake-agent");
        fs::write(&executable, format!("#!/bin/sh\n{unix_body}\n"))
            .expect("fake CLI should be writable");
        let mut permissions = fs::metadata(&executable)
            .expect("fake CLI should exist")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).expect("fake CLI should be executable");
        executable
    };

    #[cfg(windows)]
    let executable = {
        let executable = directory.join("fake-agent.cmd");
        fs::write(&executable, format!("@echo off\r\n{windows_body}\r\n"))
            .expect("fake CLI should be writable");
        executable
    };

    let _ = unix_body;
    let _ = windows_body;
    Box::leak(executable.to_string_lossy().into_owned().into_boxed_str())
}

fn read_only_test_adapter(program: &'static str) -> CliExecutionAdapter {
    CliExecutionAdapter::new(
        aifuel_core::ProviderKey::Gemini,
        program,
        &["--help"],
        &["--prompt", "--approval-mode", "--output-format"],
        empty_arguments,
        parse_public_output,
        ExecutionCapabilities::new(false, false, false, false).with_read_only(),
    )
}

fn cli_test_request() -> RunRequest {
    let mut request = read_only_project_request();
    request.provider = aifuel_core::ProviderKey::Gemini;
    request
}

fn read_only_project_request() -> RunRequest {
    RunRequest {
        provider: aifuel_core::ProviderKey::Antigravity,
        model: Some("model-a".to_owned()),
        effort: None,
        external_tools: None,
        account: None,
        prompt: "hello".to_owned(),
        output: OutputFormat::Text,
        working_directory: Some(std::env::temp_dir()),
        access: AccessMode::ReadOnly,
        resume: None,
        timeout: None,
        interaction_handler: None,
    }
}

#[test]
fn read_only_project_access_requires_verified_provider_enforcement() {
    let request = read_only_project_request();
    let unverified = CliExecutionAdapter::new(
        aifuel_core::ProviderKey::Antigravity,
        "agy",
        &[],
        &[],
        |_| Ok(Vec::new()),
        parse_public_output,
        ExecutionCapabilities::new(false, false, false, false),
    );
    assert!(matches!(
        unverified.validate(&request),
        Err(AgentRunError::InvalidRequest(_))
    ));

    let verified = CliExecutionAdapter::new(
        aifuel_core::ProviderKey::Codex,
        "codex",
        &[],
        &[],
        |_| Ok(Vec::new()),
        parse_public_output,
        ExecutionCapabilities::new(false, false, false, false).with_read_only(),
    );
    let mut codex_request = request;
    codex_request.provider = aifuel_core::ProviderKey::Codex;
    assert!(verified.validate(&codex_request).is_ok());
}

#[test]
fn unverified_read_only_prompt_only_runs_are_rejected() {
    let mut request = read_only_project_request();
    request.working_directory = None;
    let adapter = CliExecutionAdapter::new(
        aifuel_core::ProviderKey::Antigravity,
        "agy",
        &[],
        &[],
        |_| Ok(Vec::new()),
        parse_public_output,
        ExecutionCapabilities::new(false, false, false, false).with_unsupported_read_only(),
    );

    assert!(matches!(
        adapter.validate(&request),
        Err(AgentRunError::InvalidRequest(_))
    ));
}

#[test]
fn unverified_read_only_resumes_are_rejected_without_a_resolved_working_directory() {
    let mut request = read_only_project_request();
    request.working_directory = None;
    request.resume = Some("native-session".to_owned());
    let adapter = CliExecutionAdapter::new(
        aifuel_core::ProviderKey::Antigravity,
        "agy",
        &[],
        &[],
        |_| Ok(Vec::new()),
        parse_public_output,
        ExecutionCapabilities::new(false, false, false, false).with_unsupported_read_only(),
    );

    assert!(matches!(
        adapter.validate(&request),
        Err(AgentRunError::InvalidRequest(_))
    ));
}

#[test]
fn provider_failure_preserves_process_output_diagnostics_and_exit_code() {
    let directory = TestDirectory::new("failure");
    let program = install_fake_cli(
        &directory.0,
        "if [ \"$1\" = \"--help\" ]; then printf '%s\\n' '--prompt --approval-mode --output-format'; exit 0; fi\nprintf 'partial provider output\\n'\nprintf 'provider failure detail\\n' >&2\nexit 17",
        "if \"%~1\"==\"--help\" (echo --prompt --approval-mode --output-format & exit /b 0)\necho partial provider output\n>&2 echo provider failure detail\nexit /b 17",
    );
    let mut request = cli_test_request();
    request.working_directory = None;

    let result = read_only_test_adapter(program)
        .execute(&request, &RunCancellationToken::new())
        .expect("the failed process should return its captured result");

    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(result.exit_code, Some(17));
    assert_eq!(result.output, "partial provider output\n");
    let diagnostics_line_ending = if cfg!(windows) { "\r\n" } else { "\n" };
    let expected_diagnostics = format!("provider failure detail{diagnostics_line_ending}");
    assert_eq!(
        result.diagnostics.as_deref(),
        Some(expected_diagnostics.as_str())
    );
}

#[cfg(unix)]
#[test]
fn provider_process_timeout_returns_captured_output_and_timeout_status() {
    let directory = TestDirectory::new("timeout");
    let program = install_fake_cli(
        &directory.0,
        "if [ \"$1\" = \"--help\" ]; then printf '%s\\n' '--prompt --approval-mode --output-format'; exit 0; fi\nprintf 'partial provider output\\n'\nexec sleep 30",
        "",
    );
    let mut request = cli_test_request();
    request.working_directory = None;
    request.timeout = Some(Duration::from_secs(1));

    let result = read_only_test_adapter(program)
        .execute(&request, &RunCancellationToken::new())
        .expect("the timed-out process should return its captured result");

    assert_eq!(result.status, RunStatus::Timeout);
    assert!(result.timed_out);
    assert!(result.output.contains("partial provider output"));
}

#[cfg(unix)]
#[test]
fn run_timeout_bounds_provider_capability_preflight() {
    let directory = TestDirectory::new("slow-preflight");
    let program = install_fake_cli(
        &directory.0,
        "if [ \"$1\" = \"--help\" ]; then exec sleep 30; fi",
        "",
    );
    let mut request = cli_test_request();
    request.working_directory = None;
    request.timeout = Some(Duration::from_secs(1));

    let error = read_only_test_adapter(program)
        .execute(&request, &RunCancellationToken::new())
        .expect_err("preflight should stop at the run deadline");

    assert!(
        error
            .to_string()
            .contains("provider capability preflight timed out")
    );
}

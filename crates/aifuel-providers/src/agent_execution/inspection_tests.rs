#![cfg(unix)]

use super::AUTHENTICATION_PROBE_TIMEOUT;
use crate::agent_execution::{CliExecutionAdapter, ExecutionCapabilities, parse_public_output};
use aifuel_app::RunManager;
use aifuel_core::{AgentAuthenticationState, AgentRunError, ProviderKey, RunRequest};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("aifuel-auth-probe-{}-{suffix}", std::process::id()));
        fs::create_dir(&path).expect("test directory should be creatable");
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn no_arguments(_request: &RunRequest) -> Result<Vec<String>, AgentRunError> {
    Ok(Vec::new())
}

fn shell_quote(path: &std::path::Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn fake_claude_report(script_body: &str) -> (aifuel_core::AgentIntegrationInfo, String) {
    let directory = TestDirectory::new();
    let args_log = directory.0.join("args");
    let executable = directory.0.join("fake-claude");
    fs::write(
        &executable,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\n{}\n",
            shell_quote(&args_log),
            script_body
        ),
    )
    .expect("fake executable should be writable");
    let mut permissions = fs::metadata(&executable)
        .expect("fake executable should exist")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&executable, permissions).expect("fake executable should be runnable");

    let program: &'static str =
        Box::leak(executable.to_string_lossy().into_owned().into_boxed_str());
    let adapter = CliExecutionAdapter::new(
        ProviderKey::Claude,
        program,
        &[],
        &[],
        no_arguments,
        parse_public_output,
        ExecutionCapabilities::new(false, false, false, false),
    )
    .with_authentication_probe(&["auth", "status"]);
    let manager = RunManager::new(vec![Arc::new(adapter)]);
    let agent = manager
        .list_agents(Some(ProviderKey::Claude))
        .pop()
        .expect("registered provider should be returned");
    let args = fs::read_to_string(args_log).expect("the fake status command should be logged");
    manager.shutdown();
    (agent, args)
}

#[test]
fn claude_auth_status_reports_authenticated_without_exposing_command_output() {
    let (agent, args) = fake_claude_report(
        "printf '%s\\n' '{\"loggedIn\":true,\"email\":\"private@example.com\"}'\nprintf '%s\\n' 'token=secret-value' >&2\nexit 0",
    );

    assert_eq!(
        agent.native_authentication.state,
        AgentAuthenticationState::Authenticated
    );
    assert!(agent.native_authentication.reason.contains("exit status 0"));
    assert_eq!(args, "auth status\n");
    let report = serde_json::to_string(&agent).expect("report should serialize");
    assert!(!report.contains("private@example.com"));
    assert!(!report.contains("secret-value"));
}

#[test]
fn claude_auth_status_reports_unauthenticated_from_documented_exit_code() {
    let (agent, args) = fake_claude_report(
        "printf '%s\\n' '{\"loggedIn\":false,\"email\":\"private@example.com\"}'\nprintf '%s\\n' 'secret-value' >&2\nexit 1",
    );

    assert_eq!(
        agent.native_authentication.state,
        AgentAuthenticationState::Unauthenticated
    );
    assert!(agent.native_authentication.reason.contains("exit status 1"));
    assert_eq!(args, "auth status\n");
    let report = serde_json::to_string(&agent).expect("report should serialize");
    assert!(!report.contains("private@example.com"));
    assert!(!report.contains("secret-value"));
}

#[test]
fn claude_auth_status_timeout_reports_unknown_and_is_bounded() {
    let started_at = Instant::now();
    let (agent, args) = fake_claude_report("exec sleep 30");

    assert_eq!(
        agent.native_authentication.state,
        AgentAuthenticationState::Unknown
    );
    assert!(
        agent
            .native_authentication
            .reason
            .contains("2-second limit")
    );
    assert_eq!(args, "auth status\n");
    assert!(
        started_at.elapsed() < AUTHENTICATION_PROBE_TIMEOUT + Duration::from_secs(3),
        "auth status must not hang provider listing"
    );
}

#[test]
fn claude_auth_status_failure_reports_unknown_without_exposing_command_output() {
    let (agent, args) = fake_claude_report(
        "printf '%s\\n' '{\"email\":\"private@example.com\"}'\nprintf '%s\\n' 'secret-value' >&2\nexit 23",
    );

    assert_eq!(
        agent.native_authentication.state,
        AgentAuthenticationState::Unknown
    );
    assert!(
        agent
            .native_authentication
            .reason
            .contains("exit status 23")
    );
    assert_eq!(args, "auth status\n");
    let report = serde_json::to_string(&agent).expect("report should serialize");
    assert!(!report.contains("private@example.com"));
    assert!(!report.contains("secret-value"));
}

#![cfg(unix)]

use super::{CliExecutionAdapter, ExecutionCapabilities, parse_public_output};
use aifuel_app::RunManager;
use aifuel_core::{
    AccessMode, AgentAuthenticationState, AgentCapability, AgentExecutionAdapter,
    AgentPresenceState, AgentRunError, AgentRunOutputHandler, AgentSetupGuidance, CapabilityState,
    OutputFormat, ProviderKey, RunCancellationToken, RunRequest, RunResult, RunStatus,
};
use std::fs;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("aifuel-run-cancel-{}-{suffix}", std::process::id()));
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

fn marker_executor<'a>(
    _request: &'a RunRequest,
    _cancellation: &'a RunCancellationToken,
    _output_handler: Option<&'a dyn AgentRunOutputHandler>,
) -> Pin<Box<dyn Future<Output = Result<RunResult, AgentRunError>> + Send + 'a>> {
    Box::pin(async {
        Err(AgentRunError::InvalidRequest(
            "adapter executor override was selected".to_owned(),
        ))
    })
}

fn request() -> RunRequest {
    RunRequest {
        provider: ProviderKey::Gemini,
        model: None,
        effort: None,
        external_tools: None,
        account: None,
        prompt: "hello".to_owned(),
        output: OutputFormat::Text,
        working_directory: None,
        access: AccessMode::ReadOnly,
        resume: None,
        timeout: None,
        interaction_handler: None,
    }
}

fn shell_quote(path: &std::path::Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

#[test]
fn provider_execution_capabilities_are_data_not_provider_key_checks() {
    let mut generic_request = request();
    generic_request.effort = Some("high".to_owned());
    generic_request.external_tools = Some(Vec::new());
    let generic_adapter = CliExecutionAdapter::new(
        ProviderKey::Gemini,
        "unused",
        &[],
        &[],
        no_arguments,
        parse_public_output,
        ExecutionCapabilities::new(true, false, false, true)
            .with_read_only()
            .with_external_tools()
            .with_effort(),
    );
    assert!(generic_adapter.validate(&generic_request).is_ok());

    let mut codex_request = generic_request;
    codex_request.provider = ProviderKey::Codex;
    let codex_without_capabilities = CliExecutionAdapter::new(
        ProviderKey::Codex,
        "unused",
        &[],
        &[],
        no_arguments,
        parse_public_output,
        ExecutionCapabilities::new(true, false, false, true),
    );
    assert!(codex_without_capabilities.validate(&codex_request).is_err());

    codex_request.external_tools = None;
    assert!(codex_without_capabilities.validate(&codex_request).is_err());
}

#[test]
fn declared_executor_override_runs_before_cli_preflight() {
    let adapter = CliExecutionAdapter::new(
        ProviderKey::Gemini,
        "this-executable-does-not-exist",
        &["--help"],
        &["required-flag"],
        no_arguments,
        parse_public_output,
        ExecutionCapabilities::new(true, false, true, true).with_read_only(),
    )
    .with_executor(marker_executor);

    let error = adapter
        .execute(&request(), &RunCancellationToken::new())
        .expect_err("the declared executor should run instead of process preflight");
    assert!(error.to_string().contains("executor override was selected"));
}

#[test]
fn cancellation_kills_and_reaps_the_provider_process_and_keeps_partial_output() {
    use std::os::unix::fs::PermissionsExt;

    let directory = TestDirectory::new();
    let marker = directory.0.join("started");
    let executable = directory.0.join("gemini");
    fs::write(
        &executable,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--help\" ]; then printf '%s\\n' '--prompt --approval-mode --output-format'; exit 0; fi\nprintf 'partial provider output\\n'\nprintf started > {}\nexec sleep 30\n",
            shell_quote(&marker)
        ),
    )
    .expect("fake provider should be writable");
    let mut permissions = fs::metadata(&executable)
        .expect("fake provider should exist")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&executable, permissions).expect("fake provider should be executable");

    let program: &'static str =
        Box::leak(executable.to_string_lossy().into_owned().into_boxed_str());
    let adapter = CliExecutionAdapter::new(
        ProviderKey::Gemini,
        program,
        &["--help"],
        &["--prompt", "--approval-mode", "--output-format"],
        no_arguments,
        parse_public_output,
        ExecutionCapabilities::new(true, false, true, true).with_read_only(),
    );
    let request = request();
    let cancellation = RunCancellationToken::new();

    let result = thread::scope(|scope| {
        let cancellation_for_run = cancellation.clone();
        let handle = scope.spawn(move || adapter.execute(&request, &cancellation_for_run));
        let deadline = Instant::now() + Duration::from_secs(2);
        while !marker.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let started = marker.exists();
        cancellation.cancel();
        let result = handle.join().expect("execution worker should not panic");
        assert!(started, "provider process should reach its armed state");
        result
    })
    .expect("cancelled execution should return its result");

    assert_eq!(result.status, RunStatus::Cancelled);
    assert!(!result.timed_out);
    assert!(result.output.contains("partial provider output"));
    assert_eq!(result.session_id, None);
}

#[cfg(unix)]
#[test]
fn run_manager_lists_native_presence_and_only_reports_version_from_safe_probe() {
    use std::os::unix::fs::PermissionsExt;

    let directory = TestDirectory::new();
    let version_log = directory.0.join("version-args");
    let unknown_version_log = directory.0.join("unknown-version-args");
    let version_executable = directory.0.join("versioned-agent");
    let unknown_version_executable = directory.0.join("no-version-probe-agent");
    fs::write(
        &version_executable,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nif [ \"$1\" = \"--version\" ]; then printf 'fake-agent 1.2.3\\n'; exit 0; fi\nprintf 'unexpected command\\n' >&2\nexit 91\n",
            shell_quote(&version_log)
        ),
    )
    .expect("version fake executable should be writable");
    fs::write(
        &unknown_version_executable,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nprintf 'unexpected command\\n' >&2\nexit 91\n",
            shell_quote(&unknown_version_log)
        ),
    )
    .expect("unknown-version fake executable should be writable");
    for executable in [&version_executable, &unknown_version_executable] {
        let mut permissions = fs::metadata(executable)
            .expect("fake executable should exist")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(executable, permissions).expect("fake executable should be runnable");
    }

    let version_program: &'static str = Box::leak(
        version_executable
            .to_string_lossy()
            .into_owned()
            .into_boxed_str(),
    );
    let unknown_version_program: &'static str = Box::leak(
        unknown_version_executable
            .to_string_lossy()
            .into_owned()
            .into_boxed_str(),
    );
    let manager = RunManager::new(vec![
        Arc::new(
            CliExecutionAdapter::new(
                ProviderKey::Claude,
                version_program,
                &[],
                &[],
                no_arguments,
                parse_public_output,
                ExecutionCapabilities::new(true, false, true, true),
            )
            .with_version_probe(&["--version"]),
        ),
        Arc::new(CliExecutionAdapter::new(
            ProviderKey::Antigravity,
            unknown_version_program,
            &[],
            &[],
            no_arguments,
            parse_public_output,
            ExecutionCapabilities::new(false, false, false, false),
        )),
    ]);

    let agents = manager.list_agents(None);
    assert_eq!(agents.len(), 2);
    let claude = agents
        .iter()
        .find(|agent| agent.provider == ProviderKey::Claude)
        .expect("the selected provider should be listed");
    assert_eq!(claude.native_presence.state, AgentPresenceState::Present);
    assert_eq!(claude.native_version.version.as_deref(), Some("1.2.3"));
    let workspace_write = &claude.capabilities[&AgentCapability::WorkspaceWrite];
    assert_eq!(workspace_write.declared.state, CapabilityState::Supported);
    assert_eq!(
        workspace_write.current.state,
        CapabilityState::Unknown,
        "adapter declaration must not stand in for current enforcement evidence"
    );
    assert!(!workspace_write.current.reason.is_empty());

    let antigravity = manager
        .list_agents(Some(ProviderKey::Antigravity))
        .pop()
        .expect("provider filter should select only Antigravity");
    assert_eq!(
        antigravity.native_presence.state,
        AgentPresenceState::Present
    );
    assert_eq!(antigravity.native_version.version, None);
    assert!(!antigravity.native_version.reason.is_empty());
    assert!(manager.list_agents(Some(ProviderKey::Codex)).is_empty());

    assert_eq!(
        fs::read_to_string(&version_log).expect("version probe should be logged"),
        "--version\n",
        "listing must not send a prompt, login command, or other undocumented arguments"
    );
    assert!(
        !unknown_version_log.exists(),
        "an adapter without a documented version command must not be spawned"
    );
    manager.shutdown();
}

#[cfg(unix)]
#[test]
fn run_manager_listing_returns_setup_guidance_without_reading_authentication() {
    use std::os::unix::fs::PermissionsExt;

    let directory = TestDirectory::new();
    let args_log = directory.0.join("probe-args");
    let executable = directory.0.join("setup-agent");
    fs::write(
        &executable,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nprintf 'setup-agent 4.5.6\\n'\n",
            shell_quote(&args_log)
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
    let setup_guidance = AgentSetupGuidance {
        install: "npm install -g @example/agent",
        login: "Run `example-agent login` interactively.",
        check: "Run `example-agent login status` yourself to inspect local sign-in state.",
        documentation_url: "https://example.com/agent/setup",
    };
    let adapter = CliExecutionAdapter::new(
        ProviderKey::Claude,
        program,
        &[],
        &[],
        no_arguments,
        parse_public_output,
        ExecutionCapabilities::new(false, false, false, false),
    )
    .with_version_probe(&["--version"])
    .with_setup_guidance(setup_guidance);
    let manager = RunManager::new(vec![Arc::new(adapter)]);

    let agent = manager
        .list_agents(Some(ProviderKey::Claude))
        .pop()
        .expect("the registered provider should be returned");

    assert_eq!(agent.setup_guidance.as_ref(), Some(&setup_guidance));
    assert_eq!(
        agent.native_authentication.state,
        AgentAuthenticationState::Unknown,
        "listing must not read local credentials or run login commands"
    );
    assert!(agent.native_authentication.reason.contains("not inspected"));
    assert_eq!(
        fs::read_to_string(&args_log).expect("only the safe version probe should run"),
        "--version\n"
    );
    manager.shutdown();
}

#[cfg(unix)]
#[test]
fn run_manager_version_probe_times_out_and_reaps_the_fake_process_group() {
    use std::os::unix::fs::PermissionsExt;

    let directory = TestDirectory::new();
    let started = directory.0.join("probe-started");
    let executable = directory.0.join("slow-version-agent");
    fs::write(
        &executable,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf started > {}; exec sleep 30; fi\nexit 91\n",
            shell_quote(&started)
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
        ProviderKey::Gemini,
        program,
        &[],
        &[],
        no_arguments,
        parse_public_output,
        ExecutionCapabilities::new(false, false, false, false),
    )
    .with_version_probe(&["--version"]);
    let manager = RunManager::new(vec![Arc::new(adapter)]);
    let started_at = Instant::now();

    let agent = manager
        .list_agents(Some(ProviderKey::Gemini))
        .pop()
        .expect("registered provider should be returned");

    assert!(
        started.exists(),
        "the fake version process should have started"
    );
    assert_eq!(agent.native_version.version, None);
    assert!(agent.native_version.reason.contains("2-second limit"));
    assert!(started_at.elapsed() < Duration::from_secs(5));
    manager.shutdown();
}

#[cfg(unix)]
#[test]
fn run_manager_version_probe_bounds_captured_output() {
    use std::os::unix::fs::PermissionsExt;

    let directory = TestDirectory::new();
    let executable = directory.0.join("large-version-agent");
    fs::write(
        &executable,
        "#!/bin/sh\nprintf 'fake-agent 1.2.3 '\ndd if=/dev/zero bs=20000 count=1 2>/dev/null\nprintf '\\n'\n",
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
        ProviderKey::Gemini,
        program,
        &[],
        &[],
        no_arguments,
        parse_public_output,
        ExecutionCapabilities::new(false, false, false, false),
    )
    .with_version_probe(&["--version"]);
    let manager = RunManager::new(vec![Arc::new(adapter)]);

    let agent = manager
        .list_agents(Some(ProviderKey::Gemini))
        .pop()
        .expect("registered provider should be returned");

    assert_eq!(agent.native_version.version, None);
    assert!(agent.native_version.reason.contains("16 KiB"));
    manager.shutdown();
}

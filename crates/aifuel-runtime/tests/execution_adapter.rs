//! The [`RuntimeExecutionAdapter`] contract: one legacy
//! `AgentExecutionAdapter::execute` maps to a runtime session, run, and
//! event drain; approvals delegate to the owner interaction handler rather
//! than a second authority; and `session.close` always runs behind the run
//! so no provider session outlives it. The `CliAdapter` cases exercise the
//! real session worker and interaction plumbing; the `FakeAdapter` cases
//! cover cancellation, deadlines, and failure shapes the scripted adapter
//! can produce deterministically.

mod support;

use aifuel_core::{
    AccessMode, AgentCapability, AgentExecutionAdapter, AgentInteractionHandler,
    AgentInteractionKind, AgentInteractionRequest, AgentInteractionResponse, AgentRunError,
    AgentRunOutputHandler, ExecutionMode, IntegrationId, OutputFormat, PermissionApprovalDecision,
    RunCancellationToken, RunRequest, RunStatus,
};
use aifuel_runtime::{AgentRuntime, RuntimeExecutionAdapter};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::{
    ExecScript, FAKE_INTEGRATION, FAKE_PROVIDER, FakeAdapter, FakeScript, cli_runtime,
    fake_descriptor, run_start, runtime_at, test_dir,
};

/// A run request naming the fake integration's scripted model.
fn run_request(dir: &Path) -> RunRequest {
    RunRequest {
        integration: IntegrationId::new(FAKE_INTEGRATION),
        model: Some("scripted-model".to_owned()),
        effort: None,
        external_tools: None,
        account: None,
        prompt: "hello".to_owned(),
        output: OutputFormat::Text,
        working_directory: Some(dir.to_path_buf()),
        access: AccessMode::WorkspaceWrite,
        resume: None,
        timeout: None,
        env: Default::default(),
        interaction_handler: None,
    }
}

/// The shim the manager's `AdapterSlot.handle` would hold for `fake-cli`.
fn shim(runtime: &Arc<AgentRuntime>) -> RuntimeExecutionAdapter {
    RuntimeExecutionAdapter::resolve(runtime, &IntegrationId::new(FAKE_INTEGRATION))
        .expect("the fake adapter is registered")
}

/// A `FakeAdapter`-backed runtime with the declared capability evidence the
/// shim's `validate` gates consult.
fn fake_shim_runtime(
    dir: &Path,
    capabilities: aifuel_core::AdapterCapabilities,
    scripts: Vec<FakeScript>,
) -> (aifuel_app::RunStore, Arc<AgentRuntime>) {
    let adapter = Arc::new(
        FakeAdapter::new(capabilities, Vec::new())
            .with_scripts(scripts)
            .declaring(AgentCapability::WorkspaceWrite),
    );
    let (store, runtime) = runtime_at(dir, vec![adapter], vec![fake_descriptor()]);
    (store, Arc::new(runtime))
}

/// A run owner that records each request and answers with a canned
/// response, standing in for the manager's `OwnerInteractionHandler`.
#[derive(Debug)]
struct CannedHandler {
    response: AgentInteractionResponse,
    requests: Mutex<Vec<AgentInteractionRequest>>,
}

impl CannedHandler {
    fn new(response: AgentInteractionResponse) -> Self {
        Self {
            response,
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<AgentInteractionRequest> {
        self.requests.lock().expect("requests mutex").clone()
    }
}

impl AgentInteractionHandler for CannedHandler {
    fn interact(
        &self,
        request: AgentInteractionRequest,
        _cancellation: &RunCancellationToken,
    ) -> Result<AgentInteractionResponse, AgentRunError> {
        self.requests.lock().expect("requests mutex").push(request);
        Ok(self.response.clone())
    }
}

/// A run output sink recording the deltas the session streamed.
#[derive(Debug, Default)]
struct RecordingOutput(Mutex<Vec<String>>);

impl AgentRunOutputHandler for RecordingOutput {
    fn on_output(&self, delta: &str) {
        self.0.lock().expect("output mutex").push(delta.to_owned());
    }
}

#[test]
fn execute_streams_deltas_and_reports_the_resume_cursor() {
    let dir = test_dir("shim-succeed");
    let (_store, runtime) = cli_runtime(
        &dir,
        vec![ExecScript::Succeed {
            deltas: vec!["he".to_owned(), "llo".to_owned()],
            session_id: Some("native-77"),
        }],
    );
    let runtime = Arc::new(runtime);
    let result = shim(&runtime)
        .execute(&run_request(&dir), &RunCancellationToken::new())
        .expect("the scripted run succeeds");
    assert_eq!(result.status, RunStatus::Succeeded);
    // The assistant stream is the result's output surface.
    assert_eq!(result.output, "hello");
    // The provider session id the run reported becomes the resume cursor.
    assert_eq!(result.session_id.as_deref(), Some("native-77"));
    assert!(!result.local_session_id.is_empty());
    assert!(!result.run_id.is_empty());
    // The committed selection echoes the request, so no divergent effective
    // model is claimed: `run.started` is evidence of the session's
    // selection, not a provider-reported override.
    assert!(result.effective_model.is_none());
    assert!(result.effective_effort.is_none());
    assert_eq!(result.execution_mode, ExecutionMode::Project);
    assert_eq!(result.provider_id.as_str(), FAKE_PROVIDER);
    assert_eq!(result.integration_id.as_str(), FAKE_INTEGRATION);
    assert!(!result.timed_out);
    assert!(result.error.is_none());
}

#[test]
fn output_handler_receives_each_streamed_delta() {
    let dir = test_dir("shim-output");
    let (_store, runtime) = cli_runtime(
        &dir,
        vec![ExecScript::Succeed {
            deltas: vec!["he".to_owned(), "llo".to_owned()],
            session_id: None,
        }],
    );
    let runtime = Arc::new(runtime);
    let handler = RecordingOutput::default();
    let result = shim(&runtime)
        .execute_with_output_handler(&run_request(&dir), &RunCancellationToken::new(), &handler)
        .expect("the scripted run succeeds");
    assert_eq!(result.status, RunStatus::Succeeded);
    // The owner sees every delta in order; the result keeps the full text.
    assert_eq!(
        handler.0.lock().expect("output mutex").as_slice(),
        ["he".to_owned(), "llo".to_owned()]
    );
    assert_eq!(result.output, "hello");
}

#[test]
fn approval_requests_reach_the_owner_interaction_handler() {
    let dir = test_dir("shim-approval");
    let (_store, runtime) = cli_runtime(
        &dir,
        vec![ExecScript::Approve {
            description: "run the tool?",
        }],
    );
    let runtime = Arc::new(runtime);
    let handler = Arc::new(CannedHandler::new(AgentInteractionResponse::Permission(
        PermissionApprovalDecision::Accept,
    )));
    let mut request = run_request(&dir);
    request.interaction_handler = Some(handler.clone());
    let result = shim(&runtime)
        .execute(&request, &RunCancellationToken::new())
        .expect("the answered approval lets the run succeed");
    // The run completed only because the owner's answer flowed back through
    // `approval.answer` - the scripted provider blocks on it.
    assert_eq!(result.status, RunStatus::Succeeded);
    let requests = handler.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    // The provider-native shape survives the runtime round trip.
    assert_eq!(request.kind, AgentInteractionKind::CommandApproval);
    assert_eq!(request.method, "session/request_permission");
    assert_eq!(request.description, "run the tool?");
    assert!(request.request_id.is_string());
    assert!(!request.requires_expanded_access);
}

#[test]
fn cancelled_runs_report_cancelled_and_close_the_session() {
    let dir = test_dir("shim-cancel");
    let (_store, runtime) = fake_shim_runtime(
        &dir,
        FakeAdapter::default_capabilities(),
        vec![FakeScript::Block { cursor: None }],
    );
    let adapter = Arc::new(shim(&runtime));
    let cancellation = RunCancellationToken::new();
    let worker = {
        let adapter = Arc::clone(&adapter);
        let request = run_request(&dir);
        let cancellation = cancellation.clone();
        std::thread::spawn(move || adapter.execute(&request, &cancellation))
    };
    // Let the run reach its blocking point before cancelling.
    std::thread::sleep(Duration::from_millis(150));
    cancellation.cancel();
    let result = worker
        .join()
        .expect("the execute worker finishes")
        .expect("cancellation reports a result, not an error");
    assert_eq!(result.status, RunStatus::Cancelled);
    assert!(!result.timed_out);
    // The session guard closed the runtime session: a second run against
    // the same session id finds it gone.
    let session_id = aifuel_core::SessionId::new(result.local_session_id.clone());
    let outcome = runtime.dispatch(run_start(&session_id, "again"), &support::consumer("probe"));
    let code = support::receipt_code(&outcome);
    assert!(
        matches!(
            code,
            aifuel_core::ReceiptCode::InvalidState | aifuel_core::ReceiptCode::UnknownSession
        ),
        "a closed session rejects run.start: {code:?}"
    );
}

#[test]
fn deadline_bounded_runs_report_timeout() {
    let dir = test_dir("shim-timeout");
    let (_store, runtime) = fake_shim_runtime(
        &dir,
        FakeAdapter::default_capabilities(),
        vec![FakeScript::Block { cursor: None }],
    );
    let adapter = shim(&runtime);
    let mut request = run_request(&dir);
    request.timeout = Some(Duration::from_millis(100));
    let result = adapter
        .execute(&request, &RunCancellationToken::new())
        .expect("a timed-out run still reports a result");
    assert_eq!(result.status, RunStatus::Timeout);
    assert!(result.timed_out);
    assert_eq!(result.error.as_deref(), Some("agent run timed out"));
}

#[test]
fn provider_failures_report_a_failed_run() {
    let dir = test_dir("shim-failure");
    let (_store, runtime) = fake_shim_runtime(
        &dir,
        FakeAdapter::default_capabilities(),
        vec![FakeScript::Fail {
            message: "the provider exploded",
        }],
    );
    let result = shim(&runtime)
        .execute(&run_request(&dir), &RunCancellationToken::new())
        .expect("a provider failure reports a result, not an error");
    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(result.error.as_deref(), Some("the provider exploded"));
    assert!(
        result
            .diagnostics
            .as_deref()
            .is_some_and(|diagnostics| diagnostics.contains("the provider exploded"))
    );
}

#[test]
fn session_start_failures_surface_as_launcher_errors() {
    let dir = test_dir("shim-start-failure");
    let adapter = Arc::new(
        FakeAdapter::new(FakeAdapter::default_capabilities(), Vec::new())
            .failing_to_start()
            .declaring(AgentCapability::WorkspaceWrite),
    );
    let (_store, runtime) = runtime_at(&dir, vec![adapter], vec![fake_descriptor()]);
    let runtime = Arc::new(runtime);
    let error = shim(&runtime)
        .execute(&run_request(&dir), &RunCancellationToken::new())
        .expect_err("a session the adapter refuses to start fails the run");
    assert!(
        matches!(error, AgentRunError::Io(_)),
        "provider-side failures keep the launcher error shape: {error:?}"
    );
}

#[test]
fn the_request_resume_cursor_reaches_the_session() {
    let dir = test_dir("shim-resume");
    let (_store, runtime) = fake_shim_runtime(
        &dir,
        FakeAdapter::default_capabilities(),
        vec![FakeScript::Complete {
            deltas: Vec::new(),
            cursor: Some("new-cursor"),
        }],
    );
    let adapter = shim(&runtime);
    let mut request = run_request(&dir);
    request.resume = Some("old-cursor".to_owned());
    let result = adapter
        .execute(&request, &RunCancellationToken::new())
        .expect("the resumed run succeeds");
    assert_eq!(result.resumed_from.as_deref(), Some("old-cursor"));
    assert_eq!(result.session_id.as_deref(), Some("new-cursor"));
}

#[test]
fn every_shim_in_a_set_shares_one_runtime() {
    let dir = test_dir("shim-shared");
    let (_store, runtime) =
        fake_shim_runtime(&dir, FakeAdapter::default_capabilities(), Vec::new());
    let first = shim(&runtime);
    let second = RuntimeExecutionAdapter::resolve(&runtime, &IntegrationId::new(FAKE_INTEGRATION))
        .expect("the same integration resolves twice");
    assert!(
        Arc::ptr_eq(first.runtime(), second.runtime()),
        "a manager's shim set is backed by one AgentRuntime"
    );
    assert!(
        RuntimeExecutionAdapter::resolve(&runtime, &IntegrationId::new("unregistered")).is_none(),
        "integrations without a registered adapter have no shim"
    );
}

#[test]
fn validate_enforces_the_declared_capability_gates() {
    let dir = test_dir("shim-validate");
    let (_store, runtime) =
        fake_shim_runtime(&dir, FakeAdapter::default_capabilities(), Vec::new());
    let adapter = shim(&runtime);

    // Read-only requires a declared boundary the fake does not carry.
    let mut request = run_request(&dir);
    request.access = AccessMode::ReadOnly;
    let error = adapter
        .validate(&request)
        .expect_err("read-only without a declared boundary is refused");
    assert!(
        matches!(error, AgentRunError::InvalidRequest(_)),
        "unsupported access stays an invalid-request refusal: {error:?}"
    );

    // An empty prompt is refused before a session ever starts.
    let mut request = run_request(&dir);
    request.prompt = "   ".to_owned();
    assert!(matches!(
        adapter.validate(&request),
        Err(AgentRunError::InvalidRequest(_))
    ));

    // Account selection has no runtime contract surface: it is refused
    // rather than silently dropped.
    let mut request = run_request(&dir);
    request.account = Some("acct-1".to_owned());
    assert!(matches!(
        adapter.validate(&request),
        Err(AgentRunError::InvalidRequest(_))
    ));

    // External tools require the adapter's declared enforcement.
    let mut request = run_request(&dir);
    request.external_tools = Some(vec!["aifuel-gateway.tool".to_owned()]);
    assert!(matches!(
        adapter.validate(&request),
        Err(AgentRunError::InvalidRequest(_))
    ));

    // An integration the shim does not serve is unsupported, not invalid.
    let mut request = run_request(&dir);
    request.integration = IntegrationId::new("other-cli");
    assert!(matches!(
        adapter.validate(&request),
        Err(AgentRunError::UnsupportedIntegration(_))
    ));
}

#[test]
fn resume_requires_a_declared_capability() {
    let dir = test_dir("shim-resume-gate");
    let capabilities = aifuel_core::AdapterCapabilities {
        resume: false,
        ..FakeAdapter::default_capabilities()
    };
    let (_store, runtime) = fake_shim_runtime(&dir, capabilities, Vec::new());
    let adapter = shim(&runtime);
    let mut request = run_request(&dir);
    request.resume = Some("cursor".to_owned());
    assert!(matches!(
        adapter.validate(&request),
        Err(AgentRunError::InvalidRequest(_))
    ));
}

#[test]
fn effort_requires_a_declared_capability() {
    let dir = test_dir("shim-effort-gate");
    let capabilities = aifuel_core::AdapterCapabilities {
        effort: false,
        ..FakeAdapter::default_capabilities()
    };
    let (_store, runtime) = fake_shim_runtime(&dir, capabilities, Vec::new());
    let adapter = shim(&runtime);
    let mut request = run_request(&dir);
    request.effort = Some("high".to_owned());
    assert!(matches!(
        adapter.validate(&request),
        Err(AgentRunError::InvalidRequest(_))
    ));
}

#[test]
fn prompt_only_runs_borrow_a_scratch_working_directory() {
    let dir = test_dir("shim-prompt-only");
    let (_store, runtime) = cli_runtime(
        &dir,
        vec![ExecScript::Succeed {
            deltas: Vec::new(),
            session_id: None,
        }],
    );
    let runtime = Arc::new(runtime);
    let mut request = run_request(&dir);
    request.working_directory = None;
    let result = shim(&runtime)
        .execute(&request, &RunCancellationToken::new())
        .expect("the prompt-only run succeeds");
    assert_eq!(result.execution_mode, ExecutionMode::PromptOnly);
    assert!(result.working_directory.is_absolute());
    // The scratch directory is torn down behind the run.
    assert!(!result.working_directory.exists());
}

#[test]
fn declared_capabilities_report_the_serving_adapters_evidence() {
    let dir = test_dir("shim-declared");
    let (_store, runtime) =
        fake_shim_runtime(&dir, FakeAdapter::default_capabilities(), Vec::new());
    let adapter = shim(&runtime);
    let declared = adapter.declared_agent_capabilities();
    assert_eq!(
        declared
            .get(&AgentCapability::WorkspaceWrite)
            .map(|evidence| evidence.state),
        Some(aifuel_core::CapabilityState::Supported)
    );
}

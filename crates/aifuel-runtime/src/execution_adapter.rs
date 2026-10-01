//! The legacy [`AgentExecutionAdapter`] over one shared [`AgentRuntime`].
//!
//! `aifuel run` and `aifuel mcp execution` keep their `RunManager` surface -
//! owner records, interaction handling, event history, session import - while
//! provider sessions and run lifecycle move behind the runtime contract. The
//! dependency graph forbids `aifuel-app -> aifuel-runtime`, so the bridge
//! lives here: [`RuntimeExecutionAdapter`] implements the contract the
//! manager's `AdapterSlot.handle` expects and drives
//! [`AgentRuntime::dispatch`] instead of any provider machinery of its own.
//!
//! One `execute` maps to one runtime session on a run-scoped consumer:
//! `session.create` carries the request's selection, access, resume cursor,
//! and external tools; `session.subscribe` attaches the consumer; `run.start`
//! sends the prompt; the consumer's event stream then drives output
//! forwarding, approval delegation, and the terminal outcome. `session.close`
//! always runs before return so no provider session outlives the legacy run;
//! the provider resume cursor is read while the session is still live, and
//! the manager's own session store keeps it for `--resume`.
//!
//! Approval Requests never bypass the owner: `approval.requested` rebuilds
//! the legacy [`AgentInteractionRequest`](aifuel_core::AgentInteractionRequest)
//! the provider originally raised and
//! blocks on `RunRequest.interaction_handler` - the manager's
//! `OwnerInteractionHandler` - exactly as a compiled adapter would, then maps
//! the response back onto `approval.answer`. The shared approval policy
//! already narrowed the offered options, so `accept` never reaches a
//! read-only run and a widening ask reports `requires_expanded_access`.

mod drain;
mod guards;
mod interactions;

use guards::{ScratchDir, SessionGuard};

use crate::adapter::RuntimeAdapter;
use crate::dispatch::CommandOutcome;
use crate::runtime::AgentRuntime;
use aifuel_core::{
    AccessMode, AgentCapability, AgentCapabilityEvidence, AgentCommand, AgentExecutionAdapter,
    AgentIntegrationInfo, AgentRunError, AgentRunOutputHandler, AgentSetupGuidance,
    CapabilityState, CommandId, ConsumerId, Effort, ExecutionMode, IntegrationId, ModelSelection,
    ProviderId, ReceiptCode, ReceiptOutcome, RunCancellationToken, RunId, RunRequest, RunResult,
    SessionId, UserInput,
};
use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long the event drain sleeps between polls. Cancellation and deadline
/// checks run each poll, matching the legacy interaction wait granularity.
const EVENT_POLL: Duration = Duration::from_millis(50);

/// The bound on a run's terminal event after `run.cancel` was issued. An
/// adapter that never settles has its session closed by teardown anyway; the
/// grace keeps a wedged adapter from holding the legacy run open forever.
const CANCEL_SETTLE: Duration = Duration::from_secs(10);

/// One [`AgentExecutionAdapter`] serving the integration its wrapped
/// [`RuntimeAdapter`] serves, over a shared [`AgentRuntime`].
///
/// Construct with [`Self::new`] when the serving adapter is already known, or
/// [`Self::resolve`] to look it up in the runtime's first-match registration
/// order. Every shim built over the same `Arc<AgentRuntime>` shares its
/// sessions, event log, and resume cursors.
pub struct RuntimeExecutionAdapter {
    runtime: Arc<AgentRuntime>,
    adapter: Arc<dyn RuntimeAdapter>,
    /// Per-shim unique stamp for minted command ids: the runtime dedups
    /// `command_id` store-wide, so ids carry pid plus this construction-time
    /// discriminator and a counter - a recycled pid against the same store
    /// cannot collide with a stale recorded receipt.
    stamp: u64,
    sequence: AtomicU64,
}

impl RuntimeExecutionAdapter {
    /// The shim over `adapter` on `runtime`. `adapter` must be registered
    /// with `runtime` already - `resolve` is the lookup that guarantees it.
    pub fn new(runtime: Arc<AgentRuntime>, adapter: Arc<dyn RuntimeAdapter>) -> Self {
        Self {
            runtime,
            adapter,
            stamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64,
            sequence: AtomicU64::new(0),
        }
    }

    /// The shim over the adapter `runtime` resolves for `integration` in
    /// first-match order. `None` when no registered adapter serves it.
    pub fn resolve(
        runtime: &Arc<AgentRuntime>,
        integration: &IntegrationId,
    ) -> Option<RuntimeExecutionAdapter> {
        let adapter = runtime
            .registered_adapters()
            .into_iter()
            .find(|adapter| adapter.integration() == *integration)?;
        Some(Self::new(Arc::clone(runtime), adapter))
    }

    /// The shared runtime this shim dispatches through.
    pub fn runtime(&self) -> &Arc<AgentRuntime> {
        &self.runtime
    }

    /// A unique command id inside this process and store.
    fn command_id(&self) -> CommandId {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        CommandId::new(format!(
            "exec-{}-{:x}-{sequence}",
            std::process::id(),
            self.stamp
        ))
    }

    /// Whether the serving adapter's compiled evidence declares
    /// `capability` - the same declared-capability gate the legacy CLI
    /// execution adapters enforced in `validate`.
    fn declares(&self, capability: AgentCapability) -> bool {
        self.adapter
            .agent_info()
            .capabilities
            .get(&capability)
            .is_some_and(|assessment| assessment.declared.state == CapabilityState::Supported)
    }

    /// `session.create` then `session.subscribe` on the run's consumer,
    /// returning the live runtime session id. The subscription replays the
    /// just-created session's prelude so the drain sees a complete causal
    /// stream.
    fn open_session(
        &self,
        request: &RunRequest,
        consumer: &ConsumerId,
        cwd: &Path,
    ) -> Result<SessionId, AgentRunError> {
        // The contract's `Effort` is a closed set; a request effort outside
        // it fails loudly here rather than silently dropping to the
        // provider default the way a missing flag would read.
        let effort = request
            .effort
            .as_deref()
            .map(|spelling| {
                Effort::parse(spelling).ok_or_else(|| {
                    AgentRunError::InvalidRequest(format!(
                        "effort {spelling:?} is not selectable; expected low, medium, high, or max"
                    ))
                })
            })
            .transpose()?;
        let outcome = self.runtime.dispatch(
            AgentCommand::SessionCreate {
                command_id: self.command_id(),
                cwd: cwd.to_path_buf(),
                selection: ModelSelection {
                    integration_id: request.integration.clone(),
                    model: request.model.clone().unwrap_or_default(),
                    effort,
                },
                access: request.access,
                resume_cursor: request.resume.clone(),
                external_tools: request.external_tools.clone().unwrap_or_default(),
            },
            consumer,
        );
        let session_id = match &outcome.receipt.outcome {
            ReceiptOutcome::Ok {
                session_id: Some(session_id),
                ..
            } => session_id.clone(),
            ReceiptOutcome::Ok { .. } => {
                return Err(internal_error(
                    "session.create answered ok without a session id",
                ));
            }
            ReceiptOutcome::Err { code, message } => {
                return Err(receipt_error(*code, message));
            }
        };
        let outcome = self.runtime.dispatch(
            AgentCommand::SessionSubscribe {
                command_id: self.command_id(),
                session_id: session_id.clone(),
                last_seen_seq: 0,
            },
            consumer,
        );
        receipt_ok(&outcome).map(|()| session_id)
    }

    /// `run.start` for the prompt, returning the accepted runtime run id.
    fn start_run(
        &self,
        request: &RunRequest,
        session_id: &SessionId,
        consumer: &ConsumerId,
    ) -> Result<RunId, AgentRunError> {
        let outcome = self.runtime.dispatch(
            AgentCommand::RunStart {
                command_id: self.command_id(),
                session_id: session_id.clone(),
                input: UserInput {
                    text: request.prompt.clone(),
                    attachments: Vec::new(),
                },
            },
            consumer,
        );
        match &outcome.receipt.outcome {
            ReceiptOutcome::Ok {
                run_id: Some(run_id),
                ..
            } => Ok(run_id.clone()),
            ReceiptOutcome::Ok { .. } => Err(internal_error(
                "run.start answered ok without an accepted run id",
            )),
            ReceiptOutcome::Err { code, message } => Err(receipt_error(*code, message)),
        }
    }

    /// `run.cancel` for the in-flight run. The receipt is intentionally
    /// ignored: a session that already finished answers `invalid_state`,
    /// which is indistinguishable from "the run already completed".
    fn cancel_run(&self, session_id: &SessionId, run_id: &RunId, consumer: &ConsumerId) {
        let _ = self.runtime.dispatch(
            AgentCommand::RunCancel {
                command_id: self.command_id(),
                session_id: session_id.clone(),
                run_id: run_id.clone(),
            },
            consumer,
        );
    }

    /// `session.close`, releasing the provider session the run used.
    fn close_session(&self, session_id: &SessionId, consumer: &ConsumerId) {
        let _ = self.runtime.dispatch(
            AgentCommand::SessionClose {
                command_id: self.command_id(),
                session_id: session_id.clone(),
            },
            consumer,
        );
    }

    /// The blocking `execute` body.
    fn run(
        &self,
        request: &RunRequest,
        cancellation: &RunCancellationToken,
        output_handler: Option<&dyn AgentRunOutputHandler>,
    ) -> Result<RunResult, AgentRunError> {
        self.validate(request)?;
        let run_stamp = self.sequence.fetch_add(1, Ordering::Relaxed);
        let consumer = ConsumerId::new(format!(
            "exec-consumer-{}-{:x}-{run_stamp}",
            std::process::id(),
            self.stamp
        ));
        let events = self
            .runtime
            .events(&consumer)
            .ok_or_else(|| internal_error("the run's event consumer could not be registered"))?;
        // A prompt-only run borrows a private scratch directory the way the
        // legacy execution adapters' temporary directory did.
        let cwd = match &request.working_directory {
            Some(directory) => directory.clone(),
            None => {
                let directory = std::env::temp_dir()
                    .join(format!("aifuel-run-{}-{run_stamp}", std::process::id()));
                std::fs::create_dir_all(&directory).map_err(AgentRunError::Io)?;
                directory
            }
        };
        let _scratch = ScratchDir(request.working_directory.is_none().then(|| cwd.clone()));
        let session_id = self.open_session(request, &consumer, &cwd)?;
        let _session = SessionGuard {
            adapter: self,
            consumer: &consumer,
            session_id: session_id.clone(),
        };
        let run_id = self.start_run(request, &session_id, &consumer)?;
        let drain = self.watch(
            request,
            cancellation,
            &consumer,
            &events,
            &session_id,
            &run_id,
            output_handler,
        )?;
        // The provider cursor is read while the adapter still owns the
        // session: a live adapter reports it directly, and `session.close`
        // below only ends the runtime session afterwards.
        let provider_cursor = self.runtime.resume_cursor(&session_id);
        let effective_model = drain.effective_model(request);
        let effective_effort = drain.effective_effort(request);
        let terminal = drain.terminal;
        Ok(RunResult {
            run_id: run_id.as_str().to_owned(),
            local_session_id: session_id.as_str().to_owned(),
            session_id: provider_cursor,
            resumed_from: request.resume.clone(),
            provider_id: self.provider(),
            integration_id: self.integration(),
            requested_model: request.model.clone(),
            requested_effort: request.effort.clone(),
            effective_model,
            effective_effort,
            requested_account_id: request.account.clone(),
            account_id: None,
            execution_mode: if request.working_directory.is_some() {
                ExecutionMode::Project
            } else {
                ExecutionMode::PromptOnly
            },
            permission_profile: request.access,
            status: terminal.status,
            exit_code: None,
            output: drain.output,
            error: terminal.error,
            diagnostics: (!drain.diagnostics.is_empty()).then(|| drain.diagnostics.join("\n")),
            usage: drain.usage,
            timed_out: terminal.timed_out,
            working_directory: cwd,
        })
    }
}

/// Whether the receipt is an `ok` outcome; error outcomes map to the legacy
/// error surface rather than inventing a run.
fn receipt_ok(outcome: &CommandOutcome) -> Result<(), AgentRunError> {
    match &outcome.receipt.outcome {
        ReceiptOutcome::Ok { .. } => Ok(()),
        ReceiptOutcome::Err { code, message } => Err(receipt_error(*code, message)),
    }
}

/// Map a receipt rejection onto the legacy error surface. Provider-side
/// failures keep the launcher I/O mapping the manager reports as
/// `agent_unavailable`; request-side rejections stay `invalid_request`, the
/// same closed reason a legacy `validate` refusal produced.
fn receipt_error(code: ReceiptCode, message: &str) -> AgentRunError {
    match code {
        ReceiptCode::ProviderError => AgentRunError::Io(io::Error::other(message.to_owned())),
        _ => AgentRunError::InvalidRequest(message.to_owned()),
    }
}

/// An internal contract violation: the runtime answered in a shape the
/// command's contract does not allow. It surfaces like a launcher failure
/// rather than a request error because the caller could not have caused it.
fn internal_error(message: &str) -> AgentRunError {
    AgentRunError::Io(io::Error::other(message.to_owned()))
}

impl AgentExecutionAdapter for RuntimeExecutionAdapter {
    fn integration(&self) -> IntegrationId {
        self.adapter.integration()
    }

    fn provider(&self) -> ProviderId {
        self.adapter.provider()
    }

    fn setup_guidance(&self) -> Option<AgentSetupGuidance> {
        self.adapter.agent_info().setup_guidance
    }

    /// The serving adapter's declared capabilities: `agent_info` already
    /// carries the compiled adapter's own declarations, so this reports
    /// their `declared` evidence verbatim.
    fn declared_agent_capabilities(&self) -> BTreeMap<AgentCapability, AgentCapabilityEvidence> {
        self.adapter
            .agent_info()
            .capabilities
            .iter()
            .map(|(capability, assessment)| (*capability, assessment.declared.clone()))
            .collect()
    }

    fn agent_info(&self) -> AgentIntegrationInfo {
        self.adapter.agent_info()
    }

    /// The same capability gates the compiled CLI execution adapters
    /// enforced, read from the serving adapter's declared evidence and the
    /// runtime contract's own flags. `request.output` is presentation-only
    /// at this surface: the session adapter owns the provider wire format,
    /// so no Jsonl capability check applies here.
    fn validate(&self, request: &RunRequest) -> Result<(), AgentRunError> {
        if request.integration != self.integration() {
            return Err(AgentRunError::UnsupportedIntegration(
                request.integration.clone(),
            ));
        }
        if request.prompt.trim().is_empty() {
            return Err(AgentRunError::InvalidRequest(
                "prompt must not be empty".to_owned(),
            ));
        }
        let provider = self.provider();
        // Every read-only run requires the provider to declare a verified
        // boundary, matching the compiled adapters' `supports_read_only`.
        if request.access == AccessMode::ReadOnly && !self.declares(AgentCapability::ReadOnly) {
            return Err(AgentRunError::InvalidRequest(format!(
                "{provider} cannot enforce read-only access for this request"
            )));
        }
        let capabilities = self.adapter.capabilities();
        // An empty selection declares no tools - it reads the same as a
        // missing one at every boundary, so it gates identically.
        if request
            .external_tools
            .as_ref()
            .is_some_and(|tools| !tools.is_empty())
            && !capabilities.external_tools
        {
            return Err(AgentRunError::InvalidRequest(format!(
                "{provider} cannot enforce an exact external MCP tool selection"
            )));
        }
        // The gate is the adapter's effort flag, not the spelling: request
        // resolution only reports what was asked for, and an unspellable
        // effort still fails at `session.create`, where `ModelSelection` is
        // built - the same fail point a compiled adapter's launch had.
        if request.effort.is_some() && !capabilities.effort {
            return Err(AgentRunError::InvalidRequest(format!(
                "{provider} cannot report a verified effort setting"
            )));
        }
        if request.resume.is_some() && !capabilities.resume {
            return Err(AgentRunError::InvalidRequest(format!(
                "{provider} does not support explicit session continuation"
            )));
        }
        // The runtime session contract carries no account selection, so a
        // request asking for one is refused even where a provider binary
        // could spell it: honoring it here would silently drop it.
        if request.account.is_some() {
            return Err(AgentRunError::InvalidRequest(format!(
                "{provider} does not expose provider account selection"
            )));
        }
        if request.access == AccessMode::WorkspaceWrite
            && !self.declares(AgentCapability::WorkspaceWrite)
        {
            return Err(AgentRunError::InvalidRequest(format!(
                "{provider} cannot enforce workspace-write access"
            )));
        }
        // Full access subsumes workspace-write: an adapter that cannot
        // enforce the narrower boundary cannot claim the wider one either.
        if request.access == AccessMode::Full && !self.declares(AgentCapability::WorkspaceWrite) {
            return Err(AgentRunError::InvalidRequest(format!(
                "{provider} cannot enforce full access"
            )));
        }
        Ok(())
    }

    fn execute(
        &self,
        request: &RunRequest,
        cancellation: &RunCancellationToken,
    ) -> Result<RunResult, AgentRunError> {
        self.run(request, cancellation, None)
    }

    /// Execute while forwarding assistant `message.delta` text to the owner
    /// handler - the runtime's streaming output surface feeds the legacy
    /// `AgentRunOutputHandler` contract.
    fn execute_with_output_handler(
        &self,
        request: &RunRequest,
        cancellation: &RunCancellationToken,
        output_handler: &dyn AgentRunOutputHandler,
    ) -> Result<RunResult, AgentRunError> {
        self.run(request, cancellation, Some(output_handler))
    }
}

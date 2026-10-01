//! Adapter commands and the run/session state the driver tracks.
//!
//! Commands dispatch inside the driver loop so every emission happens
//! in the run's causal order: `run.started` when a prompt is
//! dispatched, then the terminal facts land through server mail when
//! the prompt response returns. Bus-event handling lives in
//! [`super::events`].

use super::{ServerMail, authed, prompt_task};
use crate::opencode_runtime::protocol;
use crate::opencode_runtime::serve::BasicAuth;
use crate::opencode_runtime::session::{DriverCommand, OpenCodeSession, SessionSetup};
use aifuel_core::{
    AgentEventKind, AgentRuntimeError, MessageStream, ReceiptCode, RunId, SessionStatus, TokenUsage,
};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

/// What the driver tracks across the session: the in-flight run's
/// event state plus the last session status emitted (provider status
/// events repeat, and contract consumers do not need the spam).
pub(super) struct SessionFlow {
    pub run: Option<RunFlow>,
    pub(super) last_status: SessionStatus,
}

impl Default for SessionFlow {
    fn default() -> Self {
        Self {
            run: None,
            // The session prelude already announced `idle`.
            last_status: SessionStatus::Idle,
        }
    }
}

/// One in-flight run's mapping state.
pub(super) struct RunFlow {
    pub run_id: RunId,
    /// `cancel` asked the provider to abort; the terminal outcome is
    /// `cancelled` even if the prompt response reports a generic
    /// failure.
    pub cancel_requested: bool,
    /// Text or reasoning part id -> contract stream, open until the
    /// part reports `time.end` or the run ends.
    pub(super) open_streams: BTreeMap<String, MessageStream>,
    /// Text or reasoning part id -> cumulative text seen so far.
    pub(super) part_text: BTreeMap<String, String>,
    /// Tool call id -> emitted state, so repeats dedupe.
    pub(super) tools: BTreeMap<String, ToolTrack>,
    /// `step-finish` token accounting, used only when the prompt
    /// response reports none.
    pub(super) usage_fallback: Option<TokenUsage>,
}

/// One tool call's emitted lifecycle, deduplicated by call id.
pub(super) struct ToolTrack {
    pub tool: String,
    pub started: bool,
    pub done: bool,
}

impl RunFlow {
    pub(super) fn new(run_id: RunId) -> Self {
        Self {
            run_id,
            cancel_requested: false,
            open_streams: BTreeMap::new(),
            part_text: BTreeMap::new(),
            tools: BTreeMap::new(),
            usage_fallback: None,
        }
    }
}

/// One adapter command inside the driver loop. `StartPrompt` spawns the
/// blocking prompt task; permission answers and aborts are short
/// control requests awaited inline.
#[allow(clippy::too_many_arguments)]
pub(super) async fn on_command(
    session: &Arc<OpenCodeSession>,
    client: &reqwest::Client,
    base_url: &reqwest::Url,
    auth: Option<&BasicAuth>,
    setup: &SessionSetup,
    mail: UnboundedSender<ServerMail>,
    flow: &mut SessionFlow,
    provider_session: &str,
    command: DriverCommand,
) {
    match command {
        DriverCommand::StartPrompt {
            parts,
            selection,
            run_id,
            reply,
        } => {
            let model = match protocol::model_ref(&selection.model) {
                Ok(model) => model,
                Err(error) => {
                    session.clear_prompt_starting();
                    let _ = reply.send(Err(error));
                    return;
                }
            };
            let body = protocol::prompt_body(&run_id, model, setup.access, parts);
            let url = match protocol::endpoint(
                base_url,
                &format!("/session/{provider_session}/message"),
                &setup.cwd,
            ) {
                Ok(url) => url,
                Err(error) => {
                    session.clear_prompt_starting();
                    let _ = reply.send(Err(error));
                    return;
                }
            };
            tokio::spawn(prompt_task(client.clone(), url, auth.cloned(), body, mail));
            {
                let mut state = session.state.lock().expect("session state mutex");
                state.prompt_starting = false;
                state.active_run = Some(run_id.clone());
            }
            flow.run = Some(RunFlow::new(run_id.clone()));
            flow.last_status = SessionStatus::Working;
            session.emit(AgentEventKind::RunStarted {
                run_id: run_id.clone(),
                selection,
            });
            session.emit(AgentEventKind::SessionStatus {
                status: SessionStatus::Working,
            });
            let _ = reply.send(Ok(run_id));
        }
        DriverCommand::AnswerPermission {
            request_id,
            permission_id,
            response,
            decision,
            reply,
        } => {
            let url = match protocol::endpoint(
                base_url,
                &format!("/session/{provider_session}/permissions/{permission_id}"),
                &setup.cwd,
            ) {
                Ok(url) => url,
                Err(error) => {
                    let _ = reply.send(Err(error));
                    return;
                }
            };
            let result = authed(client.post(url), auth)
                .json(&serde_json::json!({"response": response}))
                .send()
                .await;
            match result {
                Ok(response) if response.status().is_success() => {
                    // The resolved fact lands inside the run's causal
                    // order. `answered_by` is a placeholder the runtime
                    // pump rewrites.
                    session.emit(AgentEventKind::ApprovalResolved {
                        request_id,
                        decision,
                        answered_by: "adapter".to_owned(),
                    });
                    emit_status(session, flow, SessionStatus::Working);
                    let _ = reply.send(Ok(()));
                }
                Ok(response) => {
                    let message = format!(
                        "the server rejected the permission answer (HTTP {})",
                        response.status()
                    );
                    emit_error(session, flow, &message);
                    let _ = reply.send(Err(AgentRuntimeError::provider_error(message)));
                }
                Err(error) => {
                    let message = format!("the permission answer request failed: {error}");
                    emit_error(session, flow, &message);
                    let _ = reply.send(Err(AgentRuntimeError::provider_error(message)));
                }
            }
        }
        DriverCommand::Abort { reply } => {
            if let Some(run) = flow.run.as_mut() {
                run.cancel_requested = true;
            }
            let url = match protocol::endpoint(
                base_url,
                &format!("/session/{provider_session}/abort"),
                &setup.cwd,
            ) {
                Ok(url) => url,
                Err(error) => {
                    let _ = reply.send(Err(error));
                    return;
                }
            };
            match authed(client.post(url), auth).send().await {
                Ok(response) if response.status().is_success() => {
                    let _ = reply.send(Ok(()));
                }
                Ok(response) => {
                    let _ = reply.send(Err(AgentRuntimeError::provider_error(format!(
                        "the server rejected the abort request (HTTP {})",
                        response.status()
                    ))));
                }
                Err(error) => {
                    let _ = reply.send(Err(AgentRuntimeError::provider_error(format!(
                        "the abort request failed: {error}"
                    ))));
                }
            }
        }
        // `Shutdown` is handled by the driver loop itself.
        DriverCommand::Shutdown => {}
    }
}

/// Emit a session status that differs from the last one emitted.
pub(super) fn emit_status(
    session: &Arc<OpenCodeSession>,
    flow: &mut SessionFlow,
    status: SessionStatus,
) {
    if flow.last_status == status {
        return;
    }
    flow.last_status = status;
    session.emit(AgentEventKind::SessionStatus { status });
}

/// A provider failure fact scoped to the active run when one exists.
pub(super) fn emit_error(session: &Arc<OpenCodeSession>, flow: &SessionFlow, message: &str) {
    session.emit(AgentEventKind::Error {
        run_id: flow.run.as_ref().map(|run| run.run_id.clone()),
        code: ReceiptCode::ProviderError,
        message: message.to_owned(),
        retryable: false,
    });
}

/// A transient provider failure the server is already retrying.
pub(super) fn emit_error_retryable(
    session: &Arc<OpenCodeSession>,
    flow: &SessionFlow,
    message: &str,
) {
    session.emit(AgentEventKind::Error {
        run_id: flow.run.as_ref().map(|run| run.run_id.clone()),
        code: ReceiptCode::ProviderError,
        message: format!("the provider is retrying: {message}"),
        retryable: true,
    });
}

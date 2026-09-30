//! The per-session protocol driver.
//!
//! One plain thread runs one `current_thread` tokio runtime per session.
//! A spawned reader task owns agent stdout and pushes decoded frames over
//! a channel; the driver loop `select!`s between adapter commands and
//! server messages, so a command can never tear a half-read frame. Every
//! run-scoped emission happens here, inside the run's causal order.

mod dispatch;
mod handshake;
mod teardown;

use super::mapping::TurnEvents;
use super::protocol::{read_message, send};
use super::session::{AcpSession, DriverCommand, SessionSetup, SetupReport, Transport};
use crate::agent_execution::MAX_CAPTURE_BYTES;
use crate::agent_execution::read_bounded;
use aifuel_core::{
    AgentEventKind, AgentRuntimeError, ApprovalDecision, RunCancellationToken, RunId, SessionStatus,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// The setup handshake deadline, matching the sibling run paths.
const SETUP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long teardown waits on stderr capture after the process dies.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
/// JSON-RPC method-not-found, for server requests this adapter does not
/// support. They are answered, never silently approved.
const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC internal error, for client-side method failures.
const INTERNAL_ERROR: i64 = -32603;
/// JSON-RPC server-defined error, for policy refusals this client
/// enforces (read-only writes, out-of-workspace paths).
const POLICY_DENIED: i64 = -32000;

/// One decoded frame from the agent, or the terminal fact of its
/// stream ending.
enum ServerMail {
    Message(Value),
    /// Output EOF or reader shutdown.
    Closed,
    /// A read or protocol failure; the session cannot continue.
    Failed(String),
}

/// A client request the driver is still waiting on.
enum PendingRpc {
    /// `session/prompt`: the response's `stopReason` is the run's
    /// terminal outcome.
    Prompt { run_id: RunId },
    /// `session/set_config_option`: the reply completes `model.select`.
    SetConfig {
        reply: mpsc::Sender<Result<(), AgentRuntimeError>>,
    },
}

/// How the driver loop ended, deciding the terminal facts teardown emits.
enum Exit {
    /// `stop` requested the shutdown.
    Requested,
    /// The adapter dropped the session's command sender.
    Dropped,
    /// The agent closed its output.
    ServerClosed,
    /// Reading or decoding agent output failed.
    ServerFailed(String),
    /// The setup handshake failed before the session was announced.
    SetupFailed,
}

/// The session driver entry point, run on a dedicated
/// `current_thread` runtime by [`AcpSession::start_driver`].
pub(super) async fn run(
    session: Arc<AcpSession>,
    connector: super::session::Connector,
    setup: SessionSetup,
    setup_tx: mpsc::Sender<SetupReport>,
    mut commands: UnboundedReceiver<DriverCommand>,
) {
    let transport = match connector(&setup) {
        Ok(transport) => transport,
        Err(error) => {
            let _ = setup_tx.send(Err(error.message));
            return;
        }
    };
    let Transport {
        mut stdin,
        stdout,
        stderr,
        child,
    } = transport;
    *session.child.lock().expect("child mutex") = child;
    let (mail, mut messages) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(read_loop(stdout, mail, session.reader_cancel.clone()));
    let stderr_task = stderr.map(|stderr| {
        tokio::spawn(async move {
            read_bounded(stderr, MAX_CAPTURE_BYTES)
                .await
                .map(|captured| captured.text)
        })
    });

    let exit = match handshake::handshake(&session, &mut stdin, &mut messages, &setup).await {
        Ok(opened) => {
            let _ = setup_tx.send(Ok(opened));
            main_loop(&session, &mut stdin, &mut messages, &mut commands).await
        }
        Err(reason) => {
            let _ = setup_tx.send(Err(reason));
            Exit::SetupFailed
        }
    };
    teardown::teardown(&session, stderr_task, exit).await;
}

/// Agent stdout reader: bounded framing per message, channel delivery
/// to the driver loop. The read poll checks cancellation every 100ms,
/// so `reader_cancel` unblocks it promptly on shutdown.
async fn read_loop(
    mut stdout: BufReader<Box<dyn AsyncRead + Unpin + Send>>,
    mail: UnboundedSender<ServerMail>,
    cancellation: RunCancellationToken,
) {
    loop {
        let message = match read_message(&mut stdout, None, &cancellation).await {
            Ok(Some(message)) => ServerMail::Message(message),
            Ok(None) | Err(aifuel_core::AgentRunError::Cancelled) => ServerMail::Closed,
            Err(error) => ServerMail::Failed(error.to_string()),
        };
        let terminal = !matches!(message, ServerMail::Message(_));
        if mail.send(message).is_err() || terminal {
            return;
        }
    }
}

/// Answer one server request with a protocol error: it is rejected,
/// never silently approved, and the agent stops waiting on it.
pub(super) async fn reject_server_request(
    stdin: &mut Box<dyn AsyncWrite + Unpin + Send>,
    message: &Value,
    code: i64,
    reason: &str,
    deadline: Option<std::time::Instant>,
) {
    let Some(id) = message.get("id").cloned() else {
        return;
    };
    let reply = json!({
        "id": id,
        "error": {
            "code": code,
            "message": reason,
        }
    });
    let _ = send(stdin, reply, deadline).await;
}

/// The steady-state loop: adapter commands and server messages on one
/// select, commands first so host answers land before more notifications
/// are consumed.
async fn main_loop(
    session: &Arc<AcpSession>,
    stdin: &mut Box<dyn AsyncWrite + Unpin + Send>,
    messages: &mut UnboundedReceiver<ServerMail>,
    commands: &mut UnboundedReceiver<DriverCommand>,
) -> Exit {
    let mut next_rpc: u64 = 100;
    let mut pending_rpc: BTreeMap<u64, PendingRpc> = BTreeMap::new();
    let mut turn: Option<TurnEvents> = None;
    loop {
        tokio::select! {
            biased;
            command = commands.recv() => match command {
                None => break Exit::Dropped,
                Some(DriverCommand::Shutdown) => break Exit::Requested,
                Some(DriverCommand::Prompt { run_id, blocks, selection, reply }) => {
                    let id = next_rpc;
                    next_rpc += 1;
                    let session_id = session
                        .state
                        .lock()
                        .expect("session state mutex")
                        .provider_session
                        .clone()
                        .unwrap_or_default();
                    let message = json!({
                        "id": id,
                        "method": "session/prompt",
                        "params": {"sessionId": session_id, "prompt": blocks},
                    });
                    match send(stdin, message, None).await {
                        Ok(()) => {
                            pending_rpc.insert(id, PendingRpc::Prompt {
                                run_id: run_id.clone(),
                            });
                            {
                                let mut state =
                                    session.state.lock().expect("session state mutex");
                                state.prompt_starting = false;
                                state.active_run = Some(run_id.clone());
                            }
                            turn = Some(TurnEvents::new(run_id.clone()));
                            session.emit(AgentEventKind::RunStarted { run_id, selection });
                            session.emit(AgentEventKind::SessionStatus {
                                status: SessionStatus::Working,
                            });
                            if reply.send(Ok(())).is_err() {
                                // `send` gave up waiting: the run was
                                // announced, so retract the live turn
                                // with `session/cancel` rather than
                                // leave an orphaned provider run.
                                let message = json!({
                                    "method": "session/cancel",
                                    "params": {"sessionId": session_id},
                                });
                                let _ = send(stdin, message, None).await;
                            }
                        }
                        Err(error) => {
                            session.clear_prompt_starting();
                            let _ = reply.send(Err(AgentRuntimeError::provider_error(
                                error.to_string(),
                            )));
                        }
                    }
                }
                Some(DriverCommand::Answer { request_id, decision, message, reply }) => {
                    match send(stdin, message, None).await {
                        Ok(()) => {
                            // The resolved fact lands inside the run's
                            // causal order, emitted by the driver after
                            // the answer reached the wire. `answered_by`
                            // is a placeholder the runtime pump rewrites.
                            session.emit(AgentEventKind::ApprovalResolved {
                                request_id,
                                decision,
                                answered_by: "adapter".to_owned(),
                            });
                            session.emit(AgentEventKind::SessionStatus {
                                status: SessionStatus::Working,
                            });
                            let _ = reply.send(Ok(()));
                        }
                        Err(error) => {
                            session.emit(AgentEventKind::Error {
                                run_id: turn.as_ref().map(TurnEvents::run_id).cloned(),
                                code: aifuel_core::ReceiptCode::ProviderError,
                                message: error.to_string(),
                                retryable: false,
                            });
                            let _ = reply.send(Err(AgentRuntimeError::provider_error(
                                error.to_string(),
                            )));
                        }
                    }
                }
                Some(DriverCommand::CancelTurn { reply }) => {
                    let session_id = session
                        .state
                        .lock()
                        .expect("session state mutex")
                        .provider_session
                        .clone()
                        .unwrap_or_default();
                    let message = json!({
                        "method": "session/cancel",
                        "params": {"sessionId": session_id},
                    });
                    match send(stdin, message, None).await {
                        Ok(()) => {
                            // The spec requires answering every pending
                            // `session/request_permission` with the
                            // `cancelled` outcome when a turn is
                            // cancelled.
                            cancel_pending_permissions(session, stdin).await;
                            let _ = reply.send(Ok(()));
                        }
                        Err(error) => {
                            let _ = reply.send(Err(AgentRuntimeError::provider_error(
                                error.to_string(),
                            )));
                        }
                    }
                }
                Some(DriverCommand::SetModel { model, reply }) => {
                    let (config_id, session_id) = {
                        let state = session.state.lock().expect("session state mutex");
                        (
                            state.model_option.as_ref().map(|option| option.id.clone()),
                            state.provider_session.clone().unwrap_or_default(),
                        )
                    };
                    let Some(config_id) = config_id else {
                        let _ = reply.send(Err(AgentRuntimeError::unsupported(
                            "the agent advertised no model configuration option",
                        )));
                        continue;
                    };
                    let id = next_rpc;
                    next_rpc += 1;
                    let message = json!({
                        "id": id,
                        "method": "session/set_config_option",
                        "params": {
                            "sessionId": session_id,
                            "configId": config_id,
                            "value": model,
                        },
                    });
                    match send(stdin, message, None).await {
                        Ok(()) => {
                            pending_rpc.insert(id, PendingRpc::SetConfig { reply });
                        }
                        Err(error) => {
                            let _ = reply.send(Err(AgentRuntimeError::provider_error(
                                error.to_string(),
                            )));
                        }
                    }
                }
            },
            mail = messages.recv() => match mail {
                None | Some(ServerMail::Closed) => break Exit::ServerClosed,
                Some(ServerMail::Failed(reason)) => break Exit::ServerFailed(reason),
                Some(ServerMail::Message(message)) => {
                    dispatch::handle_message(
                        session,
                        stdin,
                        &message,
                        &mut pending_rpc,
                        &mut turn,
                    )
                    .await;
                }
            },
        }
    }
}

/// Answer every parked `session/request_permission` with the
/// `cancelled` outcome, as the protocol requires after
/// `session/cancel`. Each request also emits its resolved fact so hosts
/// holding it see the same answer the agent received.
async fn cancel_pending_permissions(
    session: &Arc<AcpSession>,
    stdin: &mut Box<dyn AsyncWrite + Unpin + Send>,
) {
    let pending = {
        let mut state = session.state.lock().expect("session state mutex");
        std::mem::take(&mut state.pending)
    };
    for (request_id, entry) in pending {
        let reply = json!({
            "id": entry.server_id,
            "result": {"outcome": {"outcome": "cancelled"}},
        });
        let _ = send(stdin, reply, None).await;
        session.emit(AgentEventKind::ApprovalResolved {
            request_id,
            decision: ApprovalDecision::OptionId("cancel".to_owned()),
            answered_by: "adapter".to_owned(),
        });
    }
}

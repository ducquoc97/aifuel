//! The per-session protocol driver.
//!
//! One plain thread runs one `current_thread` tokio runtime per session.
//! A spawned reader task owns app-server stdout and pushes decoded
//! messages over a channel; the driver loop `select!`s between adapter
//! commands and server messages, so a command can never tear a
//! half-read frame. Every run-scoped emission happens here, inside the
//! run's causal order.

mod dispatch;
mod handshake;
mod teardown;

use super::mapping::TurnEvents;
use super::session::{CodexSession, DriverCommand, SessionSetup, SetupReport, Transport};
use crate::agent_execution::MAX_CAPTURE_BYTES;
use crate::agent_execution::read_bounded;
use crate::codex::app_server::{protocol_read_message as read_message, protocol_send as send};
use aifuel_core::{
    AgentEventKind, AgentRuntimeError, ModelSelection, RunCancellationToken, RunId, SessionStatus,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// The setup handshake deadline, matching the app-server run path.
const SETUP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long teardown waits on stderr capture after the process dies.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
/// JSON-RPC method-not-found, for server requests this adapter does not
/// support. They are answered, never silently approved.
const METHOD_NOT_FOUND: i64 = -32601;

/// One decoded message from the app-server, or the terminal fact of its
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
    /// `turn/start`: the reply completes `send` with the provider turn id.
    TurnStart {
        reply: mpsc::Sender<Result<RunId, AgentRuntimeError>>,
        selection: ModelSelection,
    },
    /// `turn/interrupt`: the request reached the wire already; a
    /// rejected reply still surfaces as an `error` event.
    Interrupt { run_id: RunId },
}

/// How the driver loop ended, deciding the terminal facts teardown emits.
enum Exit {
    /// `stop` requested the shutdown.
    Requested,
    /// The adapter dropped the session's command sender.
    Dropped,
    /// The app-server closed its output.
    ServerClosed,
    /// Reading or decoding app-server output failed.
    ServerFailed(String),
    /// The setup handshake failed before the session was announced.
    SetupFailed,
}

/// The session driver entry point, run on a dedicated
/// `current_thread` runtime by [`CodexSession::start_driver`].
pub(super) async fn run(
    session: Arc<CodexSession>,
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

    let exit = match handshake::handshake(&mut stdin, &mut messages, &setup).await {
        Ok(thread_id) => {
            let _ = setup_tx.send(Ok(thread_id.clone()));
            main_loop(
                &session,
                &mut stdin,
                &mut messages,
                &mut commands,
                &setup,
                thread_id,
            )
            .await
        }
        Err(reason) => {
            let _ = setup_tx.send(Err(reason));
            Exit::SetupFailed
        }
    };
    teardown::teardown(&session, stderr_task, exit).await;
}

/// App-server stdout reader: bounded framing per message, channel
/// delivery to the driver loop. The read poll checks cancellation every
/// 100ms, so `reader_cancel` unblocks it promptly on shutdown.
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
/// never silently approved, and the server stops waiting on it.
pub(super) async fn reject_server_request(
    stdin: &mut Box<dyn AsyncWrite + Unpin + Send>,
    message: &Value,
    deadline: Option<std::time::Instant>,
) {
    let Some(id) = message.get("id").cloned() else {
        return;
    };
    let reply = json!({
        "id": id,
        "error": {
            "code": METHOD_NOT_FOUND,
            "message": "the request method is not supported by this client",
        }
    });
    let _ = send(stdin, reply, deadline).await;
}

/// The steady-state loop: adapter commands and server messages on one
/// select, commands first so host answers land before more notifications
/// are consumed.
async fn main_loop(
    session: &Arc<CodexSession>,
    stdin: &mut Box<dyn AsyncWrite + Unpin + Send>,
    messages: &mut UnboundedReceiver<ServerMail>,
    commands: &mut UnboundedReceiver<DriverCommand>,
    setup: &SessionSetup,
    thread_id: String,
) -> Exit {
    let mut next_rpc: u64 = 2;
    let mut pending_rpc: BTreeMap<u64, PendingRpc> = BTreeMap::new();
    let mut turn: Option<TurnEvents> = None;
    // Turn ids whose `turn/start` reply could not reach `send` (the
    // caller timed out): the provider still runs them, so the driver
    // asks for interruption and drops their notifications.
    let mut orphans: BTreeSet<String> = BTreeSet::<String>::new();
    loop {
        tokio::select! {
            biased;
            command = commands.recv() => match command {
                None => break Exit::Dropped,
                Some(DriverCommand::Shutdown) => break Exit::Requested,
                Some(DriverCommand::StartTurn { items, selection, reply }) => {
                    let id = next_rpc;
                    next_rpc += 1;
                    let message = dispatch::turn_start_message(id, &thread_id, setup, &selection, items);
                    match send(stdin, message, None).await {
                        Ok(()) => {
                            pending_rpc.insert(id, PendingRpc::TurnStart { reply, selection });
                        }
                        Err(error) => {
                            session.clear_turn_starting();
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
                Some(DriverCommand::Interrupt { turn_id, reply }) => {
                    let id = next_rpc;
                    next_rpc += 1;
                    let message = json!({
                        "id": id,
                        "method": "turn/interrupt",
                        "params": {"threadId": thread_id, "turnId": turn_id},
                    });
                    match send(stdin, message, None).await {
                        Ok(()) => {
                            pending_rpc.insert(
                                id,
                                PendingRpc::Interrupt {
                                    run_id: RunId::new(turn_id),
                                },
                            );
                            let _ = reply.send(Ok(()));
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
                        &mut next_rpc,
                        &mut pending_rpc,
                        &mut turn,
                        &mut orphans,
                        &thread_id,
                    )
                    .await;
                }
            },
        }
    }
}

//! Steady-state message handling: `session/update` notifications map
//! to runtime events, `session/request_permission` becomes an Approval
//! Request, the `fs/*` file services run against the session workspace
//! policy, and responses resolve the matching client request.

use super::{
    INTERNAL_ERROR, METHOD_NOT_FOUND, POLICY_DENIED, PendingRpc, cancel_pending_permissions,
    reject_server_request,
};
use crate::acp_runtime::interactions;
use crate::acp_runtime::mapping::{self, TurnEvents};
use crate::acp_runtime::protocol::send;
use crate::acp_runtime::session::{AcpSession, PendingApproval};
use crate::agent_execution::MAX_CAPTURE_BYTES;
use aifuel_core::{AccessMode, AgentEventKind, ReceiptCode, RunOutcome, SessionStatus};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tokio::io::AsyncWrite;

/// How long a `fs/*` response may take to assemble; the agent is
/// blocked on the answer, so the work stays bounded.
const FS_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// Route one decoded frame. Responses resolve `pending_rpc`;
/// notifications map to runtime events; server requests are answered or
/// explicitly rejected.
pub(super) async fn handle_message(
    session: &Arc<AcpSession>,
    stdin: &mut Box<dyn AsyncWrite + Unpin + Send>,
    message: &Value,
    pending_rpc: &mut BTreeMap<u64, PendingRpc>,
    turn: &mut Option<TurnEvents>,
) {
    if let Some(id) = mapping::response_id(message) {
        handle_response(session, stdin, id, message, pending_rpc, turn).await;
        return;
    }
    match mapping::method(message) {
        Some("session/update") => {
            // Run-scoped updates belong to the active prompt; a stray
            // update outside one is protocol noise.
            let Some(active) = turn.as_mut() else {
                return;
            };
            if let Some(update) = message
                .get("params")
                .and_then(|params| params.get("update"))
            {
                for event in active.apply_update(update, session) {
                    session.emit(event);
                }
            }
        }
        Some("session/request_permission") => {
            handle_permission(session, stdin, message, turn).await;
        }
        Some("fs/read_text_file") => {
            handle_fs_read(session, stdin, message).await;
        }
        Some("fs/write_text_file") => {
            handle_fs_write(session, stdin, message).await;
        }
        Some(method) if mapping::is_server_request(message) => {
            reject_server_request(
                stdin,
                message,
                METHOD_NOT_FOUND,
                &format!("this client does not implement {method}"),
                None,
            )
            .await;
        }
        // Unknown notifications and unpaired responses are ignored.
        _ => {}
    }
}

/// Match a response frame to the client request that opened it. The
/// `session/prompt` response is the run's terminal fact.
async fn handle_response(
    session: &Arc<AcpSession>,
    stdin: &mut Box<dyn AsyncWrite + Unpin + Send>,
    id: u64,
    message: &Value,
    pending_rpc: &mut BTreeMap<u64, PendingRpc>,
    turn: &mut Option<TurnEvents>,
) {
    let Some(pending) = pending_rpc.remove(&id) else {
        return;
    };
    let result = mapping::response_result(message);
    match pending {
        PendingRpc::Prompt { run_id } => {
            let (outcome, error) = match &result {
                Ok(result) => {
                    let stop_reason = result
                        .get("stopReason")
                        .and_then(Value::as_str)
                        .unwrap_or("end_turn")
                        .to_owned();
                    (
                        mapping::prompt_outcome(&stop_reason),
                        mapping::prompt_error(&stop_reason),
                    )
                }
                Err(error) => (RunOutcome::Failed, Some(error.clone())),
            };
            // A ended turn retracts its permission requests; answer the
            // parked ones `cancelled` so the agent cannot hang.
            cancel_pending_permissions(session, stdin).await;
            let finished = turn
                .take()
                .unwrap_or_else(|| TurnEvents::new(run_id.clone()));
            if let Some(message) = error {
                session.emit(AgentEventKind::Error {
                    run_id: Some(run_id),
                    code: ReceiptCode::ProviderError,
                    message,
                    retryable: false,
                });
            }
            for event in finished.finish_events(outcome) {
                session.emit(event);
            }
            {
                let mut state = session.state.lock().expect("session state mutex");
                state.active_run = None;
            }
            if !session.state.lock().expect("session state mutex").closed {
                session.emit(AgentEventKind::SessionStatus {
                    status: SessionStatus::Idle,
                });
            }
        }
        PendingRpc::SetConfig { reply } => match result {
            Ok(result) => {
                if let Some(options) = result.get("configOptions")
                    && let Some(option) = interactions::model_option(options)
                {
                    session
                        .state
                        .lock()
                        .expect("session state mutex")
                        .model_option = Some(option);
                }
                let _ = reply.send(Ok(()));
            }
            Err(error) => {
                let _ = reply.send(Err(aifuel_core::AgentRuntimeError::provider_error(error)));
            }
        },
    }
}

/// `session/request_permission`: normalize the offered options through
/// the shared approval policy, emit `approval.requested`, and park the
/// JSON-RPC id until a host decision arrives. The request is never
/// answered implicitly, and outside a prompt turn it is rejected: the
/// protocol only raises it while a prompt runs.
async fn handle_permission(
    session: &Arc<AcpSession>,
    stdin: &mut Box<dyn AsyncWrite + Unpin + Send>,
    message: &Value,
    turn: &mut Option<TurnEvents>,
) {
    let Some(id) = message.get("id").cloned() else {
        return;
    };
    let Some(active) = turn.as_ref() else {
        reject_server_request(
            stdin,
            message,
            METHOD_NOT_FOUND,
            "session/request_permission arrived outside a prompt turn",
            None,
        )
        .await;
        return;
    };
    let run_id = active.run_id().clone();
    let (options, request) = interactions::permission_request(message, session.access);
    let request_id = session.next_request_id();
    let offered: Vec<String> = request
        .options
        .iter()
        .map(|option| option.id.clone())
        .collect();
    {
        let mut state = session.state.lock().expect("session state mutex");
        state.pending.insert(
            request_id.clone(),
            PendingApproval {
                server_id: id,
                options,
                offered,
            },
        );
    }
    session.emit(AgentEventKind::SessionStatus {
        status: SessionStatus::WaitingApproval,
    });
    session.emit(AgentEventKind::ApprovalRequested {
        request_id,
        run_id,
        request,
    });
}

/// `fs/read_text_file`: serve the file when it resolves inside the
/// session workspace; refuse otherwise.
async fn handle_fs_read(
    session: &Arc<AcpSession>,
    stdin: &mut Box<dyn AsyncWrite + Unpin + Send>,
    message: &Value,
) {
    let deadline = Some(Instant::now() + FS_DEADLINE);
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let Some(path) = params.get("path").and_then(Value::as_str) else {
        reject_server_request(
            stdin,
            message,
            INTERNAL_ERROR,
            "fs/read_text_file: missing path",
            deadline,
        )
        .await;
        return;
    };
    let Some(path) = resolve_within(&session.cwd, Path::new(path)) else {
        reject_server_request(
            stdin,
            message,
            POLICY_DENIED,
            "fs/read_text_file: the path is outside the session workspace",
            deadline,
        )
        .await;
        return;
    };
    let line = params.get("line").and_then(Value::as_u64);
    let limit = params.get("limit").and_then(Value::as_u64);
    // The workspace's tokio build carries no `fs` feature; the read is
    // bounded to the frame limit and the driver serves one session, so
    // a synchronous read is the honest size here.
    let captured = match std::fs::metadata(&path).and_then(|meta| {
        if meta.len() > MAX_CAPTURE_BYTES as u64 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "the file exceeds the protocol frame limit",
            ));
        }
        std::fs::read_to_string(&path)
    }) {
        Ok(captured) => captured,
        Err(error) => {
            reject_server_request(
                stdin,
                message,
                INTERNAL_ERROR,
                &format!("fs/read_text_file: {error}"),
                deadline,
            )
            .await;
            return;
        }
    };
    // `line` is the 1-based start; `limit` bounds the returned lines.
    let content = match (line, limit) {
        (None, None) => captured,
        (line, limit) => {
            let skip = line.unwrap_or(1).saturating_sub(1) as usize;
            let lines: Vec<&str> = captured.lines().skip(skip).collect();
            match limit {
                Some(limit) => lines
                    .into_iter()
                    .take(limit as usize)
                    .collect::<Vec<_>>()
                    .join("\n"),
                None => lines.join("\n"),
            }
        }
    };
    let reply = json!({
        "id": message.get("id").cloned().unwrap_or(Value::Null),
        "result": {"content": content},
    });
    let _ = send(stdin, reply, deadline).await;
}

/// `fs/write_text_file`: only `read_write` sessions may write, and only
/// inside the session workspace.
async fn handle_fs_write(
    session: &Arc<AcpSession>,
    stdin: &mut Box<dyn AsyncWrite + Unpin + Send>,
    message: &Value,
) {
    let deadline = Some(Instant::now() + FS_DEADLINE);
    if session.access == AccessMode::ReadOnly {
        reject_server_request(
            stdin,
            message,
            POLICY_DENIED,
            "fs/write_text_file: the session is read-only",
            deadline,
        )
        .await;
        return;
    }
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let (Some(path), Some(content)) = (
        params.get("path").and_then(Value::as_str),
        params.get("content").and_then(Value::as_str),
    ) else {
        reject_server_request(
            stdin,
            message,
            INTERNAL_ERROR,
            "fs/write_text_file: missing path or content",
            deadline,
        )
        .await;
        return;
    };
    let Some(path) = resolve_within(&session.cwd, Path::new(path)) else {
        reject_server_request(
            stdin,
            message,
            POLICY_DENIED,
            "fs/write_text_file: the path is outside the session workspace",
            deadline,
        )
        .await;
        return;
    };
    match std::fs::write(&path, content) {
        Ok(()) => {
            let reply = json!({
                "id": message.get("id").cloned().unwrap_or(Value::Null),
                "result": Value::Null,
            });
            let _ = send(stdin, reply, deadline).await;
        }
        Err(error) => {
            reject_server_request(
                stdin,
                message,
                INTERNAL_ERROR,
                &format!("fs/write_text_file: {error}"),
                deadline,
            )
            .await;
        }
    }
}

/// Resolve `path` against the workspace root and require the result to
/// stay inside it. Relative paths resolve under `cwd`; the canonical
/// form must still be contained, so `..` and symlink escapes are
/// refused. For writes to files that do not exist yet, the deepest
/// existing ancestor is canonicalized instead.
fn resolve_within(cwd: &Path, path: &Path) -> Option<PathBuf> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let base = cwd.canonicalize().ok()?;
    let resolved = match joined.canonicalize() {
        Ok(resolved) => resolved,
        Err(_) => {
            // The target does not exist: anchor the nearest existing
            // ancestor and re-join the remainder.
            let mut missing: Vec<std::ffi::OsString> = Vec::new();
            let mut cursor: &Path = joined.as_path();
            loop {
                match cursor.canonicalize() {
                    Ok(existing) => {
                        let mut resolved = existing;
                        for component in missing.iter().rev() {
                            resolved.push(component);
                        }
                        break resolved;
                    }
                    Err(_) => {
                        missing.push(cursor.file_name()?.to_os_string());
                        cursor = cursor.parent()?;
                    }
                }
            }
        }
    };
    resolved.starts_with(&base).then_some(resolved)
}

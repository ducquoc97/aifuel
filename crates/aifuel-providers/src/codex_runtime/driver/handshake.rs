//! The setup handshake: `initialize`, `initialized`, then
//! `thread/start` or `thread/resume`, matching the existing app-server
//! client. It owns no session state; the caller learns the Codex thread
//! id or the failure reason through its return.

use super::{SETUP_TIMEOUT, ServerMail, reject_server_request};
use crate::codex::app_server::protocol_send as send;
use crate::codex_runtime::session::SessionSetup;
use crate::codex_runtime::{APPROVAL_POLICY, thread_sandbox};
use serde_json::{Value, json};
use std::time::Instant;
use tokio::io::AsyncWrite;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::timeout;

/// Returns the Codex thread id the app-server created or resumed.
pub(super) async fn handshake(
    stdin: &mut Box<dyn AsyncWrite + Unpin + Send>,
    messages: &mut UnboundedReceiver<ServerMail>,
    setup: &SessionSetup,
) -> Result<String, String> {
    let deadline = Instant::now() + SETUP_TIMEOUT;
    send(
        stdin,
        json!({
            "id": 0,
            "method": "initialize",
            "params": {
                "clientInfo": {"name": "aifuel_agent_runtime", "version": env!("CARGO_PKG_VERSION")},
                "capabilities": {"experimentalApi": true},
            }
        }),
        Some(deadline),
    )
    .await
    .map_err(|error| error.to_string())?;
    wait_response(stdin, messages, 0, deadline).await?;
    send(
        stdin,
        json!({"method": "initialized", "params": {}}),
        Some(deadline),
    )
    .await
    .map_err(|error| error.to_string())?;
    let model = setup
        .model
        .as_ref()
        .map_or(Value::Null, |model| json!(model));
    let sandbox = thread_sandbox(setup.access);
    let request = match &setup.resume_cursor {
        Some(thread_id) => json!({
            "method": "thread/resume",
            "params": {
                "threadId": thread_id,
                "cwd": setup.cwd,
                "model": model,
                "approvalPolicy": APPROVAL_POLICY,
                "sandbox": sandbox,
                "config": {"mcp_servers": {}},
                "excludeTurns": true,
            }
        }),
        None => json!({
            "method": "thread/start",
            "params": {
                "cwd": setup.cwd,
                "model": model,
                "ephemeral": false,
                "approvalPolicy": APPROVAL_POLICY,
                "sandbox": sandbox,
                "config": {"mcp_servers": {}},
            }
        }),
    };
    send(
        stdin,
        json!({"id": 1, "method": request["method"], "params": request["params"]}),
        Some(deadline),
    )
    .await
    .map_err(|error| error.to_string())?;
    let response = wait_response(stdin, messages, 1, deadline).await?;
    response["result"]["thread"]["id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "the app-server did not report a thread id".to_owned())
}

/// Read until the response for request `id` arrives or the deadline
/// passes. Server-initiated requests received during setup are answered
/// with method-not-found so the server never blocks on them.
async fn wait_response(
    stdin: &mut Box<dyn AsyncWrite + Unpin + Send>,
    messages: &mut UnboundedReceiver<ServerMail>,
    id: u64,
    deadline: Instant,
) -> Result<Value, String> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("the app-server setup handshake timed out".to_owned());
        }
        match timeout(remaining, messages.recv()).await {
            Err(_) => return Err("the app-server setup handshake timed out".to_owned()),
            Ok(Some(ServerMail::Message(message))) => {
                if message.get("id").and_then(Value::as_u64) == Some(id) {
                    if let Some(error) = message.get("error").filter(|error| !error.is_null()) {
                        return Err(format!(
                            "the app-server rejected a setup request: {}",
                            error["message"].as_str().unwrap_or("unknown error")
                        ));
                    }
                    return Ok(message);
                }
                reject_server_request(stdin, &message, Some(deadline)).await;
            }
            Ok(Some(ServerMail::Closed)) => {
                return Err("the app-server closed during setup".to_owned());
            }
            Ok(Some(ServerMail::Failed(reason))) => return Err(reason),
            Ok(None) => return Err("the app-server reader ended during setup".to_owned()),
        }
    }
}

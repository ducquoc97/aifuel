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
    // A selected tool set spawns the filtered AI Fuel Gateway MCP server
    // through the thread config; empty selections register no server.
    let config = crate::codex::app_server::mcp_server_config(&setup.external_tools)
        .map_err(|error| error.to_string())?;
    let request = match &setup.resume_cursor {
        Some(thread_id) => json!({
            "method": "thread/resume",
            "params": {
                "threadId": thread_id,
                "cwd": setup.cwd,
                "model": model,
                "approvalPolicy": APPROVAL_POLICY,
                "sandbox": sandbox,
                "config": config,
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
                "config": config,
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
    let thread_id = response["result"]["thread"]["id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "the app-server did not report a thread id".to_owned())?;
    if !setup.external_tools.is_empty() {
        wait_for_mcp_tools(stdin, messages, &thread_id, &setup.external_tools, deadline).await?;
    }
    Ok(thread_id)
}

/// Poll `mcpServerStatus/list` until the managed Gateway server reports
/// exactly the selected tools, the same readiness gate the one-shot run
/// path applies. A session that would run without its declared external
/// tools fails the handshake instead of silently starting without them.
async fn wait_for_mcp_tools(
    stdin: &mut Box<dyn AsyncWrite + Unpin + Send>,
    messages: &mut UnboundedReceiver<ServerMail>,
    thread_id: &str,
    expected_tools: &[String],
    deadline: Instant,
) -> Result<(), String> {
    // Ids 0 and 1 ran the handshake; 2 belongs to the first post-setup
    // request, matching the run path's numbering.
    let mut request_id = 3_u64;
    loop {
        if Instant::now() >= deadline {
            return Err(
                "selected Codex MCP tools did not become ready within the 10-second setup limit"
                    .to_owned(),
            );
        }
        send(
            stdin,
            json!({
                "id": request_id,
                "method": "mcpServerStatus/list",
                "params": {"threadId": thread_id, "detail": "full", "limit": 100}
            }),
            Some(deadline),
        )
        .await
        .map_err(|error| error.to_string())?;
        let response = wait_response(stdin, messages, request_id, deadline).await?;
        request_id = request_id.saturating_add(1);
        match crate::codex::app_server::mcp_tools_are_ready(&response, expected_tools) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(error) => return Err(error.to_string()),
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
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

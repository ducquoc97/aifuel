//! Codex App Server MCP configuration and readiness checks.

use super::protocol::{expect_successful_response, protocol_error, send};
use aifuel_core::{
    AIFUEL_GATEWAY_REGISTRATION_NAME, AgentRunError, RunCancellationToken, RunRequest,
};
use serde_json::{Value, json};
use std::time::Instant;
use tokio::io::{AsyncRead, AsyncWrite, BufReader};

pub(super) fn app_server_config(request: &RunRequest) -> Result<Value, AgentRunError> {
    let mut mcp_servers = serde_json::Map::new();
    if let Some(tools) = request
        .external_tools
        .as_ref()
        .filter(|tools| !tools.is_empty())
    {
        let executable = std::env::current_exe().map_err(AgentRunError::Io)?;
        mcp_servers.insert(
            AIFUEL_GATEWAY_REGISTRATION_NAME.to_owned(),
            crate::codex_mcp_runtime_entry(&executable, tools)
                .map_err(|error| protocol_error(&error))?,
        );
    }
    Ok(json!({"mcp_servers":mcp_servers}))
}

pub(super) async fn wait_for_mcp_tools<W, R>(
    stdin: &mut W,
    stdout: &mut BufReader<R>,
    thread_id: &str,
    expected_tools: &[String],
    setup_deadline: Instant,
    cancellation: &RunCancellationToken,
) -> Result<(), AgentRunError>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    let mut request_id = 3_u64;
    loop {
        if cancellation.is_cancelled() {
            return Err(AgentRunError::Cancelled);
        }
        if Instant::now() >= setup_deadline {
            return Err(AgentRunError::Timeout(
                "selected Codex MCP tools did not become ready within the 10-second setup limit"
                    .to_owned(),
            ));
        }
        send(
            stdin,
            json!({
                "id":request_id,
                "method":"mcpServerStatus/list",
                "params":{"threadId":thread_id,"detail":"full","limit":100}
            }),
            Some(setup_deadline),
        )
        .await?;
        let response =
            expect_successful_response(stdout, request_id, setup_deadline, cancellation).await?;
        request_id = request_id.saturating_add(1);
        if mcp_tools_are_ready(&response, expected_tools)? {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

pub(super) fn mcp_tools_are_ready(
    response: &Value,
    expected_tools: &[String],
) -> Result<bool, AgentRunError> {
    let servers = response["result"]["data"]
        .as_array()
        .ok_or_else(|| protocol_error("Codex App Server returned invalid MCP status data"))?;
    if servers.iter().any(|server| {
        server["name"].as_str() != Some(AIFUEL_GATEWAY_REGISTRATION_NAME)
            && server["runtimeStatus"].as_str() == Some("connected")
    }) {
        return Err(protocol_error(
            "an unrelated Codex MCP server is enabled for the managed run",
        ));
    }
    let Some(gateway) = servers
        .iter()
        .find(|server| server["name"].as_str() == Some(AIFUEL_GATEWAY_REGISTRATION_NAME))
    else {
        return Ok(false);
    };
    match gateway["runtimeStatus"].as_str() {
        Some("starting") | Some("notStarted") | None => return Ok(false),
        Some("connected") => {}
        _ => {
            return Err(protocol_error(
                "the selected AI Fuel Gateway MCP server could not start",
            ));
        }
    }

    let available_tools = gateway["tools"]
        .as_object()
        .ok_or_else(|| protocol_error("Codex App Server returned invalid Gateway tools"))?;
    let mut actual = available_tools.keys().cloned().collect::<Vec<_>>();
    let mut expected = expected_tools.to_vec();
    actual.sort();
    expected.sort();
    if actual != expected {
        return Err(protocol_error(
            "Codex App Server did not expose exactly the selected Gateway tools",
        ));
    }
    Ok(true)
}

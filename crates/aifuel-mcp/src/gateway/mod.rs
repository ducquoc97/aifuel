mod connection;
mod handler;
mod host_transport;
mod identity;
mod process;
mod progress;
mod prompts;
mod remote_endpoint;
mod remote_transport;
mod request;
mod resources;
mod snapshot;
mod sse;
mod state;
mod transport;

use aifuel_app::McpGatewayFacade;
use handler::{GatewayServerHandler, GatewayServerService};
use rmcp::ServiceExt;
use state::GatewayState;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

/// Serve the selected external MCP servers to an MCP Host over standard input
/// and output. This process is separate from AI Fuel's read-only monitoring MCP.
pub async fn serve(facade: McpGatewayFacade) -> Result<(), String> {
    serve_with_tool_allowlist(facade, None).await
}

/// Serve the selected external MCP servers, optionally exposing only the
/// exact gateway tool names in `allowed_tools` for this connection.
pub async fn serve_with_tool_allowlist(
    facade: McpGatewayFacade,
    allowed_tools: Option<Vec<String>>,
) -> Result<(), String> {
    let allowed_tools = validate_tool_allowlist(allowed_tools)?;
    let limits = facade.gateway_limits().clone();
    let output_budget = Arc::new(Semaphore::new(limits.max_output_buffer_bytes));
    let overflow = CancellationToken::new();
    let state = Arc::new(GatewayState::new(facade, overflow.clone(), allowed_tools));
    let cancellation = CancellationToken::new();
    let transport = host_transport::HostTransport::new(
        limits.max_message_bytes,
        output_budget,
        Duration::from_secs(limits.output_stall_seconds),
        cancellation.clone(),
    )
    .map_err(|_| "MCP Gateway could not start its output writer".to_owned())?;
    let mut transport = transport;
    transport
        .prepare_legacy_host()
        .await
        .map_err(|_| "MCP Gateway could not initialize the host protocol session".to_owned())?;
    let server = GatewayServerHandler::new(Arc::clone(&state));
    let service = GatewayServerService::new(server)
        .serve_with_ct(transport, cancellation.clone())
        .await
        .map_err(|_| "MCP Gateway could not initialize the host protocol session".to_owned())?;
    let mut service = tokio::spawn(service.waiting());
    let updates = tokio::spawn(Arc::clone(&state).watch_tool_list_changes(cancellation.clone()));
    let resource_updates =
        tokio::spawn(Arc::clone(&state).watch_resource_list_changes(cancellation.clone()));
    let prompt_updates =
        tokio::spawn(Arc::clone(&state).watch_prompt_list_changes(cancellation.clone()));
    let result = tokio::select! {
        result = &mut service => {
            result.map_err(|_| "MCP Gateway protocol service task failed".to_owned())?
        }
        _ = overflow.cancelled() => {
            cancellation.cancel();
            let _ = service.await;
            let _ = updates.await;
            let _ = resource_updates.await;
            let _ = prompt_updates.await;
            state.shutdown().await;
            return Err("MCP Gateway stopped because its notification buffer filled".to_owned());
        }
    };
    cancellation.cancel();
    let _ = updates.await;
    let _ = resource_updates.await;
    let _ = prompt_updates.await;
    state.shutdown().await;
    match result {
        Ok(rmcp::service::QuitReason::Closed) => Ok(()),
        Ok(rmcp::service::QuitReason::Cancelled) => {
            Err("MCP Gateway stopped before it could deliver a response".to_owned())
        }
        Ok(_) | Err(_) => Err("MCP Gateway protocol session failed".to_owned()),
    }
}

fn validate_tool_allowlist(
    allowed_tools: Option<Vec<String>>,
) -> Result<Option<Vec<String>>, String> {
    let Some(allowed_tools) = allowed_tools else {
        return Ok(None);
    };
    let mut seen = HashSet::with_capacity(allowed_tools.len());
    for name in &allowed_tools {
        if !seen.insert(name) {
            return Err(format!(
                "MCP Gateway tool allowlist contains duplicate tool name {name:?}"
            ));
        }
    }
    Ok(Some(allowed_tools))
}

#[cfg(test)]
mod tests {
    use super::validate_tool_allowlist;

    #[test]
    fn tool_allowlist_rejects_duplicate_names() {
        let error = validate_tool_allowlist(Some(vec![
            "docs__search".to_owned(),
            "docs__search".to_owned(),
        ]))
        .expect_err("duplicate requested tools must be rejected");

        assert!(error.contains("duplicate tool name"));
    }

    #[test]
    fn absent_tool_allowlist_remains_unfiltered() {
        assert_eq!(
            validate_tool_allowlist(None).expect("no filter is valid"),
            None
        );
    }
}

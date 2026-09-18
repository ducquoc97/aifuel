mod connection;
mod handler;
mod host_transport;
mod identity;
mod process;
mod progress;
mod request;
mod snapshot;
mod state;
mod transport;

use aifuel_app::McpGatewayFacade;
use handler::GatewayServerHandler;
use rmcp::ServiceExt;
use state::GatewayState;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

/// Serve one selected local MCP server to an MCP Host over standard input and
/// output. This process is separate from AI Fuel's read-only monitoring MCP.
pub async fn serve(facade: McpGatewayFacade) -> Result<(), String> {
    if facade.selected_servers().len() > 1 {
        return Err("the local gateway currently accepts one selected MCP server".to_owned());
    }

    let limits = facade.gateway_limits().clone();
    let output_budget = Arc::new(Semaphore::new(limits.max_output_buffer_bytes));
    let overflow = CancellationToken::new();
    let state = Arc::new(GatewayState::new(facade, overflow.clone()));
    let cancellation = CancellationToken::new();
    let transport = host_transport::HostTransport::new(
        limits.max_message_bytes,
        output_budget,
        Duration::from_secs(limits.output_stall_seconds),
        cancellation.clone(),
    )
    .map_err(|_| "MCP Gateway could not start its output writer".to_owned())?;
    let server = GatewayServerHandler::new(Arc::clone(&state));
    let service = server
        .serve_with_ct(transport, cancellation.clone())
        .await
        .map_err(|_| "MCP Gateway could not initialize the host protocol session".to_owned())?;
    let mut service = tokio::spawn(service.waiting());
    let updates = tokio::spawn(Arc::clone(&state).watch_tool_list_changes(cancellation.clone()));
    let result = tokio::select! {
        result = &mut service => {
            result.map_err(|_| "MCP Gateway protocol service task failed".to_owned())?
        }
        _ = overflow.cancelled() => {
            cancellation.cancel();
            let _ = service.await;
            let _ = updates.await;
            state.shutdown().await;
            return Err("MCP Gateway stopped because its notification buffer filled".to_owned());
        }
    };
    cancellation.cancel();
    let _ = updates.await;
    state.shutdown().await;
    match result {
        Ok(rmcp::service::QuitReason::Closed) => Ok(()),
        Ok(rmcp::service::QuitReason::Cancelled) => {
            Err("MCP Gateway stopped before it could deliver a response".to_owned())
        }
        Ok(_) | Err(_) => Err("MCP Gateway protocol session failed".to_owned()),
    }
}

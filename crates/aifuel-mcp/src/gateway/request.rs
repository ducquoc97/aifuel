use rmcp::model::{ErrorData as McpError, ServerResult};
use rmcp::service::{RequestHandle, RoleClient, ServiceError};
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub(super) enum UpstreamRequestError {
    Protocol(McpError),
    Cancelled,
    TimedOut,
    Disconnected,
}

pub(super) async fn await_server_result(
    mut handle: RequestHandle<RoleClient>,
    deadline: Instant,
    cancellation: CancellationToken,
) -> Result<ServerResult, UpstreamRequestError> {
    enum Outcome {
        Response(
            Box<Result<Result<ServerResult, ServiceError>, tokio::sync::oneshot::error::RecvError>>,
        ),
        Cancelled,
        TimedOut,
    }
    let outcome = tokio::select! {
        response = &mut handle.rx => Outcome::Response(Box::new(response)),
        _ = cancellation.cancelled() => Outcome::Cancelled,
        _ = tokio::time::sleep_until(deadline) => Outcome::TimedOut,
    };
    match outcome {
        Outcome::Response(response) => match *response {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(ServiceError::McpError(error))) => Err(UpstreamRequestError::Protocol(error)),
            Ok(Err(_)) | Err(_) => Err(UpstreamRequestError::Disconnected),
        },
        Outcome::Cancelled => {
            let _ = tokio::time::timeout(
                Duration::from_millis(100),
                handle.cancel(Some("MCP Host cancelled the request".to_owned())),
            )
            .await;
            Err(UpstreamRequestError::Cancelled)
        }
        Outcome::TimedOut => {
            let _ = tokio::time::timeout(
                Duration::from_millis(100),
                handle.cancel(Some("gateway request deadline exceeded".to_owned())),
            )
            .await;
            Err(UpstreamRequestError::TimedOut)
        }
    }
}

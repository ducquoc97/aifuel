use super::super::resources::{ResourceSnapshot, make_snapshot as make_resource_snapshot};
use super::{GatewayState, ServerSnapshotError, selected_server_limits};
use futures_util::future::join_all;
use rmcp::model::{ErrorCode, ErrorData as McpError};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

impl GatewayState {
    pub(in crate::gateway) async fn resource_snapshot(
        &self,
        cancellation: CancellationToken,
        operation_deadline: Instant,
    ) -> Result<Arc<ResourceSnapshot>, McpError> {
        if self.servers.is_empty() {
            return Ok(Arc::new(ResourceSnapshot::empty()));
        }
        let mut current = tokio::select! {
            result = tokio::time::timeout_at(operation_deadline, self.resource_aggregate.lock()) => {
                result.map_err(|_| McpError::internal_error("MCP server discovery timed out", None))?
            }
            _ = cancellation.cancelled() => {
                return Err(McpError::new(ErrorCode(-32800), "MCP request was cancelled", None));
            }
        };
        let refresh = self.events.resources_dirty.swap(false, Ordering::AcqRel);
        if !refresh && let Some(snapshot) = current.as_ref() {
            return Ok(Arc::clone(snapshot));
        }

        let server_ids = self.servers.keys().cloned().collect::<Vec<_>>();
        let results = join_all(server_ids.iter().cloned().map(|server_id| {
            let cancellation = cancellation.clone();
            async move {
                let result = self
                    .server_resource_snapshot(&server_id, refresh, cancellation, operation_deadline)
                    .await;
                (server_id, result)
            }
        }))
        .await;
        if cancellation.is_cancelled() {
            return Err(McpError::new(
                ErrorCode(-32800),
                "MCP request was cancelled",
                None,
            ));
        }

        let mut snapshots = Vec::new();
        let mut first_error = None;
        for (_, result) in results {
            match result {
                Ok(snapshot) => snapshots.push(snapshot),
                Err(ServerSnapshotError::Retryable(error)) => {
                    first_error.get_or_insert(error);
                }
                Err(ServerSnapshotError::IdentityConflict(error)) => {
                    self.events.resources_dirty.store(true, Ordering::Release);
                    return Err(error);
                }
            }
        }
        if snapshots.is_empty() {
            return Err(first_error.unwrap_or_else(|| {
                McpError::internal_error(
                    "no selected MCP server returned a usable resources/list snapshot",
                    None,
                )
            }));
        }

        let snapshot = match super::super::resources::aggregate(
            &self.cursor_scope,
            &snapshots,
            self.next_resource_snapshot_id
                .fetch_add(1, Ordering::Relaxed),
        ) {
            Ok(snapshot) => Arc::new(snapshot),
            Err(error) => {
                self.events.resources_dirty.store(true, Ordering::Release);
                return Err(error);
            }
        };
        if let Some(cached) = current.as_ref()
            && cached.resources == snapshot.resources
            && cached.templates == snapshot.templates
            && cached.resource_routes == snapshot.resource_routes
        {
            return Ok(Arc::clone(cached));
        }
        if current.is_some() {
            self.pending_resource_list_notification
                .store(true, Ordering::Release);
        }
        *current = Some(Arc::clone(&snapshot));
        Ok(snapshot)
    }

    async fn server_resource_snapshot(
        &self,
        server_id: &str,
        refresh: bool,
        cancellation: CancellationToken,
        operation_deadline: Instant,
    ) -> Result<Arc<ResourceSnapshot>, ServerSnapshotError> {
        let server = self.servers.get(server_id).expect("selected server exists");
        let mut current = tokio::select! {
            result = tokio::time::timeout_at(operation_deadline, server.resource_snapshot.lock()) => {
                result.map_err(|_| ServerSnapshotError::Retryable(McpError::internal_error(
                    "MCP server discovery timed out",
                    None,
                )))?
            }
            _ = cancellation.cancelled() => {
                return Err(ServerSnapshotError::Retryable(McpError::new(
                    ErrorCode(-32800),
                    "MCP request was cancelled",
                    None,
                )));
            }
        };
        if !refresh && let Some(snapshot) = current.as_ref() {
            return Ok(Arc::clone(snapshot));
        }

        let limits = selected_server_limits(&server.server);
        let discovery_deadline = Instant::now()
            + Duration::from_secs(limits.discovery_seconds)
                .min(operation_deadline.saturating_duration_since(Instant::now()));
        let result = async {
            let connection = self
                .connection(server_id, cancellation.clone(), discovery_deadline)
                .await
                .map_err(ServerSnapshotError::Retryable)?;
            let _request_permit = connection
                .try_request_slot()
                .map_err(ServerSnapshotError::Retryable)?;
            let resources = connection
                .list_resources(discovery_deadline, cancellation.clone())
                .await
                .map_err(ServerSnapshotError::Retryable)?;
            let templates = connection
                .list_resource_templates(discovery_deadline, cancellation)
                .await
                .map_err(ServerSnapshotError::Retryable)?;
            Ok::<_, ServerSnapshotError>((resources, templates))
        }
        .await;

        let result = match result {
            Ok((resources, templates)) => {
                match make_resource_snapshot(
                    &server.server,
                    resources,
                    templates,
                    limits,
                    self.next_resource_snapshot_id.load(Ordering::Relaxed),
                ) {
                    Ok(snapshot) => Ok(Arc::new(snapshot)),
                    Err(error) => Err(ServerSnapshotError::IdentityConflict(error)),
                }
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(snapshot) => {
                if let Some(cached) = current.as_ref()
                    && cached.resources == snapshot.resources
                    && cached.templates == snapshot.templates
                {
                    return Ok(Arc::clone(cached));
                }
                *current = Some(Arc::clone(&snapshot));
                Ok(snapshot)
            }
            Err(ServerSnapshotError::Retryable(_error)) if current.is_some() => {
                self.events.resources_dirty.store(true, Ordering::Release);
                eprintln!(
                    "aifuel: selected MCP server {server_id:?} is unavailable; retaining its last resource list"
                );
                Ok(Arc::clone(current.as_ref().expect("snapshot exists")))
            }
            Err(ServerSnapshotError::Retryable(error)) => {
                self.events.resources_dirty.store(true, Ordering::Release);
                eprintln!("aifuel: selected MCP server {server_id:?} is unavailable");
                Err(ServerSnapshotError::Retryable(error))
            }
            Err(error @ ServerSnapshotError::IdentityConflict(_)) => Err(error),
        }
    }
}

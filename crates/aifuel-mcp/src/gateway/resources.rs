use super::identity::{decode_resource_uri, resource_template, resource_uri, snapshot_cursor};
use aifuel_app::{SelectedMcpServer, ServerLimits};
use rmcp::model::{
    ErrorData as McpError, ListResourceTemplatesResult, ListResourcesResult, RequestId, Resource,
    ResourceTemplate, ServerJsonRpcMessage, ServerResult,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Mutex;

/// A complete, validated list snapshot for one selected upstream server.
///
/// The gateway owns cursors and routes. The upstream's cursor is never
/// exposed to the host, which keeps a host cursor tied to this exact snapshot
/// and list kind.
pub(super) struct ResourceSnapshot {
    pub(super) id: u64,
    pub(super) cursor_scope: String,
    pub(super) server_ids: HashSet<String>,
    pub(super) resources: Vec<Resource>,
    pub(super) templates: Vec<ResourceTemplate>,
    pub(super) resource_routes: HashMap<String, ResourceRoute>,
    pub(super) cursors: Mutex<HashMap<String, CursorPosition>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ResourceRoute {
    pub(super) server_id: String,
    pub(super) upstream_uri: String,
}

#[derive(Clone, Copy)]
pub(super) enum ResourceListKind {
    Resources,
    Templates,
}

#[derive(Clone, Copy)]
pub(super) struct CursorPosition {
    pub(super) kind: ResourceListKind,
    pub(super) offset: usize,
}

impl ResourceSnapshot {
    pub(super) fn empty() -> Self {
        Self {
            id: 0,
            cursor_scope: String::new(),
            server_ids: HashSet::new(),
            resources: Vec::new(),
            templates: Vec::new(),
            resource_routes: HashMap::new(),
            cursors: Mutex::new(HashMap::new()),
        }
    }

    pub(super) fn route(&self, uri: &str) -> Result<ResourceRoute, McpError> {
        if let Some(route) = self.resource_routes.get(uri) {
            return Ok(route.clone());
        }
        let Ok((server_id, upstream_uri)) = decode_resource_uri(uri) else {
            return Err(McpError::invalid_params(
                "unknown or invalid gateway resource URI",
                None,
            ));
        };
        if !self.server_ids.contains(&server_id) {
            return Err(McpError::invalid_params(
                "gateway resource URI addresses an unselected server",
                None,
            ));
        }
        Ok(ResourceRoute {
            server_id,
            upstream_uri,
        })
    }

    pub(super) async fn page_resources(
        &self,
        cursor: Option<&str>,
        request_id: RequestId,
        max_message_bytes: usize,
    ) -> Result<ListResourcesResult, McpError> {
        let start = self.start(cursor, ResourceListKind::Resources).await?;
        let mut page = Vec::new();
        let mut end = start;
        loop {
            if end == self.resources.len() {
                let result = ListResourcesResult {
                    meta: None,
                    next_cursor: None,
                    resources: page,
                };
                ensure_resources_response_size(&result, &request_id, max_message_bytes)?;
                return Ok(result);
            }
            page.push(self.resources[end].clone());
            let next_offset = end + 1;
            let next_cursor = (next_offset < self.resources.len())
                .then(|| snapshot_cursor("resources", &self.cursor_scope, self.id, next_offset));
            let candidate = ListResourcesResult {
                meta: None,
                next_cursor: next_cursor.clone(),
                resources: page.clone(),
            };
            if resources_response_size(&candidate, &request_id)? > max_message_bytes {
                page.pop();
                if page.is_empty() {
                    return Err(McpError::internal_error(
                        "one gateway resource exceeds the host message byte limit",
                        None,
                    ));
                }
                let next_offset = start + page.len();
                let next_cursor = (next_offset < self.resources.len()).then(|| {
                    snapshot_cursor("resources", &self.cursor_scope, self.id, next_offset)
                });
                let result = ListResourcesResult {
                    meta: None,
                    next_cursor: next_cursor.clone(),
                    resources: page,
                };
                ensure_resources_response_size(&result, &request_id, max_message_bytes)?;
                self.remember(next_cursor, ResourceListKind::Resources, next_offset)
                    .await;
                return Ok(result);
            }
            end = next_offset;
            if end == self.resources.len() {
                self.remember(next_cursor, ResourceListKind::Resources, end)
                    .await;
                return Ok(candidate);
            }
        }
    }

    pub(super) async fn page_templates(
        &self,
        cursor: Option<&str>,
        request_id: RequestId,
        max_message_bytes: usize,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        let start = self.start(cursor, ResourceListKind::Templates).await?;
        let mut page = Vec::new();
        let mut end = start;
        loop {
            if end == self.templates.len() {
                let result = ListResourceTemplatesResult {
                    meta: None,
                    next_cursor: None,
                    resource_templates: page,
                };
                ensure_templates_response_size(&result, &request_id, max_message_bytes)?;
                return Ok(result);
            }
            page.push(self.templates[end].clone());
            let next_offset = end + 1;
            let next_cursor = (next_offset < self.templates.len()).then(|| {
                snapshot_cursor(
                    "resource-templates",
                    &self.cursor_scope,
                    self.id,
                    next_offset,
                )
            });
            let candidate = ListResourceTemplatesResult {
                meta: None,
                next_cursor: next_cursor.clone(),
                resource_templates: page.clone(),
            };
            if templates_response_size(&candidate, &request_id)? > max_message_bytes {
                page.pop();
                if page.is_empty() {
                    return Err(McpError::internal_error(
                        "one gateway resource template exceeds the host message byte limit",
                        None,
                    ));
                }
                let next_offset = start + page.len();
                let next_cursor = (next_offset < self.templates.len()).then(|| {
                    snapshot_cursor(
                        "resource-templates",
                        &self.cursor_scope,
                        self.id,
                        next_offset,
                    )
                });
                let result = ListResourceTemplatesResult {
                    meta: None,
                    next_cursor: next_cursor.clone(),
                    resource_templates: page,
                };
                ensure_templates_response_size(&result, &request_id, max_message_bytes)?;
                self.remember(next_cursor, ResourceListKind::Templates, next_offset)
                    .await;
                return Ok(result);
            }
            end = next_offset;
            if end == self.templates.len() {
                self.remember(next_cursor, ResourceListKind::Templates, end)
                    .await;
                return Ok(candidate);
            }
        }
    }

    async fn start(
        &self,
        cursor: Option<&str>,
        expected_kind: ResourceListKind,
    ) -> Result<usize, McpError> {
        let Some(cursor) = cursor else { return Ok(0) };
        let position = self.cursors.lock().await.get(cursor).copied();
        let Some(position) = position else {
            return Err(McpError::invalid_params(
                "resource list cursor is stale or invalid",
                None,
            ));
        };
        if !same_kind(position.kind, expected_kind) {
            return Err(McpError::invalid_params(
                "resource list cursor belongs to a different list",
                None,
            ));
        }
        let item_count = match expected_kind {
            ResourceListKind::Resources => self.resources.len(),
            ResourceListKind::Templates => self.templates.len(),
        };
        if position.offset >= item_count {
            return Err(McpError::invalid_params(
                "resource list cursor is stale or invalid",
                None,
            ));
        }
        Ok(position.offset)
    }

    async fn remember(&self, cursor: Option<String>, kind: ResourceListKind, offset: usize) {
        if let Some(cursor) = cursor {
            self.cursors
                .lock()
                .await
                .insert(cursor, CursorPosition { kind, offset });
        }
    }
}

pub(super) fn make_snapshot(
    server: &SelectedMcpServer,
    upstream_resources: Vec<Resource>,
    upstream_templates: Vec<ResourceTemplate>,
    limits: &ServerLimits,
    snapshot_id: u64,
) -> Result<ResourceSnapshot, McpError> {
    let mut resources = Vec::with_capacity(upstream_resources.len());
    let mut resource_routes = HashMap::with_capacity(upstream_resources.len());
    for mut resource in upstream_resources {
        let upstream_uri = resource.uri.clone();
        let gateway_uri = resource_uri(&server.id, &upstream_uri)
            .map_err(|error| McpError::invalid_params(error, None))?;
        if resource_routes
            .insert(
                gateway_uri.clone(),
                ResourceRoute {
                    server_id: server.id.clone(),
                    upstream_uri,
                },
            )
            .is_some()
        {
            return Err(McpError::internal_error(
                "upstream MCP resources contain conflicting gateway identities",
                None,
            ));
        }
        resource.uri = gateway_uri;
        resources.push(resource);
    }

    let mut templates = Vec::with_capacity(upstream_templates.len());
    let mut template_identities = HashSet::with_capacity(upstream_templates.len());
    for mut template in upstream_templates {
        let gateway_template = resource_template(&server.id, &template.uri_template)
            .map_err(|error| McpError::invalid_params(error, None))?;
        if !template_identities.insert(gateway_template.clone()) {
            return Err(McpError::internal_error(
                "upstream MCP resource templates contain conflicting gateway identities",
                None,
            ));
        }
        template.uri_template = gateway_template;
        templates.push(template);
    }
    resources.sort_by(|left, right| left.uri.cmp(&right.uri));
    templates.sort_by(|left, right| left.uri_template.cmp(&right.uri_template));

    let bytes = serde_json::to_vec(&(&resources, &templates))
        .map_err(|_| McpError::internal_error("could not encode MCP resource lists", None))?;
    if resources.len().saturating_add(templates.len()) > limits.max_list_entries
        || bytes.len() > limits.max_list_snapshot_bytes
    {
        return Err(McpError::internal_error(
            "upstream MCP server exceeded the configured resource snapshot limit",
            None,
        ));
    }
    Ok(ResourceSnapshot {
        id: snapshot_id,
        cursor_scope: server.id.clone(),
        server_ids: HashSet::from([server.id.clone()]),
        resources,
        templates,
        resource_routes,
        cursors: Mutex::new(HashMap::new()),
    })
}

pub(super) fn aggregate(
    cursor_scope: &str,
    snapshots: &[Arc<ResourceSnapshot>],
    snapshot_id: u64,
) -> Result<ResourceSnapshot, McpError> {
    let mut ordered = snapshots.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| left.cursor_scope.cmp(&right.cursor_scope));

    let mut resources = Vec::new();
    let mut templates = Vec::new();
    let mut resource_routes = HashMap::new();
    let mut server_ids = HashSet::new();
    for snapshot in ordered {
        server_ids.extend(snapshot.server_ids.iter().cloned());
        for resource in &snapshot.resources {
            if resource_routes.contains_key(&resource.uri) {
                return Err(McpError::internal_error(
                    "selected MCP servers contain conflicting gateway resource identities",
                    None,
                ));
            }
            let route = snapshot.resource_routes.get(&resource.uri).ok_or_else(|| {
                McpError::internal_error("selected MCP resource snapshot is invalid", None)
            })?;
            resource_routes.insert(resource.uri.clone(), route.clone());
            resources.push(resource.clone());
        }
        templates.extend(snapshot.templates.iter().cloned());
    }
    resources.sort_by(|left, right| left.uri.cmp(&right.uri));
    templates.sort_by(|left, right| left.uri_template.cmp(&right.uri_template));
    Ok(ResourceSnapshot {
        id: snapshot_id,
        cursor_scope: cursor_scope.to_owned(),
        server_ids,
        resources,
        templates,
        resource_routes,
        cursors: Mutex::new(HashMap::new()),
    })
}

fn same_kind(left: ResourceListKind, right: ResourceListKind) -> bool {
    matches!(
        (left, right),
        (ResourceListKind::Resources, ResourceListKind::Resources)
            | (ResourceListKind::Templates, ResourceListKind::Templates)
    )
}

fn ensure_resources_response_size(
    result: &ListResourcesResult,
    request_id: &RequestId,
    limit: usize,
) -> Result<(), McpError> {
    if resources_response_size(result, request_id)? > limit {
        return Err(McpError::internal_error(
            "resource list response exceeds the host message byte limit",
            None,
        ));
    }
    Ok(())
}

fn ensure_templates_response_size(
    result: &ListResourceTemplatesResult,
    request_id: &RequestId,
    limit: usize,
) -> Result<(), McpError> {
    if templates_response_size(result, request_id)? > limit {
        return Err(McpError::internal_error(
            "resource template list response exceeds the host message byte limit",
            None,
        ));
    }
    Ok(())
}

fn resources_response_size(
    result: &ListResourcesResult,
    request_id: &RequestId,
) -> Result<usize, McpError> {
    serde_json::to_vec(&ServerJsonRpcMessage::response(
        ServerResult::ListResourcesResult(result.clone()),
        request_id.clone(),
    ))
    .map(|bytes| bytes.len())
    .map_err(|_| McpError::internal_error("could not encode resource list response", None))
}

fn templates_response_size(
    result: &ListResourceTemplatesResult,
    request_id: &RequestId,
) -> Result<usize, McpError> {
    serde_json::to_vec(&ServerJsonRpcMessage::response(
        ServerResult::ListResourceTemplatesResult(result.clone()),
        request_id.clone(),
    ))
    .map(|bytes| bytes.len())
    .map_err(|_| McpError::internal_error("could not encode resource template response", None))
}

#[cfg(test)]
mod tests {
    use super::{ResourceSnapshot, make_snapshot};
    use aifuel_app::{McpServerDefinition, SelectedMcpServer, ServerLimits, StdioServerDefinition};
    use rmcp::model::{RequestId, Resource};
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn selected_server() -> SelectedMcpServer {
        SelectedMcpServer {
            id: "docs".to_owned(),
            definition: McpServerDefinition::Stdio(StdioServerDefinition {
                command: "fixture".to_owned(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
                env_from: BTreeMap::new(),
                limits: ServerLimits::default(),
            }),
            user_home: PathBuf::from("/home/test"),
        }
    }

    #[tokio::test]
    async fn pagination_is_byte_bounded_and_rejects_stale_cursors() {
        let mut resources = serde_json::from_value::<Vec<Resource>>(json!([
            {"uri":"file:///one","name":"one","description":"one"},
            {"uri":"file:///two","name":"two","description":"two"}
        ]))
        .expect("resource fixture should deserialize");
        for resource in &mut resources {
            resource.description = Some("x".repeat(120));
        }
        let snapshot = make_snapshot(
            &selected_server(),
            resources,
            Vec::new(),
            &ServerLimits::default(),
            1,
        )
        .expect("resource snapshot should build");
        let request_id = RequestId::Number(1);
        let first = snapshot
            .page_resources(Some("invalid"), request_id.clone(), 1_000)
            .await;
        assert!(first.is_err(), "unknown cursors must fail explicitly");

        let first = snapshot
            .page_resources(None, request_id.clone(), 350)
            .await
            .expect("the first resource should fit its downstream page");
        assert_eq!(first.resources.len(), 1);
        let cursor = first
            .next_cursor
            .clone()
            .expect("a second page is required");
        let second = snapshot
            .page_resources(Some(&cursor), request_id, 350)
            .await
            .expect("the second resource should fit its downstream page");
        assert_eq!(second.resources.len(), 1);
        assert!(second.next_cursor.is_none());

        let empty = ResourceSnapshot::empty();
        assert!(
            empty
                .page_resources(Some(&cursor), RequestId::Number(2), 1_000)
                .await
                .is_err()
        );
    }
}

use super::identity::{cursor as tool_cursor, tool_name};
use aifuel_app::{SelectedMcpServer, ServerLimits};
use rmcp::model::{
    ErrorData as McpError, ListToolsResult, RequestId, ServerJsonRpcMessage, ServerResult,
    TaskSupport, Tool,
};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

pub(super) fn make_snapshot(
    server: &SelectedMcpServer,
    upstream: Vec<Tool>,
    limits: &ServerLimits,
    snapshot_id: u64,
) -> Result<ToolSnapshot, McpError> {
    let server_snapshot = Arc::new(make_server_snapshot(server, upstream, limits)?);
    ToolSnapshot::aggregate(&server.id, &[server_snapshot], snapshot_id)
}

pub(super) fn make_server_snapshot(
    server: &SelectedMcpServer,
    upstream: Vec<Tool>,
    limits: &ServerLimits,
) -> Result<ServerToolSnapshot, McpError> {
    let mut tools = Vec::new();
    let mut routes = HashMap::new();
    let mut excluded_task_tools = false;
    for mut tool in upstream {
        match tool
            .execution
            .as_ref()
            .and_then(|execution| execution.task_support)
        {
            Some(TaskSupport::Required) => {
                excluded_task_tools = true;
                continue;
            }
            Some(TaskSupport::Optional) => {
                if let Some(execution) = tool.execution.as_mut() {
                    execution.task_support = Some(TaskSupport::Forbidden);
                }
            }
            _ => {}
        }
        let upstream_name = tool.name.to_string();
        let gateway_name = tool_name(&server.id, &upstream_name);
        if routes.insert(gateway_name.clone(), upstream_name).is_some() {
            return Err(McpError::internal_error(
                "selected MCP server contains conflicting gateway tool identities",
                None,
            ));
        }
        tool.name = Cow::Owned(gateway_name);
        tools.push(tool);
    }
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    let bytes = serde_json::to_vec(&tools)
        .map_err(|_| McpError::internal_error("could not encode MCP tool list", None))?;
    if tools.len() > limits.max_list_entries || bytes.len() > limits.max_list_snapshot_bytes {
        return Err(McpError::internal_error(
            "selected MCP server exceeded the configured tool snapshot limit",
            None,
        ));
    }
    if excluded_task_tools {
        eprintln!(
            "aifuel: excluded one or more selected MCP tools that require unsupported task execution"
        );
    }
    Ok(ServerToolSnapshot {
        server_id: server.id.clone(),
        tools,
        routes,
    })
}

pub(super) struct ServerToolSnapshot {
    pub(super) server_id: String,
    pub(super) tools: Vec<Tool>,
    pub(super) routes: HashMap<String, String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ToolRoute {
    pub(super) server_id: String,
    pub(super) upstream_name: String,
}

pub(super) struct ToolSnapshot {
    id: u64,
    cursor_scope: String,
    pub(super) tools: Vec<Tool>,
    /// Single-server lookup retained until the state and handler use
    /// server-qualified routing directly.
    pub(super) routes: HashMap<String, String>,
    pub(super) server_routes: HashMap<String, ToolRoute>,
    cursors: Mutex<HashMap<String, usize>>,
}

impl ToolSnapshot {
    pub(super) fn aggregate(
        cursor_scope: &str,
        snapshots: &[Arc<ServerToolSnapshot>],
        snapshot_id: u64,
    ) -> Result<Self, McpError> {
        let mut ordered = snapshots.iter().collect::<Vec<_>>();
        ordered.sort_by(|left, right| left.server_id.cmp(&right.server_id));

        let mut tools = Vec::new();
        let mut routes = HashMap::new();
        let mut server_routes = HashMap::new();
        for snapshot in ordered {
            for tool in &snapshot.tools {
                let gateway_name = tool.name.to_string();
                let upstream_name = snapshot.routes.get(&gateway_name).ok_or_else(|| {
                    McpError::internal_error("selected MCP tool snapshot is invalid", None)
                })?;
                if routes.contains_key(&gateway_name) {
                    return Err(McpError::internal_error(
                        "selected MCP servers contain conflicting gateway tool identities",
                        None,
                    ));
                }
                routes.insert(gateway_name.clone(), upstream_name.clone());
                server_routes.insert(
                    gateway_name,
                    ToolRoute {
                        server_id: snapshot.server_id.clone(),
                        upstream_name: upstream_name.clone(),
                    },
                );
                tools.push(tool.clone());
            }
        }
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(Self {
            id: snapshot_id,
            cursor_scope: cursor_scope.to_owned(),
            tools,
            routes,
            server_routes,
            cursors: Mutex::new(HashMap::new()),
        })
    }

    pub(super) fn empty() -> Self {
        Self::empty_for_scope("", 0)
    }

    pub(super) fn empty_for_scope(cursor_scope: &str, snapshot_id: u64) -> Self {
        Self {
            id: snapshot_id,
            cursor_scope: cursor_scope.to_owned(),
            tools: Vec::new(),
            routes: HashMap::new(),
            server_routes: HashMap::new(),
            cursors: Mutex::new(HashMap::new()),
        }
    }

    pub(super) async fn page(
        &self,
        cursor: Option<&str>,
        request_id: RequestId,
        max_message_bytes: usize,
    ) -> Result<ListToolsResult, McpError> {
        let start = match cursor {
            None => 0,
            Some(cursor) => *self.cursors.lock().await.get(cursor).ok_or_else(|| {
                McpError::invalid_params("tools/list cursor is stale or invalid", None)
            })?,
        };
        if start > self.tools.len() || (start == self.tools.len() && cursor.is_some()) {
            return Err(McpError::invalid_params(
                "tools/list cursor is stale or invalid",
                None,
            ));
        }
        let mut page_tools = Vec::new();
        let mut end = start;
        loop {
            if end == self.tools.len() {
                return Ok(tools_page(page_tools, None));
            }
            page_tools.push(self.tools[end].clone());
            let next_offset = end + 1;
            let next_cursor = (next_offset < self.tools.len())
                .then(|| tool_cursor("tools", &self.cursor_scope, self.id, next_offset));
            let candidate = tools_page(page_tools.clone(), next_cursor.clone());
            if response_bytes(&candidate, &request_id)? > max_message_bytes {
                page_tools.pop();
                if page_tools.is_empty() {
                    return Err(McpError::internal_error(
                        "one selected MCP tool exceeds the host message byte limit",
                        None,
                    ));
                }
                let next_offset = start + page_tools.len();
                let next_cursor = (next_offset < self.tools.len())
                    .then(|| tool_cursor("tools", &self.cursor_scope, self.id, next_offset));
                let result = tools_page(page_tools, next_cursor.clone());
                if response_bytes(&result, &request_id)? > max_message_bytes {
                    return Err(McpError::internal_error(
                        "selected MCP tool page exceeds the host message byte limit",
                        None,
                    ));
                }
                if let Some(cursor) = next_cursor {
                    self.cursors.lock().await.insert(cursor, next_offset);
                }
                return Ok(result);
            }
            end = next_offset;
            if end == self.tools.len() {
                return Ok(candidate);
            }
        }
    }
}

fn tools_page(tools: Vec<Tool>, next_cursor: Option<String>) -> ListToolsResult {
    ListToolsResult {
        meta: None,
        next_cursor,
        tools,
    }
}

fn response_bytes(result: &ListToolsResult, request_id: &RequestId) -> Result<usize, McpError> {
    serde_json::to_vec(&ServerJsonRpcMessage::response(
        ServerResult::ListToolsResult(result.clone()),
        request_id.clone(),
    ))
    .map(|bytes| bytes.len())
    .map_err(|_| McpError::internal_error("could not encode tools/list response", None))
}

#[cfg(test)]
mod tests {
    use super::{ServerToolSnapshot, ToolRoute, ToolSnapshot};
    use rmcp::model::Tool;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Arc;

    #[test]
    fn aggregation_routes_colliding_upstream_names_to_their_selected_servers() {
        let docs = snapshot("docs", "docs__echo", "echo");
        let memory = snapshot("memory", "memory__echo", "echo");
        let combined = ToolSnapshot::aggregate("gateway-session", &[memory, docs], 1)
            .expect("distinct namespaced identities should aggregate");

        assert_eq!(
            combined
                .tools
                .iter()
                .map(|tool| tool.name.as_ref())
                .collect::<Vec<_>>(),
            vec!["docs__echo", "memory__echo"]
        );
        assert_eq!(
            combined.server_routes.get("docs__echo"),
            Some(&ToolRoute {
                server_id: "docs".to_owned(),
                upstream_name: "echo".to_owned(),
            })
        );
        assert_eq!(
            combined.server_routes.get("memory__echo"),
            Some(&ToolRoute {
                server_id: "memory".to_owned(),
                upstream_name: "echo".to_owned(),
            })
        );
    }

    #[test]
    fn aggregation_rejects_a_conflicting_gateway_identity() {
        let docs = snapshot("docs", "shared__echo", "echo");
        let memory = snapshot("memory", "shared__echo", "echo");

        let Err(error) = ToolSnapshot::aggregate("gateway-session", &[docs, memory], 1) else {
            panic!("conflicting identities must be rejected");
        };

        assert!(
            error
                .to_string()
                .contains("conflicting gateway tool identities")
        );
    }

    fn snapshot(
        server_id: &str,
        public_name: &str,
        upstream_name: &str,
    ) -> Arc<ServerToolSnapshot> {
        Arc::new(ServerToolSnapshot {
            server_id: server_id.to_owned(),
            tools: vec![tool(public_name)],
            routes: HashMap::from([(public_name.to_owned(), upstream_name.to_owned())]),
        })
    }

    fn tool(name: &str) -> Tool {
        serde_json::from_value(json!({
            "name": name,
            "description": "fixture tool",
            "inputSchema": {"type":"object"}
        }))
        .expect("fixture tool should deserialize")
    }
}

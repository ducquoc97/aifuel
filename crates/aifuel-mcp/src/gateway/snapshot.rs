use super::identity::{cursor as tool_cursor, tool_name};
use aifuel_app::{SelectedMcpServer, ServerLimits};
use rmcp::model::{
    ErrorData as McpError, ListToolsResult, RequestId, ServerJsonRpcMessage, ServerResult,
    TaskSupport, Tool,
};
use std::borrow::Cow;
use std::collections::HashMap;
use tokio::sync::Mutex;

pub(super) fn make_snapshot(
    server: &SelectedMcpServer,
    upstream: Vec<Tool>,
    limits: &ServerLimits,
    snapshot_id: u64,
) -> Result<ToolSnapshot, McpError> {
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
                "upstream MCP server tools contain conflicting gateway identities",
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
            "upstream MCP server exceeded the configured tool snapshot limit",
            None,
        ));
    }
    if excluded_task_tools {
        eprintln!(
            "aifuel: excluded one or more upstream MCP tools that require unsupported task execution"
        );
    }
    Ok(ToolSnapshot {
        id: snapshot_id,
        server_id: server.id.clone(),
        tools,
        routes,
        cursors: Mutex::new(HashMap::new()),
    })
}

pub(super) struct ToolSnapshot {
    id: u64,
    server_id: String,
    pub(super) tools: Vec<Tool>,
    pub(super) routes: HashMap<String, String>,
    cursors: Mutex<HashMap<String, usize>>,
}

impl ToolSnapshot {
    pub(super) fn empty() -> Self {
        Self {
            id: 0,
            server_id: String::new(),
            tools: Vec::new(),
            routes: HashMap::new(),
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
                .then(|| tool_cursor(&self.server_id, self.id, next_offset));
            let candidate = tools_page(page_tools.clone(), next_cursor.clone());
            if response_bytes(&candidate, &request_id)? > max_message_bytes {
                page_tools.pop();
                if page_tools.is_empty() {
                    return Err(McpError::internal_error(
                        "one local MCP tool exceeds the host message byte limit",
                        None,
                    ));
                }
                let next_offset = start + page_tools.len();
                let next_cursor = (next_offset < self.tools.len())
                    .then(|| tool_cursor(&self.server_id, self.id, next_offset));
                let result = tools_page(page_tools, next_cursor.clone());
                if response_bytes(&result, &request_id)? > max_message_bytes {
                    return Err(McpError::internal_error(
                        "local MCP tool page exceeds the host message byte limit",
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

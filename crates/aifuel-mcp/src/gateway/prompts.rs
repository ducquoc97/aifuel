use super::identity::{cursor, prompt_name, resource_uri};
use aifuel_app::{SelectedMcpServer, ServerLimits};
use rmcp::model::{
    ErrorData as McpError, GetPromptResult, ListPromptsResult, Prompt, RequestId,
    ServerJsonRpcMessage, ServerResult,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

pub(super) fn make_server_snapshot(
    server: &SelectedMcpServer,
    upstream: Vec<Prompt>,
    limits: &ServerLimits,
) -> Result<ServerPromptSnapshot, PromptSnapshotBuildError> {
    let mut prompts = Vec::with_capacity(upstream.len());
    let mut routes = HashMap::new();
    for mut prompt in upstream {
        let upstream_name = prompt.name.clone();
        let gateway_name = prompt_name(&server.id, &upstream_name);
        if routes.insert(gateway_name.clone(), upstream_name).is_some() {
            return Err(PromptSnapshotBuildError::IdentityConflict(
                McpError::internal_error(
                    "selected MCP server contains conflicting gateway prompt identities",
                    None,
                ),
            ));
        }
        prompt.name = gateway_name;
        prompts.push(prompt);
    }
    prompts.sort_by(|left, right| left.name.cmp(&right.name));
    let bytes = serde_json::to_vec(&prompts).map_err(|_| {
        PromptSnapshotBuildError::Invalid(McpError::internal_error(
            "could not encode MCP prompt list",
            None,
        ))
    })?;
    if prompts.len() > limits.max_list_entries || bytes.len() > limits.max_list_snapshot_bytes {
        return Err(PromptSnapshotBuildError::Invalid(McpError::internal_error(
            "selected MCP server exceeded the configured prompt snapshot limit",
            None,
        )));
    }
    Ok(ServerPromptSnapshot {
        server_id: server.id.clone(),
        prompts: prompts.into_iter().map(Arc::new).collect(),
        routes,
    })
}

pub(super) struct ServerPromptSnapshot {
    pub(super) server_id: String,
    pub(super) prompts: Vec<Arc<Prompt>>,
    pub(super) routes: HashMap<String, String>,
}

#[derive(Debug)]
pub(super) enum PromptSnapshotBuildError {
    IdentityConflict(McpError),
    Invalid(McpError),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PromptRoute {
    pub(super) server_id: String,
    pub(super) upstream_name: String,
}

pub(super) struct PromptSnapshot {
    id: u64,
    cursor_scope: String,
    pub(super) prompts: Vec<Arc<Prompt>>,
    pub(super) routes: HashMap<String, PromptRoute>,
    cursors: Mutex<HashMap<String, usize>>,
}

impl PromptSnapshot {
    pub(super) fn aggregate(
        cursor_scope: &str,
        snapshots: &[Arc<ServerPromptSnapshot>],
        snapshot_id: u64,
    ) -> Result<Self, McpError> {
        let mut ordered = snapshots.iter().collect::<Vec<_>>();
        ordered.sort_by(|left, right| left.server_id.cmp(&right.server_id));

        let mut prompts = Vec::new();
        let mut routes = HashMap::new();
        for snapshot in ordered {
            for prompt in &snapshot.prompts {
                let gateway_name = prompt.name.clone();
                let upstream_name = snapshot.routes.get(&gateway_name).ok_or_else(|| {
                    McpError::internal_error("selected MCP prompt snapshot is invalid", None)
                })?;
                if routes.contains_key(&gateway_name) {
                    return Err(McpError::internal_error(
                        "selected MCP servers contain conflicting gateway prompt identities",
                        None,
                    ));
                }
                routes.insert(
                    gateway_name,
                    PromptRoute {
                        server_id: snapshot.server_id.clone(),
                        upstream_name: upstream_name.clone(),
                    },
                );
                prompts.push(Arc::clone(prompt));
            }
        }
        prompts.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(Self {
            id: snapshot_id,
            cursor_scope: cursor_scope.to_owned(),
            prompts,
            routes,
            cursors: Mutex::new(HashMap::new()),
        })
    }

    pub(super) fn empty_for_scope(cursor_scope: &str, snapshot_id: u64) -> Self {
        Self {
            id: snapshot_id,
            cursor_scope: cursor_scope.to_owned(),
            prompts: Vec::new(),
            routes: HashMap::new(),
            cursors: Mutex::new(HashMap::new()),
        }
    }

    pub(super) async fn page(
        &self,
        cursor_value: Option<&str>,
        request_id: RequestId,
        max_message_bytes: usize,
    ) -> Result<ListPromptsResult, McpError> {
        let start = match cursor_value {
            None => 0,
            Some(cursor_value) => {
                *self.cursors.lock().await.get(cursor_value).ok_or_else(|| {
                    McpError::invalid_params("prompts/list cursor is stale or invalid", None)
                })?
            }
        };
        if start > self.prompts.len() || (start == self.prompts.len() && cursor_value.is_some()) {
            return Err(McpError::invalid_params(
                "prompts/list cursor is stale or invalid",
                None,
            ));
        }

        let mut page_prompts = Vec::new();
        let mut end = start;
        loop {
            if end == self.prompts.len() {
                return Ok(prompts_page(page_prompts, None));
            }
            page_prompts.push(self.prompts[end].as_ref().clone());
            let next_offset = end + 1;
            let next_cursor = (next_offset < self.prompts.len())
                .then(|| cursor("prompts", &self.cursor_scope, self.id, next_offset));
            let candidate = prompts_page(page_prompts.clone(), next_cursor.clone());
            if response_bytes(&candidate, &request_id)? > max_message_bytes {
                page_prompts.pop();
                if page_prompts.is_empty() {
                    return Err(McpError::internal_error(
                        "one selected MCP prompt exceeds the host message byte limit",
                        None,
                    ));
                }
                let next_offset = start + page_prompts.len();
                let next_cursor = (next_offset < self.prompts.len())
                    .then(|| cursor("prompts", &self.cursor_scope, self.id, next_offset));
                let result = prompts_page(page_prompts, next_cursor.clone());
                if response_bytes(&result, &request_id)? > max_message_bytes {
                    return Err(McpError::internal_error(
                        "selected MCP prompt page exceeds the host message byte limit",
                        None,
                    ));
                }
                if let Some(next_cursor) = next_cursor {
                    self.cursors.lock().await.insert(next_cursor, next_offset);
                }
                return Ok(result);
            }
            end = next_offset;
            if end == self.prompts.len() {
                return Ok(candidate);
            }
        }
    }
}

fn prompts_page(prompts: Vec<Prompt>, next_cursor: Option<String>) -> ListPromptsResult {
    ListPromptsResult {
        meta: None,
        next_cursor,
        prompts,
    }
}

pub(super) fn rewrite_prompt_result(
    server_id: &str,
    mut result: GetPromptResult,
) -> GetPromptResult {
    for message in &mut result.messages {
        match &mut message.content {
            rmcp::model::PromptMessageContent::Resource { resource } => {
                rewrite_resource_contents(server_id, &mut resource.resource);
            }
            rmcp::model::PromptMessageContent::ResourceLink { link } => {
                if !is_direct_http_link(&link.uri)
                    && let Ok(uri) = resource_uri(server_id, &link.uri)
                {
                    link.uri = uri;
                }
            }
            rmcp::model::PromptMessageContent::Text { .. }
            | rmcp::model::PromptMessageContent::Image { .. } => {}
        }
    }
    result
}

fn rewrite_resource_contents(server_id: &str, contents: &mut rmcp::model::ResourceContents) {
    let uri = match contents {
        rmcp::model::ResourceContents::TextResourceContents { uri, .. }
        | rmcp::model::ResourceContents::BlobResourceContents { uri, .. } => uri,
    };
    if !is_direct_http_link(uri)
        && let Ok(routed) = resource_uri(server_id, uri)
    {
        *uri = routed;
    }
}

fn is_direct_http_link(uri: &str) -> bool {
    uri.split_once(':').is_some_and(|(scheme, _)| {
        scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
    })
}

fn response_bytes(result: &ListPromptsResult, request_id: &RequestId) -> Result<usize, McpError> {
    serde_json::to_vec(&ServerJsonRpcMessage::response(
        ServerResult::ListPromptsResult(result.clone()),
        request_id.clone(),
    ))
    .map(|bytes| bytes.len())
    .map_err(|_| McpError::internal_error("could not encode prompts/list response", None))
}

#[cfg(test)]
mod tests {
    use super::{PromptSnapshot, make_server_snapshot, rewrite_prompt_result};
    use aifuel_app::McpGatewayFacade;
    use rmcp::model::{Prompt, PromptMessageContent, PromptMessageRole};
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Arc;

    #[test]
    fn prompt_snapshot_namespaces_and_rejects_duplicates() {
        let facade = McpGatewayFacade::from_json(
            br#"{"servers":{"docs":{"transport":"stdio","command":"fixture"}},"defaults":["docs"]}"#,
            "codex",
            PathBuf::from("/tmp"),
        )
        .expect("catalog should parse");
        let server = &facade.selected_servers()[0];
        let snapshot = make_server_snapshot(
            server,
            vec![Prompt::new("hello", Some("description"), None)],
            &Default::default(),
        )
        .expect("prompt should be valid");
        assert_eq!(snapshot.prompts[0].name, "docs__hello");
        let snapshot = Arc::new(snapshot);
        let combined = PromptSnapshot::aggregate("session", &[snapshot], 1)
            .expect("prompt snapshot should aggregate");
        assert_eq!(combined.routes["docs__hello"].upstream_name, "hello");
    }

    #[test]
    fn prompt_messages_keep_roles_images_and_direct_links_while_routing_embedded_resources() {
        let raw = json!({
            "description":"kept",
            "messages":[
                {"role":"user","content":{"type":"text","text":"hello"}},
                {"role":"assistant","content":{"type":"image","data":"AQI=","mimeType":"image/png"}},
                {"role":"user","content":{"type":"resource","resource":{"resource":{"uri":"file:///guide.md","text":"guide"}}}},
                {"role":"assistant","content":{"type":"resource_link","uri":"https://example.test/doc","name":"web"}}
            ]
        });
        let result: rmcp::model::GetPromptResult =
            serde_json::from_value(raw).expect("prompt result should deserialize");
        let result = rewrite_prompt_result("docs", result);
        assert!(matches!(result.messages[0].role, PromptMessageRole::User));
        assert!(matches!(
            result.messages[1].content,
            PromptMessageContent::Image { .. }
        ));
        let PromptMessageContent::Resource { resource } = &result.messages[2].content else {
            panic!("embedded resource should remain embedded")
        };
        let rmcp::model::ResourceContents::TextResourceContents { uri, .. } = &resource.resource
        else {
            panic!("text resource should remain text")
        };
        assert!(uri.starts_with("aifuel-resource+"));
        let PromptMessageContent::ResourceLink { link } = &result.messages[3].content else {
            panic!("resource link should remain a link")
        };
        assert_eq!(link.uri, "https://example.test/doc");
    }
}

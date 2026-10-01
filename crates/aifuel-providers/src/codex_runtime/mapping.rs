//! Pure mapping from app-server notifications to [`AgentEventKind`].
//!
//! One [`TurnEvents`] holds the per-turn projection state: which message
//! streams produced deltas (so a completed item can fall back to its
//! full text, matching the CLI session's terminal-delta fill), which
//! are still open (so a finished turn closes them), and the token usage
//! the last `thread/tokenUsage/updated` reported. Every method is a
//! pure projection of the protocol payload; the driver emits the
//! returned kinds in order.

use aifuel_core::{
    AgentEventKind, FileDiff, MessageStream, ReceiptCode, RunId, RunOutcome, SessionStatus,
    TodoItem, TodoItemStatus, TokenUsage,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// Per-turn projection from app-server notifications to event kinds.
pub(super) struct TurnEvents {
    run_id: RunId,
    /// The last `thread/tokenUsage/updated` `last` bucket, attached to
    /// `run.completed` when the turn finishes.
    usage: Option<TokenUsage>,
    /// Item ids that streamed at least one delta, so `item/completed`
    /// knows whether full-text fallback is needed.
    streamed: BTreeSet<String>,
    /// Message streams still open, keyed by item id, so a turn that
    /// ends without `item/completed` closes them.
    open: BTreeMap<String, MessageStream>,
}

impl TurnEvents {
    pub fn new(run_id: RunId) -> Self {
        Self {
            run_id,
            usage: None,
            streamed: BTreeSet::new(),
            open: BTreeMap::new(),
        }
    }

    pub fn run_id(&self) -> &RunId {
        &self.run_id
    }

    /// Map one run-scoped notification into event kinds, in order.
    pub fn map(&mut self, method: &str, params: &Value) -> Vec<AgentEventKind> {
        match method {
            "item/agentMessage/delta" => self.delta(params, MessageStream::Assistant),
            "item/reasoning/textDelta"
            | "item/reasoning/summaryTextDelta"
            | "item/reasoning/summaryPartAdded"
            | "item/plan/delta" => self.delta(params, MessageStream::Thinking),
            "item/started" => item_started(&self.run_id, &params["item"]),
            "item/completed" => self.item_completed(&params["item"]),
            "turn/plan/updated" => self.plan_updated(params),
            "thread/tokenUsage/updated" => {
                self.usage = token_usage(&params["tokenUsage"]["last"]);
                Vec::new()
            }
            "error" => vec![AgentEventKind::Error {
                run_id: Some(self.run_id.clone()),
                code: ReceiptCode::ProviderError,
                message: params["error"]["message"]
                    .as_str()
                    .unwrap_or("the app-server reported an error")
                    .to_owned(),
                retryable: params["willRetry"].as_bool().unwrap_or(false),
            }],
            // Turn lifecycle (`turn/started`), raw output deltas, item
            // progress, diffs, and provider notices carry no contract
            // projection here.
            _ => Vec::new(),
        }
    }

    /// The terminal events for the completed `turn` payload: open
    /// message streams close first, then the run's outcome.
    pub fn finish_events(mut self, turn: &Value) -> Vec<AgentEventKind> {
        let mut events = Vec::new();
        for (_, stream) in std::mem::take(&mut self.open) {
            events.push(AgentEventKind::MessageCompleted {
                run_id: self.run_id.clone(),
                stream,
            });
        }
        match turn["status"].as_str().unwrap_or_default() {
            "completed" => events.push(self.completed(RunOutcome::Success)),
            "interrupted" | "cancelled" => events.push(self.completed(RunOutcome::Cancelled)),
            status => {
                let message = turn["error"]["message"].as_str().map_or_else(
                    || format!("the turn ended with an unexpected status {status:?}"),
                    str::to_owned,
                );
                events.push(AgentEventKind::Error {
                    run_id: Some(self.run_id.clone()),
                    code: ReceiptCode::ProviderError,
                    message,
                    retryable: false,
                });
                events.push(self.completed(RunOutcome::Failed));
            }
        }
        events
    }

    fn completed(&self, outcome: RunOutcome) -> AgentEventKind {
        AgentEventKind::RunCompleted {
            run_id: self.run_id.clone(),
            outcome,
            usage: self.usage.clone(),
        }
    }

    /// One streamed fragment. A stream opens on its first non-empty
    /// delta; item completion closes it.
    fn delta(&mut self, params: &Value, stream: MessageStream) -> Vec<AgentEventKind> {
        let item_id = params["itemId"].as_str().unwrap_or_default().to_owned();
        let text = params["delta"].as_str().unwrap_or_default();
        if text.is_empty() {
            return Vec::new();
        }
        self.streamed.insert(item_id.clone());
        self.open.insert(item_id, stream);
        vec![AgentEventKind::MessageDelta {
            run_id: self.run_id.clone(),
            stream,
            text: text.to_owned(),
        }]
    }

    /// `turn/plan/updated` carries the agent's task list verbatim.
    fn plan_updated(&self, params: &Value) -> Vec<AgentEventKind> {
        let items = params["plan"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|step| {
                // Codex's TurnPlanStepStatus uses camelCase spellings.
                let status = match step["status"].as_str() {
                    Some("pending") => TodoItemStatus::Pending,
                    Some("inProgress") => TodoItemStatus::InProgress,
                    Some("completed") => TodoItemStatus::Completed,
                    _ => return None,
                };
                Some(TodoItem {
                    content: step["step"].as_str().unwrap_or_default().to_owned(),
                    status,
                })
            })
            .collect();
        vec![AgentEventKind::TodosUpdated {
            run_id: self.run_id.clone(),
            items,
        }]
    }

    /// `item/completed`: message items close their streams (with the
    /// full-text fallback when nothing streamed), tool items report
    /// their terminal result, and context compaction returns the
    /// session to working.
    fn item_completed(&mut self, item: &Value) -> Vec<AgentEventKind> {
        let item_id = item["id"].as_str().unwrap_or_default();
        match item["type"].as_str() {
            Some("agentMessage") => {
                let mut events = Vec::new();
                let text = item["text"].as_str().unwrap_or_default();
                // A completed item that never streamed delivers its
                // full text as one delta, matching the CLI session's
                // terminal fill.
                if !text.is_empty() && !self.streamed.contains(item_id) {
                    events.push(AgentEventKind::MessageDelta {
                        run_id: self.run_id.clone(),
                        stream: MessageStream::Assistant,
                        text: text.to_owned(),
                    });
                    self.open
                        .insert(item_id.to_owned(), MessageStream::Assistant);
                }
                if self.open.remove(item_id).is_some() {
                    events.push(AgentEventKind::MessageCompleted {
                        run_id: self.run_id.clone(),
                        stream: MessageStream::Assistant,
                    });
                }
                events
            }
            Some("reasoning") | Some("plan") => self
                .open
                .remove(item_id)
                .map(|_| {
                    vec![AgentEventKind::MessageCompleted {
                        run_id: self.run_id.clone(),
                        stream: MessageStream::Thinking,
                    }]
                })
                .unwrap_or_default(),
            Some("contextCompaction") => vec![AgentEventKind::SessionStatus {
                status: SessionStatus::Working,
            }],
            _ => tool_completed(&self.run_id, item),
        }
    }
}

/// `item/started` projections: tool item kinds announce themselves,
/// context compaction flips the session status, and message items wait
/// for their first delta instead.
fn item_started(run_id: &RunId, item: &Value) -> Vec<AgentEventKind> {
    let tool = tool_label(item);
    match item["type"].as_str() {
        Some("commandExecution") => vec![AgentEventKind::ToolStarted {
            run_id: run_id.clone(),
            tool,
            summary: item["command"].as_str().unwrap_or_default().to_owned(),
        }],
        Some("fileChange") => vec![AgentEventKind::ToolStarted {
            run_id: run_id.clone(),
            tool,
            summary: change_summary(item),
        }],
        Some("mcpToolCall") | Some("dynamicToolCall") | Some("collabAgentToolCall") => {
            vec![AgentEventKind::ToolStarted {
                run_id: run_id.clone(),
                tool,
                summary: compact_json(&item["arguments"]),
            }]
        }
        Some("webSearch") => vec![AgentEventKind::ToolStarted {
            run_id: run_id.clone(),
            tool,
            summary: item["query"].as_str().unwrap_or_default().to_owned(),
        }],
        Some("contextCompaction") => vec![AgentEventKind::SessionStatus {
            status: SessionStatus::Compacting,
        }],
        _ => Vec::new(),
    }
}

/// `item/completed` projections for tool item kinds. `ok` reports the
/// item's own terminal status, never a guess.
fn tool_completed(run_id: &RunId, item: &Value) -> Vec<AgentEventKind> {
    let ok = item["status"].as_str() == Some("completed");
    let tool = tool_label(item);
    match item["type"].as_str() {
        Some("commandExecution") => vec![AgentEventKind::ToolCompleted {
            run_id: run_id.clone(),
            tool,
            ok,
            diff: None,
            output: item["aggregatedOutput"]
                .as_str()
                .filter(|output| !output.is_empty())
                .map(str::to_owned),
        }],
        Some("fileChange") => {
            let changes = item["changes"].as_array();
            match changes {
                Some(changes) if !changes.is_empty() => changes
                    .iter()
                    .map(|change| AgentEventKind::ToolCompleted {
                        run_id: run_id.clone(),
                        tool: tool.clone(),
                        ok,
                        diff: Some(FileDiff {
                            path: PathBuf::from(change["path"].as_str().unwrap_or_default()),
                            patch: change["diff"].as_str().unwrap_or_default().to_owned(),
                        }),
                        output: None,
                    })
                    .collect(),
                _ => vec![AgentEventKind::ToolCompleted {
                    run_id: run_id.clone(),
                    tool,
                    ok,
                    diff: None,
                    output: None,
                }],
            }
        }
        Some("mcpToolCall") => vec![AgentEventKind::ToolCompleted {
            run_id: run_id.clone(),
            tool,
            ok,
            diff: None,
            output: nonempty_json(
                item.get("result")
                    .filter(|result| !result.is_null())
                    .or_else(|| item.get("error")),
            ),
        }],
        Some("dynamicToolCall") | Some("collabAgentToolCall") => {
            vec![AgentEventKind::ToolCompleted {
                run_id: run_id.clone(),
                tool,
                ok,
                diff: None,
                output: nonempty_json(
                    item.get("contentItems")
                        .or_else(|| item.get("output"))
                        .or_else(|| item.get("result")),
                ),
            }]
        }
        Some("webSearch") => vec![AgentEventKind::ToolCompleted {
            run_id: run_id.clone(),
            tool,
            ok: true,
            diff: None,
            output: nonempty_json(item.get("results")),
        }],
        _ => Vec::new(),
    }
}

/// The stable tool label hosts display: the provider-native capability
/// spelled in the same terms the one-shot path reports.
fn tool_label(item: &Value) -> String {
    match item["type"].as_str() {
        Some("commandExecution") => "command_execution".to_owned(),
        Some("fileChange") => "file_change".to_owned(),
        Some("webSearch") => "web_search".to_owned(),
        Some("mcpToolCall") => format!(
            "mcp:{}.{}",
            item["server"].as_str().unwrap_or_default(),
            item["tool"].as_str().unwrap_or_default()
        ),
        Some("dynamicToolCall") => item["tool"].as_str().unwrap_or("dynamic_tool").to_owned(),
        Some("collabAgentToolCall") => {
            format!("collab:{}", item["tool"].as_str().unwrap_or("agent"))
        }
        Some(other) => other.to_owned(),
        None => "tool".to_owned(),
    }
}

/// A one-line summary for a `fileChange` item at start time: the paths
/// already known, or a generic label when none are populated yet.
fn change_summary(item: &Value) -> String {
    let paths = item["changes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|change| change["path"].as_str())
        .collect::<Vec<_>>();
    if paths.is_empty() {
        "file change".to_owned()
    } else {
        format!("file change: {}", paths.join(", "))
    }
}

/// Compact JSON for tool arguments and results. Bounds to one line so
/// summaries stay summaries.
fn compact_json(value: &Value) -> String {
    if value.is_null() {
        return String::new();
    }
    let mut text = serde_json::to_string(value).unwrap_or_default();
    text.truncate(200);
    text
}

/// A serialized JSON payload for `tool.completed` output, skipping
/// missing and null values.
fn nonempty_json(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if value.is_null() {
        return None;
    }
    serde_json::to_string(value)
        .ok()
        .filter(|text| text != "null")
}

/// The per-turn token accounting bucket `thread/tokenUsage/updated`
/// reports. Missing counts stay `None`, never reported as zero.
fn token_usage(last: &Value) -> Option<TokenUsage> {
    if last.is_null() {
        return None;
    }
    let usage = TokenUsage {
        input_tokens: last["inputTokens"].as_u64(),
        output_tokens: last["outputTokens"].as_u64(),
    };
    (usage.input_tokens.is_some() || usage.output_tokens.is_some()).then_some(usage)
}

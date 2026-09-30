//! Pure mapping from ACP wire frames to [`AgentEventKind`].
//!
//! One [`TurnEvents`] holds the per-turn projection state: which message
//! streams are still open (so a finished turn closes them), which tool
//! calls have been announced (so an update can finish one that never
//! got a `tool_call` start), and the token usage the last
//! `usage_update` reported. Every method is a pure projection of the
//! protocol payload; the driver emits the returned kinds in order.

use super::session::AcpSession;
use aifuel_core::{
    AgentEventKind, MessageStream, RunId, RunOutcome, TodoItem, TodoItemStatus, TokenUsage,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

/// The `method` of a request or notification frame.
pub(super) fn method(message: &Value) -> Option<&str> {
    message.get("method").and_then(Value::as_str)
}

/// Whether the frame is a server-initiated request: it carries both a
/// `method` and an `id` the client must answer.
pub(super) fn is_server_request(message: &Value) -> bool {
    message.get("method").is_some() && message.get("id").is_some()
}

/// Whether the frame is the response to the client request `id`.
pub(super) fn is_response_for(message: &Value, id: &Value) -> bool {
    message.get("method").is_none()
        && message.get("id") == Some(id)
        && (message.get("result").is_some() || message.get("error").is_some())
}

/// The numeric id of a response frame (`id` present, no `method`), so
/// the dispatcher can match it to a pending client request.
pub(super) fn response_id(message: &Value) -> Option<u64> {
    if message.get("method").is_some() {
        return None;
    }
    message.get("id").and_then(Value::as_u64)
}

/// The `result` payload of a response, or the error message string.
pub(super) fn response_result(message: &Value) -> Result<Value, String> {
    if let Some(error) = message.get("error").filter(|error| !error.is_null()) {
        let detail = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the agent returned an error");
        let code = error.get("code").and_then(Value::as_i64);
        return Err(match code {
            Some(code) => format!("{detail} (code {code})"),
            None => detail.to_owned(),
        });
    }
    Ok(message.get("result").cloned().unwrap_or(Value::Null))
}

/// The `session/prompt` `stopReason` as a run outcome. A reason the
/// contract cannot name is a failed run with the reason surfaced in an
/// error event by the caller.
pub(super) fn prompt_outcome(stop_reason: &str) -> RunOutcome {
    match stop_reason {
        "cancelled" => RunOutcome::Cancelled,
        "end_turn" => RunOutcome::Success,
        // `max_tokens`, `max_turn_requests`, `refusal`, and unknown
        // spellings are all provider-reported non-success endings.
        _ => RunOutcome::Failed,
    }
}

/// Whether a non-success, non-cancelled stop reason carries an error
/// worth surfacing as an `error` fact before `run.completed`.
pub(super) fn prompt_error(stop_reason: &str) -> Option<String> {
    match stop_reason {
        "end_turn" | "cancelled" => None,
        other => Some(format!(
            "the agent ended the prompt with stop reason {other:?}"
        )),
    }
}

/// Per-turn projection from `session/update` notifications to event
/// kinds.
pub(super) struct TurnEvents {
    run_id: RunId,
    /// The last `usage_update` bucket, attached to `run.completed`.
    usage: Option<TokenUsage>,
    /// Message streams still open; a finished turn closes them. At most
    /// the two streams exist.
    open: Vec<MessageStream>,
    /// Tool calls in flight, by `toolCallId`.
    tools: BTreeMap<String, ToolState>,
}

/// One announced tool call.
struct ToolState {
    /// The stable tool label hosts display.
    tool: String,
    /// A terminal status already arrived.
    done: bool,
}

impl TurnEvents {
    pub fn new(run_id: RunId) -> Self {
        Self {
            run_id,
            usage: None,
            open: Vec::new(),
            tools: BTreeMap::new(),
        }
    }

    pub fn run_id(&self) -> &RunId {
        &self.run_id
    }

    /// The terminal events for a finished turn: open message streams
    /// close first, then the run's outcome.
    pub fn finish_events(self, outcome: RunOutcome) -> Vec<AgentEventKind> {
        let mut events: Vec<AgentEventKind> = self
            .open
            .iter()
            .map(|stream| AgentEventKind::MessageCompleted {
                run_id: self.run_id.clone(),
                stream: *stream,
            })
            .collect();
        events.push(AgentEventKind::RunCompleted {
            run_id: self.run_id.clone(),
            outcome,
            usage: self.usage.clone(),
        });
        events
    }

    /// Map one `session/update` payload into event kinds, in order.
    /// `session` is touched only for `config_option_update`, which keeps
    /// the session's advertised model selector current.
    pub fn apply_update(
        &mut self,
        update: &Value,
        session: &Arc<AcpSession>,
    ) -> Vec<AgentEventKind> {
        match update.get("sessionUpdate").and_then(Value::as_str) {
            Some("agent_message_chunk") => self.chunk(update, MessageStream::Assistant),
            Some("agent_thought_chunk") => self.chunk(update, MessageStream::Thinking),
            Some("plan") => self.plan(update),
            Some("tool_call") => self.tool_call(update),
            Some("tool_call_update") => self.tool_call_update(update),
            Some("usage_update") => {
                self.usage = usage(update);
                Vec::new()
            }
            Some("config_option_update") => {
                if let Some(options) = update.get("configOptions")
                    && let Some(option) = super::interactions::model_option(options)
                {
                    session
                        .state
                        .lock()
                        .expect("session state mutex")
                        .model_option = Some(option);
                }
                Vec::new()
            }
            // `user_message_chunk` replays the client's own message;
            // `available_commands_update`, `current_mode_update`, and
            // `session_info_update` carry no contract projection here.
            _ => Vec::new(),
        }
    }

    /// One streamed text fragment. A stream opens on its first
    /// non-empty delta and closes when the turn finishes.
    fn chunk(&mut self, update: &Value, stream: MessageStream) -> Vec<AgentEventKind> {
        let text = update
            .get("content")
            .and_then(|content| content.get("text"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if text.is_empty() {
            return Vec::new();
        }
        if !self.open.contains(&stream) {
            self.open.push(stream);
        }
        vec![AgentEventKind::MessageDelta {
            run_id: self.run_id.clone(),
            stream,
            text: text.to_owned(),
        }]
    }

    /// `plan` carries the agent's task list verbatim.
    fn plan(&self, update: &Value) -> Vec<AgentEventKind> {
        let items = update
            .get("entries")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                let status = match entry.get("status").and_then(Value::as_str) {
                    Some("pending") => TodoItemStatus::Pending,
                    Some("in_progress") => TodoItemStatus::InProgress,
                    Some("completed") => TodoItemStatus::Completed,
                    _ => return None,
                };
                Some(TodoItem {
                    content: entry
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    status,
                })
            })
            .collect();
        vec![AgentEventKind::TodosUpdated {
            run_id: self.run_id.clone(),
            items,
        }]
    }

    /// A new `tool_call`: announce it, and finish it immediately when
    /// the first report already carries a terminal status.
    fn tool_call(&mut self, update: &Value) -> Vec<AgentEventKind> {
        // The call's fields sit directly on the update object; a nested
        // `toolCall` object is tolerated for agents that wrap them.
        let call = update.get("toolCall").unwrap_or(update);
        let id = call
            .get("toolCallId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let tool = tool_label(call);
        let summary = call
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let status = call.get("status").and_then(Value::as_str);
        let done = matches!(status, Some("completed") | Some("failed"));
        self.tools.insert(
            id.clone(),
            ToolState {
                tool: tool.clone(),
                done,
            },
        );
        let mut events = vec![AgentEventKind::ToolStarted {
            run_id: self.run_id.clone(),
            tool,
            summary,
        }];
        if done {
            events.push(self.complete_call(&id, call));
        }
        events
    }

    /// A `tool_call_update`: terminal statuses finish the call;
    /// progress fields carry no contract projection.
    fn tool_call_update(&mut self, update: &Value) -> Vec<AgentEventKind> {
        let id = update
            .get("toolCallId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let status = update.get("status").and_then(Value::as_str);
        let done = matches!(status, Some("completed") | Some("failed"));
        let known = self.tools.get(&id).map(|state| state.done);
        match (known, done) {
            // A terminal update on an announced call finishes it.
            (Some(false), true) => {
                if let Some(state) = self.tools.get_mut(&id) {
                    state.done = true;
                }
                vec![self.complete_call(&id, update)]
            }
            // An update for a call the agent never announced still
            // reports its start and finish; nothing is dropped.
            (None, true) => {
                let tool = tool_label(update);
                let summary = update
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                self.tools.insert(
                    id.clone(),
                    ToolState {
                        tool: tool.clone(),
                        done: true,
                    },
                );
                vec![
                    AgentEventKind::ToolStarted {
                        run_id: self.run_id.clone(),
                        tool,
                        summary,
                    },
                    self.complete_call(&id, update),
                ]
            }
            _ => Vec::new(),
        }
    }

    /// The `tool.completed` fact for one call. `ok` reports the call's
    /// own terminal status, never a guess; output carries `rawOutput`
    /// or the call's text content when present.
    fn complete_call(&mut self, id: &str, payload: &Value) -> AgentEventKind {
        let state = self.tools.get(id);
        let tool = state
            .map(|state| state.tool.clone())
            .unwrap_or_else(|| tool_label(payload));
        let ok = payload.get("status").and_then(Value::as_str) == Some("completed");
        let output = nonempty_json(payload.get("rawOutput")).or_else(|| {
            let texts: Vec<String> = payload
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|content| content.get("type").and_then(Value::as_str) == Some("content"))
                .filter_map(|content| {
                    content
                        .get("content")
                        .and_then(|inner| inner.get("text"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .collect();
            (!texts.is_empty()).then(|| texts.join("\n"))
        });
        AgentEventKind::ToolCompleted {
            run_id: self.run_id.clone(),
            tool,
            ok,
            diff: None,
            output,
        }
    }
}

/// The stable tool label hosts display: the ACP `kind` spelled in the
/// same terms the protocol reports, falling back to the call's title.
fn tool_label(call: &Value) -> String {
    call.get("kind")
        .and_then(Value::as_str)
        .filter(|kind| !kind.is_empty())
        .map(str::to_owned)
        .or_else(|| call.get("title").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| "tool".to_owned())
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

/// `usage_update` reports the context window's used tokens; the
/// contract's `input_tokens` carries it and `output_tokens` stays
/// unknown rather than guessed.
fn usage(update: &Value) -> Option<TokenUsage> {
    let used = update.get("used").and_then(Value::as_u64)?;
    Some(TokenUsage {
        input_tokens: Some(used),
        output_tokens: None,
    })
}

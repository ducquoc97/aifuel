use super::*;

pub(super) fn ordinary_input_response(
    pending: &PendingRunInput,
    response: serde_json::Value,
) -> Result<AgentInteractionResponse, RunManagementError> {
    let input_error =
        |message: &str| RunManagementError::new(RunManagementErrorCode::InputConflict, message);
    if pending.interaction_kind == AgentInteractionKind::McpElicitation {
        if response.is_object() {
            return Ok(AgentInteractionResponse::Elicitation(response));
        }
        return Err(input_error(
            "MCP elicitation responses must be JSON objects",
        ));
    }
    if pending.interaction_kind != AgentInteractionKind::OrdinaryInput {
        return Err(input_error(
            "this provider interaction does not accept ordinary input",
        ));
    }

    let answers = match response {
        serde_json::Value::String(answer) => {
            if pending.question_ids.len() > 1 {
                return Err(input_error(
                    "this request has multiple questions; provide an object keyed by question ID",
                ));
            }
            let question = pending
                .question_ids
                .first()
                .cloned()
                .unwrap_or_else(|| "answer".to_owned());
            BTreeMap::from([(question, vec![answer])])
        }
        serde_json::Value::Object(object) => {
            let mut answers = BTreeMap::new();
            for (question, value) in object {
                if !pending.question_ids.is_empty()
                    && !pending.question_ids.iter().any(|id| id == &question)
                {
                    return Err(input_error("response contains an unknown question ID"));
                }
                let values = match value {
                    serde_json::Value::String(answer) => vec![answer],
                    serde_json::Value::Array(values) => values
                        .into_iter()
                        .map(|value| match value {
                            serde_json::Value::String(answer) => Ok(answer),
                            _ => Err(input_error("question answers must be strings")),
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                    _ => {
                        return Err(input_error(
                            "question answers must be strings or string arrays",
                        ));
                    }
                };
                answers.insert(question, values);
            }
            if answers.is_empty() {
                return Err(input_error("at least one question answer is required"));
            }
            answers
        }
        _ => {
            return Err(input_error(
                "ordinary input response must be a string or a question-answer object",
            ));
        }
    };
    Ok(AgentInteractionResponse::Answers(answers))
}

pub(super) fn map_validation_error(error: AgentRunError) -> RunManagementError {
    match error {
        AgentRunError::InvalidRequest(message) => RunManagementError::invalid_request(message),
        AgentRunError::UnsupportedProvider(provider) => RunManagementError::new(
            RunManagementErrorCode::AgentUnavailable,
            format!("provider {provider} has no registered Agent Integration"),
        ),
        AgentRunError::Timeout(message) => {
            RunManagementError::new(RunManagementErrorCode::ConnectionTimeout, message)
        }
        AgentRunError::Cancelled => RunManagementError::invalid_request("request was cancelled"),
        AgentRunError::Io(error) => RunManagementError::new(
            RunManagementErrorCode::AgentUnavailable,
            format!("Agent Integration is unavailable: {error}"),
        ),
    }
}

pub(super) fn event_size(event: &RunEvent) -> usize {
    event
        .data
        .as_ref()
        .map_or(0, String::len)
        .saturating_add(96)
}

pub(super) fn encode_cursor(run_id: &str, sequence: u64) -> String {
    let owner = run_id
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{owner}.{sequence:x}")
}

pub(super) fn decode_cursor(run_id: &str, cursor: &str) -> Result<u64, RunManagementError> {
    let (owner, sequence) = cursor
        .split_once('.')
        .ok_or_else(|| RunManagementError::invalid_cursor("event cursor is malformed"))?;
    let expected = run_id
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if owner != expected || sequence.is_empty() {
        return Err(RunManagementError::invalid_cursor(
            "event cursor belongs to another run or is malformed",
        ));
    }
    u64::from_str_radix(sequence, 16)
        .map_err(|_| RunManagementError::invalid_cursor("event cursor sequence is malformed"))
}

pub(super) fn bound_content(value: Option<String>, limit: usize) -> (Option<String>, bool, usize) {
    let Some(value) = value else {
        return (None, false, 0);
    };
    let bytes = value.len();
    let (value, truncated) = truncate_string(Some(value), limit);
    (value, truncated, bytes)
}

pub(super) fn truncate_string(value: Option<String>, limit: usize) -> (Option<String>, bool) {
    let Some(mut value) = value else {
        return (None, false);
    };
    if value.len() <= limit {
        return (Some(value), false);
    }
    let end = utf8_prefix_len(&value, limit);
    value.truncate(end);
    (Some(value), true)
}

pub(super) fn utf8_prefix_len(value: &str, limit: usize) -> usize {
    let mut end = limit.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    end
}

pub(super) fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

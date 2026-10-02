use crate::ResolvedAuth;
use crate::wire::anthropic_messages::{
    classify_event, messages_url, request_body, request_headers,
};
use crate::wire::stream::{DataVerdict, StreamEnd, drive_stream};
use aifuel_core::{EndpointConfig, KeyDelivery, TokenUsage};
use reqwest::header::{AUTHORIZATION, HeaderName};
use std::collections::BTreeMap;
use std::str::FromStr;

fn endpoint() -> EndpointConfig {
    EndpointConfig {
        base_url: "https://api.anthropic.com".to_owned(),
        extra_headers: BTreeMap::new(),
        request_timeout_seconds: None,
    }
}

fn text_delta(text: &str) -> String {
    serde_json::json!({
        "type": "content_block_delta",
        "index": 0,
        "delta": {"type": "text_delta", "text": text},
    })
    .to_string()
}

#[test]
fn messages_url_appends_the_versioned_path() {
    assert_eq!(
        messages_url("https://api.anthropic.com"),
        "https://api.anthropic.com/v1/messages"
    );
    assert_eq!(
        messages_url("https://gateway.example/anthropic/"),
        "https://gateway.example/anthropic/v1/messages"
    );
}

#[test]
fn request_body_carries_the_required_max_tokens_and_stream_flag() {
    // The Messages API rejects a request without `max_tokens`; the run
    // contract has no token budget, so the adapter declares one.
    let body = request_body("claude-stub-4", "say hi");
    assert_eq!(body["model"], "claude-stub-4");
    assert_eq!(body["stream"], true);
    assert!(
        body["max_tokens"].as_u64().is_some_and(|max| max >= 4_096),
        "max_tokens must be present and usable, got {}",
        body["max_tokens"]
    );
    assert_eq!(
        body["messages"],
        serde_json::json!([{"role": "user", "content": "say hi"}])
    );
}

#[test]
fn request_headers_declare_the_messages_api_version() {
    let headers = request_headers(&endpoint(), &ResolvedAuth::None).unwrap();
    assert_eq!(headers["anthropic-version"], "2023-06-01");
    assert_eq!(headers["accept"], "text/event-stream");
}

#[test]
fn api_key_delivers_through_x_api_key_not_bearer() {
    let auth = ResolvedAuth::ApiKey {
        key: "k".to_owned(),
        delivery: KeyDelivery::Header {
            name: "x-api-key".to_owned(),
        },
    };
    let headers = request_headers(&endpoint(), &auth).unwrap();
    assert_eq!(headers["x-api-key"], "k");
    assert!(!headers.contains_key(AUTHORIZATION));
    assert_eq!(headers["anthropic-version"], "2023-06-01");
}

#[test]
fn managed_auth_is_applied_after_configured_headers() {
    // A configured `x-api-key` header cannot override the managed binding.
    let mut endpoint = endpoint();
    endpoint
        .extra_headers
        .insert("x-api-key".to_owned(), "smuggled".to_owned());
    let auth = ResolvedAuth::ApiKey {
        key: "managed-key".to_owned(),
        delivery: KeyDelivery::Header {
            name: "x-api-key".to_owned(),
        },
    };
    let headers = request_headers(&endpoint, &auth).unwrap();
    let name = HeaderName::from_str("x-api-key").unwrap();
    assert_eq!(headers.get_all(&name).iter().count(), 1);
    assert_eq!(headers[&name], "managed-key");
}

#[test]
fn message_start_reports_model_and_input_usage() {
    let data = serde_json::json!({
        "type": "message_start",
        "message": {
            "id": "msg_1",
            "model": "claude-stub-4",
            "usage": {"input_tokens": 25, "output_tokens": 1},
        },
    });
    assert_eq!(
        classify_event(Some("message_start"), &data.to_string()),
        DataVerdict::Usage {
            usage: TokenUsage {
                input_tokens: Some(25),
                output_tokens: Some(1),
            },
            model: Some("claude-stub-4".to_owned()),
        }
    );
}

#[test]
fn text_deltas_yield_their_text() {
    assert_eq!(
        classify_event(Some("content_block_delta"), &text_delta("Hello")),
        DataVerdict::Delta {
            text: "Hello".to_owned(),
            terminal: false,
            model: None,
            usage: None,
        }
    );
}

#[test]
fn non_text_deltas_and_framing_events_are_ignored() {
    // The adapter sends no tools or thinking config, so tool/json and
    // thinking deltas never carry answer text.
    let json_delta = serde_json::json!({
        "type": "content_block_delta",
        "index": 0,
        "delta": {"type": "input_json_delta", "partial_json": "{\"x\":"},
    });
    for data in [
        json_delta.to_string(),
        serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}).to_string(),
        serde_json::json!({"type": "content_block_stop", "index": 0}).to_string(),
        serde_json::json!({"type": "ping"}).to_string(),
        serde_json::json!({"type": "a_future_event", "x": 1}).to_string(),
    ] {
        assert_eq!(
            classify_event(None, &data),
            DataVerdict::Ignored { model: None },
            "{data}"
        );
    }
}

#[test]
fn message_delta_with_a_stop_reason_finishes_the_turn() {
    // stop_reason is the finish verdict; cumulative output_tokens ride
    // along as the final accounting.
    let data = serde_json::json!({
        "type": "message_delta",
        "delta": {"stop_reason": "end_turn", "stop_sequence": null},
        "usage": {"output_tokens": 15},
    });
    assert_eq!(
        classify_event(Some("message_delta"), &data.to_string()),
        DataVerdict::Finish {
            model: None,
            usage: Some(TokenUsage {
                input_tokens: None,
                output_tokens: Some(15),
            }),
        }
    );
}

#[test]
fn message_delta_without_a_stop_reason_is_accounting_only() {
    let data = serde_json::json!({
        "type": "message_delta",
        "delta": {},
        "usage": {"output_tokens": 9},
    });
    assert_eq!(
        classify_event(None, &data.to_string()),
        DataVerdict::Usage {
            usage: TokenUsage {
                input_tokens: None,
                output_tokens: Some(9),
            },
            model: None,
        }
    );
}

#[test]
fn message_stop_is_the_terminator() {
    let data = serde_json::json!({"type": "message_stop"});
    assert_eq!(
        classify_event(Some("message_stop"), &data.to_string()),
        DataVerdict::Complete {
            model: None,
            usage: None,
        }
    );
}

#[test]
fn error_events_fail_the_run() {
    // Anthropic names the event `error` and the payload type `error`; a
    // compatible server may send only one of the two.
    let payload = serde_json::json!({
        "type": "error",
        "error": {"type": "overloaded_error", "message": "Overloaded"},
    });
    for (event, data) in [
        (Some("error"), payload.to_string()),
        (None, payload.to_string()),
    ] {
        assert_eq!(
            classify_event(event, &data),
            DataVerdict::Failed {
                message: "Overloaded".to_owned()
            },
            "event={event:?}"
        );
    }
}

#[test]
fn unparseable_payload_fails_the_run() {
    assert!(matches!(
        classify_event(None, "not-json"),
        DataVerdict::Failed { .. }
    ));
}

#[test]
fn a_framed_full_message_is_tolerated() {
    // A compatible server that ignores `stream` can answer one `message`
    // payload: its text blocks still land as the answer, marked terminal.
    let data = serde_json::json!({
        "type": "message",
        "model": "claude-stub-4",
        "content": [
            {"type": "text", "text": "all "},
            {"type": "text", "text": "at once"},
        ],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 12, "output_tokens": 4},
    });
    assert_eq!(
        classify_event(None, &data.to_string()),
        DataVerdict::Delta {
            text: "all at once".to_owned(),
            terminal: true,
            model: Some("claude-stub-4".to_owned()),
            usage: Some(TokenUsage {
                input_tokens: Some(12),
                output_tokens: Some(4),
            }),
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_full_anthropic_stream_completes_with_merged_usage() {
    // The documented sequence end to end: usage splits across
    // `message_start` (input) and `message_delta` (output), and must merge
    // into one TokenUsage on the run.
    let frames = [
        serde_json::json!({"type": "message_start", "message": {"model": "claude-stub-4", "usage": {"input_tokens": 25, "output_tokens": 1}}}),
        serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hel"}}),
        serde_json::json!({"type": "ping"}),
        serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "lo"}}),
        serde_json::json!({"type": "content_block_stop", "index": 0}),
        serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": null}, "usage": {"output_tokens": 15}}),
        serde_json::json!({"type": "message_stop"}),
    ];
    let mut stream = ScriptedChunks(
        frames
            .iter()
            .map(|frame| {
                format!(
                    "event: {}\ndata: {frame}\n\n",
                    frame["type"].as_str().unwrap()
                )
                .into_bytes()
            })
            .collect(),
    );
    let outcome = drive_stream(
        &mut stream,
        crate::wire::anthropic_messages::classify_event,
        None,
        std::time::Duration::from_secs(60),
        std::time::Duration::from_millis(10),
        &aifuel_core::RunCancellationToken::new(),
        None,
    )
    .await;
    assert_eq!(outcome.end, StreamEnd::Completed);
    assert_eq!(outcome.output, "Hello");
    assert_eq!(outcome.model.as_deref(), Some("claude-stub-4"));
    assert_eq!(
        outcome.usage,
        Some(TokenUsage {
            input_tokens: Some(25),
            output_tokens: Some(15),
        })
    );
}

/// A `ChunkStream` over pre-framed SSE bytes.
struct ScriptedChunks(std::collections::VecDeque<Vec<u8>>);

impl crate::wire::stream::ChunkStream for ScriptedChunks {
    async fn next_chunk(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        Ok(self.0.pop_front())
    }
}

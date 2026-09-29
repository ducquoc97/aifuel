use crate::wire::openai_chat::{DataVerdict, classify_event, completions_url, request_body};

fn delta(text: &str) -> String {
    serde_json::json!({"choices": [{"index": 0, "delta": {"content": text}}]}).to_string()
}

#[test]
fn delta_chunks_yield_their_text() {
    let verdict = classify_event(None, &delta("Hello"));
    assert_eq!(
        verdict,
        DataVerdict::Delta {
            text: "Hello".to_owned(),
            terminal: false,
            model: None,
            usage: None,
        }
    );
}

#[test]
fn done_payload_is_terminal() {
    assert_eq!(
        classify_event(None, "[DONE]"),
        DataVerdict::Complete {
            model: None,
            usage: None,
        }
    );
}

#[test]
fn finish_reason_is_terminal_even_with_content() {
    // Some providers attach text to the finishing chunk; both the content
    // and the terminal signal must be honored.
    let data =
        serde_json::json!({"choices": [{"index": 0, "delta": {"content": "tail"}, "finish_reason": "stop"}]}).to_string();
    assert_eq!(
        classify_event(None, &data),
        DataVerdict::Delta {
            text: "tail".to_owned(),
            terminal: true,
            model: None,
            usage: None,
        }
    );
}

#[test]
fn bare_finish_reason_marks_the_turn_done() {
    // A finish chunk ends the turn, not the stream: `include_usage` peers
    // still send a usage-only chunk before `[DONE]`.
    let data = serde_json::json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]})
        .to_string();
    assert_eq!(
        classify_event(None, &data),
        DataVerdict::Finish {
            model: None,
            usage: None,
        }
    );
}

#[test]
fn provider_error_object_fails_the_run() {
    let data = serde_json::json!({"error": {"message": "model not found", "code": 404}});
    assert_eq!(
        classify_event(None, &data.to_string()),
        DataVerdict::Failed {
            message: "model not found".to_owned()
        }
    );
}

#[test]
fn provider_error_string_fails_the_run() {
    let data = serde_json::json!({"error": "overloaded"});
    assert_eq!(
        classify_event(None, &data.to_string()),
        DataVerdict::Failed {
            message: "overloaded".to_owned()
        }
    );
}

#[test]
fn named_error_event_fails_the_run() {
    // Some compatible servers dispatch `event: error` rather than an
    // unnamed payload.
    let data = serde_json::json!({"message": "boom"});
    assert_eq!(
        classify_event(Some("error"), &data.to_string()),
        DataVerdict::Failed {
            message: data.to_string()
        }
    );
}

#[test]
fn role_priming_and_unmapped_usage_chunks_are_ignored() {
    // First chunks commonly carry only `delta.role`. Usage objects without
    // recognized fields (only `total_tokens` here) still classify as Ignored;
    // recognized prompt/completion accounting is a `Usage` verdict.
    let role = serde_json::json!({"choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": null}]});
    assert_eq!(
        classify_event(None, &role.to_string()),
        DataVerdict::Ignored { model: None }
    );
    let usage = serde_json::json!({"choices": [], "usage": {"total_tokens": 9}});
    assert_eq!(
        classify_event(None, &usage.to_string()),
        DataVerdict::Ignored { model: None }
    );
}

#[test]
fn a_usage_only_chunk_reports_accounting() {
    let data = serde_json::json!({"model": "m", "choices": [], "usage": {"prompt_tokens": 7, "completion_tokens": 3}});
    assert_eq!(
        classify_event(None, &data.to_string()),
        DataVerdict::Usage {
            usage: aifuel_core::TokenUsage {
                input_tokens: Some(7),
                output_tokens: Some(3),
            },
            model: Some("m".to_owned()),
        }
    );
}

#[test]
fn a_finish_chunk_with_inline_usage_keeps_both() {
    // Compatible endpoints sometimes put `usage` on the finish chunk itself;
    // the accounting must not be discarded with the terminal signal.
    let data = serde_json::json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 4, "completion_tokens": 2}});
    assert_eq!(
        classify_event(None, &data.to_string()),
        DataVerdict::Finish {
            model: None,
            usage: Some(aifuel_core::TokenUsage {
                input_tokens: Some(4),
                output_tokens: Some(2),
            }),
        }
    );
}

#[test]
fn unparseable_payload_fails_the_run() {
    // A non-JSON `data:` payload is a broken stream, not noise to skip.
    assert!(matches!(
        classify_event(None, "not-json"),
        DataVerdict::Failed { .. }
    ));
}

#[test]
fn non_streamed_message_shape_is_tolerated() {
    // A compatible server that ignores `stream` answers with
    // `message.content`; the text is still recoverable.
    let data = serde_json::json!({"choices": [{"index": 0, "message": {"role": "assistant", "content": "all at once"}, "finish_reason": "stop"}]});
    assert_eq!(
        classify_event(None, &data.to_string()),
        DataVerdict::Delta {
            text: "all at once".to_owned(),
            terminal: true,
            model: None,
            usage: None,
        }
    );
}

#[test]
fn chunk_model_id_is_observed() {
    let data = serde_json::json!({"model": "llama3", "choices": [{"index": 0, "delta": {"content": "x"}}]});
    assert_eq!(
        classify_event(None, &data.to_string()),
        DataVerdict::Delta {
            text: "x".to_owned(),
            terminal: false,
            model: Some("llama3".to_owned()),
            usage: None,
        }
    );
}

#[test]
fn completions_url_tolerates_trailing_slashes() {
    assert_eq!(
        completions_url("http://localhost:11434/v1"),
        "http://localhost:11434/v1/chat/completions"
    );
    assert_eq!(
        completions_url("http://localhost:11434/v1/"),
        "http://localhost:11434/v1/chat/completions"
    );
}

#[test]
fn request_body_is_a_streaming_single_message() {
    let body = request_body("llama3", "say hi");
    assert_eq!(body["model"], "llama3");
    assert_eq!(body["stream"], true);
    assert_eq!(
        body["messages"],
        serde_json::json!([{"role": "user", "content": "say hi"}])
    );
}

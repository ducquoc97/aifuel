//! Pure-function coverage of the wire shapes and frame parsing.
//! These pin the provider spellings the driver depends on.

use super::super::protocol::{self, Frame, ToolAnswer};
use aifuel_core::{AccessMode, ApprovalDecision, ApprovalKind, Effort, MessageStream};
use serde_json::{Value, json};

fn parsed(line: &str) -> Value {
    serde_json::from_str(line).expect("test frames are valid JSON")
}

#[test]
fn spawn_args_wire_the_stream_json_session() {
    let args = protocol::spawn_args(
        Some("claude-sonnet-4-5"),
        Some(Effort::High),
        AccessMode::WorkspaceWrite,
        Some("sess-prev"),
    );
    let expected = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--permission-prompt-tool",
        "stdio",
        "--permission-mode",
        "acceptEdits",
        "--model",
        "claude-sonnet-4-5",
        "--effort",
        "high",
        "--resume",
        "sess-prev",
    ];
    assert_eq!(args, expected);
}

#[test]
fn access_mode_maps_to_claude_permission_modes() {
    for (access, expected) in [
        (AccessMode::ReadOnly, "plan"),
        (AccessMode::WorkspaceWrite, "acceptEdits"),
        (AccessMode::Full, "bypassPermissions"),
    ] {
        let args = protocol::spawn_args(None, None, access, None);
        let position = args
            .iter()
            .position(|arg| arg == "--permission-mode")
            .expect("the flag is present");
        assert_eq!(args[position + 1], expected);
    }
}

#[test]
fn user_message_is_one_text_block_user_frame() {
    let value = parsed(&protocol::user_message(Some("sess-1"), "hello world"));
    assert_eq!(value["type"], "user");
    assert_eq!(value["session_id"], "sess-1");
    assert_eq!(value["message"]["role"], "user");
    assert_eq!(
        value["message"]["content"],
        json!([{"type": "text", "text": "hello world"}])
    );
    assert!(value["parent_tool_use_id"].is_null());
}

#[test]
fn control_lines_serialize_in_the_provider_shape() {
    let init = parsed(&protocol::initialize_request("init-9"));
    assert_eq!(init["type"], "control_request");
    assert_eq!(init["request"]["subtype"], "initialize");

    let interrupt = parsed(&protocol::interrupt_request("int-9"));
    assert_eq!(interrupt["type"], "control_request");
    assert_eq!(interrupt["request_id"], "int-9");
    assert_eq!(interrupt["request"]["subtype"], "interrupt");

    let set_model = parsed(&protocol::set_model_request("m-1", Some("claude-opus")));
    assert_eq!(set_model["request"]["subtype"], "set_model");
    assert_eq!(set_model["request"]["model"], "claude-opus");

    let allow = parsed(&protocol::permission_response(
        "req-1",
        true,
        &json!({"file_path": "/tmp/x"}),
        "",
    ));
    assert_eq!(allow["type"], "control_response");
    assert_eq!(allow["response"]["request_id"], "req-1");
    assert_eq!(allow["response"]["response"]["behavior"], "allow");
    assert_eq!(
        allow["response"]["response"]["updatedInput"],
        json!({"file_path": "/tmp/x"})
    );

    let deny = parsed(&protocol::permission_response(
        "req-2",
        false,
        &json!({}),
        "no",
    ));
    assert_eq!(deny["response"]["response"]["behavior"], "deny");
    assert_eq!(deny["response"]["response"]["message"], "no");

    let error = parsed(&protocol::error_response("req-3", "not supported"));
    assert_eq!(error["response"]["subtype"], "error");
    assert_eq!(error["response"]["error"], "not supported");
}

#[test]
fn init_frame_yields_the_resume_cursor() {
    let frame = protocol::parse_frame(&parsed(
        "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"sess-9\",\"model\":\"m\"}",
    ));
    let Frame::Init { session_id } = frame else {
        panic!("expected an init frame, got {frame:?}")
    };
    assert_eq!(session_id.as_deref(), Some("sess-9"));
}

#[test]
fn assistant_blocks_split_by_type() {
    let frame = protocol::parse_frame(&parsed(
        "{\"type\":\"assistant\",\"message\":{\"content\":[\
         {\"type\":\"text\",\"text\":\"answer\"},\
         {\"type\":\"thinking\",\"thinking\":\"plan\"},\
         {\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"Write\",\"input\":{\"file_path\":\"x\"}}]}}",
    ));
    let Frame::Assistant { blocks } = frame else {
        panic!("expected an assistant frame, got {frame:?}")
    };
    assert_eq!(blocks.len(), 3);
    assert!(matches!(&blocks[0], protocol::Block::Text(text) if text == "answer"));
    assert!(matches!(&blocks[1], protocol::Block::Thinking(text) if text == "plan"));
    let protocol::Block::ToolUse {
        tool_use_id, name, ..
    } = &blocks[2]
    else {
        panic!("expected a tool_use block")
    };
    assert_eq!(tool_use_id, "toolu_1");
    assert_eq!(name, "Write");
}

#[test]
fn stream_event_deltas_become_message_streams() {
    let text = protocol::parse_frame(&parsed(
        "{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\
         \"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}}",
    ));
    assert!(matches!(text, Frame::TextDelta(t) if t == "Hello"));

    let thinking = protocol::parse_frame(&parsed(
        "{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\
         \"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"step\"}}}",
    ));
    assert!(matches!(thinking, Frame::ThinkingDelta(t) if t == "step"));

    let tool_start = protocol::parse_frame(&parsed(
        "{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_start\",\
         \"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_7\",\"name\":\"Bash\"}}}",
    ));
    let Frame::StreamToolStart { tool_use_id, name } = tool_start else {
        panic!("expected a stream tool start")
    };
    assert_eq!((tool_use_id.as_str(), name.as_str()), ("toolu_7", "Bash"));

    // input_json_delta and other API events stay ignored.
    let ignored = protocol::parse_frame(&parsed(
        "{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\
         \"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}}",
    ));
    assert!(matches!(ignored, Frame::Ignored));
}

#[test]
fn user_tool_results_carry_outcome_and_output() {
    let frame = protocol::parse_frame(&parsed(
        "{\"type\":\"user\",\"message\":{\"content\":[\
         {\"type\":\"tool_result\",\"tool_use_id\":\"toolu_1\",\"content\":\"done\"},\
         {\"type\":\"tool_result\",\"tool_use_id\":\"toolu_2\",\"is_error\":true,\
          \"content\":[{\"type\":\"text\",\"text\":\"failed hard\"}]}]}}",
    ));
    let Frame::ToolResults(results) = frame else {
        panic!("expected tool results, got {frame:?}")
    };
    assert_eq!(results.len(), 2);
    assert!(results[0].ok);
    assert_eq!(results[0].output.as_deref(), Some("done"));
    assert!(!results[1].ok);
    assert_eq!(results[1].output.as_deref(), Some("failed hard"));
}

#[test]
fn can_use_tool_becomes_a_control_request() {
    let frame = protocol::parse_frame(&parsed(
        "{\"type\":\"control_request\",\"request_id\":\"req-9\",\"request\":{\"subtype\":\"can_use_tool\",\"tool_name\":\"Write\",\"input\":{\"file_path\":\"/tmp/x\"}}}",
    ));
    let Frame::ControlRequest {
        request_id,
        request,
    } = frame
    else {
        panic!("expected a control request")
    };
    assert_eq!(request_id, "req-9");
    assert_eq!(request["subtype"], "can_use_tool");
}

#[test]
fn result_frames_map_terminal_facts() {
    let ok = protocol::parse_frame(&parsed(
        "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"sess-2\",\"usage\":{\"input_tokens\":2,\"cache_read_input_tokens\":10,\"output_tokens\":5}}",
    ));
    let Frame::Result(result) = ok else {
        panic!("expected a result frame")
    };
    assert!(!result.is_error);
    assert!(!protocol::is_aborted(&result));
    assert_eq!(result.session_id.as_deref(), Some("sess-2"));
    let usage = result.usage.expect("usage is reported");
    assert_eq!(usage.input_tokens, Some(12));
    assert_eq!(usage.output_tokens, Some(5));

    let aborted = protocol::parse_frame(&parsed(
        "{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"terminal_reason\":\"aborted_streaming\",\"result\":null}",
    ));
    let Frame::Result(result) = aborted else {
        panic!("expected a result frame")
    };
    assert!(result.is_error);
    assert!(protocol::is_aborted(&result));
}

#[test]
fn rate_limit_events_carry_quota_evidence() {
    let frame = protocol::parse_frame(&parsed(
        "{\"type\":\"rate_limit_event\",\"rate_limit_info\":{\"status\":\"allowed\",\"unifiedWindows\":{\"five_hour\":{\"utilization\":0.5,\"resetsAt\":1700000000.0},\"seven_day\":{\"utilization\":0.75,\"resetsAt\":1700100000.0}}}}",
    ));
    let Frame::Quota(quota) = frame else {
        panic!("expected a quota frame")
    };
    // The most constrained window wins: 75% used leaves 25%.
    assert_eq!(quota.remaining_pct, Some(25.0));
    assert_eq!(quota.resets_at, Some(1700100000.0));
    assert!(!quota.depleted);
}

#[test]
fn unknown_frames_are_ignored_not_failures() {
    for line in [
        "{\"type\":\"keep_alive\"}",
        "{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"x\"}}",
        "{\"type\":\"system\",\"subtype\":\"commands_changed\"}",
        "{\"type\":\"tool_progress\"}",
        "{\"not\":\"a frame\"}",
    ] {
        assert!(
            matches!(protocol::parse_frame(&parsed(line)), Frame::Ignored),
            "unexpected mapping for {line}"
        );
    }
}

#[test]
fn permission_decisions_validate_against_offered_options() {
    let options = vec![
        "accept".to_owned(),
        "decline".to_owned(),
        "cancel".to_owned(),
    ];
    assert!(matches!(
        protocol::tool_answer(&options, &ApprovalDecision::OptionId("accept".into())),
        Ok(ToolAnswer::Allow)
    ));
    assert!(matches!(
        protocol::tool_answer(&options, &ApprovalDecision::OptionId("decline".into())),
        Ok(ToolAnswer::Deny { cancel: false, .. })
    ));
    assert!(matches!(
        protocol::tool_answer(&options, &ApprovalDecision::OptionId("cancel".into())),
        Ok(ToolAnswer::Deny { cancel: true, .. })
    ));
    // An unoffered option and free text both fail instead of guessing.
    assert!(protocol::tool_answer(&options, &ApprovalDecision::OptionId("retry".into())).is_err());
    assert!(protocol::tool_answer(&options, &ApprovalDecision::Text("yes".into())).is_err());
}

#[test]
fn approval_options_follow_the_access_policy() {
    let request = json!({
        "subtype": "can_use_tool",
        "tool_name": "Write",
        "input": {"file_path": "/tmp/x"},
        "description": "Write x",
    });
    let writable = protocol::approval_request(&request, AccessMode::WorkspaceWrite);
    assert_eq!(writable.kind, ApprovalKind::ToolPermission);
    assert_eq!(
        writable
            .options
            .iter()
            .map(|option| option.id.as_str())
            .collect::<Vec<_>>(),
        ["accept", "decline", "cancel"]
    );
    // Read-only sessions never offer accept: accepting would widen the
    // declared access boundary.
    let readonly = protocol::approval_request(&request, AccessMode::ReadOnly);
    assert_eq!(
        readonly
            .options
            .iter()
            .map(|option| option.id.as_str())
            .collect::<Vec<_>>(),
        ["decline", "cancel"]
    );
    // Exiting plan mode is a plan approval, not a tool permission.
    let plan = protocol::approval_request(
        &json!({"subtype": "can_use_tool", "tool_name": "ExitPlanMode", "input": {}}),
        AccessMode::ReadOnly,
    );
    assert_eq!(plan.kind, ApprovalKind::PlanApproval);
}

#[test]
fn tool_summaries_pick_identifying_fields() {
    assert_eq!(
        protocol::tool_summary("Bash", &json!({"command": "git status"})),
        "Bash: git status"
    );
    assert_eq!(
        protocol::tool_summary("Write", &json!({"file_path": "/tmp/a.rs"})),
        "Write: /tmp/a.rs"
    );
    let long = "x".repeat(500);
    let summary = protocol::tool_summary("Tool", &json!({"unknown": long}));
    assert!(summary.chars().count() <= 160);
}

#[test]
fn quota_summary_ignores_empty_payloads() {
    assert!(protocol::quota_summary(&json!({"status": "allowed"})).is_none());
    let rejected = protocol::quota_summary(&json!({"status": "rejected", "resetsAt": 42.0}));
    let summary = rejected.expect("a rejection is observable");
    assert!(summary.depleted);
    assert_eq!(summary.resets_at, Some(42.0));
    assert_eq!(summary.remaining_pct, None);
}

// `MessageStream` stays referenced so the contract stream names stay
// in scope for the session tests.
#[test]
fn message_stream_names_match_the_contract() {
    assert_eq!(MessageStream::Assistant.as_str(), "assistant");
    assert_eq!(MessageStream::Thinking.as_str(), "thinking");
}

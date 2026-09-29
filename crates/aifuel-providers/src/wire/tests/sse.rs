use crate::wire::sse::{SseError, SseEvent, SseParser};

fn collected(parser: &mut SseParser) -> Vec<SseEvent> {
    let mut events = Vec::new();
    while let Some(event) = parser.next_event() {
        events.push(event);
    }
    events
}

#[test]
fn multi_line_data_lines_join_with_newline() {
    // SSE folds repeated `data:` lines of one event into a single payload,
    // which is how providers split large chunks.
    let mut parser = SseParser::new();
    parser.feed(b"data: first\ndata: second\n\n").unwrap();
    let events = collected(&mut parser);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, "first\nsecond");
    assert_eq!(events[0].event, None);
}

#[test]
fn utf8_split_across_feeds_decodes_once_complete() {
    // A multi-byte character may straddle response chunks; bytes are
    // buffered until the line terminates, so the event still decodes.
    let mut parser = SseParser::new();
    parser.feed(b"data: caf\xc3").unwrap();
    assert!(collected(&mut parser).is_empty());
    parser.feed(b"\xa9\n\n").unwrap();
    let events = collected(&mut parser);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, "café");
}

#[test]
fn event_lines_may_split_across_feeds_anywhere() {
    let mut parser = SseParser::new();
    for byte in b"data: hel".iter().chain(b"lo\n\n") {
        parser.feed(&[*byte]).unwrap();
    }
    let events = collected(&mut parser);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, "hello");
}

#[test]
fn done_payload_dispatches_like_any_event() {
    let mut parser = SseParser::new();
    parser.feed(b"data: [DONE]\n\n").unwrap();
    let events = collected(&mut parser);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, "[DONE]");
}

#[test]
fn crlf_lines_and_keepalive_comments_are_handled() {
    // Provider keepalives arrive as `:` comment lines and must not surface
    // as events or corrupt a following event.
    let mut parser = SseParser::new();
    parser.feed(b": keepalive\r\n\r\ndata: ok\r\n\r\n").unwrap();
    let events = collected(&mut parser);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, "ok");
}

#[test]
fn event_field_is_captured_for_dispatch() {
    // Protocols like Anthropic messages dispatch on `event:`; the parser
    // records it even though openai_chat keys on data alone.
    let mut parser = SseParser::new();
    parser.feed(b"event: error\ndata: {}\n\n").unwrap();
    let events = collected(&mut parser);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event.as_deref(), Some("error"));
}

#[test]
fn oversized_event_is_rejected() {
    // The per-event buffer is bounded: a peer that never terminates an
    // event fails the stream instead of growing memory.
    let mut parser = SseParser::with_max_event_bytes(8);
    let error = parser.feed(b"data: payload-that-is-too-large").unwrap_err();
    assert_eq!(error, SseError::EventTooLarge);
}

#[test]
fn eof_flushes_a_trailing_unterminated_event() {
    // A peer that closes right after `data: [DONE]` without the final blank
    // line still yields its event; whether that is a clean completion is
    // decided by the caller's terminal-payload rule.
    let mut parser = SseParser::new();
    parser.feed(b"data: [DONE]\n").unwrap();
    parser.finish().unwrap();
    let events = collected(&mut parser);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, "[DONE]");
}

#[test]
fn empty_data_events_are_dropped() {
    // A comment-like block with no data is not an event.
    let mut parser = SseParser::new();
    parser.feed(b"event: ping\n\n").unwrap();
    assert!(collected(&mut parser).is_empty());
}

#[test]
fn invalid_utf8_payload_fails_decode() {
    let mut parser = SseParser::new();
    let error = parser.feed(b"data: \xff\xfe\n\n").unwrap_err();
    assert_eq!(error, SseError::InvalidUtf8);
}

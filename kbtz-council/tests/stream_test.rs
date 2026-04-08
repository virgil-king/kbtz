use kbtz_council::stream::{parse_stream_line, StreamEvent};

#[test]
fn parse_assistant_text_event() {
    let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello world"}]}}"#;
    let event = parse_stream_line(line).unwrap();
    match event {
        StreamEvent::AssistantText(text) => assert_eq!(text, "Hello world"),
        other => panic!("expected AssistantText, got {:?}", other),
    }
}

#[test]
fn parse_thinking_event() {
    let line = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"Let me analyze..."}]}}"#;
    let event = parse_stream_line(line).unwrap();
    match event {
        StreamEvent::Thinking(text) => assert_eq!(text, "Let me analyze..."),
        other => panic!("expected Thinking, got {:?}", other),
    }
}

#[test]
fn parse_tool_use_event() {
    let line = r#"{"type":"tool_use","name":"Read","input":{"file_path":"/tmp/foo.txt"}}"#;
    let event = parse_stream_line(line).unwrap();
    match event {
        StreamEvent::ToolUse { name, input } => {
            assert_eq!(name, "Read");
            assert!(input.contains("foo.txt"));
        }
        other => panic!("expected ToolUse, got {:?}", other),
    }
}

#[test]
fn parse_result_event() {
    let line = r#"{"type":"result","result":"Done with the task","session_id":"abc-123","cost_usd":0.05,"duration_ms":12000}"#;
    let event = parse_stream_line(line).unwrap();
    match event {
        StreamEvent::Result { result, session_id } => {
            assert_eq!(result, "Done with the task");
            assert_eq!(session_id.as_deref(), Some("abc-123"));
        }
        other => panic!("expected Result, got {:?}", other),
    }
}

#[test]
fn parse_result_event_without_session_id() {
    let line = r#"{"type":"result","result":"Done","cost_usd":0.01}"#;
    let event = parse_stream_line(line).unwrap();
    match event {
        StreamEvent::Result { session_id, .. } => assert!(session_id.is_none()),
        other => panic!("expected Result, got {:?}", other),
    }
}

#[test]
fn parse_unknown_type_returns_other() {
    let line = r#"{"type":"system","message":"starting"}"#;
    let event = parse_stream_line(line).unwrap();
    assert!(matches!(event, StreamEvent::Other(_)));
}

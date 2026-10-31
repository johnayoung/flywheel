//! Edge cases of `ClaudeStreamNormalizer` pinned by a mutation survey: each
//! test fails under one planted normalizer bug the main fixture suite let
//! through. JSONL strings in, normalized events out. No subprocesses.

use flywheel_agents::claude_code::ClaudeStreamNormalizer;
use flywheel_agents::{AgentEvent, EventType};
use serde_json::{Value, json};

fn feed(normalizer: &mut ClaudeStreamNormalizer, lines: &[Value]) -> Vec<AgentEvent> {
    lines
        .iter()
        .flat_map(|line| normalizer.feed_line(&line.to_string()).events)
        .collect()
}

fn feed_one(line: Value) -> Vec<AgentEvent> {
    feed(&mut ClaudeStreamNormalizer::new(), &[line])
}

fn finished(events: &[AgentEvent]) -> AgentEvent {
    let finished: Vec<_> = events
        .iter()
        .filter(|e| e.kind == EventType::SessionFinished)
        .collect();
    assert_eq!(finished.len(), 1, "{events:?}");
    finished[0].clone()
}

fn assistant(message: &Value) -> Value {
    json!({"type": "assistant", "message": message})
}

/// Recorded shape (a real run, 2026-07): the first turn's assistant
/// messages carried `stop_reason: "tool_use"`, every later message carried
/// `null`, and the terminal result said `end_turn`. The stale first-turn
/// value must not stand in for how the session actually ended: the harness
/// only salvages a lost envelope when the stop reason is `end_turn`.
#[test]
fn result_stop_reason_wins_over_a_stale_assistant_stop_reason() {
    let events = feed(
        &mut ClaudeStreamNormalizer::new(),
        &[
            assistant(&json!({
                "content": [{"type": "tool_use", "id": "t-1", "name": "Read", "input": {}}],
                "stop_reason": "tool_use",
            })),
            json!({"type": "user", "message": {"content": [
                {"type": "tool_result", "tool_use_id": "t-1", "content": "ok"}
            ]}}),
            assistant(&json!({
                "content": [{"type": "text", "text": "all done"}],
                "stop_reason": null,
            })),
            json!({
                "type": "result", "subtype": "success", "is_error": false,
                "result": "all done", "stop_reason": "end_turn",
            }),
        ],
    );
    let finished = finished(&events);
    assert_eq!(finished.str_field("stop_reason"), Some("end_turn"));
    assert_eq!(finished.str_field("normalized_stop"), Some("completed"));
}

#[test]
fn assistant_stop_reason_is_the_fallback_when_the_result_carries_none() {
    let events = feed(
        &mut ClaudeStreamNormalizer::new(),
        &[
            assistant(&json!({
                "content": [{"type": "text", "text": "x"}],
                "stop_reason": "max_tokens",
            })),
            json!({"type": "result", "subtype": "unrecognized", "is_error": false}),
        ],
    );
    let finished = finished(&events);
    assert_eq!(finished.str_field("stop_reason"), Some("max_tokens"));
    assert_eq!(finished.str_field("normalized_stop"), Some("max-tokens"));
}

#[test]
fn empty_result_stop_reason_defers_to_the_assistant_value() {
    let events = feed(
        &mut ClaudeStreamNormalizer::new(),
        &[
            assistant(&json!({
                "content": [{"type": "text", "text": "x"}],
                "stop_reason": "end_turn",
            })),
            json!({"type": "result", "subtype": "success", "is_error": false, "stop_reason": ""}),
        ],
    );
    assert_eq!(finished(&events).str_field("stop_reason"), Some("end_turn"));
}

#[test]
fn empty_session_id_never_overwrites_a_captured_one() {
    let mut normalizer = ClaudeStreamNormalizer::new();
    let events = feed(
        &mut normalizer,
        &[
            json!({"type": "system", "subtype": "init", "session_id": "sess-1"}),
            json!({"type": "result", "subtype": "success", "is_error": false, "session_id": ""}),
        ],
    );
    assert_eq!(normalizer.session_id(), Some("sess-1"));
    assert_eq!(finished(&events).str_field("session_id"), Some("sess-1"));
}

#[test]
fn tool_use_without_a_name_is_not_a_tool_call() {
    let events = feed_one(assistant(&json!({
        "content": [{"type": "tool_use", "id": "t-1", "input": {"command": "ls"}}],
    })));
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].kind, EventType::Raw);
    assert_eq!(events[0].native_type.as_deref(), Some("assistant"));
}

#[test]
fn assistant_with_only_unrecognized_blocks_is_recorded_raw() {
    let events = feed_one(assistant(&json!({
        "content": [{"type": "server_tool_use", "id": "s-1"}],
    })));
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].kind, EventType::Raw);
    assert_eq!(
        events[0].payload["data"]["message"]["content"][0]["type"],
        "server_tool_use"
    );
}

#[test]
fn null_assistant_error_field_is_not_an_error() {
    let mut line = assistant(&json!({"content": [{"type": "text", "text": "fine"}]}));
    line["error"] = Value::Null;
    let events = feed_one(line);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].kind, EventType::AssistantMessage);
}

#[test]
fn user_frames_without_tool_results_are_recorded_raw() {
    for content in [
        json!([{"type": "text", "text": "a human turn"}]),
        json!([{"type": "tool_result", "content": "no id"}]),
    ] {
        let events = feed_one(json!({"type": "user", "message": {"content": content}}));
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].kind, EventType::Raw);
        assert_eq!(events[0].native_type.as_deref(), Some("user"));
    }
}

#[test]
fn tool_result_without_is_error_reports_unknown_not_success() {
    let events = feed_one(json!({"type": "user", "message": {"content": [
        {"type": "tool_result", "tool_use_id": "t-1", "content": "out"}
    ]}}));
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, EventType::ToolCallFinished);
    assert_eq!(events[0].payload["is_error"], Value::Null);
}

#[test]
fn result_is_error_is_carried_and_defaults_to_false() {
    let errored = finished(&feed_one(json!({
        "type": "result", "subtype": "error_during_execution", "is_error": true,
    })));
    assert_eq!(errored.payload["is_error"], json!(true));
    let absent = finished(&feed_one(json!({"type": "result", "subtype": "success"})));
    assert_eq!(absent.payload["is_error"], json!(false));
    // An unrecognized subtype with no stop reason falls back on is_error.
    let unknown = finished(&feed_one(json!({
        "type": "result", "subtype": "error_max_budget_usd", "is_error": true,
    })));
    assert_eq!(unknown.str_field("normalized_stop"), Some("error"));
}

#[test]
fn partial_delta_carries_block_index_and_parent_tool() {
    let events = feed_one(json!({
        "type": "stream_event",
        "parent_tool_use_id": "toolu-parent",
        "event": {"type": "content_block_delta", "index": 2,
                  "delta": {"type": "text_delta", "text": "hi"}},
    }));
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, EventType::AssistantDelta);
    assert_eq!(events[0].payload["index"], json!(2));
    assert_eq!(
        events[0].payload["parent_tool_use_id"],
        json!("toolu-parent")
    );
}

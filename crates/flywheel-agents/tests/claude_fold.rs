//! `EventFolder` token accounting and stop evidence pinned by a mutation
//! survey of the claude-code path: which usage mapping is authoritative,
//! when a mapping is replaced, and the pending-tool-use flag on a finished
//! run (the harness refuses envelope salvage on a mid-tool-call stop).

use flywheel_agents::fold::number_as_i64;
use flywheel_agents::{AgentEvent, AgentExit, EventFolder, EventType};
use serde_json::{Value, json};

fn exit() -> AgentExit {
    AgentExit {
        returncode: Some(0),
        stderr_tail: None,
        duration_seconds: Some(0.1),
    }
}

fn event(kind: EventType, payload: Value) -> AgentEvent {
    AgentEvent::new(kind, payload)
}

fn finished() -> AgentEvent {
    event(
        EventType::SessionFinished,
        json!({"normalized_stop": "completed", "subtype": "success", "is_error": false}),
    )
}

#[test]
fn assistant_message_usage_is_the_fallback_when_the_result_has_none() {
    let mut folder = EventFolder::new();
    folder.feed(&event(
        EventType::AssistantMessage,
        json!({"text": "hi", "usage": {"input_tokens": 12, "output_tokens": 3}}),
    ));
    folder.feed(&finished());
    let usage = folder.completed(exit(), None, None).usage.unwrap();
    assert_eq!(usage["input_tokens"], 12);
    assert_eq!(usage["output_tokens"], 3);
}

#[test]
fn context_usage_is_replaced_by_the_latest_non_empty_mapping() {
    let mut folder = EventFolder::new();
    folder.feed(&event(
        EventType::ContextUsage,
        json!({"usage": {"input_tokens": 10, "output_tokens": 2}}),
    ));
    folder.feed(&event(
        EventType::ContextUsage,
        json!({"usage": {"input_tokens": 25, "output_tokens": 7}}),
    ));
    folder.feed(&event(EventType::ContextUsage, json!({"usage": {}})));
    folder.feed(&event(EventType::ContextUsage, json!({})));
    folder.feed(&finished());
    let usage = folder.completed(exit(), None, None).usage.unwrap();
    assert_eq!(usage["input_tokens"], 25);
    assert_eq!(usage["output_tokens"], 7);
    assert_eq!(usage.len(), 2);
}

#[test]
fn no_usage_anywhere_is_none_not_an_empty_mapping() {
    let mut folder = EventFolder::new();
    folder.feed(&event(EventType::AssistantMessage, json!({"text": "hi"})));
    folder.feed(&finished());
    assert_eq!(folder.completed(exit(), None, None).usage, None);
}

#[test]
fn finished_run_with_an_unanswered_tool_call_is_pending() {
    let mut folder = EventFolder::new();
    folder.feed(&event(
        EventType::ToolCallStarted,
        json!({"tool_use_id": "t-1", "tool_name": "Bash", "tool_input": {}}),
    ));
    folder.feed(&event(
        EventType::ToolCallFinished,
        json!({"tool_use_id": "t-1", "is_error": false, "content": "ok"}),
    ));
    folder.feed(&event(
        EventType::ToolCallStarted,
        json!({"tool_use_id": "t-2", "tool_name": "Edit", "tool_input": {}}),
    ));
    folder.feed(&finished());
    let run = folder.completed(exit(), None, None);
    assert!(run.stop.finished);
    assert!(run.stop.pending_tool_use);
}

#[test]
fn oversized_unsigned_counter_saturates() {
    assert_eq!(number_as_i64(&json!(u64::MAX)), Some(i64::MAX));
}

//! Fixture-driven tests for `CodexStreamNormalizer`: `codex exec --json`
//! JSONL strings in, normalized events out. No subprocesses.

use flywheel_agents::codex::CodexStreamNormalizer;
use flywheel_agents::{AgentEvent, EventType};
use serde_json::{Value, json};

fn thread_line(id: &str) -> String {
    json!({"type": "thread.started", "thread_id": id}).to_string()
}

fn item_line(phase: &str, item: Value) -> String {
    json!({"type": format!("item.{phase}"), "item": item}).to_string()
}

fn turn_completed_line(usage: Option<Value>) -> String {
    let mut obj = json!({"type": "turn.completed"});
    if let Some(usage) = usage {
        obj["usage"] = usage;
    }
    obj.to_string()
}

fn kinds(events: &[AgentEvent]) -> Vec<EventType> {
    events.iter().map(|e| e.kind).collect()
}

#[test]
fn thread_started_emits_session_started_and_remembers_id() {
    let mut normalizer = CodexStreamNormalizer::new();
    assert_eq!(normalizer.session_id(), None);
    let outcome = normalizer.feed_line(&thread_line("codex-42"));
    assert_eq!(outcome.native_type.as_deref(), Some("thread.started"));
    assert_eq!(kinds(&outcome.events), vec![EventType::SessionStarted]);
    assert_eq!(outcome.events[0].str_field("session_id"), Some("codex-42"));
    assert_eq!(normalizer.session_id(), Some("codex-42"));
    let finished = normalizer.feed_line(&turn_completed_line(None));
    assert_eq!(finished.events[0].kind, EventType::SessionFinished);
    assert_eq!(finished.events[0].str_field("session_id"), Some("codex-42"));
    assert!(finished.finished);
}

#[test]
fn turn_started_yields_no_events() {
    let outcome =
        CodexStreamNormalizer::new().feed_line(&json!({"type": "turn.started"}).to_string());
    assert_eq!(outcome.native_type.as_deref(), Some("turn.started"));
    assert!(outcome.events.is_empty());
}

#[test]
fn agent_message_only_on_completed_and_deduped() {
    let mut normalizer = CodexStreamNormalizer::new();
    let item = json!({"type": "agent_message", "id": "msg-1", "text": "hello"});
    assert!(
        normalizer
            .feed_line(&item_line("started", item.clone()))
            .events
            .is_empty()
    );
    assert!(
        normalizer
            .feed_line(&item_line("updated", item.clone()))
            .events
            .is_empty()
    );
    let outcome = normalizer.feed_line(&item_line("completed", item.clone()));
    assert_eq!(kinds(&outcome.events), vec![EventType::AssistantMessage]);
    assert_eq!(outcome.events[0].str_field("text"), Some("hello"));
    assert!(
        normalizer
            .feed_line(&item_line("completed", item))
            .events
            .is_empty()
    );
}

#[test]
fn item_type_key_accepted_alongside_type() {
    let outcome = CodexStreamNormalizer::new().feed_line(&item_line(
        "completed",
        json!({"item_type": "agent_message", "id": "msg-1", "text": "drifted"}),
    ));
    assert_eq!(kinds(&outcome.events), vec![EventType::AssistantMessage]);
    assert_eq!(outcome.events[0].str_field("text"), Some("drifted"));
}

#[test]
fn reasoning_becomes_thought_on_completed_only() {
    let mut normalizer = CodexStreamNormalizer::new();
    let item = json!({"type": "reasoning", "id": "r-1", "text": "pondering deeply"});
    assert!(
        normalizer
            .feed_line(&item_line("started", item.clone()))
            .events
            .is_empty()
    );
    let outcome = normalizer.feed_line(&item_line("completed", item));
    assert_eq!(kinds(&outcome.events), vec![EventType::Thought]);
    assert_eq!(
        outcome.events[0].str_field("thinking"),
        Some("pondering deeply")
    );
}

#[test]
fn command_started_once_then_finished_pairing() {
    let mut normalizer = CodexStreamNormalizer::new();
    let base = json!({"type": "command_execution", "id": "cmd-1", "command": "echo hi"});
    let started = normalizer
        .feed_line(&item_line("started", base.clone()))
        .events;
    assert_eq!(kinds(&started), vec![EventType::ToolCallStarted]);
    assert_eq!(started[0].str_field("tool_use_id"), Some("cmd-1"));
    assert_eq!(started[0].str_field("tool_name"), Some("command_execution"));
    assert_eq!(
        started[0].payload["tool_input"],
        json!({"command": "echo hi"})
    );
    assert!(
        normalizer
            .feed_line(&item_line("updated", base.clone()))
            .events
            .is_empty()
    );
    let mut done = base;
    done["aggregated_output"] = json!("hi\n");
    done["exit_code"] = json!(0);
    done["status"] = json!("completed");
    let completed = normalizer.feed_line(&item_line("completed", done)).events;
    assert_eq!(kinds(&completed), vec![EventType::ToolCallFinished]);
    assert_eq!(completed[0].payload["is_error"], json!(false));
    assert_eq!(completed[0].payload["content"], json!("hi\n"));
}

#[test]
fn command_first_sight_on_completed_emits_pair_and_error_rules() {
    let pair = CodexStreamNormalizer::new()
        .feed_line(&item_line(
            "completed",
            json!({"type": "command_execution", "id": "cmd-2", "command": "false", "aggregated_output": "boom", "exit_code": 2, "status": "failed"}),
        ))
        .events;
    assert_eq!(
        kinds(&pair),
        vec![EventType::ToolCallStarted, EventType::ToolCallFinished]
    );
    assert_eq!(pair[1].payload["is_error"], json!(true));
    let nonzero = CodexStreamNormalizer::new()
        .feed_line(&item_line("completed", json!({"type": "command_execution", "id": "cmd-3", "exit_code": 1, "status": "completed"})))
        .events;
    assert_eq!(nonzero[1].payload["is_error"], json!(true));
    let failed = CodexStreamNormalizer::new()
        .feed_line(&item_line(
            "completed",
            json!({"type": "command_execution", "id": "cmd-4", "status": "failed"}),
        ))
        .events;
    assert_eq!(failed[1].payload["is_error"], json!(true));
    let missing_id = CodexStreamNormalizer::new()
        .feed_line(&item_line(
            "completed",
            json!({"type": "command_execution", "status": "completed"}),
        ))
        .events;
    assert_eq!(kinds(&missing_id), vec![EventType::Raw]);
}

#[test]
fn file_change_pair_plus_file_changed_events() {
    let changes =
        json!([{"path": "src/app.py", "kind": "update"}, {"path": "src/new.py", "kind": "add"}]);
    let events = CodexStreamNormalizer::new()
        .feed_line(&item_line(
            "completed",
            json!({"type": "file_change", "id": "fc-1", "status": "completed", "changes": changes}),
        ))
        .events;
    assert_eq!(
        kinds(&events),
        vec![
            EventType::ToolCallStarted,
            EventType::ToolCallFinished,
            EventType::FileChanged,
            EventType::FileChanged
        ]
    );
    assert_eq!(events[0].str_field("tool_name"), Some("file_change"));
    assert_eq!(events[0].payload["tool_input"], json!({"changes": changes}));
    assert_eq!(events[1].payload["is_error"], json!(false));
    assert_eq!(events[1].payload["content"], Value::Null);
    assert_eq!(
        Value::Object(events[2].payload.clone()),
        json!({"path": "src/app.py", "kind": "update"})
    );
    assert_eq!(
        Value::Object(events[3].payload.clone()),
        json!({"path": "src/new.py", "kind": "add"})
    );
}

#[test]
fn mcp_tool_call_and_web_search_pairs() {
    let mcp = CodexStreamNormalizer::new()
        .feed_line(&item_line("completed", json!({"type": "mcp_tool_call", "id": "mcp-1", "server": "serena", "tool": "find_symbol", "status": "failed"})))
        .events;
    assert_eq!(
        kinds(&mcp),
        vec![EventType::ToolCallStarted, EventType::ToolCallFinished]
    );
    assert_eq!(
        mcp[0].str_field("tool_name"),
        Some("mcp:serena.find_symbol")
    );
    assert_eq!(mcp[1].payload["is_error"], json!(true));
    let search = CodexStreamNormalizer::new()
        .feed_line(&item_line(
            "completed",
            json!({"type": "web_search", "id": "ws-1", "query": "codex jsonl schema"}),
        ))
        .events;
    assert_eq!(search[0].str_field("tool_name"), Some("web_search"));
    assert_eq!(
        search[0].payload["tool_input"],
        json!({"query": "codex jsonl schema"})
    );
    assert_eq!(search[1].payload["is_error"], json!(false));
}

#[test]
fn todo_list_plan_updated_on_updated_and_completed() {
    let mut normalizer = CodexStreamNormalizer::new();
    let items = json!([{"text": "step one", "completed": false}]);
    let item = json!({"type": "todo_list", "id": "todo-1", "items": items});
    assert!(
        normalizer
            .feed_line(&item_line("started", item.clone()))
            .events
            .is_empty()
    );
    let updated = normalizer
        .feed_line(&item_line("updated", item.clone()))
        .events;
    assert_eq!(kinds(&updated), vec![EventType::PlanUpdated]);
    assert_eq!(updated[0].payload["items"], items);
    assert_eq!(
        kinds(&normalizer.feed_line(&item_line("completed", item)).events),
        vec![EventType::PlanUpdated]
    );
}

#[test]
fn error_items_and_envelopes() {
    let item = CodexStreamNormalizer::new()
        .feed_line(&item_line(
            "completed",
            json!({"type": "error", "message": "item exploded"}),
        ))
        .events;
    assert_eq!(kinds(&item), vec![EventType::Error]);
    assert_eq!(item[0].str_field("message"), Some("item exploded"));
    let outcome = CodexStreamNormalizer::new()
        .feed_line(&json!({"type": "error", "message": "stream exploded"}).to_string());
    assert_eq!(outcome.native_type.as_deref(), Some("error"));
    assert_eq!(
        outcome.events[0].str_field("message"),
        Some("stream exploded")
    );
}

#[test]
fn unknown_items_envelopes_and_lines_are_raw() {
    let mut normalizer = CodexStreamNormalizer::new();
    let item = normalizer.feed_line(&item_line(
        "completed",
        json!({"type": "quantum_flux", "id": "q-1"}),
    ));
    assert_eq!(item.native_type.as_deref(), Some("item.completed"));
    assert_eq!(kinds(&item.events), vec![EventType::Raw]);
    assert_eq!(
        item.events[0].native_type.as_deref(),
        Some("item.completed")
    );
    let envelope = normalizer.feed_line(&json!({"type": "mystery", "x": 1}).to_string());
    assert_eq!(envelope.events[0].native_type.as_deref(), Some("mystery"));
    assert_eq!(
        envelope.events[0].payload["data"],
        json!({"type": "mystery", "x": 1})
    );
    for line in ["plain text line", "{not json", "[1,2,3]"] {
        let outcome = normalizer.feed_line(line);
        assert_eq!(outcome.native_type, None);
        assert_eq!(outcome.events[0].payload["data"], json!({"line": line}));
    }
    assert!(normalizer.feed_line("   ").events.is_empty());
    let depth = 10_000;
    let nested = format!("{}null{}", "{\"a\":".repeat(depth), "}".repeat(depth));
    assert_eq!(
        kinds(&normalizer.feed_line(&nested).events),
        vec![EventType::Raw]
    );
}

#[test]
fn turn_completed_usage_coercion_and_counting() {
    let mut normalizer = CodexStreamNormalizer::new();
    let outcome = normalizer.feed_line(&turn_completed_line(Some(
        json!({"input_tokens": 11, "cached_input_tokens": 4, "output_tokens": 3}),
    )));
    let payload = &outcome.events[0].payload;
    assert_eq!(payload["normalized_stop"], json!("completed"));
    assert_eq!(payload["stop_reason"], json!("turn.completed"));
    assert_eq!(payload["subtype"], Value::Null);
    assert_eq!(payload["is_error"], json!(false));
    assert_eq!(payload["num_turns"], json!(1));
    assert_eq!(payload["total_cost_usd"], Value::Null);
    assert_eq!(payload["result_text"], Value::Null);
    assert_eq!(
        payload["usage"],
        json!({"input_tokens": 7, "output_tokens": 3, "cache_read_input_tokens": 4, "cache_creation_input_tokens": 0})
    );
    let second = normalizer.feed_line(&turn_completed_line(None));
    assert!(!second.events[0].payload.contains_key("usage"));
    assert_eq!(second.events[0].payload["num_turns"], json!(2));
    let bools = CodexStreamNormalizer::new().feed_line(&turn_completed_line(Some(
        json!({"input_tokens": true, "output_tokens": 3}),
    )));
    assert_eq!(bools.events[0].payload["usage"]["input_tokens"], json!(0));
    assert_eq!(bools.events[0].payload["usage"]["output_tokens"], json!(3));
}

#[test]
fn turn_failed_emits_error_then_finished() {
    let mut normalizer = CodexStreamNormalizer::new();
    normalizer.feed_line(&thread_line("codex-9"));
    let outcome = normalizer
        .feed_line(&json!({"type": "turn.failed", "error": {"message": "boom"}}).to_string());
    assert_eq!(outcome.native_type.as_deref(), Some("turn.failed"));
    assert_eq!(
        kinds(&outcome.events),
        vec![EventType::Error, EventType::SessionFinished]
    );
    assert_eq!(outcome.events[0].str_field("message"), Some("boom"));
    let payload = &outcome.events[1].payload;
    assert_eq!(payload["normalized_stop"], json!("error"));
    assert_eq!(payload["stop_reason"], json!("turn.failed"));
    assert_eq!(payload["is_error"], json!(true));
    assert_eq!(payload["num_turns"], json!(1));
    assert_eq!(payload["session_id"], json!("codex-9"));
    assert!(!payload.contains_key("usage"));
    assert!(outcome.finished);
}

/// Codex's `input_tokens` includes `cached_input_tokens` (its own
/// `total_tokens` is input + output; record shape taken from a real codex
/// session log). Flywheel's four canonical counters are disjoint and are
/// summed for token totals and the `max_tokens` budget, so the projection
/// must not count cached input twice.
#[test]
fn turn_completed_usage_counts_cached_input_once() {
    let native = json!({
        "input_tokens": 87207,
        "cached_input_tokens": 86912,
        "output_tokens": 53,
        "reasoning_output_tokens": 0,
        "total_tokens": 87260
    });
    let outcome = CodexStreamNormalizer::new().feed_line(&turn_completed_line(Some(native)));
    let usage = &outcome.events[0].payload["usage"];
    assert_eq!(
        *usage,
        json!({"input_tokens": 295, "output_tokens": 53, "cache_read_input_tokens": 86912, "cache_creation_input_tokens": 0})
    );
    let canonical_sum: i64 = [
        "input_tokens",
        "output_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
    ]
    .iter()
    .map(|k| usage[*k].as_i64().unwrap())
    .sum();
    assert_eq!(canonical_sum, 87260, "must equal codex's own total_tokens");

    // A malformed record whose cached count exceeds input saturates the
    // uncached remainder at zero rather than going negative.
    let skewed = CodexStreamNormalizer::new().feed_line(&turn_completed_line(Some(
        json!({"input_tokens": 3, "cached_input_tokens": 5, "output_tokens": 1}),
    )));
    assert_eq!(skewed.events[0].payload["usage"]["input_tokens"], json!(0));
    assert_eq!(
        skewed.events[0].payload["usage"]["cache_read_input_tokens"],
        json!(5)
    );
}

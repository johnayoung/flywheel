//! Fixture-driven tests for `ClaudeStreamNormalizer`: JSONL strings in,
//! normalized events out. No subprocesses.

use flywheel_agents::claude_code::ClaudeStreamNormalizer;
use flywheel_agents::jsonl::ControlFrame;
use flywheel_agents::{AgentEvent, AgentExit, EventFolder, EventType, StopReason};
use serde_json::{Value, json};

fn init_line(session_id: &str) -> String {
    json!({"type": "system", "subtype": "init", "session_id": session_id}).to_string()
}

fn assistant_line(blocks: Value, usage: Option<Value>, stop_reason: Option<&str>) -> String {
    let mut message = json!({"content": blocks});
    if let Some(stop) = stop_reason {
        message["stop_reason"] = json!(stop);
    }
    if let Some(usage) = usage {
        message["usage"] = usage;
    }
    json!({"type": "assistant", "message": message}).to_string()
}

fn text(t: &str) -> Value {
    json!({"type": "text", "text": t})
}

fn user_line(blocks: Value) -> String {
    json!({"type": "user", "message": {"content": blocks}}).to_string()
}

fn result_line(overrides: Value) -> String {
    let mut obj = json!({
        "type": "result", "subtype": "success", "is_error": false, "result": "done",
        "num_turns": 2, "total_cost_usd": 0.01,
    });
    if let Value::Object(map) = overrides {
        for (k, v) in map {
            obj[k] = v;
        }
    }
    obj.to_string()
}

fn feed(normalizer: &mut ClaudeStreamNormalizer, lines: &[String]) -> Vec<AgentEvent> {
    lines
        .iter()
        .flat_map(|line| normalizer.feed_line(line).events)
        .collect()
}

fn only(events: &[AgentEvent], kind: EventType) -> Vec<AgentEvent> {
    events.iter().filter(|e| e.kind == kind).cloned().collect()
}

#[test]
fn assistant_text_concat_is_envelope_free() {
    let events = feed(
        &mut ClaudeStreamNormalizer::new(),
        &[
            init_line("sess-42"),
            assistant_line(json!([text("Alpha ")]), None, Some("end_turn")),
            assistant_line(json!([text("beta.")]), None, Some("end_turn")),
            result_line(json!({"result": "terminal summary"})),
        ],
    );
    let texts: String = only(&events, EventType::AssistantMessage)
        .iter()
        .map(|e| e.str_field("text").unwrap().to_string())
        .collect();
    assert_eq!(texts, "Alpha beta.");
    let finished = only(&events, EventType::SessionFinished);
    assert_eq!(finished.len(), 1);
    assert_eq!(
        finished[0].str_field("result_text"),
        Some("terminal summary")
    );
}

#[test]
fn result_text_fallback_when_no_text_blocks() {
    let events = feed(
        &mut ClaudeStreamNormalizer::new(),
        &[
            init_line("sess-42"),
            assistant_line(
                json!([{"type": "tool_use", "id": "t-1", "name": "Bash", "input": {}}]),
                None,
                Some("tool_use"),
            ),
            user_line(json!([{"type": "tool_result", "tool_use_id": "t-1", "content": "ok"}])),
            result_line(json!({"result": "only the result text"})),
        ],
    );
    assert!(only(&events, EventType::AssistantMessage).is_empty());
    let mut folder = EventFolder::new();
    for event in &events {
        folder.feed(event);
    }
    let run = folder.completed(
        AgentExit {
            returncode: Some(0),
            ..Default::default()
        },
        None,
        None,
    );
    assert_eq!(run.final_text, "only the result text");
}

#[test]
fn usage_zero_filled_across_canonical_keys() {
    let events = feed(
        &mut ClaudeStreamNormalizer::new(),
        &[
            assistant_line(
                json!([text("hi")]),
                Some(json!({"input_tokens": 12})),
                Some("end_turn"),
            ),
            result_line(json!({"usage": {"output_tokens": 9}})),
        ],
    );
    let context = only(&events, EventType::ContextUsage);
    assert_eq!(context.len(), 1);
    assert_eq!(
        context[0].payload["usage"],
        json!({"input_tokens": 12, "output_tokens": 0, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0})
    );
    let finished = only(&events, EventType::SessionFinished);
    assert_eq!(
        finished[0].payload["usage"],
        json!({"input_tokens": 0, "output_tokens": 9, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0})
    );
}

#[test]
fn session_id_captured_and_carried_into_result() {
    let mut normalizer = ClaudeStreamNormalizer::new();
    assert_eq!(normalizer.session_id(), None);
    let events = feed(
        &mut normalizer,
        &[init_line("sess-42"), result_line(json!({}))],
    );
    assert_eq!(normalizer.session_id(), Some("sess-42"));
    let started = only(&events, EventType::SessionStarted);
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].str_field("session_id"), Some("sess-42"));
    assert_eq!(
        only(&events, EventType::SessionFinished)[0].str_field("session_id"),
        Some("sess-42")
    );
}

#[test]
fn session_id_also_captured_from_assistant_and_result_envelopes() {
    let mut normalizer = ClaudeStreamNormalizer::new();
    let line =
        json!({"type": "assistant", "session_id": "sess-7", "message": {"content": [text("x")]}})
            .to_string();
    normalizer.feed_line(&line);
    assert_eq!(normalizer.session_id(), Some("sess-7"));
    normalizer.feed_line(&result_line(json!({"session_id": "sess-8"})));
    assert_eq!(normalizer.session_id(), Some("sess-8"));
}

#[test]
fn thinking_block_becomes_thought() {
    let events = feed(
        &mut ClaudeStreamNormalizer::new(),
        &[assistant_line(
            json!([{"type": "thinking", "thinking": "pondering deeply"}]),
            None,
            None,
        )],
    );
    let thoughts = only(&events, EventType::Thought);
    assert_eq!(thoughts.len(), 1);
    assert_eq!(thoughts[0].str_field("thinking"), Some("pondering deeply"));
    assert_eq!(thoughts[0].native_type.as_deref(), Some("assistant"));
}

#[test]
fn tool_use_and_tool_result_payloads() {
    let events = feed(
        &mut ClaudeStreamNormalizer::new(),
        &[
            assistant_line(
                json!([{"type": "tool_use", "id": "t-9", "name": "Read", "input": {"path": "x.py"}}]),
                None,
                Some("tool_use"),
            ),
            user_line(
                json!([{"type": "tool_result", "tool_use_id": "t-9", "content": "file contents", "is_error": false}]),
            ),
        ],
    );
    let started = only(&events, EventType::ToolCallStarted);
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].str_field("tool_use_id"), Some("t-9"));
    assert_eq!(started[0].str_field("tool_name"), Some("Read"));
    assert_eq!(started[0].payload["tool_input"], json!({"path": "x.py"}));
    let finished = only(&events, EventType::ToolCallFinished);
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].str_field("tool_use_id"), Some("t-9"));
    assert_eq!(finished[0].payload["is_error"], json!(false));
    assert_eq!(finished[0].payload["content"], json!("file contents"));
}

#[test]
fn permission_denials_become_events() {
    let line = result_line(json!({
        "permission_denials": [
            {"tool_name": "Bash", "tool_input": {"command": "rm -rf /"}},
            {"unnamed": true},
        ]
    }));
    let outcome = ClaudeStreamNormalizer::new().feed_line(&line);
    let denied = only(&outcome.events, EventType::PermissionDenied);
    assert_eq!(denied.len(), 2);
    assert_eq!(denied[0].str_field("tool_name"), Some("Bash"));
    assert_eq!(
        denied[0].payload["denial"]["tool_input"],
        json!({"command": "rm -rf /"})
    );
    assert_eq!(denied[1].payload["tool_name"], Value::Null);
    assert_eq!(
        outcome.events.last().unwrap().kind,
        EventType::SessionFinished
    );
    assert!(outcome.finished);
}

#[test]
fn deeply_nested_json_line_is_raw_not_crash() {
    let depth = 10_000;
    let line = format!("{}null{}", "{\"a\":".repeat(depth), "}".repeat(depth));
    let outcome = ClaudeStreamNormalizer::new().feed_line(&line);
    assert_eq!(outcome.native_type, None);
    assert_eq!(outcome.events.len(), 1);
    assert_eq!(outcome.events[0].kind, EventType::Raw);
    assert_eq!(outcome.events[0].payload["data"], json!({"line": line}));
}

#[test]
fn non_json_and_non_mapping_lines_are_raw() {
    let mut normalizer = ClaudeStreamNormalizer::new();
    for line in ["plain text line", "{not json", "[1,2,3]"] {
        let outcome = normalizer.feed_line(line);
        assert_eq!(outcome.native_type, None);
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(outcome.events[0].kind, EventType::Raw);
        assert_eq!(outcome.events[0].payload["data"], json!({"line": line}));
    }
    let blank = normalizer.feed_line("   ");
    assert_eq!(blank.native_type, None);
    assert!(blank.events.is_empty());
}

#[test]
fn native_type_returned_correctly() {
    let mut normalizer = ClaudeStreamNormalizer::new();
    assert_eq!(
        normalizer.feed_line(&init_line("s")).native_type.as_deref(),
        Some("system")
    );
    assert_eq!(
        normalizer
            .feed_line(&assistant_line(json!([text("x")]), None, None))
            .native_type
            .as_deref(),
        Some("assistant")
    );
    assert_eq!(
        normalizer
            .feed_line(&result_line(json!({})))
            .native_type
            .as_deref(),
        Some("result")
    );
    let outcome = normalizer.feed_line(&json!({"type": "mystery"}).to_string());
    assert_eq!(outcome.native_type.as_deref(), Some("mystery"));
    assert_eq!(outcome.events[0].kind, EventType::Raw);
    assert_eq!(outcome.events[0].native_type.as_deref(), Some("mystery"));
    assert_eq!(normalizer.feed_line("not json").native_type, None);
}

#[test]
fn stop_reason_carried_and_normalized_per_subtype() {
    for (subtype, is_error, expected) in [
        ("success", false, StopReason::Completed),
        ("error_max_turns", false, StopReason::MaxTurns),
        ("error_during_execution", true, StopReason::Error),
    ] {
        let events = feed(
            &mut ClaudeStreamNormalizer::new(),
            &[
                assistant_line(json!([text("working")]), None, Some("end_turn")),
                result_line(json!({"subtype": subtype, "is_error": is_error})),
            ],
        );
        let finished = only(&events, EventType::SessionFinished);
        assert_eq!(finished.len(), 1);
        assert_eq!(finished[0].str_field("stop_reason"), Some("end_turn"));
        assert_eq!(finished[0].str_field("subtype"), Some(subtype));
        assert_eq!(
            finished[0].str_field("normalized_stop"),
            Some(expected.as_str())
        );
    }
}

#[test]
fn result_falls_back_to_its_own_stop_reason_and_carries_errors() {
    let outcome = ClaudeStreamNormalizer::new().feed_line(&result_line(json!({
        "subtype": "unrecognized", "stop_reason": "end_turn", "errors": ["API Error: 529"], "api_error_status": 529
    })));
    let finished = &outcome.events[0];
    assert_eq!(finished.str_field("stop_reason"), Some("end_turn"));
    assert_eq!(finished.str_field("normalized_stop"), Some("completed"));
    assert_eq!(finished.payload["errors"], json!(["API Error: 529"]));
    assert_eq!(finished.payload["api_error_status"], json!(529));
}

#[test]
fn rate_limit_event_normalizes_with_and_without_reset() {
    let mut normalizer = ClaudeStreamNormalizer::new();
    let line = json!({
        "type": "rate_limit_event", "uuid": "u-1", "session_id": "sess-1",
        "rate_limit_info": {"status": "allowed_warning", "resetsAt": 1751990400, "rateLimitType": "five_hour"},
    })
    .to_string();
    let outcome = normalizer.feed_line(&line);
    assert_eq!(outcome.events.len(), 1);
    let event = &outcome.events[0];
    assert_eq!(event.kind, EventType::RateLimited);
    assert_eq!(event.payload["resets_at_epoch"], json!(1751990400.0));
    assert_eq!(event.payload["detail"]["status"], json!("allowed_warning"));
    assert_eq!(normalizer.session_id(), Some("sess-1"));
    let bare = json!({"type": "rate_limit_event", "uuid": "u-2", "session_id": "sess-1", "rate_limit_info": {"status": "allowed"}}).to_string();
    let outcome = normalizer.feed_line(&bare);
    assert_eq!(outcome.events[0].payload["resets_at_epoch"], Value::Null);
}

#[test]
fn partial_message_deltas_become_assistant_delta() {
    let mut normalizer = ClaudeStreamNormalizer::new();
    let delta = json!({
        "type": "stream_event", "uuid": "u", "session_id": "s",
        "event": {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hel"}},
    })
    .to_string();
    let outcome = normalizer.feed_line(&delta);
    assert_eq!(outcome.events[0].kind, EventType::AssistantDelta);
    assert_eq!(outcome.events[0].str_field("text"), Some("Hel"));
    assert_eq!(
        outcome.events[0].str_field("delta_type"),
        Some("text_delta")
    );
    let thinking = json!({
        "type": "stream_event", "uuid": "u", "session_id": "s",
        "event": {"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "hm"}},
    })
    .to_string();
    assert_eq!(
        normalizer.feed_line(&thinking).events[0].str_field("thinking"),
        Some("hm")
    );
    let start = json!({"type": "stream_event", "uuid": "u", "session_id": "s", "event": {"type": "message_start"}}).to_string();
    assert_eq!(normalizer.feed_line(&start).events[0].kind, EventType::Raw);
}

#[test]
fn assistant_error_field_becomes_error_event() {
    let line =
        json!({"type": "assistant", "error": "overloaded", "message": {"content": [text("x")]}})
            .to_string();
    let outcome = ClaudeStreamNormalizer::new().feed_line(&line);
    let errors = only(&outcome.events, EventType::Error);
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].str_field("message"), Some("overloaded"));
}

#[test]
fn hook_and_other_system_frames_are_raw() {
    let mut normalizer = ClaudeStreamNormalizer::new();
    let hook = json!({"type": "system", "subtype": "hook_started", "hook_event": "PostToolUse"})
        .to_string();
    let outcome = normalizer.feed_line(&hook);
    assert_eq!(outcome.events[0].kind, EventType::Raw);
    assert_eq!(outcome.events[0].native_type.as_deref(), Some("system"));
}

#[test]
fn control_response_frames_are_peeled_off() {
    let mut normalizer = ClaudeStreamNormalizer::new();
    let ok = json!({"type": "control_response", "response": {"subtype": "success", "request_id": "req_1", "response": {"a": 1}}}).to_string();
    let outcome = normalizer.feed_line(&ok);
    assert!(outcome.events.is_empty());
    assert_eq!(
        outcome.control,
        Some(ControlFrame::Response {
            request_id: "req_1".into(),
            result: Ok(json!({"a": 1}))
        })
    );
    let err = json!({"type": "control_response", "response": {"subtype": "error", "request_id": "req_2", "error": "nope"}}).to_string();
    assert_eq!(
        normalizer.feed_line(&err).control,
        Some(ControlFrame::Response {
            request_id: "req_2".into(),
            result: Err("nope".into())
        })
    );
    let malformed = json!({"type": "control_response"}).to_string();
    let outcome = normalizer.feed_line(&malformed);
    assert_eq!(outcome.control, None);
    assert_eq!(outcome.events[0].kind, EventType::Raw);
}

#[test]
fn control_request_frames_distinguish_approvals() {
    let mut normalizer = ClaudeStreamNormalizer::new();
    let approval = json!({"type": "control_request", "request_id": "c-1", "request": {"subtype": "can_use_tool", "tool_name": "Bash", "input": {}}}).to_string();
    let outcome = normalizer.feed_line(&approval);
    assert!(outcome.events.is_empty());
    match outcome.control {
        Some(ControlFrame::Request {
            request_id,
            subtype,
            request,
        }) => {
            assert_eq!(request_id, "c-1");
            assert_eq!(subtype, "can_use_tool");
            assert_eq!(request["tool_name"], json!("Bash"));
        }
        other => panic!("unexpected {other:?}"),
    }
    let hook = json!({"type": "control_request", "request_id": "c-2", "request": {"subtype": "hook_callback", "callback_id": "hook_0"}}).to_string();
    let outcome = normalizer.feed_line(&hook);
    assert_eq!(outcome.events[0].kind, EventType::Raw);
    assert!(
        matches!(outcome.control, Some(ControlFrame::Request { ref subtype, .. }) if subtype == "hook_callback")
    );
    let cancel = json!({"type": "control_cancel_request", "request_id": "c-1"}).to_string();
    assert_eq!(
        normalizer.feed_line(&cancel).control,
        Some(ControlFrame::CancelRequest {
            request_id: "c-1".into()
        })
    );
}

#[test]
fn sdk_shaped_exchange_folds_to_completed_run() {
    // The same logical exchange the Python SDK-parity test used, as the
    // stream-json lines the SDK itself consumed underneath.
    let lines = [
        json!({"type": "system", "subtype": "init", "session_id": "sess-1", "model": "claude-opus-4"}).to_string(),
        json!({"type": "assistant", "message": {"content": [
            {"type": "text", "text": "Let me check."},
            {"type": "thinking", "thinking": "planning the check"},
            {"type": "tool_use", "id": "tu-1", "name": "Bash", "input": {"command": "ls"}},
        ], "usage": {"input_tokens": 10, "output_tokens": 5}}}).to_string(),
        json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "tu-1", "is_error": false, "content": "ok"}]}}).to_string(),
        json!({"type": "assistant", "message": {"stop_reason": "end_turn", "content": [{"type": "text", "text": " Done."}], "usage": {"input_tokens": 20, "output_tokens": 9}}}).to_string(),
        json!({"type": "result", "subtype": "success", "is_error": false, "num_turns": 2, "total_cost_usd": 0.01, "result": "Let me check. Done.", "usage": {"input_tokens": 20, "output_tokens": 9}, "session_id": "sess-1"}).to_string(),
    ];
    let events = feed(&mut ClaudeStreamNormalizer::new(), &lines);
    assert_eq!(
        events.iter().map(|e| e.kind).collect::<Vec<_>>(),
        vec![
            EventType::SessionStarted,
            EventType::AssistantMessage,
            EventType::Thought,
            EventType::ToolCallStarted,
            EventType::ContextUsage,
            EventType::ToolCallFinished,
            EventType::AssistantMessage,
            EventType::ContextUsage,
            EventType::SessionFinished,
        ]
    );
    let mut folder = EventFolder::new();
    for event in &events {
        folder.feed(event);
    }
    let run = folder.completed(AgentExit::default(), None, None);
    assert_eq!(run.final_text, "Let me check. Done.");
    assert_eq!(run.stop.reason, StopReason::Completed);
    assert_eq!(run.stop.native.as_deref(), Some("end_turn"));
    assert_eq!(run.stop.native_subtype.as_deref(), Some("success"));
    assert_eq!(run.usage.unwrap()["input_tokens"], 20);
    assert_eq!(run.num_turns, Some(2));
    assert_eq!(run.total_cost_usd, Some(0.01));
    assert_eq!(run.native_session_id.as_deref(), Some("sess-1"));
    assert_eq!(run.tool_interactions.len(), 1);
    assert_eq!(
        run.tool_interactions[0].result.as_ref().unwrap().content,
        Some(json!("ok"))
    );
}

"""Unit tests for ``CodexStreamNormalizer.feed_line``.

Pure fixture-driven: ``codex exec --json`` JSONL strings in, normalized
``AgentEvent`` tuples out. No subprocesses, no vendor CLI.
"""

from __future__ import annotations

import json
from typing import Any

from flywheel_agents import AgentEvent, EventType
from flywheel_agents.codex import CodexStreamNormalizer


def _thread_line(thread_id: str = "codex-42") -> str:
    return json.dumps({"type": "thread.started", "thread_id": thread_id})


def _item_line(phase: str, item: dict[str, Any]) -> str:
    return json.dumps({"type": f"item.{phase}", "item": item})


def _turn_completed_line(usage: dict[str, Any] | None = None) -> str:
    obj: dict[str, Any] = {"type": "turn.completed"}
    if usage is not None:
        obj["usage"] = usage
    return json.dumps(obj)


def _feed(normalizer: CodexStreamNormalizer, lines: list[str]) -> list[AgentEvent]:
    events: list[AgentEvent] = []
    for line in lines:
        _, produced = normalizer.feed_line(line)
        events.extend(produced)
    return events


def _only(events: list[AgentEvent], type_: EventType) -> list[AgentEvent]:
    return [event for event in events if event.type is type_]


def test_thread_started_emits_session_started_and_remembers_id() -> None:
    normalizer = CodexStreamNormalizer()
    assert normalizer.session_id is None
    native_type, events = normalizer.feed_line(_thread_line("codex-42"))
    assert native_type == "thread.started"
    assert [e.type for e in events] == [EventType.SESSION_STARTED]
    assert events[0].payload["session_id"] == "codex-42"
    assert normalizer.session_id == "codex-42"
    finished = _feed(normalizer, [_turn_completed_line()])
    assert finished[0].type is EventType.SESSION_FINISHED
    assert finished[0].payload["session_id"] == "codex-42"


def test_turn_started_yields_no_events() -> None:
    native_type, events = CodexStreamNormalizer().feed_line(
        json.dumps({"type": "turn.started"})
    )
    assert native_type == "turn.started"
    assert events == ()


def test_agent_message_only_on_completed() -> None:
    normalizer = CodexStreamNormalizer()
    item = {"type": "agent_message", "id": "msg-1", "text": "hello"}
    assert normalizer.feed_line(_item_line("started", item))[1] == ()
    assert normalizer.feed_line(_item_line("updated", item))[1] == ()
    _, events = normalizer.feed_line(_item_line("completed", item))
    assert [e.type for e in events] == [EventType.ASSISTANT_MESSAGE]
    assert events[0].payload["text"] == "hello"


def test_agent_message_deduped_on_repeated_completed() -> None:
    normalizer = CodexStreamNormalizer()
    item = {"type": "agent_message", "id": "msg-1", "text": "hello"}
    events = _feed(
        normalizer,
        [_item_line("completed", item), _item_line("completed", item)],
    )
    assert len(_only(events, EventType.ASSISTANT_MESSAGE)) == 1


def test_item_type_key_accepted_alongside_type() -> None:
    _, events = CodexStreamNormalizer().feed_line(
        _item_line(
            "completed",
            {"item_type": "agent_message", "id": "msg-1", "text": "drifted"},
        )
    )
    assert [e.type for e in events] == [EventType.ASSISTANT_MESSAGE]
    assert events[0].payload["text"] == "drifted"


def test_reasoning_becomes_thought_on_completed_only() -> None:
    normalizer = CodexStreamNormalizer()
    item = {"type": "reasoning", "id": "r-1", "text": "pondering deeply"}
    assert normalizer.feed_line(_item_line("started", item))[1] == ()
    _, events = normalizer.feed_line(_item_line("completed", item))
    assert [e.type for e in events] == [EventType.THOUGHT]
    assert events[0].payload["thinking"] == "pondering deeply"


def test_command_started_once_then_finished_pairing() -> None:
    normalizer = CodexStreamNormalizer()
    base = {"type": "command_execution", "id": "cmd-1", "command": "echo hi"}
    _, started = normalizer.feed_line(_item_line("started", base))
    assert [e.type for e in started] == [EventType.TOOL_CALL_STARTED]
    assert started[0].payload["tool_use_id"] == "cmd-1"
    assert started[0].payload["tool_name"] == "command_execution"
    assert started[0].payload["tool_input"] == {"command": "echo hi"}
    _, updated = normalizer.feed_line(_item_line("updated", base))
    assert updated == ()  # no duplicate STARTED
    _, completed = normalizer.feed_line(
        _item_line(
            "completed",
            {
                **base,
                "aggregated_output": "hi\n",
                "exit_code": 0,
                "status": "completed",
            },
        )
    )
    assert [e.type for e in completed] == [EventType.TOOL_CALL_FINISHED]
    assert completed[0].payload["tool_use_id"] == "cmd-1"
    assert completed[0].payload["is_error"] is False
    assert completed[0].payload["content"] == "hi\n"


def test_command_first_sight_on_completed_emits_pair() -> None:
    _, events = CodexStreamNormalizer().feed_line(
        _item_line(
            "completed",
            {
                "type": "command_execution",
                "id": "cmd-2",
                "command": "false",
                "aggregated_output": "boom",
                "exit_code": 2,
                "status": "failed",
            },
        )
    )
    assert [e.type for e in events] == [
        EventType.TOOL_CALL_STARTED,
        EventType.TOOL_CALL_FINISHED,
    ]
    assert events[1].payload["is_error"] is True


def test_command_error_when_exit_code_nonzero_even_if_status_ok() -> None:
    _, events = CodexStreamNormalizer().feed_line(
        _item_line(
            "completed",
            {
                "type": "command_execution",
                "id": "cmd-3",
                "command": "flaky",
                "exit_code": 1,
                "status": "completed",
            },
        )
    )
    finished = _only(list(events), EventType.TOOL_CALL_FINISHED)
    assert finished[0].payload["is_error"] is True


def test_command_error_when_status_failed_without_exit_code() -> None:
    _, events = CodexStreamNormalizer().feed_line(
        _item_line(
            "completed",
            {"type": "command_execution", "id": "cmd-4", "status": "failed"},
        )
    )
    finished = _only(list(events), EventType.TOOL_CALL_FINISHED)
    assert finished[0].payload["is_error"] is True


def test_file_change_pair_plus_file_changed_events() -> None:
    changes = [
        {"path": "src/app.py", "kind": "update"},
        {"path": "src/new.py", "kind": "add"},
    ]
    _, events = CodexStreamNormalizer().feed_line(
        _item_line(
            "completed",
            {
                "type": "file_change",
                "id": "fc-1",
                "status": "completed",
                "changes": changes,
            },
        )
    )
    assert [e.type for e in events] == [
        EventType.TOOL_CALL_STARTED,
        EventType.TOOL_CALL_FINISHED,
        EventType.FILE_CHANGED,
        EventType.FILE_CHANGED,
    ]
    assert events[0].payload["tool_name"] == "file_change"
    assert events[0].payload["tool_input"] == {"changes": changes}
    assert events[1].payload["is_error"] is False
    assert events[1].payload["content"] is None
    assert events[2].payload == {"path": "src/app.py", "kind": "update"}
    assert events[3].payload == {"path": "src/new.py", "kind": "add"}


def test_mcp_tool_call_names_server_and_tool() -> None:
    _, events = CodexStreamNormalizer().feed_line(
        _item_line(
            "completed",
            {
                "type": "mcp_tool_call",
                "id": "mcp-1",
                "server": "serena",
                "tool": "find_symbol",
                "status": "failed",
            },
        )
    )
    assert [e.type for e in events] == [
        EventType.TOOL_CALL_STARTED,
        EventType.TOOL_CALL_FINISHED,
    ]
    assert events[0].payload["tool_name"] == "mcp:serena.find_symbol"
    assert events[1].payload["is_error"] is True


def test_web_search_pair_carries_query() -> None:
    _, events = CodexStreamNormalizer().feed_line(
        _item_line(
            "completed",
            {"type": "web_search", "id": "ws-1", "query": "codex jsonl schema"},
        )
    )
    assert [e.type for e in events] == [
        EventType.TOOL_CALL_STARTED,
        EventType.TOOL_CALL_FINISHED,
    ]
    assert events[0].payload["tool_name"] == "web_search"
    assert events[0].payload["tool_input"] == {"query": "codex jsonl schema"}
    assert events[1].payload["is_error"] is False


def test_todo_list_plan_updated_on_updated_and_completed() -> None:
    normalizer = CodexStreamNormalizer()
    items = [{"text": "step one", "completed": False}]
    item = {"type": "todo_list", "id": "todo-1", "items": items}
    assert normalizer.feed_line(_item_line("started", item))[1] == ()
    _, updated = normalizer.feed_line(_item_line("updated", item))
    assert [e.type for e in updated] == [EventType.PLAN_UPDATED]
    assert updated[0].payload["items"] == items
    _, completed = normalizer.feed_line(_item_line("completed", item))
    assert [e.type for e in completed] == [EventType.PLAN_UPDATED]


def test_error_item_becomes_error_event() -> None:
    _, events = CodexStreamNormalizer().feed_line(
        _item_line("completed", {"type": "error", "message": "item exploded"})
    )
    assert [e.type for e in events] == [EventType.ERROR]
    assert events[0].payload["message"] == "item exploded"


def test_top_level_error_envelope() -> None:
    native_type, events = CodexStreamNormalizer().feed_line(
        json.dumps({"type": "error", "message": "stream exploded"})
    )
    assert native_type == "error"
    assert [e.type for e in events] == [EventType.ERROR]
    assert events[0].payload["message"] == "stream exploded"


def test_unknown_item_type_is_raw() -> None:
    native_type, events = CodexStreamNormalizer().feed_line(
        _item_line("completed", {"type": "quantum_flux", "id": "q-1"})
    )
    assert native_type == "item.completed"
    assert [e.type for e in events] == [EventType.RAW]
    assert events[0].native_type == "item.completed"


def test_unknown_envelope_type_is_raw() -> None:
    native_type, events = CodexStreamNormalizer().feed_line(
        json.dumps({"type": "mystery", "x": 1})
    )
    assert native_type == "mystery"
    assert [e.type for e in events] == [EventType.RAW]
    assert events[0].native_type == "mystery"
    assert events[0].payload["data"] == {"type": "mystery", "x": 1}


def test_non_json_and_non_mapping_lines_are_raw() -> None:
    normalizer = CodexStreamNormalizer()
    for line in ("plain text line", "{not json", "[1,2,3]"):
        native_type, events = normalizer.feed_line(line)
        assert native_type is None
        assert [e.type for e in events] == [EventType.RAW]
        assert events[0].payload["data"] == {"line": line}
    assert normalizer.feed_line("   ") == (None, ())


def test_deeply_nested_json_line_is_raw_not_crash() -> None:
    depth = 10_000
    line = '{"a":' * depth + "null" + "}" * depth
    native_type, events = CodexStreamNormalizer().feed_line(line)
    assert native_type is None
    assert [e.type for e in events] == [EventType.RAW]
    assert events[0].payload["data"] == {"line": line}


def test_turn_completed_usage_coercion() -> None:
    _, events = CodexStreamNormalizer().feed_line(
        _turn_completed_line(
            {"input_tokens": 11, "cached_input_tokens": 4, "output_tokens": 3}
        )
    )
    assert [e.type for e in events] == [EventType.SESSION_FINISHED]
    payload = events[0].payload
    assert payload["normalized_stop"] == "completed"
    assert payload["stop_reason"] == "turn.completed"
    assert payload["subtype"] is None
    assert payload["is_error"] is False
    assert payload["num_turns"] == 1
    assert payload["total_cost_usd"] is None
    assert payload["result_text"] is None
    assert payload["usage"] == {
        "input_tokens": 11,
        "output_tokens": 3,
        "cache_read_input_tokens": 4,
        "cache_creation_input_tokens": 0,
    }


def test_turn_completed_without_usage_omits_usage_key() -> None:
    _, events = CodexStreamNormalizer().feed_line(_turn_completed_line())
    assert "usage" not in events[0].payload


def test_turn_completed_usage_excludes_bools() -> None:
    _, events = CodexStreamNormalizer().feed_line(
        _turn_completed_line({"input_tokens": True, "output_tokens": 3})
    )
    assert events[0].payload["usage"] == {
        "input_tokens": 0,
        "output_tokens": 3,
        "cache_read_input_tokens": 0,
        "cache_creation_input_tokens": 0,
    }


def test_multiple_turn_completed_increment_num_turns() -> None:
    normalizer = CodexStreamNormalizer()
    events = _feed(
        normalizer, [_turn_completed_line(), _turn_completed_line()]
    )
    finished = _only(events, EventType.SESSION_FINISHED)
    assert [e.payload["num_turns"] for e in finished] == [1, 2]


def test_turn_failed_emits_error_then_finished() -> None:
    normalizer = CodexStreamNormalizer()
    normalizer.feed_line(_thread_line("codex-9"))
    native_type, events = normalizer.feed_line(
        json.dumps({"type": "turn.failed", "error": {"message": "boom"}})
    )
    assert native_type == "turn.failed"
    assert [e.type for e in events] == [
        EventType.ERROR,
        EventType.SESSION_FINISHED,
    ]
    assert events[0].payload["message"] == "boom"
    payload = events[1].payload
    assert payload["normalized_stop"] == "error"
    assert payload["stop_reason"] == "turn.failed"
    assert payload["is_error"] is True
    assert payload["num_turns"] == 1
    assert payload["session_id"] == "codex-9"
    assert "usage" not in payload

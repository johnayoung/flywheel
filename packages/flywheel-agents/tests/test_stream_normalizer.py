"""Unit tests for ``ClaudeStreamNormalizer.feed_line``.

Pure fixture-driven: JSONL strings in, normalized ``AgentEvent`` tuples out.
No subprocesses, no vendor CLI.
"""

from __future__ import annotations

import json
from typing import Any

from flywheel_agents import AgentEvent, AgentExit, EventFolder, EventType, StopReason
from flywheel_agents.claude_code import ClaudeStreamNormalizer


def _init_line(session_id: str = "sess-42") -> str:
    return json.dumps(
        {"type": "system", "subtype": "init", "session_id": session_id}
    )


def _assistant_line(
    blocks: list[dict[str, Any]],
    *,
    usage: dict[str, int] | None = None,
    stop_reason: str | None = "end_turn",
) -> str:
    message: dict[str, Any] = {"content": blocks}
    if stop_reason is not None:
        message["stop_reason"] = stop_reason
    if usage is not None:
        message["usage"] = usage
    return json.dumps({"type": "assistant", "message": message})


def _text(text: str) -> dict[str, Any]:
    return {"type": "text", "text": text}


def _user_line(blocks: list[dict[str, Any]]) -> str:
    return json.dumps({"type": "user", "message": {"content": blocks}})


def _result_line(**overrides: Any) -> str:
    obj: dict[str, Any] = {
        "type": "result",
        "subtype": "success",
        "is_error": False,
        "result": "done",
        "num_turns": 2,
        "total_cost_usd": 0.01,
    }
    obj.update(overrides)
    return json.dumps(obj)


def _feed(normalizer: ClaudeStreamNormalizer, lines: list[str]) -> list[AgentEvent]:
    events: list[AgentEvent] = []
    for line in lines:
        _, produced = normalizer.feed_line(line)
        events.extend(produced)
    return events


def _only(events: list[AgentEvent], type_: EventType) -> list[AgentEvent]:
    return [event for event in events if event.type is type_]


def test_assistant_text_concat_is_envelope_free() -> None:
    events = _feed(
        ClaudeStreamNormalizer(),
        [
            _init_line(),
            _assistant_line([_text("Alpha ")]),
            _assistant_line([_text("beta.")]),
            _result_line(result="terminal summary"),
        ],
    )
    texts = [e.payload["text"] for e in _only(events, EventType.ASSISTANT_MESSAGE)]
    assert "".join(texts) == "Alpha beta."
    assert "terminal summary" not in "".join(texts)
    finished = _only(events, EventType.SESSION_FINISHED)
    assert len(finished) == 1
    assert finished[0].payload["result_text"] == "terminal summary"


def test_result_text_fallback_when_no_text_blocks() -> None:
    events = _feed(
        ClaudeStreamNormalizer(),
        [
            _init_line(),
            _assistant_line(
                [{"type": "tool_use", "id": "t-1", "name": "Bash", "input": {}}],
                stop_reason="tool_use",
            ),
            _user_line(
                [{"type": "tool_result", "tool_use_id": "t-1", "content": "ok"}]
            ),
            _result_line(result="only the result text"),
        ],
    )
    assert _only(events, EventType.ASSISTANT_MESSAGE) == []
    folder = EventFolder()
    for event in events:
        folder.feed(event)
    run = folder.completed(exit=AgentExit(returncode=0))
    assert run.final_text == "only the result text"


def test_usage_zero_filled_across_canonical_keys() -> None:
    events = _feed(
        ClaudeStreamNormalizer(),
        [
            _assistant_line([_text("hi")], usage={"input_tokens": 12}),
            _result_line(usage={"output_tokens": 9}),
        ],
    )
    context = _only(events, EventType.CONTEXT_USAGE)
    assert len(context) == 1
    assert context[0].payload["usage"] == {
        "input_tokens": 12,
        "output_tokens": 0,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0,
    }
    finished = _only(events, EventType.SESSION_FINISHED)[0]
    assert finished.payload["usage"] == {
        "input_tokens": 0,
        "output_tokens": 9,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0,
    }


def test_session_id_captured_and_carried_into_result() -> None:
    normalizer = ClaudeStreamNormalizer()
    assert normalizer.session_id is None
    events = _feed(normalizer, [_init_line("sess-42"), _result_line()])
    assert normalizer.session_id == "sess-42"
    started = _only(events, EventType.SESSION_STARTED)
    assert len(started) == 1
    assert started[0].payload["session_id"] == "sess-42"
    finished = _only(events, EventType.SESSION_FINISHED)[0]
    assert finished.payload["session_id"] == "sess-42"


def test_thinking_block_becomes_thought() -> None:
    events = _feed(
        ClaudeStreamNormalizer(),
        [_assistant_line([{"type": "thinking", "thinking": "pondering deeply"}])],
    )
    thoughts = _only(events, EventType.THOUGHT)
    assert len(thoughts) == 1
    assert thoughts[0].payload["thinking"] == "pondering deeply"
    assert thoughts[0].native_type == "assistant"


def test_tool_use_and_tool_result_payloads() -> None:
    events = _feed(
        ClaudeStreamNormalizer(),
        [
            _assistant_line(
                [
                    {
                        "type": "tool_use",
                        "id": "t-9",
                        "name": "Read",
                        "input": {"path": "x.py"},
                    }
                ],
                stop_reason="tool_use",
            ),
            _user_line(
                [
                    {
                        "type": "tool_result",
                        "tool_use_id": "t-9",
                        "content": "file contents",
                        "is_error": False,
                    }
                ]
            ),
        ],
    )
    started = _only(events, EventType.TOOL_CALL_STARTED)
    assert len(started) == 1
    assert started[0].payload["tool_use_id"] == "t-9"
    assert started[0].payload["tool_name"] == "Read"
    assert started[0].payload["tool_input"] == {"path": "x.py"}
    finished = _only(events, EventType.TOOL_CALL_FINISHED)
    assert len(finished) == 1
    assert finished[0].payload["tool_use_id"] == "t-9"
    assert finished[0].payload["is_error"] is False
    assert finished[0].payload["content"] == "file contents"


def test_permission_denials_become_events() -> None:
    line = _result_line(
        permission_denials=[
            {"tool_name": "Bash", "tool_input": {"command": "rm -rf /"}},
            {"unnamed": True},
        ]
    )
    _, events = ClaudeStreamNormalizer().feed_line(line)
    denied = [e for e in events if e.type is EventType.PERMISSION_DENIED]
    assert len(denied) == 2
    assert denied[0].payload["tool_name"] == "Bash"
    assert denied[0].payload["denial"]["tool_input"] == {"command": "rm -rf /"}
    assert denied[1].payload["tool_name"] is None
    assert events[-1].type is EventType.SESSION_FINISHED


def test_deeply_nested_json_line_is_raw_not_crash() -> None:
    depth = 10_000
    line = '{"a":' * depth + "null" + "}" * depth
    native_type, events = ClaudeStreamNormalizer().feed_line(line)
    assert native_type is None
    assert [e.type for e in events] == [EventType.RAW]
    assert events[0].payload["data"] == {"line": line}


def test_non_json_and_non_mapping_lines_are_raw() -> None:
    normalizer = ClaudeStreamNormalizer()
    for line in ("plain text line", "{not json", "[1,2,3]"):
        native_type, events = normalizer.feed_line(line)
        assert native_type is None
        assert [e.type for e in events] == [EventType.RAW]
        assert events[0].payload["data"] == {"line": line}
    assert normalizer.feed_line("   ") == (None, ())


def test_native_type_returned_correctly() -> None:
    normalizer = ClaudeStreamNormalizer()
    assert normalizer.feed_line(_init_line())[0] == "system"
    assert normalizer.feed_line(_assistant_line([_text("x")]))[0] == "assistant"
    assert normalizer.feed_line(_result_line())[0] == "result"
    native_type, events = normalizer.feed_line(json.dumps({"type": "mystery"}))
    assert native_type == "mystery"
    assert [e.type for e in events] == [EventType.RAW]
    assert events[0].native_type == "mystery"
    assert normalizer.feed_line("not json")[0] is None


def test_stop_reason_carried_and_normalized_per_subtype() -> None:
    cases = [
        ("success", False, StopReason.COMPLETED),
        ("error_max_turns", False, StopReason.MAX_TURNS),
        ("error_during_execution", True, StopReason.ERROR),
    ]
    for subtype, is_error, expected in cases:
        normalizer = ClaudeStreamNormalizer()
        events = _feed(
            normalizer,
            [
                _assistant_line([_text("working")], stop_reason="end_turn"),
                _result_line(subtype=subtype, is_error=is_error),
            ],
        )
        finished = _only(events, EventType.SESSION_FINISHED)
        assert len(finished) == 1
        payload = finished[0].payload
        assert payload["stop_reason"] == "end_turn"
        assert payload["subtype"] == subtype
        assert payload["normalized_stop"] == expected.value

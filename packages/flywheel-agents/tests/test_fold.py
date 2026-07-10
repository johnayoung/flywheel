"""Unit tests for ``EventFolder`` — normalized events in, ``CompletedRun`` out.

Events are constructed directly; no normalizer, no subprocess.
"""

from __future__ import annotations

from typing import Any

from flywheel_agents import (
    AgentEvent,
    AgentExit,
    EventFolder,
    EventType,
    StopReason,
)

_EXIT = AgentExit(returncode=0, duration_seconds=0.1)


def _event(type_: EventType, **payload: Any) -> AgentEvent:
    return AgentEvent(type=type_, payload=payload)


def _finished(**payload: Any) -> AgentEvent:
    base: dict[str, Any] = {
        "normalized_stop": StopReason.COMPLETED.value,
        "subtype": "success",
        "is_error": False,
    }
    base.update(payload)
    return AgentEvent(type=EventType.SESSION_FINISHED, payload=base)


def test_pending_tool_use_true_without_result() -> None:
    folder = EventFolder()
    folder.feed(
        _event(
            EventType.TOOL_CALL_STARTED,
            tool_use_id="t-1",
            tool_name="Bash",
            tool_input={"command": "ls"},
        )
    )
    run = folder.completed(exit=_EXIT)
    assert run.stop.pending_tool_use is True
    assert run.stop.reason is StopReason.UNKNOWN
    assert len(run.tool_interactions) == 1
    assert run.tool_interactions[0].result is None


def test_tool_result_clears_pending() -> None:
    folder = EventFolder()
    folder.feed(
        _event(
            EventType.TOOL_CALL_STARTED,
            tool_use_id="t-1",
            tool_name="Bash",
            tool_input={"command": "ls"},
        )
    )
    folder.feed(
        _event(
            EventType.TOOL_CALL_FINISHED,
            tool_use_id="t-1",
            is_error=False,
            content="src tests",
        )
    )
    folder.feed(_finished())
    run = folder.completed(exit=_EXIT)
    assert run.stop.pending_tool_use is False
    assert run.stop.reason is StopReason.COMPLETED
    interaction = run.tool_interactions[0]
    assert interaction.tool_name == "Bash"
    assert interaction.result is not None
    assert interaction.result.is_error is False
    assert interaction.result.content == "src tests"


def test_stop_unknown_without_finished() -> None:
    folder = EventFolder()
    folder.feed(_event(EventType.ASSISTANT_MESSAGE, text="working"))
    run = folder.completed(exit=_EXIT)
    assert run.stop.reason is StopReason.UNKNOWN
    assert run.stop.is_error is False
    assert run.stop.pending_tool_use is False


def test_usage_from_context_usage_when_finished_carries_none() -> None:
    context_usage = {
        "input_tokens": 10,
        "output_tokens": 2,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0,
    }
    folder = EventFolder()
    folder.feed(_event(EventType.CONTEXT_USAGE, usage=context_usage))
    folder.feed(_finished())
    run = folder.completed(exit=_EXIT)
    assert run.usage == context_usage


def test_finished_usage_wins_over_context_usage() -> None:
    context_usage = {"input_tokens": 10, "output_tokens": 2}
    finished_usage = {"input_tokens": 99, "output_tokens": 44}
    folder = EventFolder()
    folder.feed(_event(EventType.CONTEXT_USAGE, usage=context_usage))
    folder.feed(_finished(usage=finished_usage))
    run = folder.completed(exit=_EXIT)
    assert run.usage == finished_usage


def test_final_text_falls_back_to_result_text() -> None:
    folder = EventFolder()
    folder.feed(_finished(result_text="from the result envelope"))
    run = folder.completed(exit=_EXIT)
    assert run.final_text == "from the result envelope"


def test_assistant_text_wins_over_result_text() -> None:
    folder = EventFolder()
    folder.feed(_event(EventType.ASSISTANT_MESSAGE, text="real "))
    folder.feed(_event(EventType.ASSISTANT_MESSAGE, text="text"))
    folder.feed(_finished(result_text="ignored fallback"))
    run = folder.completed(exit=_EXIT)
    assert run.final_text == "real text"


def test_permission_denied_and_rate_limited_accumulate() -> None:
    folder = EventFolder()
    folder.feed(_event(EventType.PERMISSION_DENIED, tool_name="Bash"))
    folder.feed(_event(EventType.PERMISSION_DENIED))
    folder.feed(_event(EventType.RATE_LIMITED, resets_at_epoch=123))
    folder.feed(_event(EventType.RATE_LIMITED))
    run = folder.completed(exit=_EXIT)
    assert len(run.permission_denials) == 2
    assert run.permission_denials[0].tool_name == "Bash"
    assert run.permission_denials[1].tool_name is None
    assert len(run.rate_limit_events) == 2
    assert run.rate_limit_events[0].resets_at_epoch == 123.0
    assert run.rate_limit_events[1].resets_at_epoch is None


def test_session_id_from_started_and_finished() -> None:
    folder = EventFolder()
    folder.feed(_event(EventType.SESSION_STARTED, session_id="sess-a"))
    assert folder.session_id == "sess-a"
    folder.feed(_finished(session_id="sess-b"))
    run = folder.completed(exit=_EXIT)
    assert run.native_session_id == "sess-b"


def test_event_count_counts_every_event() -> None:
    folder = EventFolder()
    folder.feed(_event(EventType.RAW, data={"line": "garbage"}))
    folder.feed(_event(EventType.THOUGHT, thinking="hm"))
    folder.feed(_event(EventType.ASSISTANT_MESSAGE, text="hi"))
    folder.feed(_event(EventType.CONTEXT_USAGE, usage={"input_tokens": 1}))
    folder.feed(_finished())
    run = folder.completed(exit=_EXIT)
    assert run.event_count == 5

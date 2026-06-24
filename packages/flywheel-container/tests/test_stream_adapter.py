"""Held-out oracle for spec 00044 G4 — stream-json → IterationResult adapter.

RED until G4 lands. ``parse_stream_json`` folds the Claude CLI's stream-json
JSONL into a ``StreamOutcome`` (transcript, session id, usage, cost, turns);
``iteration_result_from_stream`` turns that into an ``IterationResult`` with the
envelope parsed from the transcript, ``messages`` empty, and token usage on the
``usage`` field (the G2 path). Pure — no Docker. Do not weaken assertions.
"""

from __future__ import annotations

import json

from flywheel_core.envelope import OPENING_FENCE, CLOSING_FENCE
from flywheel_core.envelope import Intent, ValidEnvelope

from flywheel_container import (
    iteration_result_from_stream,
    parse_stream_json,
)


def _envelope(intent: str) -> str:
    return f'{OPENING_FENCE}\n{{"intent": "{intent}"}}\n{CLOSING_FENCE}'


def _assistant(text: str, *, usage: dict | None = None, stop_reason: str = "end_turn") -> str:
    msg: dict = {"content": [{"type": "text", "text": text}], "stop_reason": stop_reason}
    if usage is not None:
        msg["usage"] = usage
    return json.dumps({"type": "assistant", "message": msg})


def _result(result: str, *, cost: float, turns: int, usage: dict | None = None) -> str:
    obj: dict = {
        "type": "result",
        "subtype": "success",
        "result": result,
        "total_cost_usd": cost,
        "num_turns": turns,
        "is_error": False,
    }
    if usage is not None:
        obj["usage"] = usage
    return json.dumps(obj)


def _system_init(session_id: str) -> str:
    return json.dumps(
        {"type": "system", "subtype": "init", "session_id": session_id}
    )


def test_parse_accumulates_text_session_cost_turns() -> None:
    lines = [
        _system_init("sess-xyz"),
        _assistant("Working on it. "),
        _assistant(
            "Done.\n" + _envelope("verify"),
            usage={
                "input_tokens": 120,
                "output_tokens": 30,
                "cache_creation_input_tokens": 4,
                "cache_read_input_tokens": 2,
            },
        ),
        _result("all set", cost=0.0123, turns=3),
    ]
    out = parse_stream_json(lines)
    assert out.session_id == "sess-xyz"
    assert out.transcript == "Working on it. Done.\n" + _envelope("verify")
    assert out.result_text == "all set"
    assert out.total_cost_usd == 0.0123
    assert out.num_turns == 3
    assert out.usage == {
        "input_tokens": 120,
        "output_tokens": 30,
        "cache_creation_input_tokens": 4,
        "cache_read_input_tokens": 2,
    }


def test_result_usage_is_authoritative_over_assistant() -> None:
    lines = [
        _assistant("x", usage={"input_tokens": 1}),
        _result("done", cost=0.0, turns=1, usage={"input_tokens": 999}),
    ]
    out = parse_stream_json(lines)
    assert out.usage["input_tokens"] == 999


def test_non_json_lines_are_skipped() -> None:
    lines = ["not json", "", "  ", _assistant("hi"), "garbage {", _result("ok", cost=0.0, turns=1)]
    out = parse_stream_json(lines)
    assert out.transcript == "hi"
    assert out.result_text == "ok"


def test_iteration_result_parses_envelope_and_carries_usage() -> None:
    lines = [
        _assistant(
            "ready\n" + _envelope("verify"),
            usage={"input_tokens": 50, "output_tokens": 10},
        ),
        _result("graded", cost=0.05, turns=2),
    ]
    result = iteration_result_from_stream(parse_stream_json(lines))
    # Envelope parsed from the accumulated transcript.
    assert isinstance(result.envelope, ValidEnvelope)
    assert result.envelope.intent is Intent.VERIFY
    # No SDK messages; tokens ride on the usage dict (G2).
    assert result.messages == ()
    assert result.usage == {
        "input_tokens": 50,
        "output_tokens": 10,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0,
    }
    assert result.signals.total_cost_usd == 0.05
    assert result.signals.num_turns == 2
    assert result.signals.session_id is None
    assert result.failure is None


def test_no_usage_yields_none_on_iteration_result() -> None:
    lines = [_assistant("hi\n" + _envelope("continue")), _result("ok", cost=0.0, turns=1)]
    result = iteration_result_from_stream(parse_stream_json(lines))
    # No usage observed anywhere → usage is None (harness falls back to the
    # empty messages tuple, i.e. zero tokens).
    assert result.usage is None
    assert isinstance(result.envelope, ValidEnvelope)
    assert result.envelope.intent is Intent.CONTINUE

"""stream-json → ``IterationResult`` adapter for the container backend.

Increment G4 of [spec 00044]. The agent runs in the container as
``claude --print --verbose --output-format stream-json`` (prompt on stdin); its
stdout is a JSONL stream. This module turns that stream into flywheel's
:class:`~flywheel_core.IterationResult` — the one piece of container-backend
logic that is genuinely novel, and it is a pure, fixture-testable function.

SDK-free: it imports only plain ``flywheel_core`` types (``IterationResult`` /
``InvocationSignals`` are dataclasses; ``parse_envelope`` is pure) — never the
agent SDK. ``messages`` is empty by construction, so token accounting flows
through ``IterationResult.usage`` (the G2 plain-dict path).

stream-json schema (the Claude CLI; validated against ``mattpocock/sandcastle``):

- ``{"type":"assistant","message":{"content":[{"type":"text","text":...}],
  "usage":{"input_tokens",...},"stop_reason":...}}`` — assistant text + usage.
- ``{"type":"result","result":...,"total_cost_usd":...,"num_turns":...,
  "subtype":...,"is_error":...,"usage":{...}}`` — terminal turn summary.
- ``{"type":"system","subtype":"init","session_id":...}`` — session id.
"""

from __future__ import annotations

import json
from collections.abc import Iterable, Mapping
from dataclasses import dataclass, field

from flywheel_core import InvocationSignals, IterationResult
from flywheel_core.envelope import parse_envelope

_USAGE_KEYS = (
    "input_tokens",
    "output_tokens",
    "cache_creation_input_tokens",
    "cache_read_input_tokens",
)


@dataclass(frozen=True)
class StreamOutcome:
    """Structured projection of one agent iteration's stream-json output."""

    transcript: str
    result_text: str | None = None
    session_id: str | None = None
    usage: dict[str, int] = field(default_factory=dict)
    total_cost_usd: float | None = None
    num_turns: int | None = None
    stop_reason: str | None = None
    result_subtype: str | None = None
    is_error: bool = False


def _coerce_usage(raw: object) -> dict[str, int]:
    if not isinstance(raw, Mapping):
        return {}
    out: dict[str, int] = {}
    for key in _USAGE_KEYS:
        value = raw.get(key)
        if isinstance(value, bool):  # bool is an int subclass — exclude
            continue
        if isinstance(value, (int, float)):
            out[key] = int(value)
    return out


def parse_stream_json(lines: Iterable[str]) -> StreamOutcome:
    """Fold a stream-json line stream into a :class:`StreamOutcome`.

    Tolerant of interleaved non-JSON lines (skipped). Assistant text blocks are
    concatenated into the transcript (the envelope rides in here). Usage is the
    turn total: the ``result`` event's usage is authoritative; absent that, the
    last assistant message's usage wins (it is turn-cumulative, mirroring the
    SDK path's ResultMessage reconciliation).
    """
    texts: list[str] = []
    session_id: str | None = None
    result_text: str | None = None
    assistant_usage: dict[str, int] = {}
    result_usage: dict[str, int] = {}
    total_cost_usd: float | None = None
    num_turns: int | None = None
    stop_reason: str | None = None
    result_subtype: str | None = None
    is_error = False

    for line in lines:
        stripped = line.strip()
        if not stripped.startswith("{"):
            continue
        try:
            obj = json.loads(stripped)
        except ValueError:
            continue
        if not isinstance(obj, Mapping):
            continue
        kind = obj.get("type")

        if kind == "assistant":
            message = obj.get("message")
            if isinstance(message, Mapping):
                content = message.get("content")
                if isinstance(content, list):
                    for block in content:
                        if (
                            isinstance(block, Mapping)
                            and block.get("type") == "text"
                            and isinstance(block.get("text"), str)
                        ):
                            texts.append(block["text"])
                sr = message.get("stop_reason")
                if isinstance(sr, str):
                    stop_reason = sr
                usage = _coerce_usage(message.get("usage"))
                if usage:
                    assistant_usage = usage

        elif kind == "result":
            res = obj.get("result")
            if isinstance(res, str):
                result_text = res
            cost = obj.get("total_cost_usd")
            if isinstance(cost, (int, float)) and not isinstance(cost, bool):
                total_cost_usd = float(cost)
            turns = obj.get("num_turns")
            if isinstance(turns, int) and not isinstance(turns, bool):
                num_turns = turns
            subtype = obj.get("subtype")
            if isinstance(subtype, str):
                result_subtype = subtype
            if obj.get("is_error") is True:
                is_error = True
            usage = _coerce_usage(obj.get("usage"))
            if usage:
                result_usage = usage

        elif kind == "system" and obj.get("subtype") == "init":
            sid = obj.get("session_id")
            if isinstance(sid, str):
                session_id = sid

    usage_source = result_usage or assistant_usage
    usage = (
        {key: usage_source.get(key, 0) for key in _USAGE_KEYS}
        if usage_source
        else {}
    )
    return StreamOutcome(
        transcript="".join(texts),
        result_text=result_text,
        session_id=session_id,
        usage=usage,
        total_cost_usd=total_cost_usd,
        num_turns=num_turns,
        stop_reason=stop_reason,
        result_subtype=result_subtype,
        is_error=is_error,
    )


def iteration_result_from_stream(
    outcome: StreamOutcome,
    *,
    failure: object | None = None,
) -> IterationResult:
    """Build an :class:`~flywheel_core.IterationResult` from a stream outcome.

    The envelope is parsed from the accumulated transcript (the same closed
    contract the SDK path uses); token usage rides on ``usage`` (G2) because
    ``messages`` is empty. ``failure`` is forwarded verbatim (the caller sets it
    on a non-zero ``docker exec`` / spawn error).
    """
    signals = InvocationSignals(
        stop_reason=outcome.stop_reason,
        num_turns=outcome.num_turns,
        total_cost_usd=outcome.total_cost_usd,
        result_is_error=outcome.is_error,
        result_subtype=outcome.result_subtype,
        api_error_status=None,
        session_id=outcome.session_id,
    )
    return IterationResult(
        transcript=outcome.transcript,
        messages=(),
        envelope=parse_envelope(outcome.transcript),
        signals=signals,
        failure=failure,  # type: ignore[arg-type]
        usage=outcome.usage or None,
    )

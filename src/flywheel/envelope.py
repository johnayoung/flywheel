"""Iteration envelope parser.

Extracts and validates the ``<!-- LOOP_STATUS -->`` envelope emitted at the
end of every agent iteration (see ``docs/loop.md``). The envelope is untrusted
protocol input: malformed JSON, missing fences, duplicate envelopes,
truncation, and unknown intents are first-class outcomes, never silent
fallbacks.

Pure module: string in, typed result out. No IO, no SDK, no logging.
"""

import json
from dataclasses import dataclass
from enum import Enum
from typing import Literal


OPENING_FENCE = "<!-- LOOP_STATUS -->"
CLOSING_FENCE = "<!-- /LOOP_STATUS -->"


class Intent(str, Enum):
    VERIFY = "verify"
    BLOCKED = "blocked"
    CONTINUE = "continue"
    ABORT = "abort"


VALID_INTENTS: frozenset[str] = frozenset(member.value for member in Intent)


@dataclass(frozen=True, kw_only=True)
class ValidEnvelope:
    intent: Intent
    reason: str | None = None
    kind: Literal["valid"] = "valid"


@dataclass(frozen=True, kw_only=True)
class MissingEnvelope:
    kind: Literal["missing"] = "missing"


@dataclass(frozen=True, kw_only=True)
class TruncatedEnvelope:
    detail: str
    kind: Literal["truncated"] = "truncated"


@dataclass(frozen=True, kw_only=True)
class DuplicateEnvelope:
    count: int
    kind: Literal["duplicate"] = "duplicate"


@dataclass(frozen=True, kw_only=True)
class MalformedEnvelope:
    reason: str
    offending: str | None = None
    kind: Literal["malformed"] = "malformed"


EnvelopeResult = (
    ValidEnvelope
    | MissingEnvelope
    | TruncatedEnvelope
    | DuplicateEnvelope
    | MalformedEnvelope
)


_OFFENDING_LIMIT = 200


def _all_occurrences(haystack: str, needle: str) -> list[int]:
    positions: list[int] = []
    start = 0
    while True:
        i = haystack.find(needle, start)
        if i == -1:
            return positions
        positions.append(i)
        start = i + len(needle)


def _truncate(payload: str) -> str:
    snippet = payload.strip()
    if len(snippet) > _OFFENDING_LIMIT:
        return snippet[:_OFFENDING_LIMIT]
    return snippet


def parse_envelope(output: str) -> EnvelopeResult:
    """Parse the loop-status envelope out of an agent iteration's output.

    The contract is closed: every input maps to exactly one variant of
    :data:`EnvelopeResult`. Unknown intents, embedded fences, and partial
    fences are all treated as distinguishable failure modes rather than
    coerced into a default.
    """

    if not isinstance(output, str):
        raise TypeError("parse_envelope requires a str input")

    open_positions = _all_occurrences(output, OPENING_FENCE)
    close_positions = _all_occurrences(output, CLOSING_FENCE)

    if not open_positions and not close_positions:
        return MissingEnvelope()

    if not open_positions:
        return MalformedEnvelope(
            reason="closing fence found without a matching opening fence",
        )

    if len(open_positions) > 1:
        return DuplicateEnvelope(count=len(open_positions))

    if not close_positions:
        return TruncatedEnvelope(
            detail="opening fence found without a matching closing fence",
        )

    if len(close_positions) > 1:
        return MalformedEnvelope(
            reason="multiple closing fences for a single opening fence",
        )

    open_pos = open_positions[0]
    close_pos = close_positions[0]

    if close_pos < open_pos:
        return MalformedEnvelope(
            reason="closing fence appears before opening fence",
        )

    inner = output[open_pos + len(OPENING_FENCE) : close_pos]

    try:
        data = json.loads(inner)
    except json.JSONDecodeError as exc:
        return MalformedEnvelope(
            reason=f"envelope payload is not valid JSON: {exc.msg}",
            offending=_truncate(inner),
        )

    if not isinstance(data, dict):
        return MalformedEnvelope(
            reason=(
                "envelope payload must be a JSON object, "
                f"got {type(data).__name__}"
            ),
            offending=_truncate(inner),
        )

    if "intent" not in data:
        return MalformedEnvelope(
            reason="envelope payload missing required 'intent' field",
            offending=_truncate(inner),
        )

    raw_intent = data["intent"]
    if not isinstance(raw_intent, str) or raw_intent not in VALID_INTENTS:
        allowed = sorted(VALID_INTENTS)
        return MalformedEnvelope(
            reason=(
                f"envelope 'intent' must be one of {allowed}, "
                f"got {raw_intent!r}"
            ),
            offending=_truncate(inner),
        )

    raw_reason = data.get("reason")
    if raw_reason is not None and not isinstance(raw_reason, str):
        return MalformedEnvelope(
            reason=(
                "envelope 'reason' must be a string when present, "
                f"got {type(raw_reason).__name__}"
            ),
            offending=_truncate(inner),
        )

    return ValidEnvelope(intent=Intent(raw_intent), reason=raw_reason)

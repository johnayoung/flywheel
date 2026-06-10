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
class CommandGraderRequirement:
    """Predicate: a named command grader on the task must pass at recheck time."""

    name: str
    type: Literal["command_grader"] = "command_grader"


@dataclass(frozen=True, kw_only=True)
class FileExistsRequirement:
    """Predicate: a path must exist (``present=True``) or be absent (``present=False``)."""

    path: str
    present: bool = True
    type: Literal["file_exists"] = "file_exists"


@dataclass(frozen=True, kw_only=True)
class EnvVarSetRequirement:
    """Predicate: an environment variable must be set to a non-empty value."""

    name: str
    type: Literal["env_var_set"] = "env_var_set"


BlockedRequirement = (
    CommandGraderRequirement | FileExistsRequirement | EnvVarSetRequirement
)


VALID_REQUIREMENT_TYPES: frozenset[str] = frozenset(
    {"command_grader", "file_exists", "env_var_set"}
)


@dataclass(frozen=True, kw_only=True)
class ValidEnvelope:
    intent: Intent
    reason: str | None = None
    requires: tuple[BlockedRequirement, ...] = ()
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


def _parse_requirement(
    entry: object, inner: str
) -> BlockedRequirement | MalformedEnvelope:
    if not isinstance(entry, dict):
        return MalformedEnvelope(
            reason=(
                "envelope 'requires' entries must each be a JSON object, "
                f"got {type(entry).__name__}"
            ),
            offending=_truncate(inner),
        )

    if "type" not in entry:
        return MalformedEnvelope(
            reason="envelope 'requires' entry missing required 'type' field",
            offending=_truncate(inner),
        )

    raw_type = entry["type"]
    if not isinstance(raw_type, str) or raw_type not in VALID_REQUIREMENT_TYPES:
        allowed = sorted(VALID_REQUIREMENT_TYPES)
        return MalformedEnvelope(
            reason=(
                f"envelope 'requires' entry 'type' must be one of {allowed}, "
                f"got {raw_type!r}"
            ),
            offending=_truncate(inner),
        )

    if raw_type == "command_grader":
        if "name" not in entry:
            return MalformedEnvelope(
                reason=(
                    "envelope 'requires' command_grader entry missing required "
                    "'name' field"
                ),
                offending=_truncate(inner),
            )
        name = entry["name"]
        if not isinstance(name, str):
            return MalformedEnvelope(
                reason=(
                    "envelope 'requires' command_grader 'name' must be a string, "
                    f"got {type(name).__name__}"
                ),
                offending=_truncate(inner),
            )
        if not name:
            return MalformedEnvelope(
                reason=(
                    "envelope 'requires' command_grader 'name' must be a "
                    "non-empty string"
                ),
                offending=_truncate(inner),
            )
        return CommandGraderRequirement(name=name)

    if raw_type == "file_exists":
        if "path" not in entry:
            return MalformedEnvelope(
                reason=(
                    "envelope 'requires' file_exists entry missing required "
                    "'path' field"
                ),
                offending=_truncate(inner),
            )
        path = entry["path"]
        if not isinstance(path, str):
            return MalformedEnvelope(
                reason=(
                    "envelope 'requires' file_exists 'path' must be a string, "
                    f"got {type(path).__name__}"
                ),
                offending=_truncate(inner),
            )
        if not path:
            return MalformedEnvelope(
                reason=(
                    "envelope 'requires' file_exists 'path' must be a "
                    "non-empty string"
                ),
                offending=_truncate(inner),
            )
        if "present" in entry:
            present = entry["present"]
            if not isinstance(present, bool):
                return MalformedEnvelope(
                    reason=(
                        "envelope 'requires' file_exists 'present' must be a "
                        f"bool when provided, got {type(present).__name__}"
                    ),
                    offending=_truncate(inner),
                )
        else:
            present = True
        return FileExistsRequirement(path=path, present=present)

    # raw_type == "env_var_set"
    if "name" not in entry:
        return MalformedEnvelope(
            reason=(
                "envelope 'requires' env_var_set entry missing required "
                "'name' field"
            ),
            offending=_truncate(inner),
        )
    name = entry["name"]
    if not isinstance(name, str):
        return MalformedEnvelope(
            reason=(
                "envelope 'requires' env_var_set 'name' must be a string, "
                f"got {type(name).__name__}"
            ),
            offending=_truncate(inner),
        )
    if not name:
        return MalformedEnvelope(
            reason=(
                "envelope 'requires' env_var_set 'name' must be a "
                "non-empty string"
            ),
            offending=_truncate(inner),
        )
    return EnvVarSetRequirement(name=name)


def _parse_requires(
    data: dict[str, object], inner: str
) -> tuple[BlockedRequirement, ...] | MalformedEnvelope:
    if "requires" not in data:
        return MalformedEnvelope(
            reason=(
                "envelope payload with intent='blocked' missing required "
                "'requires' field"
            ),
            offending=_truncate(inner),
        )

    raw_requires = data["requires"]
    if not isinstance(raw_requires, list):
        return MalformedEnvelope(
            reason=(
                "envelope 'requires' must be a list when intent='blocked', "
                f"got {type(raw_requires).__name__}"
            ),
            offending=_truncate(inner),
        )

    if len(raw_requires) == 0:
        return MalformedEnvelope(
            reason=(
                "envelope 'requires' must contain at least one predicate when "
                "intent='blocked'"
            ),
            offending=_truncate(inner),
        )

    parsed: list[BlockedRequirement] = []
    for entry in raw_requires:
        result = _parse_requirement(entry, inner)
        if isinstance(result, MalformedEnvelope):
            return result
        parsed.append(result)
    return tuple(parsed)


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

    requires: tuple[BlockedRequirement, ...] = ()
    if raw_intent == Intent.BLOCKED.value:
        parsed_requires = _parse_requires(data, inner)
        if isinstance(parsed_requires, MalformedEnvelope):
            return parsed_requires
        requires = parsed_requires

    return ValidEnvelope(
        intent=Intent(raw_intent),
        reason=raw_reason,
        requires=requires,
    )

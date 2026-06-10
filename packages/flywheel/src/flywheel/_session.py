"""Pure transcript domain model + cursor-driven tailer for one run.

The session screen renders a chat-style transcript of one run's merged
audit stream. The shape of "what to render" is a function of
:class:`flywheel_core.store_protocols.AuditRecord` only, so this module stays
free of Textual imports and is exercised both by the Pilot tests and
the snapshot suite.

Two surfaces are exported:

* :func:`classify` — convert one :class:`AuditRecord` into zero or more
  :class:`TranscriptEntry` objects, each tagged with an
  :class:`EntryKind` (agent text / tool call / tool result / operator
  say / lifecycle / gate / system / result / user text). Multiple
  content blocks inside one ``AssistantMessage`` produce multiple
  entries that share the record's ``sequence`` and disambiguate via
  ``sub_index`` so the strict per-run ordering survives a fan-out.
* :class:`TranscriptTailer` — cursor-incremental reader over
  :meth:`AuditStore.read_audit_since`. Each ``fetch()`` returns only
  the records appended since the last call (FR-4 acceptance: tailing
  is cursor-incremental, never re-reading full history per tick).
  Records are piped through the same default :class:`Redactor` policy
  the audit CLI applies on read so the session inherits the operator-
  facing read path's best-effort secret suppression.
"""

from __future__ import annotations

import json
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from datetime import datetime
from enum import Enum
from typing import Any

from flywheel_core.lifecycle import Status
from flywheel_core.redaction import (
    EnvValueRedactor,
    Redactor,
    compose,
    default_policy,
)
from flywheel_core.store_protocols import (
    AuditRecord,
    AuditStore,
    EventRecord,
    SdkMessageRecord,
)


# Lifecycle statuses at which a run no longer makes forward progress.
# Mirrors :mod:`flywheel_core.audit`'s follow-loop exit predicate so the
# session view's terminal-banner gate stays aligned with the audit
# stream's stopping condition.
TERMINAL_STATUSES: frozenset[Status] = frozenset(
    {
        Status.DONE,
        Status.FAILED,
        Status.FAILED_VALIDATION,
        Status.INTERNAL_ERROR,
        Status.INTERRUPTED,
    }
)

# Maximum number of characters retained from a tool-call argument or
# tool-result body when collapsing into a transcript line. Long bodies
# are truncated with an ellipsis so a single 100KB tool result does not
# blow up the transcript widget.
_COLLAPSED_BODY_LIMIT: int = 240

# Maximum number of characters retained from an individual key=value
# pair when summarising tool-call args.
_TOOL_ARG_VALUE_LIMIT: int = 60

# Per-tool primary-argument map for the TOOL_CALL summary line. Tools in
# this table render their key argument verbatim (file_path for Edit /
# Read / Write, command for Bash, pattern for Grep / Glob) so the line
# is scannable at a glance; tools not in this table fall back to the
# generic ``k=v, k=v`` summary of the first two input keys.
_TOOL_KEY_ARG: dict[str, str] = {
    "Edit": "file_path",
    "Read": "file_path",
    "Write": "file_path",
    "Bash": "command",
    "Grep": "pattern",
    "Glob": "pattern",
}

# Maximum number of lines of detail retained on an is_error TOOL_RESULT.
# Anything beyond is dropped and replaced with a ``... +N more lines``
# suffix so a pathological stack trace cannot blow up the widget.
_TOOL_ERROR_LINE_LIMIT: int = 10

# Per-record cap on the number of content blocks fanned out into
# separate transcript entries. Keeps a pathological assistant turn
# from producing thousands of rows; the cap is generous enough that
# real turns are unaffected.
_MAX_BLOCKS_PER_MESSAGE: int = 32

# Environment variables whose ambient values seed the default-policy
# :class:`EnvValueRedactor` so the operator's own ANTHROPIC_API_KEY is
# redacted out of the transcript even when an agent has echoed it.
# Mirrors :data:`flywheel_core.audit._cli._CLI_ENV_SEEDS` so the session
# transcript and the audit CLI converge on the same protection set.
_DEFAULT_ENV_SEEDS: tuple[str, ...] = ("ANTHROPIC_API_KEY",)


class EntryKind(str, Enum):
    """The visible classes of transcript line the session screen renders.

    The names line up with the FR-4 requirement that each message class
    render distinctly: agent text, tool calls with collapsed results,
    operator ``say`` injections, and lifecycle / gate events.
    """

    AGENT_TEXT = "agent_text"
    TOOL_CALL = "tool_call"
    TOOL_RESULT = "tool_result"
    USER_TEXT = "user_text"
    OPERATOR_SAY = "operator_say"
    SYSTEM = "system"
    RESULT = "result"
    LIFECYCLE = "lifecycle"
    GATE = "gate"


@dataclass(frozen=True, kw_only=True)
class TranscriptEntry:
    """One rendered chat-style line in the session transcript.

    A single :class:`AuditRecord` may expand into more than one entry
    when an ``AssistantMessage`` carries multiple content blocks (text
    + tool_use, multiple tool_uses, ...). All entries from a single
    record share the record's ``sequence`` and disambiguate via
    ``sub_index`` so the screen renders them in the order they appeared
    inside the record without ever crossing the per-run sequence
    ordering between records.

    ``header`` is the short tag printed at the line head (``agent``,
    ``tool(Edit)``, ``operator``, ...); ``body`` is the collapsed
    payload. Both are pre-sanitised by :class:`TranscriptTailer`'s
    redactor when used end-to-end so the screen never needs to look
    inside the original payload again.

    ``control_command_id`` carries the store-side id of the control
    command this entry resolves (set on
    ``harness.control_command_applied`` and
    ``harness.control_command_failed`` events only); the session screen
    uses it to flip a pending steering-command marker to applied /
    failed when the watcher's telemetry catches up. ``control_command_error``
    is the human-readable failure detail attached to the failed variant
    so the screen can surface ``error_type: message`` inline.
    """

    sequence: int
    sub_index: int
    ts: datetime
    kind: EntryKind
    header: str
    body: str
    attempt_number: int | None
    iteration_number: int | None
    control_command_id: int | None = None
    control_command_error: str | None = None


# --- Classification helpers ------------------------------------------------


def _short(value: object, limit: int = _COLLAPSED_BODY_LIMIT) -> str:
    """Return ``value`` flattened to one line and capped at ``limit`` chars.

    Mirrors :func:`flywheel_core.workflow._short` so the session transcript
    and the dashboard's ``last_detail`` collapse use identical rules:
    newlines become spaces, an over-budget string is suffixed with the
    Unicode ellipsis.
    """

    text = str(value).replace("\n", " ").replace("\r", " ")
    if len(text) <= limit:
        return text
    keep = max(limit - 1, 1)
    return text[:keep] + "…"


def _normalize_agent_text(text: str) -> str:
    """Sanitise an AGENT_TEXT body without flattening or truncating.

    Used by the assistant-text and assistant-thinking branches so prose
    of any length reaches the session screen with paragraph breaks
    preserved (FR-1). Carriage returns are dropped so a CRLF stream
    produces a single line break per paragraph; per-line trailing
    whitespace is stripped so an editor's stray tabs or spaces after a
    ``\\n`` do not become a phantom blank line; trailing whitespace at
    the very end is stripped so the entry never renders a blank tail.
    Leading whitespace is preserved -- a code block or quoted reply may
    legitimately be indented.
    """

    cleaned = text.replace("\r\n", "\n").replace("\r", "\n")
    cleaned = "\n".join(line.rstrip() for line in cleaned.split("\n"))
    return cleaned.rstrip()


def _flatten_to_line(value: object) -> str:
    """Collapse ``value`` to a single line without truncating it.

    Used for the primary tool-call argument so a long single-line
    command surfaces in full and the widget wraps it at width rather
    than the classifier truncating with an ellipsis (FR-2 edge case:
    very long single-line commands wrap at widget width rather than
    truncating).
    """

    return str(value).replace("\n", " ").replace("\r", " ")


def _summarise_tool_input(name: str, input_obj: Any) -> str:
    """Collapse a tool_use ``input`` mapping to a scannable summary.

    For tools listed in :data:`_TOOL_KEY_ARG` the body is the value of
    the primary argument (``file_path`` for Edit/Read/Write, ``command``
    for Bash, ``pattern`` for Grep/Glob), flattened to one line so the
    row stays scannable. Long values are not truncated -- the widget
    wraps them at width. Unmapped tools, and mapped tools whose primary
    key is absent or empty, fall back to the generic ``k=v, k=v`` form
    over the first two keys; each value in that form is capped at
    :data:`_TOOL_ARG_VALUE_LIMIT` characters via :func:`_short`. Non-
    mapping inputs collapse via :func:`_short` for symmetry, and never
    raise -- an odd payload still renders rather than crashing the
    classifier.
    """

    if not isinstance(input_obj, Mapping):
        return _short(input_obj)
    primary = _TOOL_KEY_ARG.get(name)
    if primary is not None and primary in input_obj:
        flattened = _flatten_to_line(input_obj[primary])
        if flattened:
            return flattened
    if not input_obj:
        return ""
    parts: list[str] = []
    for key, value in list(input_obj.items())[:2]:
        parts.append(f"{key}={_short(value, _TOOL_ARG_VALUE_LIMIT)}")
    return ", ".join(parts)


def _classify_assistant(
    record: SdkMessageRecord, sequence: int
) -> list[TranscriptEntry]:
    """Expand one ``AssistantMessage`` into per-block transcript entries.

    Each ``text`` block becomes an :attr:`EntryKind.AGENT_TEXT` entry;
    each ``tool_use`` block becomes an :attr:`EntryKind.TOOL_CALL`
    entry whose body is the collapsed ``name(arg=val, ...)`` form. A
    block with neither ``text`` nor a known ``type`` is rendered as
    agent text using its stringified shape so unknown content never
    drops silently.
    """

    content = record.payload.get("content") or []
    if not isinstance(content, Sequence):
        return _fallback_assistant_entry(record, sequence)
    entries: list[TranscriptEntry] = []
    sub_index = 0
    for block in list(content)[:_MAX_BLOCKS_PER_MESSAGE]:
        entry = _classify_assistant_block(record, sequence, sub_index, block)
        if entry is not None:
            entries.append(entry)
            sub_index += 1
    if not entries:
        return _fallback_assistant_entry(record, sequence)
    return entries


def _classify_assistant_block(
    record: SdkMessageRecord,
    sequence: int,
    sub_index: int,
    block: Any,
) -> TranscriptEntry | None:
    """Translate one content block into a single :class:`TranscriptEntry`.

    Returns ``None`` when the block is empty (e.g. a ``text`` field
    that is the empty string) so an idle keep-alive turn does not
    leave a blank line in the transcript.
    """

    if not isinstance(block, Mapping):
        body = _short(block)
        if not body:
            return None
        return TranscriptEntry(
            sequence=sequence,
            sub_index=sub_index,
            ts=record.ts,
            kind=EntryKind.AGENT_TEXT,
            header="agent",
            body=body,
            attempt_number=record.attempt_number,
            iteration_number=record.iteration_number,
        )
    btype = block.get("type")
    if btype == "tool_use":
        name = str(block.get("name", "?"))
        summary = _summarise_tool_input(name, block.get("input"))
        return TranscriptEntry(
            sequence=sequence,
            sub_index=sub_index,
            ts=record.ts,
            kind=EntryKind.TOOL_CALL,
            header=f"tool({name})",
            body=summary,
            attempt_number=record.attempt_number,
            iteration_number=record.iteration_number,
        )
    if btype == "thinking":
        # Extended-thinking blocks are rendered as agent text with a
        # distinguishing header so operators can see the chain of
        # reasoning without a separate widget class. Thinking is prose,
        # so it shares the AGENT_TEXT no-flatten / no-truncate path with
        # the ``text`` branch below (FR-1).
        text = block.get("thinking") or block.get("text")
        if not isinstance(text, str):
            return None
        body = _normalize_agent_text(text)
        if not body:
            return None
        return TranscriptEntry(
            sequence=sequence,
            sub_index=sub_index,
            ts=record.ts,
            kind=EntryKind.AGENT_TEXT,
            header="agent(thinking)",
            body=body,
            attempt_number=record.attempt_number,
            iteration_number=record.iteration_number,
        )
    text = block.get("text")
    if isinstance(text, str):
        # AGENT_TEXT prose is rendered verbatim: original line breaks
        # survive, the body is not capped, no ellipsis is appended. The
        # screen wraps long lines to the widget width. Empty / pure-
        # whitespace blocks still collapse to ``None`` so an idle keep-
        # alive turn does not leave a blank line in the transcript.
        body = _normalize_agent_text(text)
        if not body:
            return None
        return TranscriptEntry(
            sequence=sequence,
            sub_index=sub_index,
            ts=record.ts,
            kind=EntryKind.AGENT_TEXT,
            header="agent",
            body=body,
            attempt_number=record.attempt_number,
            iteration_number=record.iteration_number,
        )
    body = _short(block)
    if not body:
        return None
    return TranscriptEntry(
        sequence=sequence,
        sub_index=sub_index,
        ts=record.ts,
        kind=EntryKind.AGENT_TEXT,
        header="agent",
        body=body,
        attempt_number=record.attempt_number,
        iteration_number=record.iteration_number,
    )


def _fallback_assistant_entry(
    record: SdkMessageRecord, sequence: int
) -> list[TranscriptEntry]:
    """Single-entry fallback when an assistant payload is empty / odd."""

    return [
        TranscriptEntry(
            sequence=sequence,
            sub_index=0,
            ts=record.ts,
            kind=EntryKind.AGENT_TEXT,
            header="agent",
            body="(empty)",
            attempt_number=record.attempt_number,
            iteration_number=record.iteration_number,
        )
    ]


def _classify_user(
    record: SdkMessageRecord, sequence: int
) -> list[TranscriptEntry]:
    """Expand one ``UserMessage`` into per-block transcript entries.

    Tool-result blocks become :attr:`EntryKind.TOOL_RESULT` lines whose
    body is the collapsed ``content`` field; plain text blocks become
    :attr:`EntryKind.USER_TEXT`. Operator ``say`` injections arrive at
    this seam too but are attributed by the matching
    ``harness.control_command_applied`` event (handled in
    :func:`_classify_event`); the user-side text remains classified as
    ``USER_TEXT`` to keep this function free of cross-record state.
    """

    content = record.payload.get("content") or []
    if not isinstance(content, Sequence):
        return _fallback_user_entry(record, sequence)
    entries: list[TranscriptEntry] = []
    sub_index = 0
    for block in list(content)[:_MAX_BLOCKS_PER_MESSAGE]:
        entry = _classify_user_block(record, sequence, sub_index, block)
        if entry is not None:
            entries.append(entry)
            sub_index += 1
    if not entries:
        return _fallback_user_entry(record, sequence)
    return entries


def _format_tool_result_body(text: str, *, is_error: bool) -> str:
    """Format a tool result body for the indented outcome line (FR-3).

    On success the body is a single brief line: literal ``ok`` plus a
    content hint -- the first line of output when there is exactly one,
    otherwise an ``ok (N lines)`` line-count summary. Empty success
    output collapses to a bare ``ok``. On error the body keeps up to
    :data:`_TOOL_ERROR_LINE_LIMIT` lines of detail verbatim and appends
    a ``... +N more lines`` suffix for anything beyond, so a 100KB
    stack trace cannot blow up the transcript widget. Carriage returns
    are normalised so a CRLF stream produces one line break per
    paragraph.
    """

    normalised = text.replace("\r\n", "\n").replace("\r", "\n").strip("\n")
    if is_error:
        return _format_tool_error_body(normalised)
    return _format_tool_success_body(normalised)


def _format_tool_success_body(text: str) -> str:
    """Render the brief one-line outcome for a successful tool result."""

    if not text.strip():
        return "ok"
    non_empty = [line for line in text.split("\n") if line.strip()]
    if not non_empty:
        return "ok"
    if len(non_empty) == 1:
        return f"ok: {_short(non_empty[0].strip())}"
    return f"ok ({len(non_empty)} lines)"


def _format_tool_error_body(text: str) -> str:
    """Render an is_error tool result with a 10-line cap + overflow marker."""

    if not text:
        return ""
    lines = text.split("\n")
    if len(lines) <= _TOOL_ERROR_LINE_LIMIT:
        return text
    kept = lines[:_TOOL_ERROR_LINE_LIMIT]
    remaining = len(lines) - _TOOL_ERROR_LINE_LIMIT
    return "\n".join(kept) + f"\n... +{remaining} more lines"


def _classify_user_block(
    record: SdkMessageRecord,
    sequence: int,
    sub_index: int,
    block: Any,
) -> TranscriptEntry | None:
    """Translate one user content block."""

    if not isinstance(block, Mapping):
        body = _short(block)
        if not body:
            return None
        return TranscriptEntry(
            sequence=sequence,
            sub_index=sub_index,
            ts=record.ts,
            kind=EntryKind.USER_TEXT,
            header="user",
            body=body,
            attempt_number=record.attempt_number,
            iteration_number=record.iteration_number,
        )
    if "tool_use_id" in block:
        body_raw = block.get("content")
        if body_raw is None:
            raw_text = ""
        elif isinstance(body_raw, str):
            raw_text = body_raw
        else:
            # A non-string content payload (e.g. SDK content-block list)
            # collapses through :func:`_short` so the classifier never
            # raises on an odd shape; the formatter then folds it into
            # the success / error body shape below.
            raw_text = _short(body_raw)
        is_error = bool(block.get("is_error"))
        header = "tool_result(error)" if is_error else "tool_result"
        body = _format_tool_result_body(raw_text, is_error=is_error)
        return TranscriptEntry(
            sequence=sequence,
            sub_index=sub_index,
            ts=record.ts,
            kind=EntryKind.TOOL_RESULT,
            header=header,
            body=body,
            attempt_number=record.attempt_number,
            iteration_number=record.iteration_number,
        )
    text = block.get("text")
    if isinstance(text, str):
        if not text:
            return None
        return TranscriptEntry(
            sequence=sequence,
            sub_index=sub_index,
            ts=record.ts,
            kind=EntryKind.USER_TEXT,
            header="user",
            body=_short(text),
            attempt_number=record.attempt_number,
            iteration_number=record.iteration_number,
        )
    body = _short(block)
    if not body:
        return None
    return TranscriptEntry(
        sequence=sequence,
        sub_index=sub_index,
        ts=record.ts,
        kind=EntryKind.USER_TEXT,
        header="user",
        body=body,
        attempt_number=record.attempt_number,
        iteration_number=record.iteration_number,
    )


def _fallback_user_entry(
    record: SdkMessageRecord, sequence: int
) -> list[TranscriptEntry]:
    """Single-entry fallback when a user payload is empty / odd."""

    return [
        TranscriptEntry(
            sequence=sequence,
            sub_index=0,
            ts=record.ts,
            kind=EntryKind.USER_TEXT,
            header="user",
            body="(empty)",
            attempt_number=record.attempt_number,
            iteration_number=record.iteration_number,
        )
    ]


def _classify_system(
    record: SdkMessageRecord, sequence: int
) -> list[TranscriptEntry]:
    """Render a system-init / system-update SDK message."""

    subtype_raw = record.payload.get("subtype")
    subtype = str(subtype_raw) if subtype_raw is not None else "(system)"
    return [
        TranscriptEntry(
            sequence=sequence,
            sub_index=0,
            ts=record.ts,
            kind=EntryKind.SYSTEM,
            header="system",
            body=subtype,
            attempt_number=record.attempt_number,
            iteration_number=record.iteration_number,
        )
    ]


def _classify_result(
    record: SdkMessageRecord, sequence: int
) -> list[TranscriptEntry]:
    """Render a ResultMessage as a turn-end marker."""

    payload = record.payload
    body = (
        f"subtype={payload.get('subtype', '?')} "
        f"turns={payload.get('num_turns', '?')} "
        f"dur={payload.get('duration_ms', '?')}ms"
    )
    return [
        TranscriptEntry(
            sequence=sequence,
            sub_index=0,
            ts=record.ts,
            kind=EntryKind.RESULT,
            header="result",
            body=body,
            attempt_number=record.attempt_number,
            iteration_number=record.iteration_number,
        )
    ]


# Telemetry event kinds the session view promotes to dedicated lifecycle
# or gate lines. Anything else (``harness.audit_finalized`` etc.) is
# rendered as a generic lifecycle entry so an unknown future event
# still appears in order rather than being dropped.
_GATE_EVENT_KINDS: frozenset[str] = frozenset(
    {
        "harness.awaiting_approval",
        "harness.manual_approved",
        "harness.manual_rejected",
    }
)


def _classify_event(
    record: EventRecord, sequence: int,
) -> list[TranscriptEntry]:
    """Render one :class:`EventRecord` as a lifecycle / gate / operator line.

    Three distinct classes the session screen surfaces explicitly:

    * ``harness.control_command_applied`` with ``payload.kind == "say"``
      becomes an :attr:`EntryKind.OPERATOR_SAY` entry so the operator
      injection is visible *as* the operator turn -- the spec calls
      this the "operator say injection" message class. The matching
      SDK ``UserMessage`` still renders as a generic user line just
      below it (different sequence value), giving the operator both
      the attribution and the literal text the SDK saw.
    * ``harness.awaiting_approval`` / ``harness.manual_approved`` /
      ``harness.manual_rejected`` become :attr:`EntryKind.GATE` lines
      so a manual-gate decision is visually distinct from generic
      lifecycle telemetry.
    * Every other ``harness.*`` event becomes a generic
      :attr:`EntryKind.LIFECYCLE` line; the kind is the header and the
      payload digest is the body. Custom plugin events without the
      ``harness.`` prefix render the same way.
    """

    kind = record.kind
    payload = record.payload
    if kind == "harness.control_command_applied":
        command_kind_raw = payload.get("kind")
        command_kind = str(command_kind_raw) if command_kind_raw is not None else "?"
        command_id_raw = payload.get("command_id")
        command_id = (
            int(command_id_raw) if isinstance(command_id_raw, int) else None
        )
        if command_kind == "say":
            inner = payload.get("payload")
            text_raw = (
                inner.get("text") if isinstance(inner, Mapping) else None
            )
            text = (
                _short(text_raw) if isinstance(text_raw, str) else "(no text)"
            )
            return [
                TranscriptEntry(
                    sequence=sequence,
                    sub_index=0,
                    ts=record.ts,
                    kind=EntryKind.OPERATOR_SAY,
                    header="operator(say)",
                    body=text,
                    attempt_number=record.attempt_number,
                    iteration_number=None,
                    control_command_id=command_id,
                )
            ]
        body = f"applied {command_kind}"
        return [
            TranscriptEntry(
                sequence=sequence,
                sub_index=0,
                ts=record.ts,
                kind=EntryKind.LIFECYCLE,
                header="control",
                body=body,
                attempt_number=record.attempt_number,
                iteration_number=None,
                control_command_id=command_id,
            )
        ]
    if kind == "harness.control_command_failed":
        command_kind_raw = payload.get("kind")
        command_kind = str(command_kind_raw) if command_kind_raw is not None else "?"
        command_id_raw = payload.get("command_id")
        command_id = (
            int(command_id_raw) if isinstance(command_id_raw, int) else None
        )
        error_type_raw = payload.get("error_type")
        message_raw = payload.get("message")
        error_type = (
            str(error_type_raw) if isinstance(error_type_raw, str) else None
        )
        error_message = (
            str(message_raw) if isinstance(message_raw, str) else None
        )
        if error_type and error_message:
            error_detail = f"{error_type}: {error_message}"
        elif error_message:
            error_detail = error_message
        elif error_type:
            error_detail = error_type
        else:
            error_detail = "(no detail)"
        return [
            TranscriptEntry(
                sequence=sequence,
                sub_index=0,
                ts=record.ts,
                kind=EntryKind.LIFECYCLE,
                header="control",
                body=f"failed {command_kind}: {error_detail}",
                attempt_number=record.attempt_number,
                iteration_number=None,
                control_command_id=command_id,
                control_command_error=error_detail,
            )
        ]
    if kind in _GATE_EVENT_KINDS:
        grader_name = payload.get("grader_name")
        ordinal = payload.get("awaiting_ordinal")
        if kind == "harness.awaiting_approval":
            head = "gate(awaiting)"
        elif kind == "harness.manual_approved":
            head = "gate(approved)"
        else:
            head = "gate(rejected)"
        parts: list[str] = []
        if isinstance(grader_name, str) and grader_name:
            parts.append(f"grader={grader_name}")
        if isinstance(ordinal, int):
            parts.append(f"ordinal={ordinal}")
        body = " ".join(parts) if parts else _short(dict(payload))
        return [
            TranscriptEntry(
                sequence=sequence,
                sub_index=0,
                ts=record.ts,
                kind=EntryKind.GATE,
                header=head,
                body=body,
                attempt_number=record.attempt_number,
                iteration_number=None,
            )
        ]
    body = _short(_payload_digest(payload))
    return [
        TranscriptEntry(
            sequence=sequence,
            sub_index=0,
            ts=record.ts,
            kind=EntryKind.LIFECYCLE,
            header=kind,
            body=body,
            attempt_number=record.attempt_number,
            iteration_number=None,
        )
    ]


def _payload_digest(payload: Mapping[str, Any]) -> str:
    """Render a payload as compact JSON for the lifecycle-line body.

    Sorted keys make the output deterministic across runs and Python
    interpreter versions; ``default=str`` is the fall-back coercion for
    any non-JSON-native scalar (datetimes, enums) so a custom plugin
    event never crashes the classifier.
    """

    if not payload:
        return ""
    try:
        return json.dumps(
            dict(payload), default=str, sort_keys=True, separators=(",", ":")
        )
    except (TypeError, ValueError):
        return _short(dict(payload))


def classify(record: AuditRecord) -> list[TranscriptEntry]:
    """Convert one :class:`AuditRecord` into ordered transcript entries.

    The record's ``sequence`` is required (the store contract assigns
    it on every record returned from ``read_audit_since``); a defensive
    ``0`` is substituted only when a synthetic record is fed in
    directly. Returns a list because an ``AssistantMessage`` with both
    text and a tool_use produces two entries that must keep their
    intra-record order.
    """

    sequence = record.sequence if record.sequence is not None else 0
    if isinstance(record, EventRecord):
        return _classify_event(record, sequence)
    # SdkMessageRecord branch.
    message_type = record.message_type
    if message_type == "AssistantMessage":
        return _classify_assistant(record, sequence)
    if message_type == "UserMessage":
        return _classify_user(record, sequence)
    if message_type == "SystemMessage":
        return _classify_system(record, sequence)
    if message_type == "ResultMessage":
        return _classify_result(record, sequence)
    return [
        TranscriptEntry(
            sequence=sequence,
            sub_index=0,
            ts=record.ts,
            kind=EntryKind.SYSTEM,
            header=message_type or "sdk",
            body=_short(_payload_digest(record.payload)),
            attempt_number=record.attempt_number,
            iteration_number=record.iteration_number,
        )
    ]


# --- Tailer ----------------------------------------------------------------


def build_default_redactor() -> Redactor:
    """Construct the default-policy :class:`Redactor` the session uses.

    Mirrors the wiring :mod:`flywheel_core.audit._cli` applies on the operator-
    facing read path: the built-in default policy composed with an
    :class:`EnvValueRedactor` seeded from :data:`_DEFAULT_ENV_SEEDS` so the
    operator's own API keys are caught even when an agent has echoed them
    verbatim. The :class:`EnvValueRedactor` silently drops absent / too-
    short values, so this is safe to call from environments where no
    secrets are set.
    """

    base = default_policy()
    env_redactor = EnvValueRedactor(list(_DEFAULT_ENV_SEEDS))
    return compose(base, env_redactor)


class TranscriptTailer:
    """Cursor-incremental tailer over one run's merged audit stream.

    Wraps :meth:`AuditStore.read_audit_since` so each :meth:`fetch` call
    returns only the records written since the previous call. The
    cursor is the per-run monotonic sequence the store assigns; it is
    initialised to ``0`` (drain everything on first fetch) and advances
    on the maximum sequence observed. Records are piped through the
    same default :class:`Redactor` policy the audit CLI applies so the
    session inherits the operator-facing read path's best-effort secret
    suppression.

    The tailer holds no I/O resources beyond the store handle; closing
    or reopening the store is the caller's concern.
    """

    def __init__(
        self,
        store: AuditStore,
        run_id: str,
        *,
        redactor: Redactor | None = None,
        cursor: int = 0,
    ) -> None:
        self._store = store
        self._run_id = run_id
        self._cursor = cursor
        # ``redactor=None`` callers want explicit raw mode (tests etc.).
        # Construction is lazy via ``build_default_redactor`` so the env
        # var sweep only fires when defaulted -- a custom redactor stays
        # untouched.
        self._redactor: Redactor | None = (
            redactor if redactor is not None else build_default_redactor()
        )

    @property
    def cursor(self) -> int:
        """The most recent sequence the tailer has read past.

        Public for tests so they can assert cursor-incremental progress
        without re-reading the full history.
        """

        return self._cursor

    @property
    def run_id(self) -> str:
        """The run this tailer is bound to (immutable after construction)."""

        return self._run_id

    def fetch(self) -> list[TranscriptEntry]:
        """Return every transcript entry written since the last fetch.

        Drains the store via repeated :meth:`AuditStore.read_audit_since`
        calls until a page comes back empty, exactly the way
        :func:`flywheel_core.audit._drain` does. Each record passes through
        the redactor (when set) before classification so the session
        screen never sees the raw payload.

        An empty list means "no new activity" -- the caller polls again
        on the next tick. The cursor is unchanged in that case so a
        future write does not get skipped.
        """

        new_entries: list[TranscriptEntry] = []
        while True:
            page = self._store.read_audit_since(self._run_id, self._cursor)
            if not page:
                return new_entries
            for raw_record in page:
                record = (
                    raw_record
                    if self._redactor is None
                    else self._redactor.redact(raw_record)
                )
                new_entries.extend(classify(record))
                seq = record.sequence
                if seq is not None and seq > self._cursor:
                    self._cursor = seq


def is_terminal(status: Status) -> bool:
    """Whether ``status`` is a leaf in the lifecycle state graph.

    Defined alongside :data:`TERMINAL_STATUSES` so the session screen
    and ``--json`` snapshot share one predicate.
    """

    return status in TERMINAL_STATUSES


__all__ = [
    "EntryKind",
    "TERMINAL_STATUSES",
    "TranscriptEntry",
    "TranscriptTailer",
    "build_default_redactor",
    "classify",
    "is_terminal",
]

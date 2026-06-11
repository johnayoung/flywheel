"""Tests for the pure transcript domain model + cursor-driven tailer.

The transcript module is the data spine of the session screen; the
Pilot tests in :mod:`test_session_screen` lean on it for rendering.
Exercising the classifier here keeps the screen tests focused on user-
visible behaviour (follow / pause / banner) instead of payload shape.
"""

from __future__ import annotations

from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from flywheel_core.lifecycle import Lifecycle, Status
from flywheel_core.redaction import default_policy
from flywheel_core.store_protocols import (
    EventRecord,
    SdkMessageRecord,
    TelemetryRecord,
)
from flywheel_core.store_sqlite import SqliteStore
from flywheel_core.telemetry_file import FileTelemetrySink

from flywheel._session import (
    EntryKind,
    TERMINAL_STATUSES,
    TranscriptTailer,
    build_default_redactor,
    classify,
    is_terminal,
)


_NOW = datetime(2026, 6, 10, 12, 0, tzinfo=timezone.utc)


def _assistant_record(
    *,
    content: list[dict[str, Any]],
    sequence: int = 1,
    attempt: int = 1,
    iteration: int = 1,
) -> SdkMessageRecord:
    return SdkMessageRecord(
        run_id="run-x",
        attempt_number=attempt,
        iteration_number=iteration,
        message_type="AssistantMessage",
        payload={"content": content},
        ts=_NOW,
        sequence=sequence,
    )


def _user_record(
    *,
    content: list[dict[str, Any]],
    sequence: int = 1,
    attempt: int = 1,
    iteration: int = 1,
) -> SdkMessageRecord:
    return SdkMessageRecord(
        run_id="run-x",
        attempt_number=attempt,
        iteration_number=iteration,
        message_type="UserMessage",
        payload={"content": content},
        ts=_NOW,
        sequence=sequence,
    )


def _event_record(
    *,
    kind: str,
    payload: dict[str, Any],
    sequence: int = 1,
    attempt: int = 1,
) -> EventRecord:
    return EventRecord(
        run_id="run-x",
        ts=_NOW,
        kind=kind,
        payload=payload,
        attempt_number=attempt,
        sequence=sequence,
    )


def test_classify_assistant_text_block_renders_agent_entry() -> None:
    record = _assistant_record(
        content=[{"type": "text", "text": "I'll edit the README now."}],
        sequence=10,
    )
    entries = classify(record)
    assert len(entries) == 1
    entry = entries[0]
    assert entry.kind is EntryKind.AGENT_TEXT
    assert entry.header == "agent"
    # AGENT_TEXT prose is preserved verbatim (no _short flattening,
    # no length cap, no ellipsis suffix). A single-line block round-
    # trips unchanged.
    assert entry.body == "I'll edit the README now."
    assert "…" not in entry.body
    assert entry.sequence == 10
    assert entry.attempt_number == 1
    assert entry.iteration_number == 1


def test_classify_assistant_multi_paragraph_text_preserves_line_breaks() -> None:
    """FR-1: a multi-paragraph assistant text block reaches the screen
    with paragraphs intact and no truncation or ellipsis appended."""

    prose = (
        "First paragraph explains the plan.\n"
        "\n"
        "Second paragraph dives into the details.\n"
        "Third line continues the second paragraph."
    )
    record = _assistant_record(
        content=[{"type": "text", "text": prose}],
        sequence=11,
    )
    entries = classify(record)
    assert len(entries) == 1
    entry = entries[0]
    assert entry.kind is EntryKind.AGENT_TEXT
    assert entry.header == "agent"
    # Original line breaks are preserved verbatim; nothing flattens to
    # spaces, nothing is dropped.
    assert entry.body == prose
    assert "\n\n" in entry.body
    assert "…" not in entry.body


def test_classify_assistant_long_text_is_not_truncated() -> None:
    """FR-1 acceptance: prose far longer than the legacy _short cap
    reaches the screen with no ellipsis and the full character count."""

    long_text = "x" * 10_000
    record = _assistant_record(
        content=[{"type": "text", "text": long_text}],
        sequence=12,
    )
    entries = classify(record)
    assert len(entries) == 1
    entry = entries[0]
    assert entry.kind is EntryKind.AGENT_TEXT
    assert entry.body == long_text
    assert len(entry.body) == 10_000
    assert "…" not in entry.body


def test_classify_assistant_text_strips_carriage_returns_and_trailing_ws() -> None:
    """Edge case: carriage returns and per-line trailing whitespace
    must not produce phantom blank lines or trailing pad."""

    raw = "line one  \r\nline two\t\r\n\r\nline four   \r\n"
    record = _assistant_record(
        content=[{"type": "text", "text": raw}],
        sequence=13,
    )
    entries = classify(record)
    assert len(entries) == 1
    entry = entries[0]
    # CR characters disappear; trailing whitespace on each line is
    # stripped; the trailing newline at the very end is stripped.
    assert "\r" not in entry.body
    assert not entry.body.endswith("\n")
    assert not entry.body.endswith(" ")
    assert entry.body == "line one\nline two\n\nline four"


def test_classify_assistant_whitespace_only_text_block_is_skipped() -> None:
    """Edge case: an empty / pure-whitespace text block produces no
    entry rather than rendering a stray blank line."""

    record = _assistant_record(
        content=[{"type": "text", "text": "   \r\n\t  \n"}],
        sequence=14,
    )
    entries = classify(record)
    # The block collapses to nothing; the classifier still emits a
    # single fallback ``(empty)`` entry rather than zero entries so an
    # otherwise-empty assistant turn does not disappear silently.
    assert len(entries) == 1
    assert entries[0].body == "(empty)"


def test_classify_assistant_thinking_block_preserves_multi_line_prose() -> None:
    """Extended-thinking blocks share the AGENT_TEXT prose path: line
    breaks survive and no truncation is applied."""

    thinking = "Step 1: read the spec.\nStep 2: write the test.\nStep 3: ship."
    record = _assistant_record(
        content=[{"type": "thinking", "thinking": thinking}],
        sequence=15,
    )
    entries = classify(record)
    assert len(entries) == 1
    entry = entries[0]
    assert entry.kind is EntryKind.AGENT_TEXT
    assert entry.header == "agent(thinking)"
    assert entry.body == thinking
    assert "…" not in entry.body


def test_classify_assistant_tool_use_shows_primary_arg_for_mapped_tool() -> None:
    """FR-2: a mapped tool (Edit) renders its primary argument value
    verbatim -- ``file_path`` -- and drops the other input keys so the
    line stays scannable."""

    record = _assistant_record(
        content=[
            {
                "type": "tool_use",
                "name": "Edit",
                "input": {"file_path": "README.md", "old": "x"},
            }
        ]
    )
    entries = classify(record)
    assert len(entries) == 1
    entry = entries[0]
    assert entry.kind is EntryKind.TOOL_CALL
    assert entry.header == "tool(Edit)"
    # Just the file_path value, no ``key=`` prefix and no other keys.
    assert entry.body == "README.md"
    assert "=" not in entry.body


def test_classify_assistant_tool_use_bash_shows_command() -> None:
    """FR-2: Bash maps to ``command`` -- the command surfaces verbatim."""

    record = _assistant_record(
        content=[
            {
                "type": "tool_use",
                "name": "Bash",
                "input": {"command": "ls -la /tmp", "description": "list"},
            }
        ]
    )
    entry = classify(record)[0]
    assert entry.header == "tool(Bash)"
    assert entry.body == "ls -la /tmp"


def test_classify_assistant_tool_use_grep_shows_pattern() -> None:
    """FR-2: Grep maps to ``pattern``."""

    record = _assistant_record(
        content=[
            {
                "type": "tool_use",
                "name": "Grep",
                "input": {"pattern": "TODO", "glob": "*.py"},
            }
        ]
    )
    entry = classify(record)[0]
    assert entry.header == "tool(Grep)"
    assert entry.body == "TODO"


def test_classify_assistant_tool_use_unmapped_keeps_kv_form() -> None:
    """FR-2: an unmapped tool falls back to the first-two-keys ``k=v``
    summary so unfamiliar tools still surface enough context."""

    record = _assistant_record(
        content=[
            {
                "type": "tool_use",
                "name": "MysteryTool",
                "input": {"alpha": "one", "beta": "two", "gamma": "three"},
            }
        ]
    )
    entry = classify(record)[0]
    assert entry.header == "tool(MysteryTool)"
    # First two keys land in the body, third is dropped.
    assert "alpha=one" in entry.body
    assert "beta=two" in entry.body
    assert "gamma" not in entry.body


def test_classify_assistant_tool_use_mapped_missing_key_falls_back() -> None:
    """Edge case: a mapped tool whose primary key is absent falls back
    to the generic ``k=v`` summary rather than raising."""

    record = _assistant_record(
        content=[
            {
                "type": "tool_use",
                "name": "Edit",
                "input": {"old": "a", "new": "b"},
            }
        ]
    )
    entry = classify(record)[0]
    assert entry.header == "tool(Edit)"
    # No ``file_path`` -- fall through to k=v of the remaining keys.
    assert "old=a" in entry.body
    assert "new=b" in entry.body


def test_classify_assistant_tool_use_non_mapping_input_does_not_raise() -> None:
    """Edge case: a non-mapping input collapses through ``_short`` and
    never raises."""

    record = _assistant_record(
        content=[
            {
                "type": "tool_use",
                "name": "Edit",
                "input": "not a mapping",
            }
        ]
    )
    entry = classify(record)[0]
    assert entry.kind is EntryKind.TOOL_CALL
    assert entry.body == "not a mapping"


def test_classify_assistant_tool_use_long_command_is_not_truncated() -> None:
    """FR-2 edge case: a very long single-line command surfaces in full
    so the widget wraps it at width rather than the classifier
    truncating with an ellipsis."""

    long_command = "echo " + "x" * 1000
    record = _assistant_record(
        content=[
            {
                "type": "tool_use",
                "name": "Bash",
                "input": {"command": long_command},
            }
        ]
    )
    entry = classify(record)[0]
    assert entry.body == long_command
    assert "…" not in entry.body


# --- Typeless content blocks (the shape _serialize_sdk_message persists) ---
#
# ``flywheel_core.invoker._serialize_sdk_message`` serializes SDK content
# blocks as bare dataclass fields: a stored ThinkingBlock is
# ``{'thinking', 'signature'}`` and a stored ToolUseBlock is
# ``{'id', 'name', 'input'}`` -- there is no ``type`` discriminator key.
# These tests build payloads through the real serializer so the
# producer/consumer shape contract can never silently regress to the raw
# stringified-dict rendering again.


def _serialized_assistant_record(
    content_blocks: list[Any], *, sequence: int = 1
) -> SdkMessageRecord:
    """Build a record whose payload is the real serializer's output."""

    from claude_agent_sdk import AssistantMessage

    from flywheel_core.invoker import _serialize_sdk_message

    payload = _serialize_sdk_message(
        AssistantMessage(content=content_blocks, model="claude-fable-5")
    )
    return SdkMessageRecord(
        run_id="run-x",
        attempt_number=1,
        iteration_number=1,
        message_type="AssistantMessage",
        payload=payload,
        ts=_NOW,
        sequence=sequence,
    )


def test_classify_serialized_tool_use_block_renders_tool_call() -> None:
    """A ToolUseBlock persisted by _serialize_sdk_message (no ``type``
    key) renders as a collapsed TOOL_CALL line, not a raw dict dump."""

    from claude_agent_sdk import ToolUseBlock

    record = _serialized_assistant_record(
        [
            ToolUseBlock(
                id="toolu_01JUhk6nvaoBTtFzq9yFLdtP",
                name="Bash",
                input={"command": "uv run pytest 2>&1 | tail -5"},
            )
        ],
        sequence=40,
    )
    entries = classify(record)
    assert len(entries) == 1
    entry = entries[0]
    assert entry.kind is EntryKind.TOOL_CALL
    assert entry.header == "tool(Bash)"
    assert entry.body == "uv run pytest 2>&1 | tail -5"
    assert "toolu_" not in entry.body


def test_classify_serialized_thinking_block_renders_prose_without_signature() -> None:
    """A ThinkingBlock persisted by _serialize_sdk_message renders its
    prose under the agent(thinking) header; the signature blob never
    reaches the transcript."""

    from claude_agent_sdk import ThinkingBlock

    prose = "Plan: fix the classifier first.\nThen extend the tests."
    record = _serialized_assistant_record(
        [ThinkingBlock(thinking=prose, signature="CAISxwMKYggOGAIqQ")],
        sequence=41,
    )
    entries = classify(record)
    assert len(entries) == 1
    entry = entries[0]
    assert entry.kind is EntryKind.AGENT_TEXT
    assert entry.header == "agent(thinking)"
    assert entry.body == prose
    assert "CAISxwMKYggOGAIqQ" not in entry.body


def test_classify_serialized_empty_thinking_block_produces_no_entries() -> None:
    """The keep-alive shape ``{'thinking': '', 'signature': ...}``
    produces zero entries -- no raw dict, no ``(empty)`` fallback."""

    from claude_agent_sdk import ThinkingBlock

    record = _serialized_assistant_record(
        [
            ThinkingBlock(thinking="", signature="CAISxwMKYggOGAIqQ"),
            ThinkingBlock(thinking="", signature="CAISkgMKYwgOGAIqQ"),
        ],
        sequence=42,
    )
    assert classify(record) == []


def test_classify_serialized_text_block_renders_agent_text() -> None:
    """A TextBlock persisted by _serialize_sdk_message renders as plain
    agent prose."""

    from claude_agent_sdk import TextBlock

    record = _serialized_assistant_record(
        [TextBlock(text="All graders passed; committing now.")],
        sequence=43,
    )
    entries = classify(record)
    assert len(entries) == 1
    entry = entries[0]
    assert entry.kind is EntryKind.AGENT_TEXT
    assert entry.header == "agent"
    assert entry.body == "All graders passed; committing now."


def test_classify_serialized_mixed_blocks_suppress_only_empty_thinking() -> None:
    """A realistic serialized turn (empty thinking + tool_use) keeps
    the tool call and drops the signature-only block."""

    from claude_agent_sdk import ThinkingBlock, ToolUseBlock

    record = _serialized_assistant_record(
        [
            ThinkingBlock(thinking="", signature="CAISxwMKYggOGAIqQ"),
            ToolUseBlock(
                id="toolu_01Gb21G696Ymyvb87qMcKmVn",
                name="Bash",
                input={"command": "uv run pytest", "timeout": 600000},
            ),
        ],
        sequence=44,
    )
    entries = classify(record)
    assert len(entries) == 1
    assert entries[0].kind is EntryKind.TOOL_CALL
    assert entries[0].header == "tool(Bash)"
    assert entries[0].body == "uv run pytest"


def test_classify_typeless_unknown_block_keeps_stringified_fallback() -> None:
    """A block matching no known shape still renders its stringified
    form so unknown content never drops silently."""

    record = _assistant_record(
        content=[{"mystery": "value"}],
        sequence=45,
    )
    entries = classify(record)
    assert len(entries) == 1
    assert entries[0].kind is EntryKind.AGENT_TEXT
    assert "mystery" in entries[0].body


def test_classify_assistant_fans_out_text_and_tool_use() -> None:
    """Multiple content blocks produce multiple entries that share the
    parent record's sequence and disambiguate via ``sub_index``."""

    record = _assistant_record(
        content=[
            {"type": "text", "text": "Calling Edit."},
            {"type": "tool_use", "name": "Edit", "input": {"file": "a"}},
        ],
        sequence=7,
    )
    entries = classify(record)
    assert [e.kind for e in entries] == [
        EntryKind.AGENT_TEXT,
        EntryKind.TOOL_CALL,
    ]
    assert [e.sub_index for e in entries] == [0, 1]
    assert all(e.sequence == 7 for e in entries)


def test_classify_user_tool_result_success_renders_brief_ok_line() -> None:
    """FR-3: a successful tool result is a single brief line carrying
    ``ok`` plus a content hint (line count for multi-line output)."""

    body = "stdout line 1\nstdout line 2"
    record = _user_record(
        content=[
            {
                "tool_use_id": "toolu_1",
                "content": body,
                "is_error": False,
            }
        ]
    )
    entries = classify(record)
    assert len(entries) == 1
    entry = entries[0]
    assert entry.kind is EntryKind.TOOL_RESULT
    assert entry.header == "tool_result"
    # Two non-empty lines -> body summarises as ``ok (2 lines)``.
    assert entry.body == "ok (2 lines)"
    # Body is single-row -- no embedded newlines on success.
    assert "\n" not in entry.body


def test_classify_user_tool_result_single_line_success_includes_first_line() -> None:
    """FR-3: a single-line successful body inlines the line as the
    content hint, ``ok: <line>``."""

    record = _user_record(
        content=[
            {"tool_use_id": "t1", "content": "file written", "is_error": False}
        ]
    )
    entry = classify(record)[0]
    assert entry.kind is EntryKind.TOOL_RESULT
    assert entry.header == "tool_result"
    assert entry.body == "ok: file written"


def test_classify_user_tool_result_empty_success_collapses_to_ok() -> None:
    """Edge case: empty success content collapses to a bare ``ok``."""

    record = _user_record(
        content=[
            {"tool_use_id": "t1", "content": "", "is_error": False}
        ]
    )
    entry = classify(record)[0]
    assert entry.kind is EntryKind.TOOL_RESULT
    assert entry.header == "tool_result"
    assert entry.body == "ok"


def test_classify_user_tool_result_flags_error() -> None:
    """is_error swaps the header to the ``(error)`` variant; a short
    error body fits on one line and survives verbatim."""

    record = _user_record(
        content=[
            {"tool_use_id": "t1", "content": "boom", "is_error": True}
        ]
    )
    entry = classify(record)[0]
    assert entry.header == "tool_result(error)"
    assert entry.body == "boom"


def test_classify_user_tool_result_error_caps_at_ten_lines() -> None:
    """FR-3 acceptance: a 30-line error body keeps the first ten lines
    verbatim and appends ``... +20 more lines``."""

    body = "\n".join(f"err line {i}" for i in range(30))
    record = _user_record(
        content=[
            {"tool_use_id": "t1", "content": body, "is_error": True}
        ]
    )
    entry = classify(record)[0]
    assert entry.kind is EntryKind.TOOL_RESULT
    assert entry.header == "tool_result(error)"
    lines = entry.body.split("\n")
    # Ten detail lines + one overflow marker line.
    assert len(lines) == 11
    assert lines[:10] == [f"err line {i}" for i in range(10)]
    assert lines[10] == "... +20 more lines"


def test_classify_user_tool_result_error_under_cap_renders_verbatim() -> None:
    """An error body within the cap retains all lines without an
    overflow marker."""

    body = "first\nsecond\nthird"
    record = _user_record(
        content=[
            {"tool_use_id": "t1", "content": body, "is_error": True}
        ]
    )
    entry = classify(record)[0]
    assert entry.body == body
    assert "more lines" not in entry.body


def test_classify_user_text_block() -> None:
    record = _user_record(content=[{"type": "text", "text": "thanks"}])
    entries = classify(record)
    assert len(entries) == 1
    assert entries[0].kind is EntryKind.USER_TEXT
    assert entries[0].body == "thanks"


def test_classify_operator_say_event_attributes_text() -> None:
    record = _event_record(
        kind="harness.control_command_applied",
        payload={
            "command_id": 42,
            "kind": "say",
            "payload": {"text": "please add docstrings"},
        },
        sequence=20,
    )
    entries = classify(record)
    assert len(entries) == 1
    entry = entries[0]
    assert entry.kind is EntryKind.OPERATOR_SAY
    assert entry.header == "operator(say)"
    assert "please add docstrings" in entry.body


def test_classify_non_say_control_event_renders_generic_control_line() -> None:
    """A non-``say`` ``control_command_applied`` renders as a human
    phrase (no JSON digest) and keeps ``control_command_id`` populated so
    :meth:`SessionScreen._reconcile_pending` can flip the pending marker
    when the watcher catches up."""

    record = _event_record(
        kind="harness.control_command_applied",
        payload={"command_id": 1, "kind": "interrupt", "payload": {}},
    )
    entries = classify(record)
    entry = entries[0]
    assert entry.kind is EntryKind.LIFECYCLE
    assert entry.header == "control"
    assert "interrupt" in entry.body
    # Humanized phrase: no JSON braces or comma-separated digest noise.
    assert "{" not in entry.body
    assert "}" not in entry.body
    # The id is the seam the screen uses to reconcile pending commands;
    # it must survive the humanization.
    assert entry.control_command_id == 1


def test_classify_control_command_failed_surfaces_error_detail() -> None:
    """``control_command_failed`` keeps populating ``control_command_id``
    and ``control_command_error`` so the screen's pending-to-failed
    pilot tests continue to surface ``error_type: message`` inline."""

    record = _event_record(
        kind="harness.control_command_failed",
        payload={
            "command_id": 7,
            "kind": "say",
            "error_type": "SDKDisconnected",
            "message": "session was closed",
        },
    )
    entry = classify(record)[0]
    assert entry.kind is EntryKind.LIFECYCLE
    assert entry.header == "control"
    assert "SDKDisconnected" in entry.body
    assert "session was closed" in entry.body
    assert entry.control_command_id == 7
    assert entry.control_command_error == "SDKDisconnected: session was closed"


def test_classify_gate_event_carries_grader_name() -> None:
    record = _event_record(
        kind="harness.awaiting_approval",
        payload={"awaiting_ordinal": 2, "grader_name": "review-migration"},
    )
    entries = classify(record)
    assert entries[0].kind is EntryKind.GATE
    assert entries[0].header == "gate(awaiting)"
    assert "review-migration" in entries[0].body
    assert "ordinal=2" in entries[0].body


def test_classify_iteration_completed_renders_humanized_phrase() -> None:
    """FR-4 acceptance: a known kind renders a brace-free phrase
    containing the iteration number and a token count."""

    record = _event_record(
        kind="harness.iteration_completed",
        payload={"iteration": 3, "usage": {"total_tokens": 1200}},
    )
    entry = classify(record)[0]
    assert entry.kind is EntryKind.LIFECYCLE
    assert entry.header == "iteration"
    assert "iteration 3" in entry.body
    # Token count present in any compact form -- 1200 renders as ``1.2k``.
    assert "1.2k" in entry.body
    # The defining contrast with the legacy JSON-digest rendering: no
    # braces survive the humanization.
    assert "{" not in entry.body
    assert "}" not in entry.body


def test_classify_iteration_completed_includes_turns_and_failure() -> None:
    """``num_turns`` and a populated ``failure`` block surface in the
    phrase when present; otherwise they are omitted."""

    record = _event_record(
        kind="harness.iteration_completed",
        payload={
            "iteration": 5,
            "usage": {"total_tokens": 500},
            "num_turns": 4,
            "failure": {
                "error_type": "EnvelopeError",
                "message": "missing envelope",
            },
        },
    )
    entry = classify(record)[0]
    assert entry.header == "iteration"
    assert "iteration 5" in entry.body
    assert "500 tokens" in entry.body
    assert "4 turns" in entry.body
    assert "EnvelopeError" in entry.body
    assert "missing envelope" in entry.body


def test_classify_attempt_started_renders_humanized_phrase() -> None:
    record = _event_record(
        kind="harness.attempt_started",
        payload={"number": 2, "agent_context": {"model": "test"}},
    )
    entry = classify(record)[0]
    assert entry.kind is EntryKind.LIFECYCLE
    assert entry.header == "attempt"
    assert entry.body == "attempt 2 started"


def test_classify_attempt_finalized_includes_outcome_and_error() -> None:
    record = _event_record(
        kind="harness.attempt_finalized",
        payload={
            "number": 1,
            "outcome": "agent_error",
            "error": "protocol failure: missing envelope",
        },
    )
    entry = classify(record)[0]
    assert entry.kind is EntryKind.LIFECYCLE
    assert entry.header == "attempt"
    assert entry.body == (
        "attempt 1 finalized (agent_error): "
        "protocol failure: missing envelope"
    )


def test_classify_retry_scheduled_shows_counter() -> None:
    record = _event_record(
        kind="harness.retry_scheduled",
        payload={"retries_used": 1, "max_retries": 3},
    )
    entry = classify(record)[0]
    assert entry.kind is EntryKind.LIFECYCLE
    assert entry.header == "retry"
    assert entry.body == "retry 1/3 scheduled"


def test_classify_known_kind_with_missing_field_falls_back_to_digest() -> None:
    """FR-4 Error Handling: a known kind whose payload is missing a
    required field falls back to the legacy ``kind + JSON digest`` pair
    rather than raising or producing a half-formed phrase."""

    record = _event_record(
        kind="harness.iteration_completed",
        # ``iteration`` is required; without it the formatter raises and
        # the classifier falls back to the digest pair.
        payload={"usage": {"total_tokens": 100}},
    )
    entry = classify(record)[0]
    assert entry.kind is EntryKind.LIFECYCLE
    # Fallback restores the raw kind as the header.
    assert entry.header == "harness.iteration_completed"
    # The body is the JSON-digest of the payload (braces present).
    assert "{" in entry.body
    assert "}" in entry.body
    assert "total_tokens" in entry.body


def test_classify_unrecognized_kind_still_shows_json_digest() -> None:
    """FR-4 acceptance: an unrecognized event kind keeps the legacy
    ``kind + JSON digest`` rendering -- nothing is suppressed, nothing
    is humanized in a way that would lose payload context."""

    record = _event_record(
        kind="harness.audit_finalized",
        payload={"reason": "shutdown", "attempts": 4},
    )
    entry = classify(record)[0]
    assert entry.kind is EntryKind.LIFECYCLE
    assert entry.header == "harness.audit_finalized"
    # Digest body retains the JSON braces -- the contrast that proves
    # humanization did not kick in for an unrecognized kind.
    assert "{" in entry.body
    assert "}" in entry.body
    assert "shutdown" in entry.body


def test_classify_known_kind_with_non_json_serializable_payload_uses_short() -> None:
    """Edge case: a payload value that cannot serialize through the
    JSON digest falls through to :func:`_short` exactly as the
    pre-humanization classifier did."""

    class _Unserializable:
        def __repr__(self) -> str:
            return "<weird>"

    # Use a kind with no formatter so the digest path is exercised
    # directly.
    record = _event_record(
        kind="harness.audit_finalized",
        payload={"weird": _Unserializable()},
    )
    entry = classify(record)[0]
    assert entry.kind is EntryKind.LIFECYCLE
    assert entry.header == "harness.audit_finalized"
    # ``_payload_digest`` catches the JSON failure and falls through to
    # ``_short(dict(payload))``; that surface contains the weird repr.
    assert "weird" in entry.body


def test_classify_result_message_renders_turn_end_marker() -> None:
    record = SdkMessageRecord(
        run_id="run-x",
        attempt_number=1,
        iteration_number=1,
        message_type="ResultMessage",
        payload={"subtype": "end_turn", "num_turns": 4, "duration_ms": 1234},
        ts=_NOW,
        sequence=99,
    )
    entries = classify(record)
    assert entries[0].kind is EntryKind.RESULT
    assert "subtype=end_turn" in entries[0].body
    assert "turns=4" in entries[0].body


def test_is_terminal_matches_lifecycle_leaf_states() -> None:
    assert is_terminal(Status.DONE)
    assert is_terminal(Status.FAILED)
    assert is_terminal(Status.FAILED_VALIDATION)
    assert is_terminal(Status.INTERNAL_ERROR)
    assert is_terminal(Status.INTERRUPTED)
    assert not is_terminal(Status.RUNNING)
    assert not is_terminal(Status.AWAITING_APPROVAL)
    # Mirror set so consumers can guard against future additions
    # without hard-coding the leaf list.
    assert Status.DONE in TERMINAL_STATUSES


def _append_assistant_line(
    sink: FileTelemetrySink,
    run_id: str,
    payload: dict[str, Any],
    *,
    attempt_number: int = 1,
    iteration_number: int = 1,
) -> None:
    """Write one AssistantMessage line the way the harness's sink does."""
    sink.append_telemetry(
        TelemetryRecord(
            run_id=run_id,
            ts=_NOW,
            kind="AssistantMessage",
            payload=payload,
            attempt_number=attempt_number,
            iteration_number=iteration_number,
        )
    )


def _seed_running(store: SqliteStore, task_id: str) -> Lifecycle:
    lc = Lifecycle(task_id=task_id, run_id=f"run-{task_id}")
    lc.transition_to(Status.READY, now=_NOW)
    lc.transition_to(Status.RUNNING, now=_NOW)
    store.create_lifecycle(lc)
    return lc


def test_tailer_returns_only_new_records_per_call(tmp_path: Path) -> None:
    """FR-4 acceptance: tailing is cursor-incremental and never re-reads
    history per tick."""

    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    sink = FileTelemetrySink(tmp_path / "logs")
    run_file = tmp_path / "logs" / "runs"
    try:
        lc = _seed_running(store, "alpha")
        _append_assistant_line(
            sink,
            lc.run_id,
            {"content": [{"type": "text", "text": "first"}]},
        )
        # Pass redactor=None so this test asserts pure cursor behaviour
        # without depending on the default-policy redactor's identity
        # for plain text.
        tailer = TranscriptTailer(
            run_file / f"{lc.run_id}.jsonl", lc.run_id, redactor=None
        )
        first = tailer.fetch()
        assert len(first) == 1
        assert first[0].body == "first"
        cursor_after_first = tailer.cursor
        assert cursor_after_first > 0
        # No new lines -> empty list, cursor unchanged.
        assert tailer.fetch() == []
        assert tailer.cursor == cursor_after_first
        # New line after the cursor -> returned exactly once.
        _append_assistant_line(
            sink,
            lc.run_id,
            {"content": [{"type": "text", "text": "second"}]},
        )
        second = tailer.fetch()
        assert len(second) == 1
        assert second[0].body == "second"
        assert tailer.cursor > cursor_after_first
    finally:
        store.close()
        sink.close()


def test_tailer_default_redactor_suppresses_anthropic_keys(
    tmp_path: Path,
) -> None:
    """Default-policy redactor catches an Anthropic key embedded in an
    assistant text block."""

    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    sink = FileTelemetrySink(tmp_path / "logs")
    try:
        lc = _seed_running(store, "beta")
        leaked = "sk-ant-api03-" + "A" * 95
        _append_assistant_line(
            sink,
            lc.run_id,
            {"content": [{"type": "text", "text": f"my key is {leaked}"}]},
        )
        tailer = TranscriptTailer(
            tmp_path / "logs" / "runs" / f"{lc.run_id}.jsonl", lc.run_id
        )
        entries = tailer.fetch()
        assert len(entries) == 1
        assert leaked not in entries[0].body
        assert "[REDACTED" in entries[0].body
    finally:
        store.close()
        sink.close()


def test_tailer_custom_redactor_is_honoured(tmp_path: Path) -> None:
    """Caller-supplied redactor overrides the default policy."""

    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    sink = FileTelemetrySink(tmp_path / "logs")
    try:
        lc = _seed_running(store, "gamma")
        _append_assistant_line(
            sink,
            lc.run_id,
            {"content": [{"type": "text", "text": "hello world"}]},
        )
        # default_policy alone (no env redactor) -- the text "hello world"
        # has no secret patterns so the body comes through verbatim.
        tailer = TranscriptTailer(
            tmp_path / "logs" / "runs" / f"{lc.run_id}.jsonl",
            lc.run_id,
            redactor=default_policy(),
        )
        entries = tailer.fetch()
        assert entries[0].body == "hello world"
    finally:
        store.close()
        sink.close()


def test_tailer_missing_file_reads_as_empty_and_survives_deletion(
    tmp_path: Path,
) -> None:
    """FR-8 edge cases: a run with no file yet fetches as empty, and
    deleting the file mid-tail surfaces end-of-stream gracefully
    instead of crashing the TUI."""

    run_file = tmp_path / "logs" / "runs" / "run-gone.jsonl"
    tailer = TranscriptTailer(run_file, "run-gone", redactor=None)
    # No file yet: empty fetch, cursor untouched.
    assert tailer.fetch() == []
    assert tailer.cursor == 0

    sink = FileTelemetrySink(tmp_path / "logs")
    _append_assistant_line(
        sink,
        "run-gone",
        {"content": [{"type": "text", "text": "hello"}]},
    )
    sink.close()
    entries = tailer.fetch()
    assert [e.body for e in entries] == ["hello"]

    # Operator deletes the run file mid-tail: subsequent fetches are
    # empty, never an exception.
    run_file.unlink()
    assert tailer.fetch() == []


def test_build_default_redactor_is_safe_when_no_env_secrets() -> None:
    """The default builder must not crash in an environment where the
    seeded env vars are absent."""

    redactor = build_default_redactor()
    # A record with no secrets round-trips unmodified.
    record = _assistant_record(
        content=[{"type": "text", "text": "hello"}]
    )
    redacted = redactor.redact(record)
    assert isinstance(redacted, SdkMessageRecord)
    assert redacted.payload["content"][0]["text"] == "hello"

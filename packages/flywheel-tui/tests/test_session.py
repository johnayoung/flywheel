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

from flywheel.lifecycle import Lifecycle, Status
from flywheel.redaction import default_policy
from flywheel.store_protocols import EventRecord, SdkMessageRecord
from flywheel.store_sqlite import SqliteStore

from flywheel_tui._session import (
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
    assert entry.body == "I'll edit the README now."
    assert entry.sequence == 10
    assert entry.attempt_number == 1
    assert entry.iteration_number == 1


def test_classify_assistant_tool_use_collapses_args() -> None:
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
    # First two keys make it into the body summary; same convention
    # the dashboard uses for ``last_detail``.
    assert "file_path=README.md" in entry.body
    assert "old=x" in entry.body


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


def test_classify_user_tool_result_collapses_size_and_body() -> None:
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
    assert f"{len(body)}B" in entry.body
    # Newlines collapse so the line stays single-row.
    assert "\n" not in entry.body


def test_classify_user_tool_result_flags_error() -> None:
    record = _user_record(
        content=[
            {"tool_use_id": "t1", "content": "boom", "is_error": True}
        ]
    )
    entries = classify(record)
    assert entries[0].header == "tool_result(error)"


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
    record = _event_record(
        kind="harness.control_command_applied",
        payload={"command_id": 1, "kind": "interrupt", "payload": {}},
    )
    entries = classify(record)
    assert entries[0].kind is EntryKind.LIFECYCLE
    assert entries[0].header == "control"
    assert "interrupt" in entries[0].body


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


def test_classify_lifecycle_event_uses_kind_as_header() -> None:
    record = _event_record(
        kind="harness.iteration_completed",
        payload={"iteration": 3, "usage": {"total_tokens": 100}},
    )
    entries = classify(record)
    assert entries[0].kind is EntryKind.LIFECYCLE
    assert entries[0].header == "harness.iteration_completed"
    # Payload digest is deterministic + sorted.
    assert "iteration" in entries[0].body


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
    try:
        lc = _seed_running(store, "alpha")
        store.save_sdk_messages(
            run_id=lc.run_id,
            attempt_number=1,
            iteration_number=1,
            messages=[
                {
                    "message_type": "AssistantMessage",
                    "content": [{"type": "text", "text": "first"}],
                }
            ],
        )
        # Pass redactor=None so this test asserts pure cursor behaviour
        # without depending on the default-policy redactor's identity
        # for plain text.
        tailer = TranscriptTailer(store, lc.run_id, redactor=None)
        first = tailer.fetch()
        assert len(first) == 1
        assert first[0].body == "first"
        cursor_after_first = tailer.cursor
        assert cursor_after_first > 0
        # No new records -> empty list, cursor unchanged.
        assert tailer.fetch() == []
        assert tailer.cursor == cursor_after_first
        # New record after the cursor -> returned exactly once.
        store.save_sdk_messages(
            run_id=lc.run_id,
            attempt_number=1,
            iteration_number=1,
            messages=[
                {
                    "message_type": "AssistantMessage",
                    "content": [{"type": "text", "text": "second"}],
                }
            ],
        )
        second = tailer.fetch()
        assert len(second) == 1
        assert second[0].body == "second"
        assert tailer.cursor > cursor_after_first
    finally:
        store.close()


def test_tailer_default_redactor_suppresses_anthropic_keys(
    tmp_path: Path,
) -> None:
    """Default-policy redactor catches an Anthropic key embedded in an
    assistant text block."""

    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        lc = _seed_running(store, "beta")
        leaked = "sk-ant-api03-" + "A" * 95
        store.save_sdk_messages(
            run_id=lc.run_id,
            attempt_number=1,
            iteration_number=1,
            messages=[
                {
                    "message_type": "AssistantMessage",
                    "content": [
                        {"type": "text", "text": f"my key is {leaked}"}
                    ],
                }
            ],
        )
        tailer = TranscriptTailer(store, lc.run_id)
        entries = tailer.fetch()
        assert len(entries) == 1
        assert leaked not in entries[0].body
        assert "[REDACTED" in entries[0].body
    finally:
        store.close()


def test_tailer_custom_redactor_is_honoured(tmp_path: Path) -> None:
    """Caller-supplied redactor overrides the default policy."""

    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        lc = _seed_running(store, "gamma")
        store.save_sdk_messages(
            run_id=lc.run_id,
            attempt_number=1,
            iteration_number=1,
            messages=[
                {
                    "message_type": "AssistantMessage",
                    "content": [{"type": "text", "text": "hello world"}],
                }
            ],
        )
        # default_policy alone (no env redactor) -- the text "hello world"
        # has no secret patterns so the body comes through verbatim.
        tailer = TranscriptTailer(store, lc.run_id, redactor=default_policy())
        entries = tailer.fetch()
        assert entries[0].body == "hello world"
    finally:
        store.close()


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

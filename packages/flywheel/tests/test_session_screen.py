"""Pilot-driven tests for :class:`flywheel._session_screen.SessionScreen`.

The screen receives ``fetch`` / ``status`` callables so these tests
drive deterministic frames without touching SQLite (the cursor and
classification paths are covered by ``test_session``). Coverage maps
to FR-3 (Escape returns to dashboard with selection preserved), FR-4
(rendering of each message class, follow-pause-resume), FR-5/FR-6
(steering verbs enqueue exactly one command, pending-to-applied /
pending-to-failed transitions, verbs disabled on inactive runs), FR-7
(terminal banner appears + steering disabled signal), and the Edge
Cases table (empty transcript renders cleanly; AWAITING_APPROVAL
surfaces gate).
"""

from __future__ import annotations

import asyncio
from collections.abc import Callable, Coroutine, Mapping
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from textual.containers import VerticalScroll
from textual.widgets import DataTable, Input, Static

from flywheel_core.lifecycle import Lifecycle, Status
from flywheel_core.store_protocols import TelemetryRecord
from flywheel_core.store_sqlite import SqliteStore
from flywheel_core.telemetry_file import FileTelemetrySink

from flywheel._dashboard import DashboardApp
from flywheel._session import EntryKind, TranscriptEntry, TranscriptTailer
from flywheel._session_screen import (
    SessionScreen,
    SessionStatus,
    _block_group,
    _is_error_entry,
    _needs_blank_separator,
    _resolve_styles,
    render_entry_text,
)
from flywheel._snapshot import DashboardSnapshot, RowSnapshot, SummaryData


def _run(coro: Callable[[], Coroutine[Any, Any, None]]) -> None:
    """Drive an async test body without pulling pytest-asyncio in."""

    asyncio.run(coro())


_NOW = datetime(2026, 6, 10, 12, 0, tzinfo=timezone.utc)
# Wall-clock prefix render_entry_text emits for every block-start line at
# ``_NOW``; pulled into a constant so the layout assertions read as
# ``f"{_TS}  agent  ack"`` rather than burying the literal in every test.
_TS = "12:00:00"


def _entry(
    *,
    kind: EntryKind,
    header: str,
    body: str,
    sequence: int = 1,
    sub_index: int = 0,
) -> TranscriptEntry:
    return TranscriptEntry(
        sequence=sequence,
        sub_index=sub_index,
        ts=_NOW,
        kind=kind,
        header=header,
        body=body,
        attempt_number=1,
        iteration_number=1,
    )


def _running_status() -> SessionStatus:
    return SessionStatus(status=Status.RUNNING, awaiting_instruction=None)


def _terminal_status(status: Status = Status.DONE) -> SessionStatus:
    return SessionStatus(status=status, awaiting_instruction=None)


def _awaiting_status(instruction: str) -> SessionStatus:
    return SessionStatus(
        status=Status.AWAITING_APPROVAL, awaiting_instruction=instruction
    )


def _static_plain(widget: Static) -> str:
    """Return the plain-text content of a :class:`Static`'s renderable.

    The session screen mounts a :class:`rich.text.Text` into each Static
    so layout assertions (leading blank, timestamp gutter) can introspect
    the rendered shape without reaching into Textual internals. Textual's
    :class:`Static` exposes the original construction-time renderable via
    the ``content`` property; on a Text that surfaces a ``plain`` attr,
    everything else falls back to ``str()`` so the helper is safe to call
    on the banner / notice widgets too.
    """

    content = widget.content
    plain = getattr(content, "plain", None)
    if isinstance(plain, str):
        return plain
    return str(content)


class _ScriptedFetch:
    """Returns a queued list of transcript entries per call.

    The default mode hands out queued entries on the first call and
    nothing thereafter so a Pilot can append entries between
    ``refresh_now`` ticks deterministically.
    """

    def __init__(self) -> None:
        self.queue: list[list[TranscriptEntry]] = []
        self.calls: int = 0

    def __call__(self) -> list[TranscriptEntry]:
        self.calls += 1
        if not self.queue:
            return []
        return self.queue.pop(0)


class _ScriptedStatus:
    """Returns the head of the queue, repeating the last value."""

    def __init__(self, initial: SessionStatus) -> None:
        self._queue: list[SessionStatus] = [initial]

    def push(self, status: SessionStatus) -> None:
        self._queue.append(status)

    def __call__(self) -> SessionStatus:
        # Advance to the next pushed value on each call, holding at the
        # last one forever (so the screen never observes a backslide).
        if len(self._queue) > 1:
            self._queue.pop(0)
        return self._queue[0]


def test_session_screen_renders_all_message_classes() -> None:
    """FR-4 + FR-5 + FR-6 acceptance: pilot test asserts rendering of each
    message class (agent text, tool call, tool result, operator say,
    lifecycle, gate event) and the inter-block layout. Multi-line
    AGENT_TEXT prose is rendered verbatim with its line breaks preserved
    (FR-1); every block-start line carries a dim ``HH:MM:SS`` prefix
    (FR-5); blank-line separators only appear between consecutive blocks
    whose group changes -- agent -> tool, tool -> operator, operator ->
    lifecycle (FR-6) -- with no blank between the tool call and its own
    result and no blank between two lifecycle-group entries (LIFECYCLE
    followed by GATE).
    """

    async def body() -> None:
        fetch = _ScriptedFetch()
        multi_line_prose = (
            "planning the edit\n"
            "\n"
            "step two: write the test"
        )
        fetch.queue.append(
            [
                _entry(
                    kind=EntryKind.AGENT_TEXT,
                    header="agent",
                    body=multi_line_prose,
                    sequence=1,
                ),
                _entry(
                    kind=EntryKind.TOOL_CALL,
                    header="tool(Edit)",
                    body="README.md",
                    sequence=2,
                ),
                _entry(
                    kind=EntryKind.TOOL_RESULT,
                    header="tool_result",
                    body="ok",
                    sequence=3,
                ),
                _entry(
                    kind=EntryKind.OPERATOR_SAY,
                    header="operator(say)",
                    body="please add docstrings",
                    sequence=4,
                ),
                _entry(
                    kind=EntryKind.LIFECYCLE,
                    header="harness.iteration_completed",
                    body="iteration=1",
                    sequence=5,
                ),
                _entry(
                    kind=EntryKind.GATE,
                    header="gate(awaiting)",
                    body="grader=review-migration",
                    sequence=6,
                ),
            ]
        )
        screen = SessionScreen(
            run_id="run-x",
            task_id="task-alpha",
            fetch=fetch,
            status=_running_status,
            poll_interval_seconds=1000.0,
        )
        app = DashboardApp(
            poll=lambda: DashboardSnapshot(
                summary=SummaryData(
                    active_workers=0,
                    task_counts={},
                    tokens_total=0,
                    cost_usd_total=0.0,
                    runtime_seconds=0,
                ),
                rows=(),
            ),
            poll_interval_seconds=1000.0,
        )
        async with app.run_test() as pilot:
            await app.push_screen(screen)
            await pilot.pause()
            # All six entry classes landed on the screen.
            assert len(screen.entries) == 6
            kinds = [e.kind for e in screen.entries]
            assert kinds == [
                EntryKind.AGENT_TEXT,
                EntryKind.TOOL_CALL,
                EntryKind.TOOL_RESULT,
                EntryKind.OPERATOR_SAY,
                EntryKind.LIFECYCLE,
                EntryKind.GATE,
            ]
            # Transcript widget mounted exactly one Static per entry.
            transcript = screen.query_one(
                "#session_transcript", VerticalScroll
            )
            statics = list(transcript.query(Static))
            assert len(statics) == 6
            # The AGENT_TEXT body kept its paragraph break verbatim.
            assert screen.entries[0].body == multi_line_prose
            assert "\n\n" in screen.entries[0].body
            # Leading-blank pattern (FR-6): no blank before the first
            # entry; blanks before each group change (tool, operator,
            # lifecycle); no blank between tool call and its result; no
            # blank between two lifecycle-group entries (LIFECYCLE then
            # GATE).
            plains = [_static_plain(s) for s in statics]
            expected_leading_blank = [
                False,  # agent (first entry on screen)
                True,   # agent -> tool group change
                False,  # tool call -> tool result (special rule)
                True,   # tool result -> operator group change
                True,   # operator -> lifecycle group change
                False,  # lifecycle -> gate within lifecycle group
            ]
            for index, (plain, blank) in enumerate(
                zip(plains, expected_leading_blank)
            ):
                if blank:
                    assert plain.startswith("\n"), (
                        f"entry #{index} should have a leading blank line"
                    )
                else:
                    assert not plain.startswith("\n"), (
                        f"entry #{index} should NOT have a leading blank line"
                    )
            # FR-5: each rendered block carries exactly one HH:MM:SS
            # prefix; multi-line AGENT_TEXT does NOT repeat the
            # timestamp on continuation lines.
            for index, plain in enumerate(plains):
                assert plain.count(_TS) == 1, (
                    f"entry #{index} should carry exactly one timestamp"
                )

    _run(body)


def test_render_entry_text_keeps_agent_text_inline_when_single_line() -> None:
    """A single-line AGENT_TEXT body still renders as ``HH:MM:SS  agent  body``.

    Preserves the dense single-line look for short prose so the
    transcript stays scannable on common short replies; the dim
    ``HH:MM:SS`` block-start prefix sits ahead of the header per FR-5.
    """

    entry = TranscriptEntry(
        sequence=1,
        sub_index=0,
        ts=_NOW,
        kind=EntryKind.AGENT_TEXT,
        header="agent",
        body="ack",
        attempt_number=1,
        iteration_number=1,
    )
    rendered = render_entry_text(entry)
    plain = rendered.plain
    assert plain == f"{_TS}  agent  ack"
    assert "\n" not in plain


def test_render_entry_text_tool_result_success_is_indented_one_line() -> None:
    """FR-3 + FR-5 rendering: a successful TOOL_RESULT renders as a single
    indented row -- two spaces in front of the header so the line visually
    nests under the preceding tool call, and the dim ``HH:MM:SS`` block-start
    prefix sits ahead of that indent."""

    entry = TranscriptEntry(
        sequence=1,
        sub_index=0,
        ts=_NOW,
        kind=EntryKind.TOOL_RESULT,
        header="tool_result",
        body="ok",
        attempt_number=1,
        iteration_number=1,
    )
    plain = render_entry_text(entry).plain
    assert plain == f"{_TS}    tool_result  ok"


def test_render_entry_text_tool_result_error_breaks_lines_below_header() -> None:
    """FR-3 + FR-5 + FR-7 rendering: a multi-line error body puts the header
    on its own indented row (after the dim timestamp prefix) and each detail
    line on its own row indented four spaces deep, with no dim style on the
    error header or body."""

    body = "first error\nsecond error\n... +20 more lines"
    entry = TranscriptEntry(
        sequence=1,
        sub_index=0,
        ts=_NOW,
        kind=EntryKind.TOOL_RESULT,
        header="tool_result(error)",
        body=body,
        attempt_number=1,
        iteration_number=1,
    )
    rendered = render_entry_text(entry)
    plain = rendered.plain
    expected = (
        f"{_TS}    tool_result(error)\n"
        "    first error\n"
        "    second error\n"
        "    ... +20 more lines"
    )
    assert plain == expected
    # Body is styled red (not dim) so a failure pops visually. The only
    # dim span in the rendered Text is the leading ``HH:MM:SS`` timestamp
    # (positions 0-8); confirm no dim style sneaks onto the header or
    # body content beyond that range.
    non_timestamp_dim = [
        span
        for span in rendered.spans
        if "dim" in str(span.style).lower() and span.start >= len(_TS)
    ]
    assert non_timestamp_dim == []


def test_render_entry_text_tool_result_error_single_line_inline() -> None:
    """A short single-line error sits on the same row as the header,
    after the dim ``HH:MM:SS`` block-start prefix (FR-5)."""

    entry = TranscriptEntry(
        sequence=1,
        sub_index=0,
        ts=_NOW,
        kind=EntryKind.TOOL_RESULT,
        header="tool_result(error)",
        body="boom",
        attempt_number=1,
        iteration_number=1,
    )
    plain = render_entry_text(entry).plain
    assert plain == f"{_TS}    tool_result(error)  boom"


def test_render_entry_text_breaks_multi_line_agent_text_below_header() -> None:
    """FR-1 + FR-5 rendering: multi-paragraph AGENT_TEXT puts the timestamped
    header on its own line and the prose underneath with original breaks
    intact; continuation lines carry no timestamp prefix.
    """

    prose = "first paragraph\n\nsecond paragraph"
    entry = TranscriptEntry(
        sequence=1,
        sub_index=0,
        ts=_NOW,
        kind=EntryKind.AGENT_TEXT,
        header="agent",
        body=prose,
        attempt_number=1,
        iteration_number=1,
    )
    rendered = render_entry_text(entry)
    plain = rendered.plain
    # Header (with the leading timestamp gutter) sits on its own line;
    # prose follows verbatim with no per-line timestamp repetition.
    assert plain == f"{_TS}  agent\n{prose}"
    # The blank line between paragraphs survives the renderer.
    assert "\n\n" in plain
    # The timestamp appears exactly once -- only on the first line of the
    # block (FR-5: "first line of each block ... continuation lines do not").
    assert plain.count(_TS) == 1


def test_render_entry_text_timestamp_prefix_is_dim_only_on_first_line() -> None:
    """FR-5 acceptance: the dim ``HH:MM:SS`` prefix sits on the first
    line of each block; continuation lines (multi-line agent prose,
    multi-line error body) do not repeat the timestamp."""

    prose = "line one\nline two"
    entry = TranscriptEntry(
        sequence=1,
        sub_index=0,
        ts=_NOW,
        kind=EntryKind.AGENT_TEXT,
        header="agent",
        body=prose,
        attempt_number=1,
        iteration_number=1,
    )
    rendered = render_entry_text(entry)
    plain = rendered.plain
    # The block-start timestamp appears exactly once -- on the header
    # line. The continuation prose line ("line two") carries no prefix.
    assert plain.count(_TS) == 1
    assert plain.startswith(f"{_TS}  agent\n")
    # The timestamp span itself is styled ``dim`` and sits at the very
    # start of the rendered Text (offset 0 -> 8).
    dim_spans = [
        span for span in rendered.spans if "dim" in str(span.style).lower()
    ]
    assert any(span.start == 0 and span.end == len(_TS) for span in dim_spans)


def test_render_entry_text_leading_blank_inserts_one_newline_above_block() -> None:
    """FR-6 plumbing: ``leading_blank=True`` prefixes the rendered Text
    with exactly one newline so the widget renders a blank row above
    the new block; the block-start timestamp follows immediately on
    the line below the blank."""

    entry = TranscriptEntry(
        sequence=1,
        sub_index=0,
        ts=_NOW,
        kind=EntryKind.TOOL_CALL,
        header="tool(Edit)",
        body="README.md",
        attempt_number=1,
        iteration_number=1,
    )
    plain = render_entry_text(entry, leading_blank=True).plain
    assert plain == f"\n{_TS}  tool(Edit)  README.md"
    # Exactly one separator newline -- no double blank.
    assert plain.count("\n") == 1


def test_session_screen_agent_then_tool_call_has_two_timestamps_one_blank() -> None:
    """FR-5 + FR-6 acceptance: an agent block followed by a tool call
    shows exactly two timestamps and one separating blank line, and a
    tool call followed by its result is not separated."""

    async def body() -> None:
        fetch = _ScriptedFetch()
        fetch.queue.append(
            [
                _entry(
                    kind=EntryKind.AGENT_TEXT,
                    header="agent",
                    body="planning the edit",
                    sequence=1,
                ),
                _entry(
                    kind=EntryKind.TOOL_CALL,
                    header="tool(Edit)",
                    body="README.md",
                    sequence=2,
                ),
                _entry(
                    kind=EntryKind.TOOL_RESULT,
                    header="tool_result",
                    body="ok",
                    sequence=3,
                ),
            ]
        )
        screen = SessionScreen(
            run_id="run-x",
            task_id="task-alpha",
            fetch=fetch,
            status=_running_status,
            poll_interval_seconds=1000.0,
        )
        app = DashboardApp(
            poll=lambda: DashboardSnapshot(
                summary=SummaryData(
                    active_workers=0,
                    task_counts={},
                    tokens_total=0,
                    cost_usd_total=0.0,
                    runtime_seconds=0,
                ),
                rows=(),
            ),
            poll_interval_seconds=1000.0,
        )
        async with app.run_test() as pilot:
            await app.push_screen(screen)
            await pilot.pause()
            transcript = screen.query_one(
                "#session_transcript", VerticalScroll
            )
            statics = list(transcript.query(Static))
            assert len(statics) == 3
            plains = [_static_plain(s) for s in statics]
            agent_plain, tool_plain, result_plain = plains
            # FR-5: each block-start line carries exactly one timestamp.
            assert agent_plain.count(_TS) == 1
            assert tool_plain.count(_TS) == 1
            assert result_plain.count(_TS) == 1
            # FR-6: agent -> tool_call crosses the group boundary, so the
            # tool widget renders one leading blank line. Together with
            # the agent line that is exactly two timestamps and one
            # separating blank line for the agent+tool pair.
            assert not agent_plain.startswith("\n")
            assert tool_plain.startswith("\n")
            assert tool_plain.count("\n") == 1
            # FR-6: tool call -> tool result stays glued together (no
            # leading blank line on the result widget).
            assert not result_plain.startswith("\n")

    _run(body)


def test_session_screen_blank_separator_persists_across_fetches() -> None:
    """FR-6 edge case: a new block arriving in a later fetch still gets
    its blank-line separator relative to the last block of the previous
    fetch -- the grouping rule compares against the last entry in
    ``screen.entries``, not just the entries in the current page.
    """

    async def body() -> None:
        fetch = _ScriptedFetch()
        # First fetch: only an agent text block.
        fetch.queue.append(
            [
                _entry(
                    kind=EntryKind.AGENT_TEXT,
                    header="agent",
                    body="hello",
                    sequence=1,
                )
            ]
        )
        screen = SessionScreen(
            run_id="run-x",
            task_id="task-alpha",
            fetch=fetch,
            status=_running_status,
            poll_interval_seconds=1000.0,
        )
        app = DashboardApp(
            poll=lambda: DashboardSnapshot(
                summary=SummaryData(
                    active_workers=0,
                    task_counts={},
                    tokens_total=0,
                    cost_usd_total=0.0,
                    runtime_seconds=0,
                ),
                rows=(),
            ),
            poll_interval_seconds=1000.0,
        )
        async with app.run_test() as pilot:
            await app.push_screen(screen)
            await pilot.pause()
            assert len(screen.entries) == 1
            # Second fetch: a tool call arrives a tick later. The agent
            # entry has already been consumed, so the screen must
            # remember it when classifying the new block's group.
            fetch.queue.append(
                [
                    _entry(
                        kind=EntryKind.TOOL_CALL,
                        header="tool(Edit)",
                        body="README.md",
                        sequence=2,
                    )
                ]
            )
            screen.refresh_now()
            await pilot.pause()
            transcript = screen.query_one(
                "#session_transcript", VerticalScroll
            )
            statics = list(transcript.query(Static))
            assert len(statics) == 2
            agent_plain, tool_plain = (_static_plain(s) for s in statics)
            # First entry: no leading blank.
            assert not agent_plain.startswith("\n")
            # Second entry, arriving in a later fetch, still gets a
            # leading blank because its group differs from the previous
            # rendered entry.
            assert tool_plain.startswith("\n")

    _run(body)


def test_resolve_styles_no_error_kind_is_double_dimmed() -> None:
    """FR-7 acceptance: every error-class entry resolves to a style
    pair whose header and body are not both ``dim``. Error tool results
    carry a red style.
    """

    error_entries = [
        # Erroring tool result (header ``(error)`` suffix is the signal).
        TranscriptEntry(
            sequence=1,
            sub_index=0,
            ts=_NOW,
            kind=EntryKind.TOOL_RESULT,
            header="tool_result(error)",
            body="boom",
            attempt_number=1,
            iteration_number=1,
        ),
        # ``harness.control_command_failed`` lifecycle entry (the
        # classifier sets ``control_command_error`` to the human
        # detail).
        TranscriptEntry(
            sequence=2,
            sub_index=0,
            ts=_NOW,
            kind=EntryKind.LIFECYCLE,
            header="control",
            body="say failed: SDKDisconnected: session was closed",
            attempt_number=1,
            iteration_number=None,
            control_command_id=7,
            control_command_error="SDKDisconnected: session was closed",
        ),
    ]
    for entry in error_entries:
        assert _is_error_entry(entry)
        header_style, body_style = _resolve_styles(entry)
        both_dim = (
            "dim" in header_style.lower() and "dim" in body_style.lower()
        )
        assert not both_dim, (
            f"{entry.header!r} resolved to dim+dim "
            f"(header={header_style!r}, body={body_style!r})"
        )
        # FR-7 explicit: error tool results carry a red style.
        assert "red" in header_style.lower()
        assert "red" in body_style.lower()


def test_resolve_styles_humanized_lifecycle_stays_dim() -> None:
    """FR-7: humanized (non-error) lifecycle phrases stay dim so the
    busy telemetry stream recedes visually behind agent prose.
    """

    entry = TranscriptEntry(
        sequence=1,
        sub_index=0,
        ts=_NOW,
        kind=EntryKind.LIFECYCLE,
        header="iteration",
        body="iteration 3 · 1.2k tokens",
        attempt_number=1,
        iteration_number=None,
    )
    assert not _is_error_entry(entry)
    header_style, body_style = _resolve_styles(entry)
    assert "dim" in header_style.lower()
    assert "dim" in body_style.lower()


def test_resolve_styles_agent_text_body_is_not_dim() -> None:
    """FR-7: agent text uses the default (non-dim) body style so the
    core content of the session reads as the visually dominant line.
    """

    entry = TranscriptEntry(
        sequence=1,
        sub_index=0,
        ts=_NOW,
        kind=EntryKind.AGENT_TEXT,
        header="agent",
        body="hello there",
        attempt_number=1,
        iteration_number=1,
    )
    header_style, body_style = _resolve_styles(entry)
    # Body is not dim.
    assert "dim" not in body_style.lower()


def test_block_group_consecutive_tool_uses_share_group() -> None:
    """FR-6 acceptance edge case: consecutive ``tool_use`` blocks count
    as one tool-activity group, so two tool calls in a row do not get a
    blank line between them."""

    first = TranscriptEntry(
        sequence=1,
        sub_index=0,
        ts=_NOW,
        kind=EntryKind.TOOL_CALL,
        header="tool(Read)",
        body="a.py",
        attempt_number=1,
        iteration_number=1,
    )
    second = TranscriptEntry(
        sequence=1,
        sub_index=1,
        ts=_NOW,
        kind=EntryKind.TOOL_CALL,
        header="tool(Read)",
        body="b.py",
        attempt_number=1,
        iteration_number=1,
    )
    assert _block_group(first) == _block_group(second)
    assert not _needs_blank_separator(first, second)


def test_session_screen_scroll_up_pauses_follow_and_indicator_shows() -> None:
    """FR-4: scrolling up pauses follow; new messages arriving while
    paused trigger the new-activity indicator (Edge Cases row)."""

    async def body() -> None:
        fetch = _ScriptedFetch()
        screen = SessionScreen(
            run_id="run-x",
            task_id="task-alpha",
            fetch=fetch,
            status=_running_status,
            poll_interval_seconds=1000.0,
        )
        app = DashboardApp(
            poll=lambda: DashboardSnapshot(
                summary=SummaryData(
                    active_workers=0,
                    task_counts={},
                    tokens_total=0,
                    cost_usd_total=0.0,
                    runtime_seconds=0,
                ),
                rows=(),
            ),
            poll_interval_seconds=1000.0,
        )
        async with app.run_test() as pilot:
            await app.push_screen(screen)
            await pilot.pause()
            assert screen.follow is True
            indicator = screen.query_one("#session_indicator", Static)
            assert indicator.has_class("hidden")
            # Pause follow via Page Up.
            await pilot.press("pageup")
            assert screen.follow is False
            # New activity arrives -> indicator flips on.
            fetch.queue.append(
                [
                    _entry(
                        kind=EntryKind.AGENT_TEXT,
                        header="agent",
                        body="new line while paused",
                        sequence=10,
                    )
                ]
            )
            screen.refresh_now()
            await pilot.pause()
            assert screen.new_activity is True
            assert not indicator.has_class("hidden")
            # End resumes follow and clears the indicator.
            await pilot.press("end")
            assert screen.follow is True
            assert screen.new_activity is False
            assert indicator.has_class("hidden")

    _run(body)


def test_session_screen_terminal_status_shows_banner() -> None:
    """FR-7: when a run reaches a terminal state the banner appears
    and announces transcript-only / steering-disabled state."""

    async def body() -> None:
        fetch = _ScriptedFetch()
        status = _ScriptedStatus(_running_status())
        screen = SessionScreen(
            run_id="run-x",
            task_id="task-alpha",
            fetch=fetch,
            status=status,
            poll_interval_seconds=1000.0,
        )
        app = DashboardApp(
            poll=lambda: DashboardSnapshot(
                summary=SummaryData(
                    active_workers=0,
                    task_counts={},
                    tokens_total=0,
                    cost_usd_total=0.0,
                    runtime_seconds=0,
                ),
                rows=(),
            ),
            poll_interval_seconds=1000.0,
        )
        async with app.run_test() as pilot:
            await app.push_screen(screen)
            await pilot.pause()
            banner = screen.query_one("#session_banner", Static)
            assert banner.has_class("hidden")
            # Lifecycle reaches DONE -> banner appears.
            status.push(_terminal_status(Status.DONE))
            screen.refresh_now()
            await pilot.pause()
            assert not banner.has_class("hidden")
            text = str(banner.render())
            assert "terminal status: done" in text
            assert "steering disabled" in text

    _run(body)


def test_session_screen_awaiting_approval_surfaces_gate_instruction() -> None:
    """Edge case: a parked run surfaces the gate's instruction prominently."""

    async def body() -> None:
        fetch = _ScriptedFetch()
        status = _ScriptedStatus(
            _awaiting_status("Confirm the migration before merging.")
        )
        screen = SessionScreen(
            run_id="run-x",
            task_id="task-alpha",
            fetch=fetch,
            status=status,
            poll_interval_seconds=1000.0,
        )
        app = DashboardApp(
            poll=lambda: DashboardSnapshot(
                summary=SummaryData(
                    active_workers=0,
                    task_counts={},
                    tokens_total=0,
                    cost_usd_total=0.0,
                    runtime_seconds=0,
                ),
                rows=(),
            ),
            poll_interval_seconds=1000.0,
        )
        async with app.run_test() as pilot:
            await app.push_screen(screen)
            await pilot.pause()
            gate = screen.query_one("#session_gate", Static)
            assert not gate.has_class("hidden")
            assert "Confirm the migration" in str(gate.render())

    _run(body)


def test_session_screen_empty_transcript_renders_without_error() -> None:
    """Edge case: a run with zero messages renders cleanly and follows
    as messages arrive."""

    async def body() -> None:
        fetch = _ScriptedFetch()
        screen = SessionScreen(
            run_id="run-x",
            task_id="task-alpha",
            fetch=fetch,
            status=_running_status,
            poll_interval_seconds=1000.0,
        )
        app = DashboardApp(
            poll=lambda: DashboardSnapshot(
                summary=SummaryData(
                    active_workers=0,
                    task_counts={},
                    tokens_total=0,
                    cost_usd_total=0.0,
                    runtime_seconds=0,
                ),
                rows=(),
            ),
            poll_interval_seconds=1000.0,
        )
        async with app.run_test() as pilot:
            await app.push_screen(screen)
            await pilot.pause()
            assert screen.entries == []
            # First message arrives -> rendered.
            fetch.queue.append(
                [
                    _entry(
                        kind=EntryKind.AGENT_TEXT,
                        header="agent",
                        body="hello",
                        sequence=1,
                    )
                ]
            )
            screen.refresh_now()
            await pilot.pause()
            assert len(screen.entries) == 1

    _run(body)


def _write_running_lifecycle(store: SqliteStore, task_id: str) -> Lifecycle:
    lc = Lifecycle(task_id=task_id, run_id=f"run-{task_id}")
    lc.transition_to(Status.READY, now=_NOW)
    lc.transition_to(Status.RUNNING, now=_NOW)
    store.create_lifecycle(lc)
    return lc


def _store_enqueue(store: SqliteStore, run_id: str) -> Callable[[str, Mapping[str, Any]], int]:
    """Bind a SqliteStore's ``enqueue_command`` to a (kind, payload) closure.

    Matches the shape the production CLI wires up so the tests exercise
    the same producer path: enqueue against the live store, return the
    assigned ``id``.
    """

    def enqueue(kind: str, payload: Mapping[str, Any]) -> int:
        record = store.enqueue_command(
            run_id, kind, payload, now=datetime.now(timezone.utc)
        )
        assert record.id is not None
        return record.id

    return enqueue


def test_dashboard_enter_opens_session_escape_restores_selection(
    tmp_path: Path,
) -> None:
    """FR-3 acceptance: Enter opens the session for the selected run;
    Escape returns to the dashboard with row selection preserved."""

    async def body() -> None:
        db = tmp_path / "db.sqlite"
        store = SqliteStore(db)
        try:
            _write_running_lifecycle(store, "alpha")
            _write_running_lifecycle(store, "beta")
            with FileTelemetrySink(tmp_path / "logs") as sink:
                for run_id, text in (
                    ("run-alpha", "alpha hello"),
                    ("run-beta", "beta hello"),
                ):
                    sink.append_telemetry(
                        TelemetryRecord(
                            run_id=run_id,
                            ts=datetime.now(timezone.utc),
                            kind="AssistantMessage",
                            payload={
                                "content": [
                                    {"type": "text", "text": text}
                                ]
                            },
                            attempt_number=1,
                            iteration_number=1,
                        )
                    )

            opened_with: list[str] = []

            def open_session(run_id: str, task_id: str) -> SessionScreen | None:
                opened_with.append(run_id)
                tailer = TranscriptTailer(
                    tmp_path / "logs" / "runs" / f"{run_id}.jsonl",
                    run_id,
                    redactor=None,
                )
                return SessionScreen(
                    run_id=run_id,
                    task_id=task_id,
                    fetch=tailer.fetch,
                    status=_running_status,
                    poll_interval_seconds=1000.0,
                )

            snap = DashboardSnapshot(
                summary=SummaryData(
                    active_workers=2,
                    task_counts={},
                    tokens_total=0,
                    cost_usd_total=0.0,
                    runtime_seconds=0,
                ),
                rows=(
                    RowSnapshot(
                        run_id="run-alpha",
                        task_id="alpha",
                        status="running",
                        attempt=1,
                        iteration=1,
                        age_seconds=0,
                        tokens=0,
                        cost_usd=0.0,
                        turns=0,
                        iterations_completed=0,
                        last_kind="ASSISTANT",
                        last_detail="(text)",
                        awaiting_instruction=None,
                    ),
                    RowSnapshot(
                        run_id="run-beta",
                        task_id="beta",
                        status="running",
                        attempt=1,
                        iteration=1,
                        age_seconds=0,
                        tokens=0,
                        cost_usd=0.0,
                        turns=0,
                        iterations_completed=0,
                        last_kind="ASSISTANT",
                        last_detail="(text)",
                        awaiting_instruction=None,
                    ),
                ),
            )
            app = DashboardApp(
                poll=lambda: snap,
                poll_interval_seconds=1000.0,
                open_session=open_session,
            )
            async with app.run_test() as pilot:
                table = app.query_one(DataTable)
                # Move to second row, then press Enter.
                await pilot.press("down")
                assert table.cursor_row == 1
                await pilot.press("enter")
                await pilot.pause()
                # The session for the second run was opened.
                assert opened_with == ["run-beta"]
                # Screen is on top of the dashboard.
                assert isinstance(app.screen, SessionScreen)
                assert app.screen.run_id == "run-beta"
                # Transcript drained beta's seeded message.
                assert any(
                    e.body == "beta hello" for e in app.screen.entries
                )
                # Escape returns to the dashboard, selection preserved.
                await pilot.press("escape")
                await pilot.pause()
                assert not isinstance(app.screen, SessionScreen)
                table = app.query_one(DataTable)
                assert table.cursor_row == 1
        finally:
            store.close()

    _run(body)


# --- Steering tests --------------------------------------------------------


def test_session_screen_say_enqueues_one_command_with_text_payload(
    tmp_path: Path,
) -> None:
    """FR-5: a submitted say message produces exactly one control_commands
    row with kind=say and payload={text: ...}."""

    async def body() -> None:
        db = tmp_path / "db.sqlite"
        store = SqliteStore(db)
        try:
            lc = _write_running_lifecycle(store, "alpha")
            screen = SessionScreen(
                run_id=lc.run_id,
                task_id=lc.task_id,
                fetch=_ScriptedFetch(),
                status=_running_status,
                poll_interval_seconds=1000.0,
                enqueue=_store_enqueue(store, lc.run_id),
            )
            app = DashboardApp(
                poll=lambda: DashboardSnapshot(
                    summary=SummaryData(
                        active_workers=0,
                        task_counts={},
                        tokens_total=0,
                        cost_usd_total=0.0,
                        runtime_seconds=0,
                    ),
                    rows=(),
                ),
                poll_interval_seconds=1000.0,
            )
            async with app.run_test() as pilot:
                await app.push_screen(screen)
                await pilot.pause()
                command_id = screen.submit_say("please add docstrings")
                await pilot.pause()
                assert command_id is not None
                # Exactly one row was written.
                claimed = store.claim_commands(
                    lc.run_id, now=datetime.now(timezone.utc)
                )
                assert len(claimed) == 1
                assert claimed[0].kind == "say"
                assert claimed[0].payload == {
                    "text": "please add docstrings"
                }
                # Pending command tracked on the screen.
                assert command_id in screen.pending_commands
                assert (
                    screen.pending_commands[command_id].kind == "say"
                )
        finally:
            store.close()

    _run(body)


def test_session_screen_interrupt_enqueues_with_empty_payload(
    tmp_path: Path,
) -> None:
    """FR-5: ctrl+x enqueues exactly one ``interrupt`` row with no payload."""

    async def body() -> None:
        db = tmp_path / "db.sqlite"
        store = SqliteStore(db)
        try:
            lc = _write_running_lifecycle(store, "alpha")
            screen = SessionScreen(
                run_id=lc.run_id,
                task_id=lc.task_id,
                fetch=_ScriptedFetch(),
                status=_running_status,
                poll_interval_seconds=1000.0,
                enqueue=_store_enqueue(store, lc.run_id),
            )
            app = DashboardApp(
                poll=lambda: DashboardSnapshot(
                    summary=SummaryData(
                        active_workers=0,
                        task_counts={},
                        tokens_total=0,
                        cost_usd_total=0.0,
                        runtime_seconds=0,
                    ),
                    rows=(),
                ),
                poll_interval_seconds=1000.0,
            )
            async with app.run_test() as pilot:
                await app.push_screen(screen)
                await pilot.pause()
                command_id = screen.submit_interrupt()
                await pilot.pause()
                assert command_id is not None
                claimed = store.claim_commands(
                    lc.run_id, now=datetime.now(timezone.utc)
                )
                assert len(claimed) == 1
                assert claimed[0].kind == "interrupt"
                assert claimed[0].payload == {}
        finally:
            store.close()

    _run(body)


def test_session_screen_approve_and_reject_only_when_awaiting_approval(
    tmp_path: Path,
) -> None:
    """FR-5/FR-6 + Edge Cases: approve / reject affordances are only honoured
    when the run is in AWAITING_APPROVAL.

    - Reject with no feedback on an AWAITING_APPROVAL run emits one
      reject row with an empty payload.
    - Reject with feedback emits one reject row with {"feedback": ...}.
    - Approve emits one approve row.
    """

    async def body() -> None:
        db = tmp_path / "db.sqlite"
        store = SqliteStore(db)
        try:
            run_id = "run-gated"
            lc = Lifecycle(task_id="gated", run_id=run_id)
            lc.transition_to(Status.READY, now=_NOW)
            lc.transition_to(Status.RUNNING, now=_NOW)
            lc.transition_to(Status.VALIDATING, now=_NOW)
            lc.transition_to(Status.AWAITING_APPROVAL, now=_NOW)
            store.create_lifecycle(lc)
            screen = SessionScreen(
                run_id=run_id,
                task_id="gated",
                fetch=_ScriptedFetch(),
                status=lambda: _awaiting_status(
                    "Confirm the migration."
                ),
                poll_interval_seconds=1000.0,
                enqueue=_store_enqueue(store, run_id),
            )
            app = DashboardApp(
                poll=lambda: DashboardSnapshot(
                    summary=SummaryData(
                        active_workers=0,
                        task_counts={},
                        tokens_total=0,
                        cost_usd_total=0.0,
                        runtime_seconds=0,
                    ),
                    rows=(),
                ),
                poll_interval_seconds=1000.0,
            )
            async with app.run_test() as pilot:
                await app.push_screen(screen)
                await pilot.pause()
                approve_id = screen.submit_approve()
                reject_no_feedback_id = screen.submit_reject(None)
                reject_with_feedback_id = screen.submit_reject(
                    "please redo the migration with smaller batches"
                )
                await pilot.pause()
                assert approve_id is not None
                assert reject_no_feedback_id is not None
                assert reject_with_feedback_id is not None
                claimed = store.claim_commands(
                    run_id, now=datetime.now(timezone.utc)
                )
                # Three rows in enqueue order.
                assert [c.kind for c in claimed] == [
                    "approve",
                    "reject",
                    "reject",
                ]
                assert claimed[0].payload == {}
                # Empty feedback rejects emit no ``feedback`` key.
                assert claimed[1].payload == {}
                assert claimed[2].payload == {
                    "feedback": "please redo the migration with smaller batches"
                }
        finally:
            store.close()

    _run(body)


def test_session_screen_pending_to_applied_transition(
    tmp_path: Path,
) -> None:
    """FR-6 acceptance: an enqueued command renders pending; once the
    matching ``harness.control_command_applied`` event lands in the
    transcript the pending marker resolves and the OPERATOR_SAY line
    is visible."""

    async def body() -> None:
        db = tmp_path / "db.sqlite"
        store = SqliteStore(db)
        try:
            lc = _write_running_lifecycle(store, "alpha")
            fetch = _ScriptedFetch()
            screen = SessionScreen(
                run_id=lc.run_id,
                task_id=lc.task_id,
                fetch=fetch,
                status=_running_status,
                poll_interval_seconds=1000.0,
                enqueue=_store_enqueue(store, lc.run_id),
            )
            app = DashboardApp(
                poll=lambda: DashboardSnapshot(
                    summary=SummaryData(
                        active_workers=0,
                        task_counts={},
                        tokens_total=0,
                        cost_usd_total=0.0,
                        runtime_seconds=0,
                    ),
                    rows=(),
                ),
                poll_interval_seconds=1000.0,
            )
            async with app.run_test() as pilot:
                await app.push_screen(screen)
                await pilot.pause()
                command_id = screen.submit_say("ship it")
                await pilot.pause()
                assert command_id is not None
                # Pending list shows the command and the widget is
                # visible (not hidden).
                pending_widget = screen.query_one(
                    "#session_pending", Static
                )
                assert not pending_widget.has_class("hidden")
                # Seed the applied event with a matching command_id.
                fetch.queue.append(
                    [
                        TranscriptEntry(
                            sequence=20,
                            sub_index=0,
                            ts=_NOW,
                            kind=EntryKind.OPERATOR_SAY,
                            header="operator(say)",
                            body="ship it",
                            attempt_number=1,
                            iteration_number=None,
                            control_command_id=command_id,
                        )
                    ]
                )
                screen.refresh_now()
                await pilot.pause()
                # Pending entry resolved out of the dict; the
                # OPERATOR_SAY entry is now in the transcript.
                assert command_id not in screen.pending_commands
                assert any(
                    e.kind == EntryKind.OPERATOR_SAY
                    and e.control_command_id == command_id
                    for e in screen.entries
                )
        finally:
            store.close()

    _run(body)


def test_session_screen_pending_to_failed_surfaces_error_detail(
    tmp_path: Path,
) -> None:
    """FR-6 acceptance: ``harness.control_command_failed`` flips the
    pending marker to a failure line carrying the event's error_detail
    inline."""

    async def body() -> None:
        db = tmp_path / "db.sqlite"
        store = SqliteStore(db)
        try:
            lc = _write_running_lifecycle(store, "alpha")
            fetch = _ScriptedFetch()
            screen = SessionScreen(
                run_id=lc.run_id,
                task_id=lc.task_id,
                fetch=fetch,
                status=_running_status,
                poll_interval_seconds=1000.0,
                enqueue=_store_enqueue(store, lc.run_id),
            )
            app = DashboardApp(
                poll=lambda: DashboardSnapshot(
                    summary=SummaryData(
                        active_workers=0,
                        task_counts={},
                        tokens_total=0,
                        cost_usd_total=0.0,
                        runtime_seconds=0,
                    ),
                    rows=(),
                ),
                poll_interval_seconds=1000.0,
            )
            async with app.run_test() as pilot:
                await app.push_screen(screen)
                await pilot.pause()
                command_id = screen.submit_say("oops")
                await pilot.pause()
                assert command_id is not None
                # Seed the failure event.
                fetch.queue.append(
                    [
                        TranscriptEntry(
                            sequence=99,
                            sub_index=0,
                            ts=_NOW,
                            kind=EntryKind.LIFECYCLE,
                            header="control",
                            body=(
                                "failed say: SDKDisconnected: "
                                "session was closed"
                            ),
                            attempt_number=1,
                            iteration_number=None,
                            control_command_id=command_id,
                            control_command_error=(
                                "SDKDisconnected: session was closed"
                            ),
                        )
                    ]
                )
                screen.refresh_now()
                await pilot.pause()
                pending = screen.pending_commands.get(command_id)
                assert pending is not None
                assert pending.status == "failed"
                assert pending.error_detail is not None
                assert "SDKDisconnected" in pending.error_detail
                pending_widget = screen.query_one(
                    "#session_pending", Static
                )
                rendered = str(pending_widget.render())
                assert "failed" in rendered
                assert "SDKDisconnected" in rendered
        finally:
            store.close()

    _run(body)


def test_session_screen_steering_disabled_on_done_run(
    tmp_path: Path,
) -> None:
    """FR-6/FR-7: verbs are unavailable when the viewed run is in a
    terminal status. submit_* methods emit an inline notice and do
    NOT touch the store."""

    async def body() -> None:
        db = tmp_path / "db.sqlite"
        store = SqliteStore(db)
        try:
            run_id = "run-done"
            lc = Lifecycle(task_id="done", run_id=run_id)
            lc.transition_to(Status.READY, now=_NOW)
            lc.transition_to(Status.RUNNING, now=_NOW)
            lc.transition_to(Status.VALIDATING, now=_NOW)
            lc.transition_to(Status.DONE, now=_NOW)
            store.create_lifecycle(lc)
            screen = SessionScreen(
                run_id=run_id,
                task_id="done",
                fetch=_ScriptedFetch(),
                status=lambda: _terminal_status(Status.DONE),
                poll_interval_seconds=1000.0,
                enqueue=_store_enqueue(store, run_id),
            )
            app = DashboardApp(
                poll=lambda: DashboardSnapshot(
                    summary=SummaryData(
                        active_workers=0,
                        task_counts={},
                        tokens_total=0,
                        cost_usd_total=0.0,
                        runtime_seconds=0,
                    ),
                    rows=(),
                ),
                poll_interval_seconds=1000.0,
            )
            async with app.run_test() as pilot:
                await app.push_screen(screen)
                await pilot.pause()
                # Compose box stays mounted (it owns the persistent
                # slash-command vocabulary) but the steering help footer
                # hides because every verb except /help / /quit would be
                # a no-op on a terminal run.
                compose = screen.query_one("#session_compose", Input)
                assert not compose.has_class("hidden")
                help_widget = screen.query_one(
                    "#session_steering_help", Static
                )
                assert help_widget.has_class("hidden")
                # Every verb returns None and writes nothing.
                assert screen.submit_say("late") is None
                assert screen.submit_interrupt() is None
                assert screen.submit_approve() is None
                assert screen.submit_reject("late feedback") is None
                claimed = store.claim_commands(
                    run_id, now=datetime.now(timezone.utc)
                )
                assert claimed == []
                # Notice widget is visible with a not-steerable note.
                notice = screen.query_one("#session_notice", Static)
                assert not notice.has_class("hidden")
                assert "not steerable" in str(notice.render())
        finally:
            store.close()

    _run(body)


def test_session_screen_status_left_active_between_render_and_submit(
    tmp_path: Path,
) -> None:
    """Edge case row: a run that left the active set between render and
    submit gets an inline not-steerable notice and no enqueue. We seed
    the status callable to first return RUNNING (the render frame), then
    DONE (the submit frame)."""

    async def body() -> None:
        db = tmp_path / "db.sqlite"
        store = SqliteStore(db)
        try:
            lc = _write_running_lifecycle(store, "alpha")
            status = _ScriptedStatus(_running_status())
            screen = SessionScreen(
                run_id=lc.run_id,
                task_id=lc.task_id,
                fetch=_ScriptedFetch(),
                status=status,
                poll_interval_seconds=1000.0,
                enqueue=_store_enqueue(store, lc.run_id),
            )
            app = DashboardApp(
                poll=lambda: DashboardSnapshot(
                    summary=SummaryData(
                        active_workers=0,
                        task_counts={},
                        tokens_total=0,
                        cost_usd_total=0.0,
                        runtime_seconds=0,
                    ),
                    rows=(),
                ),
                poll_interval_seconds=1000.0,
            )
            async with app.run_test() as pilot:
                await app.push_screen(screen)
                await pilot.pause()
                # Run terminated before the operator submitted.
                status.push(_terminal_status(Status.DONE))
                screen.refresh_now()
                await pilot.pause()
                assert screen.submit_say("too late") is None
                claimed = store.claim_commands(
                    lc.run_id, now=datetime.now(timezone.utc)
                )
                assert claimed == []
                notice = screen.query_one("#session_notice", Static)
                assert not notice.has_class("hidden")
        finally:
            store.close()

    _run(body)


def test_session_screen_applied_event_for_other_producer_ignored(
    tmp_path: Path,
) -> None:
    """An applied event for a command enqueued by another producer (CLI)
    must not be matched to this instance's pending markers."""

    async def body() -> None:
        db = tmp_path / "db.sqlite"
        store = SqliteStore(db)
        try:
            lc = _write_running_lifecycle(store, "alpha")
            fetch = _ScriptedFetch()
            screen = SessionScreen(
                run_id=lc.run_id,
                task_id=lc.task_id,
                fetch=fetch,
                status=_running_status,
                poll_interval_seconds=1000.0,
                enqueue=_store_enqueue(store, lc.run_id),
            )
            app = DashboardApp(
                poll=lambda: DashboardSnapshot(
                    summary=SummaryData(
                        active_workers=0,
                        task_counts={},
                        tokens_total=0,
                        cost_usd_total=0.0,
                        runtime_seconds=0,
                    ),
                    rows=(),
                ),
                poll_interval_seconds=1000.0,
            )
            async with app.run_test() as pilot:
                await app.push_screen(screen)
                await pilot.pause()
                my_id = screen.submit_say("mine")
                await pilot.pause()
                assert my_id is not None
                # An applied event from a CLI-issued command sneaks in.
                stranger_id = my_id + 999
                fetch.queue.append(
                    [
                        TranscriptEntry(
                            sequence=42,
                            sub_index=0,
                            ts=_NOW,
                            kind=EntryKind.OPERATOR_SAY,
                            header="operator(say)",
                            body="from the CLI",
                            attempt_number=1,
                            iteration_number=None,
                            control_command_id=stranger_id,
                        )
                    ]
                )
                screen.refresh_now()
                await pilot.pause()
                # Local pending entry is untouched.
                assert my_id in screen.pending_commands
        finally:
            store.close()

    _run(body)

"""Pilot-driven tests for :class:`flywheel_tui._session_screen.SessionScreen`.

The screen receives ``fetch`` / ``status`` callables so these tests
drive deterministic frames without touching SQLite (the cursor and
classification paths are covered by ``test_session``). Coverage maps
to FR-3 (Escape returns to dashboard with selection preserved), FR-4
(rendering of each message class, follow-pause-resume), FR-7 (terminal
banner appears + steering disabled signal), and the Edge Cases table
(empty transcript renders cleanly; AWAITING_APPROVAL surfaces gate).
"""

from __future__ import annotations

import asyncio
from collections.abc import Callable, Coroutine
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from textual.containers import VerticalScroll
from textual.widgets import DataTable, Static

from flywheel.lifecycle import Lifecycle, Status
from flywheel.store_sqlite import SqliteStore

from flywheel_tui._dashboard import DashboardApp
from flywheel_tui._session import EntryKind, TranscriptEntry, TranscriptTailer
from flywheel_tui._session_screen import SessionScreen, SessionStatus
from flywheel_tui._snapshot import DashboardSnapshot, RowSnapshot, SummaryData


def _run(coro: Callable[[], Coroutine[Any, Any, None]]) -> None:
    """Drive an async test body without pulling pytest-asyncio in."""

    asyncio.run(coro())


_NOW = datetime(2026, 6, 10, 12, 0, tzinfo=timezone.utc)


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
    """FR-4 acceptance: pilot test asserts rendering of each message
    class (agent text, tool call, tool result, operator say, lifecycle,
    gate event)."""

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
                    body="file_path=README.md",
                    sequence=2,
                ),
                _entry(
                    kind=EntryKind.TOOL_RESULT,
                    header="tool_result",
                    body="14B",
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
            # Transcript widget mounted one Static per entry.
            transcript = screen.query_one(
                "#session_transcript", VerticalScroll
            )
            assert len(transcript.query(Static)) >= 6

    _run(body)


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
            store.save_sdk_messages(
                run_id="run-alpha",
                attempt_number=1,
                iteration_number=1,
                messages=[
                    {
                        "message_type": "AssistantMessage",
                        "content": [{"type": "text", "text": "alpha hello"}],
                    }
                ],
            )
            store.save_sdk_messages(
                run_id="run-beta",
                attempt_number=1,
                iteration_number=1,
                messages=[
                    {
                        "message_type": "AssistantMessage",
                        "content": [{"type": "text", "text": "beta hello"}],
                    }
                ],
            )

            opened_with: list[str] = []

            def open_session(run_id: str, task_id: str) -> SessionScreen | None:
                opened_with.append(run_id)
                tailer = TranscriptTailer(store, run_id, redactor=None)
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

"""Pilot-driven tests for :class:`flywheel_cli._session_screen.SessionScreen`.

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

from flywheel.lifecycle import Lifecycle, Status
from flywheel.store_sqlite import SqliteStore

from flywheel_cli._dashboard import DashboardApp
from flywheel_cli._session import EntryKind, TranscriptEntry, TranscriptTailer
from flywheel_cli._session_screen import SessionScreen, SessionStatus
from flywheel_cli._snapshot import DashboardSnapshot, RowSnapshot, SummaryData


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

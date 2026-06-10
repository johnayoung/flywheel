"""Pilot-driven tests for the persistent input bar (dashboard + session).

Coverage maps to the spec 00021 FR-5 / Error Handling acceptance list:

* one happy slash command per screen,
* the dashboard's plain-text filter,
* the zero-match filter empty-state,
* the unknown-command inline error.

Each test runs an async coroutine through ``asyncio.run`` so the suite
stays plugin-free -- no pytest-asyncio required, matching the existing
:mod:`test_dashboard` / :mod:`test_session_screen` conventions.
"""

from __future__ import annotations

import asyncio
from collections.abc import Callable, Coroutine, Mapping
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from textual.widgets import DataTable, Input, Static

from flywheel_core.lifecycle import Lifecycle, Status
from flywheel_core.store_sqlite import SqliteStore

from flywheel._dashboard import DashboardApp
from flywheel._session_screen import SessionScreen, SessionStatus
from flywheel._snapshot import DashboardSnapshot, RowSnapshot, SummaryData


def _run(coro: Callable[[], Coroutine[Any, Any, None]]) -> None:
    """Drive an async test body without pulling pytest-asyncio in."""

    asyncio.run(coro())


_NOW = datetime(2026, 6, 10, 12, 0, tzinfo=timezone.utc)


def _row(
    task_id: str,
    *,
    run_id: str | None = None,
    status: str = "running",
) -> RowSnapshot:
    return RowSnapshot(
        run_id=run_id or f"run-{task_id}",
        task_id=task_id,
        status=status,
        attempt=1,
        iteration=2,
        age_seconds=3,
        tokens=42,
        cost_usd=0.123,
        turns=1,
        iterations_completed=1,
        last_kind="ASSISTANT",
        last_detail="Edit(file_path=README.md)",
        awaiting_instruction=None,
    )


def _snapshot(*rows: RowSnapshot) -> DashboardSnapshot:
    return DashboardSnapshot(
        summary=SummaryData(
            active_workers=len(rows),
            task_counts={},
            tokens_total=0,
            cost_usd_total=0.0,
            runtime_seconds=0,
        ),
        rows=tuple(rows),
    )


def _empty_snapshot() -> DashboardSnapshot:
    return DashboardSnapshot(
        summary=SummaryData(
            active_workers=0,
            task_counts={},
            tokens_total=0,
            cost_usd_total=0.0,
            runtime_seconds=0,
        ),
        rows=(),
    )


def _running_status() -> SessionStatus:
    return SessionStatus(status=Status.RUNNING, awaiting_instruction=None)


# --- Dashboard ----------------------------------------------------------------


def test_dashboard_help_slash_shows_help_text() -> None:
    """``/help`` on the dashboard surfaces the slash-command list in the
    notice widget (FR-5 happy-path coverage for the dashboard)."""

    async def body() -> None:
        snap = _snapshot(_row("alpha"))
        app = DashboardApp(poll=lambda: snap, poll_interval_seconds=1000.0)
        async with app.run_test() as pilot:
            input_widget = app.query_one("#dashboard_input", Input)
            input_widget.focus()
            await pilot.pause()
            input_widget.value = "/help"
            await input_widget.action_submit()
            await pilot.pause()
            notice = app.query_one("#dashboard_notice", Static)
            assert not notice.has_class("hidden")
            rendered = str(notice.render())
            assert "/approve" in rendered
            assert "/interrupt" in rendered
            # Successful slash commands clear the input.
            assert input_widget.value == ""

    _run(body)


def test_dashboard_plain_text_filters_rows() -> None:
    """Typing plain text into the input bar filters the dashboard rows
    in real time; clearing the text restores them (FR-5 filter)."""

    async def body() -> None:
        snap = _snapshot(_row("alpha"), _row("beta"), _row("gamma"))
        app = DashboardApp(poll=lambda: snap, poll_interval_seconds=1000.0)
        async with app.run_test() as pilot:
            await pilot.pause()
            assert app._visible_run_order == [
                "run-alpha",
                "run-beta",
                "run-gamma",
            ]
            input_widget = app.query_one("#dashboard_input", Input)
            input_widget.focus()
            await pilot.pause()
            # Type one row's task id; only that row survives.
            input_widget.value = "alpha"
            # The Input widget posts a Changed message on value
            # assignment in run_test; flush.
            await pilot.pause()
            assert app._visible_run_order == ["run-alpha"]
            assert app.filter_text == "alpha"
            # Clearing the filter restores every row.
            input_widget.value = ""
            await pilot.pause()
            assert app._visible_run_order == [
                "run-alpha",
                "run-beta",
                "run-gamma",
            ]
            assert app.filter_text == ""

    _run(body)


def test_dashboard_zero_match_filter_shows_empty_state() -> None:
    """Filter text matching zero rows displays the empty-state line
    naming the filter (Edge Cases row)."""

    async def body() -> None:
        snap = _snapshot(_row("alpha"), _row("beta"))
        app = DashboardApp(poll=lambda: snap, poll_interval_seconds=1000.0)
        async with app.run_test() as pilot:
            await pilot.pause()
            input_widget = app.query_one("#dashboard_input", Input)
            input_widget.focus()
            await pilot.pause()
            input_widget.value = "no-such-task"
            await pilot.pause()
            assert app._visible_run_order == []
            empty = app.query_one("#empty_state", Static)
            assert not empty.has_class("hidden")
            assert "no-such-task" in str(empty.render())
            # Restoring the filter brings the rows back.
            input_widget.value = ""
            await pilot.pause()
            assert empty.has_class("hidden")
            assert app._visible_run_order == ["run-alpha", "run-beta"]

    _run(body)


def test_dashboard_unknown_slash_command_inline_error_preserves_input() -> None:
    """An unknown ``/`` command surfaces the inline error and leaves the
    typed line populated for editing (Error Handling row)."""

    async def body() -> None:
        snap = _snapshot(_row("alpha"))
        app = DashboardApp(poll=lambda: snap, poll_interval_seconds=1000.0)
        async with app.run_test() as pilot:
            input_widget = app.query_one("#dashboard_input", Input)
            input_widget.focus()
            await pilot.pause()
            input_widget.value = "/nope"
            await input_widget.action_submit()
            await pilot.pause()
            notice = app.query_one("#dashboard_notice", Static)
            assert not notice.has_class("hidden")
            rendered = str(notice.render())
            assert "/nope" in rendered
            assert "/help" in rendered
            # Input preserved so the operator can edit.
            assert input_widget.value == "/nope"

    _run(body)


def test_dashboard_unfocused_input_does_not_steal_arrows_or_q() -> None:
    """The persistent input bar must not steal arrows/Enter/q/? from
    the dashboard's existing bindings when it does not have focus."""

    async def body() -> None:
        snap = _snapshot(_row("alpha"), _row("beta"))
        app = DashboardApp(poll=lambda: snap, poll_interval_seconds=1000.0)
        async with app.run_test() as pilot:
            await pilot.pause()
            table = app.query_one(DataTable)
            # Focus starts on the table.
            assert table.has_focus
            assert table.cursor_row == 0
            await pilot.press("down")
            assert table.cursor_row == 1
            await pilot.press("up")
            assert table.cursor_row == 0
            # ? still toggles help.
            help_widget = app.query_one("#help_footer", Static)
            assert help_widget.has_class("hidden")
            await pilot.press("question_mark")
            assert not help_widget.has_class("hidden")

    _run(body)


# --- Session ------------------------------------------------------------------


def test_session_help_slash_shows_help_text(tmp_path: Path) -> None:
    """``/help`` on the session screen surfaces the slash-command list
    in the inline notice widget (FR-5 happy path for the session)."""

    async def body() -> None:
        db = tmp_path / "db.sqlite"
        store = SqliteStore(db)
        try:
            lc = Lifecycle(task_id="alpha", run_id="run-alpha")
            lc.transition_to(Status.READY, now=_NOW)
            lc.transition_to(Status.RUNNING, now=_NOW)
            store.create_lifecycle(lc)
            screen = SessionScreen(
                run_id="run-alpha",
                task_id="alpha",
                fetch=lambda: [],
                status=_running_status,
                poll_interval_seconds=1000.0,
                enqueue=_unused_enqueue,
            )
            app = DashboardApp(
                poll=_empty_snapshot, poll_interval_seconds=1000.0
            )
            async with app.run_test() as pilot:
                await app.push_screen(screen)
                await pilot.pause()
                compose = screen.query_one("#session_compose", Input)
                compose.focus()
                await pilot.pause()
                compose.value = "/help"
                await compose.action_submit()
                await pilot.pause()
                notice = screen.query_one("#session_notice", Static)
                assert not notice.has_class("hidden")
                rendered = str(notice.render())
                assert "/help" in rendered
                assert "/quit" in rendered
                # Successful slash commands clear the input.
                assert compose.value == ""
        finally:
            store.close()

    _run(body)


def test_session_unknown_slash_command_preserves_input(tmp_path: Path) -> None:
    """An unknown ``/`` command on the session surfaces the inline error
    and leaves the typed line for editing."""

    async def body() -> None:
        db = tmp_path / "db.sqlite"
        store = SqliteStore(db)
        try:
            lc = Lifecycle(task_id="alpha", run_id="run-alpha")
            lc.transition_to(Status.READY, now=_NOW)
            lc.transition_to(Status.RUNNING, now=_NOW)
            store.create_lifecycle(lc)
            screen = SessionScreen(
                run_id="run-alpha",
                task_id="alpha",
                fetch=lambda: [],
                status=_running_status,
                poll_interval_seconds=1000.0,
                enqueue=_unused_enqueue,
            )
            app = DashboardApp(
                poll=_empty_snapshot, poll_interval_seconds=1000.0
            )
            async with app.run_test() as pilot:
                await app.push_screen(screen)
                await pilot.pause()
                compose = screen.query_one("#session_compose", Input)
                compose.focus()
                await pilot.pause()
                compose.value = "/nope"
                await compose.action_submit()
                await pilot.pause()
                notice = screen.query_one("#session_notice", Static)
                assert not notice.has_class("hidden")
                rendered = str(notice.render())
                assert "/nope" in rendered
                # Input preserved so the operator can edit.
                assert compose.value == "/nope"
        finally:
            store.close()

    _run(body)


def test_session_slash_interrupt_enqueues_against_viewed_run(
    tmp_path: Path,
) -> None:
    """``/interrupt`` on the session screen reuses the steering pipeline:
    one ``kind=interrupt`` row written for the viewed run."""

    async def body() -> None:
        db = tmp_path / "db.sqlite"
        store = SqliteStore(db)
        try:
            run_id = "run-alpha"
            lc = Lifecycle(task_id="alpha", run_id=run_id)
            lc.transition_to(Status.READY, now=_NOW)
            lc.transition_to(Status.RUNNING, now=_NOW)
            store.create_lifecycle(lc)

            def enqueue(kind: str, payload: Mapping[str, Any]) -> int:
                record = store.enqueue_command(
                    run_id, kind, payload, now=datetime.now(timezone.utc)
                )
                assert record.id is not None
                return record.id

            screen = SessionScreen(
                run_id=run_id,
                task_id="alpha",
                fetch=lambda: [],
                status=_running_status,
                poll_interval_seconds=1000.0,
                enqueue=enqueue,
            )
            app = DashboardApp(
                poll=_empty_snapshot, poll_interval_seconds=1000.0
            )
            async with app.run_test() as pilot:
                await app.push_screen(screen)
                await pilot.pause()
                compose = screen.query_one("#session_compose", Input)
                compose.focus()
                await pilot.pause()
                compose.value = "/interrupt"
                await compose.action_submit()
                await pilot.pause()
                claimed = store.claim_commands(
                    run_id, now=datetime.now(timezone.utc)
                )
                assert [c.kind for c in claimed] == ["interrupt"]
                assert claimed[0].payload == {}
                # Successful slash command clears the input.
                assert compose.value == ""
        finally:
            store.close()

    _run(body)


# --- Helpers ------------------------------------------------------------------


def _unused_enqueue(kind: str, payload: Mapping[str, Any]) -> int:
    """Stub enqueue used by tests that should never reach it.

    Asserts immediately so an accidental enqueue surfaces as a clear
    failure rather than a silent control-command-row leak.
    """

    raise AssertionError(
        f"unexpected enqueue: kind={kind!r} payload={payload!r}"
    )

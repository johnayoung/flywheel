"""Pilot-driven tests for :class:`flywheel._history_screen.HistoryScreen`
and its dashboard wiring (``h`` key + ``/history`` slash command).

The screen receives an injected ``fetch`` callable, so these tests drive
deterministic frames without touching SQLite — same shape as the
dashboard tests. Each test runs through ``asyncio.run`` so the suite
stays plugin-free.
"""

from __future__ import annotations

import asyncio
from collections.abc import Callable, Coroutine
from datetime import datetime, timezone
from typing import Any

from textual.widgets import DataTable, Input, Static

from flywheel_core.lifecycle import Status
from flywheel_orchestrator import HistoryRow, HistoryRun

from flywheel._dashboard import DashboardApp
from flywheel._history_screen import HistoryScreen
from flywheel._session_screen import SessionScreen, SessionStatus
from flywheel._snapshot import DashboardSnapshot, SummaryData

_T0 = datetime(2026, 6, 1, 12, 0, 0, tzinfo=timezone.utc)


def _run(coro: Callable[[], Coroutine[Any, Any, None]]) -> None:
    asyncio.run(coro())


def _hrun(
    task_id: str,
    *,
    run_id: str | None = None,
    status: Status = Status.DONE,
) -> HistoryRun:
    return HistoryRun(
        run_id=run_id or f"run-{task_id}",
        task_id=task_id,
        status=status,
        source=f".flywheel/tasks/active/30-a/{task_id}.json",
        started_at=_T0,
        finished_at=_T0,
        retries=0,
        error="",
        attempts=1,
        tokens_total=1234,
        cost_usd_total=0.5,
        turns_total=9,
    )


def _hrow(task_id: str, **kwargs: Any) -> HistoryRow:
    return HistoryRow(
        task_id=task_id,
        phase="30-a",
        latest=_hrun(task_id, **kwargs),
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


# --- HistoryScreen on its own ------------------------------------------------


class _HistoryHost(DashboardApp):
    """Minimal host: a dashboard whose history factory is the screen under
    test, so the screen runs inside a real app/screen stack."""


def test_history_screen_renders_rows_and_empty_state() -> None:
    async def body() -> None:
        rows = [_hrow("alpha"), _hrow("beta", status=Status.FAILED)]
        screen = HistoryScreen(fetch=lambda: rows)
        app = DashboardApp(
            poll=_empty_snapshot,
            poll_interval_seconds=1000.0,
            open_history=lambda: screen,
        )
        async with app.run_test() as pilot:
            await pilot.press("h")
            assert app.screen is screen
            assert screen.visible_run_order == ["run-alpha", "run-beta"]
            empty = screen.query_one("#history_empty", Static)
            assert empty.has_class("hidden")
            # Escape pops back to the dashboard.
            await pilot.press("escape")
            assert app.screen is not screen

    _run(body)


def test_history_screen_empty_fetch_shows_marker() -> None:
    async def body() -> None:
        screen = HistoryScreen(fetch=list)
        app = DashboardApp(
            poll=_empty_snapshot,
            poll_interval_seconds=1000.0,
            open_history=lambda: screen,
        )
        async with app.run_test() as pilot:
            await pilot.press("h")
            empty = screen.query_one("#history_empty", Static)
            assert not empty.has_class("hidden")
            assert "(no finished runs)" in str(empty.render())

    _run(body)


def test_history_screen_fetch_error_is_contained() -> None:
    async def body() -> None:
        calls = {"n": 0}

        def fetch() -> list[HistoryRow]:
            calls["n"] += 1
            if calls["n"] > 1:
                raise RuntimeError("store gone")
            return [_hrow("alpha")]

        screen = HistoryScreen(fetch=fetch)
        app = DashboardApp(
            poll=_empty_snapshot,
            poll_interval_seconds=1000.0,
            open_history=lambda: screen,
        )
        async with app.run_test() as pilot:
            await pilot.press("h")
            assert screen.visible_run_order == ["run-alpha"]
            # Reload hits the raising fetch: rows stay, error surfaces.
            await pilot.press("r")
            assert screen.visible_run_order == ["run-alpha"]
            error = screen.query_one("#history_error", Static)
            assert not error.has_class("hidden")
            assert "history read failed" in str(error.render())

    _run(body)


def test_history_screen_reload_picks_up_new_rows() -> None:
    async def body() -> None:
        rows: list[HistoryRow] = [_hrow("alpha")]
        screen = HistoryScreen(fetch=lambda: list(rows))
        app = DashboardApp(
            poll=_empty_snapshot,
            poll_interval_seconds=1000.0,
            open_history=lambda: screen,
        )
        async with app.run_test() as pilot:
            await pilot.press("h")
            assert screen.visible_run_order == ["run-alpha"]
            rows.append(_hrow("beta"))
            await pilot.press("r")
            assert screen.visible_run_order == ["run-alpha", "run-beta"]

    _run(body)


def test_history_screen_enter_opens_session_for_selected_run() -> None:
    async def body() -> None:
        opened: list[tuple[str, str]] = []

        def open_session(run_id: str, task_id: str) -> SessionScreen:
            opened.append((run_id, task_id))
            return SessionScreen(
                run_id=run_id,
                task_id=task_id,
                fetch=list,
                status=lambda: SessionStatus(
                    status=Status.DONE, awaiting_instruction=None
                ),
            )

        screen = HistoryScreen(
            fetch=lambda: [_hrow("alpha"), _hrow("beta")],
            open_session=open_session,
        )
        app = DashboardApp(
            poll=_empty_snapshot,
            poll_interval_seconds=1000.0,
            open_history=lambda: screen,
        )
        async with app.run_test() as pilot:
            await pilot.press("h")
            await pilot.press("down")
            await pilot.press("enter")
            assert opened == [("run-beta", "beta")]
            assert isinstance(app.screen, SessionScreen)
            # Escape unwinds back to the history screen.
            await pilot.press("escape")
            assert app.screen is screen

    _run(body)


# --- dashboard wiring ----------------------------------------------------------


def test_dashboard_slash_history_pushes_screen() -> None:
    async def body() -> None:
        screen = HistoryScreen(fetch=lambda: [_hrow("alpha")])
        app = DashboardApp(
            poll=_empty_snapshot,
            poll_interval_seconds=1000.0,
            open_history=lambda: screen,
        )
        async with app.run_test() as pilot:
            await pilot.press("ctrl+i")
            input_widget = app.query_one("#dashboard_input", Input)
            input_widget.value = "/history"
            await pilot.press("enter")
            assert app.screen is screen
            assert input_widget.value == ""

    _run(body)


def test_dashboard_history_not_wired_degrades_to_notice() -> None:
    async def body() -> None:
        app = DashboardApp(
            poll=_empty_snapshot, poll_interval_seconds=1000.0
        )
        async with app.run_test() as pilot:
            # The key binding is a silent no-op without a factory.
            await pilot.press("h")
            assert not isinstance(app.screen, HistoryScreen)
            # The slash verb surfaces the standard not-wired notice.
            await pilot.press("ctrl+i")
            input_widget = app.query_one("#dashboard_input", Input)
            input_widget.value = "/history"
            await pilot.press("enter")
            assert app.notice == "/history is not wired on this screen"

    _run(body)


def test_history_screen_renders_status_and_totals_cells() -> None:
    async def body() -> None:
        screen = HistoryScreen(
            fetch=lambda: [_hrow("alpha", status=Status.FAILED)]
        )
        app = DashboardApp(
            poll=_empty_snapshot,
            poll_interval_seconds=1000.0,
            open_history=lambda: screen,
        )
        async with app.run_test() as pilot:
            await pilot.press("h")
            table = screen.query_one(DataTable)
            row = table.get_row_at(0)
            cells = [str(c) for c in row]
            assert cells[0] == "30-a"
            assert cells[1] == "alpha"
            assert cells[2] == "failed"
            assert cells[4] == "1"
            assert cells[5] == "1234"
            assert cells[6] == "$0.5000"

    _run(body)

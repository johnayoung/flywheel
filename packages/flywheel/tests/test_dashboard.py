"""Pilot-driven tests for :class:`flywheel._dashboard.DashboardApp`.

The Textual app receives an injected ``poll`` callable, so these tests
drive deterministic frames without touching SQLite. Coverage matches
the FR-3 acceptance bullet (scripted Textual pilot test drives the
navigation), plus the error-handling and FR-7 dashboard-linger rows of
the spec's edge-case table.

Each test runs an async coroutine through ``asyncio.run`` so the suite
stays plugin-free — no pytest-asyncio required.
"""

from __future__ import annotations

import asyncio
from collections.abc import Callable, Coroutine
from datetime import datetime, timedelta, timezone
from typing import Any

from textual.widgets import DataTable, Static

from flywheel._dashboard import DashboardApp
from flywheel._snapshot import DashboardSnapshot, RowSnapshot, SummaryData


def _run(coro: Callable[[], Coroutine[Any, Any, None]]) -> None:
    """Drive an async test body without pulling pytest-asyncio in."""
    asyncio.run(coro())


def _row(task_id: str, *, run_id: str | None = None) -> RowSnapshot:
    return RowSnapshot(
        run_id=run_id or f"run-{task_id}",
        task_id=task_id,
        status="running",
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


def _summary(active: int = 0, **counts: int) -> SummaryData:
    return SummaryData(
        active_workers=active,
        task_counts=counts,
        tokens_total=0,
        cost_usd_total=0.0,
        runtime_seconds=0,
    )


def _snapshot(*rows: RowSnapshot, **kwargs: int) -> DashboardSnapshot:
    return DashboardSnapshot(
        summary=_summary(len(rows), **kwargs), rows=tuple(rows)
    )


class _Clock:
    """Deterministic clock so the linger-window math is testable."""

    def __init__(self, start: datetime) -> None:
        self.now = start

    def __call__(self) -> datetime:
        return self.now

    def advance(self, seconds: float) -> None:
        self.now = self.now + timedelta(seconds=seconds)


def test_dashboard_keyboard_navigation_and_quit() -> None:
    """FR-3 acceptance: up/down moves selection, q quits, ? toggles help.

    Drives one Pilot session through every binding and asserts the
    observable side-effects rather than poking at private widget state.
    """

    async def body() -> None:
        snapshot = _snapshot(_row("alpha"), _row("beta"), _row("gamma"))
        app = DashboardApp(poll=lambda: snapshot, poll_interval_seconds=1000.0)
        async with app.run_test() as pilot:
            table = app.query_one(DataTable)
            # Initial cursor lands on the first row.
            assert table.cursor_row == 0
            # Down moves to the next row.
            await pilot.press("down")
            assert table.cursor_row == 1
            # Down again.
            await pilot.press("down")
            assert table.cursor_row == 2
            # Up reverses.
            await pilot.press("up")
            assert table.cursor_row == 1
            # ? toggles the help footer in then out.
            help_widget = app.query_one("#help_footer", Static)
            assert help_widget.has_class("hidden")
            await pilot.press("question_mark")
            assert not help_widget.has_class("hidden")
            await pilot.press("question_mark")
            assert help_widget.has_class("hidden")
            # q quits cleanly.
            await pilot.press("q")
        assert app.return_code in (0, None)

    _run(body)


def test_dashboard_empty_state_renders_header_and_keeps_polling() -> None:
    """Zero active runs: header + empty-state line, table stays alive
    so polling can pick up the next freshly-arrived run."""

    async def body() -> None:
        app = DashboardApp(
            poll=lambda: _snapshot(), poll_interval_seconds=1000.0
        )
        async with app.run_test() as pilot:
            empty = app.query_one("#empty_state", Static)
            assert not empty.has_class("hidden")
            # Summary header still rendered with active=0.
            summary = app.query_one("#summary", Static)
            assert "active=0" in str(summary.render())
            # A forced refresh stays on the empty state.
            app.refresh_now()
            await pilot.pause()
            assert not empty.has_class("hidden")

    _run(body)


def test_dashboard_store_read_error_preserves_last_good_frame() -> None:
    """Persistent failure does not crash the app: the last good frame
    stays on screen, the status bar carries the warning, and the next
    successful poll clears it."""

    async def body() -> None:
        state: dict[str, object] = {"mode": "good"}

        def poll() -> DashboardSnapshot:
            if state["mode"] == "good":
                return _snapshot(_row("alpha"))
            raise RuntimeError("database is locked")

        app = DashboardApp(poll=poll, poll_interval_seconds=1000.0)
        async with app.run_test() as pilot:
            # First frame: alpha rendered, no error.
            table = app.query_one(DataTable)
            assert table.row_count == 1
            status_bar = app.query_one("#status_bar", Static)
            assert status_bar.has_class("hidden")
            # Switch to error mode, force a tick. Row stays; bar warns.
            state["mode"] = "error"
            app.refresh_now()
            await pilot.pause()
            assert table.row_count == 1  # last good frame preserved
            assert not status_bar.has_class("hidden")
            assert "database is locked" in str(status_bar.render())
            # Back to good mode: warning clears.
            state["mode"] = "good"
            app.refresh_now()
            await pilot.pause()
            assert status_bar.has_class("hidden")

    _run(body)


def test_dashboard_lingers_dropped_row_then_drops_it() -> None:
    """FR-7 dashboard half: a run leaving the active set keeps its row
    dimmed for ~30s before dropping."""

    async def body() -> None:
        state: dict[str, DashboardSnapshot] = {
            "snapshot": _snapshot(_row("alpha"), _row("beta"))
        }
        clock = _Clock(datetime(2026, 6, 10, 12, 0, tzinfo=timezone.utc))
        app = DashboardApp(
            poll=lambda: state["snapshot"],
            poll_interval_seconds=1000.0,
            linger_seconds=30,
            clock=clock,
        )
        async with app.run_test() as pilot:
            table = app.query_one(DataTable)
            assert table.row_count == 2
            assert app._visible_run_order == ["run-alpha", "run-beta"]
            # Beta leaves the active set; alpha stays.
            state["snapshot"] = _snapshot(_row("alpha"))
            clock.advance(1)
            app.refresh_now()
            await pilot.pause()
            # Beta still on screen (dimmed) inside the linger window.
            assert table.row_count == 2
            assert "run-beta" in app._visible_run_order
            # Advance just shy of the linger window: still present.
            clock.advance(28)
            app.refresh_now()
            await pilot.pause()
            assert "run-beta" in app._visible_run_order
            # Cross the linger threshold: beta drops.
            clock.advance(2)
            app.refresh_now()
            await pilot.pause()
            assert table.row_count == 1
            assert app._visible_run_order == ["run-alpha"]

    _run(body)


def test_dashboard_summary_header_shows_counts_and_totals() -> None:
    async def body() -> None:
        snap = DashboardSnapshot(
            summary=SummaryData(
                active_workers=2,
                task_counts={"fresh": 3, "done": 5, "retryable": 1},
                tokens_total=12345,
                cost_usd_total=1.2345,
                runtime_seconds=125,
            ),
            rows=(_row("alpha"), _row("beta")),
        )
        app = DashboardApp(poll=lambda: snap, poll_interval_seconds=1000.0)
        async with app.run_test():
            text = str(app.query_one("#summary", Static).render())
            assert "active=2" in text
            assert "queued=3" in text
            assert "done=5" in text
            assert "failed=1" in text
            assert "tokens=12k" in text
            assert "$1.23" in text
            assert "2m05s" in text

    _run(body)


def test_dashboard_cursor_survives_refresh() -> None:
    """Regression: ``DataTable.clear()`` resets the cursor to row 0 and
    ``_render`` runs on every poll tick, so without explicit restore the
    selection snapped back to the first task once a second."""

    async def body() -> None:
        snapshot = _snapshot(_row("alpha"), _row("beta"), _row("gamma"))
        app = DashboardApp(
            poll=lambda: snapshot, poll_interval_seconds=1000.0
        )
        async with app.run_test() as pilot:
            table = app.query_one(DataTable)
            await pilot.press("down")
            assert table.cursor_row == 1
            # A poll tick re-renders; the selection must stay on beta.
            app.refresh_now()
            await pilot.pause()
            assert table.cursor_row == 1

    _run(body)


def test_dashboard_cursor_follows_selected_run_when_rows_shift() -> None:
    """The restore targets the selected run id, so reordering or
    departing rows above it do not change which run is selected."""

    async def body() -> None:
        state: dict[str, DashboardSnapshot] = {
            "snapshot": _snapshot(_row("alpha"), _row("beta"), _row("gamma"))
        }
        clock = _Clock(datetime(2026, 6, 10, 12, 0, tzinfo=timezone.utc))
        app = DashboardApp(
            poll=lambda: state["snapshot"],
            poll_interval_seconds=1000.0,
            linger_seconds=0,
            clock=clock,
        )
        async with app.run_test() as pilot:
            table = app.query_one(DataTable)
            await pilot.press("down")
            assert app._visible_run_order[table.cursor_row] == "run-beta"
            # Alpha finishes and drops out instantly (linger 0).
            state["snapshot"] = _snapshot(_row("beta"), _row("gamma"))
            clock.advance(1)
            app.refresh_now()
            await pilot.pause()
            assert app._visible_run_order[table.cursor_row] == "run-beta"

    _run(body)


def test_dashboard_ctrl_c_quits() -> None:
    """ctrl+c routes through the quit path (ctrl+q is unusable inside
    VS Code's terminal, so the Textual default hint dead-ends)."""

    async def body() -> None:
        app = DashboardApp(
            poll=lambda: _snapshot(_row("alpha")),
            poll_interval_seconds=1000.0,
        )
        async with app.run_test() as pilot:
            await pilot.press("ctrl+c")
        assert app.return_code in (0, None)

    _run(body)

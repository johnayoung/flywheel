"""Textual screen that lists finished runs (the console's history view).

Opened from the dashboard via the ``h`` key or the ``/history`` slash
command. One row per task, most recently finished first — the same
grouping ``flywheel history`` prints (a retried task folds into one row
with ``runs=N``). ``Enter`` drills into the selected run through the
same session-screen factory the dashboard uses, so a completed run's
transcript is one keystroke away; ``Escape`` returns to the dashboard.

The screen takes a ``fetch`` callable rather than a store handle so
Pilot tests can drive deterministic frames; production wiring threads a
:func:`flywheel_orchestrator.collect_history_rows` closure through it.
History is fetched on mount and on demand (``r``) — finished runs do
not change underneath the operator the way live rows do, so there is
no polling timer.
"""

from __future__ import annotations

from collections.abc import Callable
from datetime import datetime

from rich.text import Text
from textual.app import ComposeResult
from textual.binding import Binding
from textual.screen import Screen
from textual.widgets import DataTable, Static

from flywheel_core.lifecycle import Status
from flywheel_orchestrator import HistoryRow


# ``(key, label)`` pairs, mirroring the dashboard table's convention:
# keys are stable cell identifiers, labels are the rendered header.
_HISTORY_COLUMNS: tuple[tuple[str, str], ...] = (
    ("phase", "phase"),
    ("task_id", "task"),
    ("status", "status"),
    ("finished", "finished"),
    ("runs", "runs"),
    ("tokens", "tokens"),
    ("cost", "cost"),
)

_STATUS_STYLES: dict[str, str] = {
    Status.DONE.value: "green",
    Status.FAILED.value: "bold red",
    Status.FAILED_VALIDATION.value: "red",
}

# Session-screen factory shape shared with the dashboard: takes
# ``(run_id, task_id)`` and returns the screen to push (or ``None`` when
# sessions are not wired, e.g. snapshot-only tests).
OpenSession = Callable[[str, str], Screen[None] | None]

FetchHistory = Callable[[], list[HistoryRow]]


def _format_finished(ts: datetime | None) -> str:
    return ts.strftime("%Y-%m-%d %H:%M") if ts is not None else "—"


def _format_tokens(tokens: int) -> str:
    if tokens >= 1_000_000:
        return f"{tokens / 1_000_000:.1f}M"
    if tokens >= 10_000:
        return f"{tokens / 1_000:.0f}k"
    return str(tokens)


def _row_to_cells(
    row: HistoryRow,
) -> tuple[str, str, str, str, str, str, str]:
    run = row.latest
    return (
        row.phase or "—",
        row.task_id,
        run.status.value,
        _format_finished(run.finished_at),
        str(1 + len(row.prior_runs)),
        _format_tokens(run.tokens_total),
        f"${run.cost_usd_total:.4f}",
    )


class HistoryScreen(Screen[None]):
    """Finished-run listing with drill-in to the per-run session view.

    Exposed for tests: ``rows`` (the most recent fetch) and
    ``visible_run_order`` (run ids in table order) — the same shape
    ``DashboardApp._visible_run_order`` takes so Pilot tests can assert
    what the table shows without poking widget internals.
    """

    CSS = """
    Screen {
        layout: vertical;
    }

    #history_header {
        height: auto;
        padding: 0 1;
        background: $boost;
    }

    #history_rows {
        height: 1fr;
        scrollbar-gutter: stable;
    }

    #history_empty {
        height: auto;
        padding: 0 1;
        color: $text-muted;
    }

    #history_error {
        height: auto;
        padding: 0 1;
        color: $warning;
    }

    .hidden {
        display: none;
    }
    """

    BINDINGS = [
        Binding("escape", "back", "Back", show=True),
        Binding("up", "cursor_up", "Up", show=True),
        Binding("down", "cursor_down", "Down", show=True),
        Binding("enter", "open_session", "Open", show=True),
        Binding("r", "reload", "Reload", show=True),
    ]

    def __init__(
        self,
        *,
        fetch: FetchHistory,
        open_session: OpenSession | None = None,
    ) -> None:
        super().__init__()
        self._fetch = fetch
        self._open_session = open_session
        self.rows: list[HistoryRow] = []
        self.visible_run_order: list[str] = []
        self._last_error: str | None = None

    # ----- Textual lifecycle --------------------------------------------

    def compose(self) -> ComposeResult:
        yield Static(
            "history — finished runs (enter=open, r=reload, esc=back)",
            id="history_header",
        )
        yield DataTable(id="history_rows")
        yield Static("", id="history_empty", classes="hidden")
        yield Static("", id="history_error", classes="hidden")

    def on_mount(self) -> None:
        table = self.query_one(DataTable)
        table.cursor_type = "row"
        table.zebra_stripes = True
        for key, label in _HISTORY_COLUMNS:
            table.add_column(label, key=key)
        table.focus()
        self.reload_now()

    # ----- Fetch + render -------------------------------------------------

    def reload_now(self) -> None:
        """Fetch one history snapshot and re-render the table.

        A fetch failure keeps the previous rows on screen and surfaces
        the error inline — same containment shape as the dashboard's
        poll boundary.
        """
        try:
            self.rows = self._fetch()
            self._last_error = None
        except Exception as exc:  # noqa: BLE001 - boundary against store errors
            self._last_error = f"history read failed: {exc}"
        self._render_history()

    def _render_history(self) -> None:
        error = self.query_one("#history_error", Static)
        if self._last_error:
            error.update(self._last_error)
            error.remove_class("hidden")
        else:
            error.update("")
            error.add_class("hidden")

        empty = self.query_one("#history_empty", Static)
        if not self.rows:
            empty.update("(no finished runs)")
            empty.remove_class("hidden")
        else:
            empty.update("")
            empty.add_class("hidden")

        table = self.query_one(DataTable)
        table.clear()
        self.visible_run_order = []
        for row in self.rows:
            cells = _row_to_cells(row)
            style = _STATUS_STYLES.get(cells[2])
            rendered: tuple[str | Text, ...] = cells
            if style is not None:
                rendered = (
                    cells[0],
                    cells[1],
                    Text(cells[2], style=style),
                    *cells[3:],
                )
            table.add_row(*rendered, key=row.latest.run_id)
            self.visible_run_order.append(row.latest.run_id)

    # ----- Bindings -------------------------------------------------------

    def action_back(self) -> None:
        self.app.pop_screen()

    def action_reload(self) -> None:
        self.reload_now()

    def action_cursor_up(self) -> None:
        self.query_one(DataTable).action_cursor_up()

    def action_cursor_down(self) -> None:
        self.query_one(DataTable).action_cursor_down()

    def on_data_table_row_selected(
        self, event: DataTable.RowSelected
    ) -> None:
        """DataTable consumes ``enter`` and emits RowSelected; dispatch here
        (mirrors the dashboard's handler)."""
        del event  # selection read from visible_run_order for stability
        self.action_open_session()

    def action_open_session(self) -> None:
        """Push the session screen for the selected finished run.

        No-op when sessions are not wired or the table is empty. The
        session screen renders a completed run fine: its transcript file
        persists under ``logs/runs/`` and the status banner pins the
        terminal state.
        """
        if self._open_session is None:
            return
        run_id = self._selected_run_id()
        if run_id is None:
            return
        task_id = next(
            (
                row.task_id
                for row in self.rows
                if row.latest.run_id == run_id
            ),
            None,
        )
        if task_id is None:
            return
        screen = self._open_session(run_id, task_id)
        if screen is None:
            return
        self.app.push_screen(screen)

    def _selected_run_id(self) -> str | None:
        table = self.query_one(DataTable)
        row_index = table.cursor_row
        if row_index is None or row_index < 0:
            return None
        if row_index >= len(self.visible_run_order):
            return None
        return self.visible_run_order[row_index]


__all__ = ["FetchHistory", "HistoryScreen", "OpenSession"]

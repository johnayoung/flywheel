"""Textual app that renders the realtime in-flight-run dashboard.

The app polls a supplied data source (~1s in production, deterministic
in tests) and renders one :class:`DashboardSnapshot` per tick. Rows that
disappear from the live set linger dimmed for ``linger_seconds`` before
dropping (FR-7 dashboard half); a polling failure leaves the last good
frame on screen with a status-bar warning rather than crashing the app.
"""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass, field
from datetime import datetime, timezone

from rich.text import Text
from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.widgets import DataTable, Static

from flywheel_tui._snapshot import DashboardSnapshot, RowSnapshot, SummaryData

# How long a row that just left the active set stays dimmed on screen.
DEFAULT_LINGER_SECONDS: int = 30

# How often the dashboard re-polls the store. ~1s matches the spec; tests
# override to a very small interval (or drive ticks manually).
DEFAULT_POLL_INTERVAL_SECONDS: float = 1.0

_HELP_LINES: tuple[str, ...] = (
    "key bindings",
    "  up/down  move row selection",
    "  q        quit",
    "  ?        toggle this help footer",
)

_HELP_TEXT: str = "\n".join(_HELP_LINES)

_TABLE_COLUMNS: tuple[str, ...] = (
    "task_id",
    "status",
    "pos",
    "age",
    "tokens",
    "cost",
    "last_action",
)


@dataclass
class _RowMemo:
    """Per-run state carried across polls.

    ``last_active_at`` is the wall clock of the last poll that observed
    the run in :func:`collect_live_rows`. When ``active`` flips to
    ``False`` the row is rendered dimmed; once
    ``now - last_active_at >= linger_seconds`` the entry is dropped.
    """

    snapshot: RowSnapshot
    last_active_at: datetime
    active: bool = True


class DashboardApp(App[int]):
    """Realtime in-flight-run dashboard.

    The constructor takes a ``poll`` callable rather than a store handle
    so tests can drive deterministic frames without touching SQLite. The
    polling loop is owned by Textual's ``set_interval``; a poll that
    raises is contained — the last good frame stays on screen, the
    status bar carries the error, and the next tick retries.
    """

    CSS = """
    Screen {
        layout: vertical;
    }

    #summary {
        height: auto;
        padding: 0 1;
    }

    #status_bar {
        height: auto;
        padding: 0 1;
        color: $warning;
    }

    #empty_state {
        height: auto;
        padding: 0 1;
        color: $text-muted;
    }

    #help_footer {
        height: auto;
        padding: 0 1;
        background: $boost;
    }

    .hidden {
        display: none;
    }
    """

    BINDINGS = [
        Binding("up", "cursor_up", "Up", show=True),
        Binding("down", "cursor_down", "Down", show=True),
        Binding("q", "quit", "Quit", show=True),
        Binding("question_mark", "toggle_help", "Help", show=True),
    ]

    def __init__(
        self,
        poll: Callable[[], DashboardSnapshot],
        *,
        poll_interval_seconds: float = DEFAULT_POLL_INTERVAL_SECONDS,
        linger_seconds: int = DEFAULT_LINGER_SECONDS,
        clock: Callable[[], datetime] | None = None,
    ) -> None:
        super().__init__()
        self._poll = poll
        self._poll_interval_seconds = poll_interval_seconds
        self._linger_seconds = linger_seconds
        self._clock = clock or _utcnow
        self._memo: dict[str, _RowMemo] = {}
        self._last_snapshot: DashboardSnapshot | None = None
        self._last_error: str | None = None
        # Exposed for tests so they can assert what the table is showing
        # without poking widget internals.
        self._visible_run_order: list[str] = []
        self._help_visible: bool = False

    # ----- Textual lifecycle -------------------------------------------------

    def compose(self) -> ComposeResult:
        yield Static("", id="summary")
        yield DataTable(id="rows")
        yield Static("", id="empty_state", classes="hidden")
        yield Static("", id="status_bar", classes="hidden")
        yield Static(_HELP_TEXT, id="help_footer", classes="hidden")

    def on_mount(self) -> None:
        table = self.query_one(DataTable)
        table.cursor_type = "row"
        table.zebra_stripes = True
        for column in _TABLE_COLUMNS:
            table.add_column(column, key=column)
        self.refresh_now()
        self.set_interval(self._poll_interval_seconds, self.refresh_now)

    # ----- Polling + render --------------------------------------------------

    def refresh_now(self) -> None:
        """Pull one snapshot and re-render.

        Exposed (rather than the internal handler name) so Pilot tests
        can synchronously drive a tick instead of waiting on the timer.
        """
        try:
            snapshot = self._poll()
            self._last_snapshot = snapshot
            self._last_error = None
        except Exception as exc:  # noqa: BLE001 — boundary against arbitrary store errors
            self._last_error = f"store read failed: {exc}"
            snapshot = self._last_snapshot
        if snapshot is None:
            snapshot = DashboardSnapshot(
                summary=SummaryData(
                    active_workers=0,
                    task_counts={},
                    tokens_total=0,
                    cost_usd_total=0.0,
                    runtime_seconds=0,
                ),
                rows=(),
            )
        self._render(snapshot)

    def _render(self, snapshot: DashboardSnapshot) -> None:
        now = self._clock()
        active_ids = {r.run_id for r in snapshot.rows}

        # Update memos for every freshly-active row; mark previously-known
        # but absent rows as departing.
        for row in snapshot.rows:
            self._memo[row.run_id] = _RowMemo(
                snapshot=row, last_active_at=now, active=True
            )

        to_drop: list[str] = []
        for run_id, memo in self._memo.items():
            if run_id in active_ids:
                continue
            memo.active = False
            if (now - memo.last_active_at).total_seconds() >= self._linger_seconds:
                to_drop.append(run_id)
        for run_id in to_drop:
            del self._memo[run_id]

        # Summary header.
        self.query_one("#summary", Static).update(
            _format_summary(snapshot.summary)
        )

        # Empty-state line (only when nothing is active and no lingerer is
        # being displayed); keep the table visible so a freshly-arrived
        # run lands without flicker.
        empty = self.query_one("#empty_state", Static)
        if not self._memo:
            empty.update("(no in-flight runs)")
            empty.remove_class("hidden")
        else:
            empty.update("")
            empty.add_class("hidden")

        # Status bar: shown only while an error is currently outstanding;
        # last good frame remains on display.
        status = self.query_one("#status_bar", Static)
        if self._last_error:
            status.update(self._last_error)
            status.remove_class("hidden")
        else:
            status.update("")
            status.add_class("hidden")

        # Rows: clear and re-add in task_id order so layout is stable.
        table = self.query_one(DataTable)
        table.clear()
        ordered = sorted(
            self._memo.items(),
            key=lambda kv: (kv[1].snapshot.task_id, kv[0]),
        )
        self._visible_run_order = []
        for run_id, memo in ordered:
            cells = _row_to_cells(memo.snapshot)
            if memo.active:
                table.add_row(*cells, key=run_id)
            else:
                dimmed = tuple(Text(c, style="dim") for c in cells)
                table.add_row(*dimmed, key=run_id)
            self._visible_run_order.append(run_id)

    # ----- Bindings ----------------------------------------------------------

    def action_cursor_up(self) -> None:
        self.query_one(DataTable).action_cursor_up()

    def action_cursor_down(self) -> None:
        self.query_one(DataTable).action_cursor_down()

    def action_toggle_help(self) -> None:
        widget = self.query_one("#help_footer", Static)
        if widget.has_class("hidden"):
            widget.remove_class("hidden")
            self._help_visible = True
        else:
            widget.add_class("hidden")
            self._help_visible = False


def _utcnow() -> datetime:
    return datetime.now(timezone.utc)


def _format_summary(summary: SummaryData) -> str:
    """Render the summary header — single line so it stays readable on
    narrow terminals; counts roll up to match
    ``flywheel-orchestrate status`` aggregated by state."""
    counts = summary.task_counts
    queued = counts.get("fresh", 0)
    done = counts.get("done", 0)
    failed = counts.get("retryable", 0)
    runtime = _format_duration(summary.runtime_seconds)
    return (
        f"active={summary.active_workers}  "
        f"queued={queued}  done={done}  failed={failed}  "
        f"tokens={summary.tokens_total}  "
        f"cost=${summary.cost_usd_total:.4f}  "
        f"runtime={runtime}"
    )


def _format_duration(seconds: int) -> str:
    if seconds < 0:
        seconds = 0
    hours, rem = divmod(seconds, 3600)
    minutes, secs = divmod(rem, 60)
    if hours:
        return f"{hours}h{minutes:02d}m{secs:02d}s"
    if minutes:
        return f"{minutes}m{secs:02d}s"
    return f"{secs}s"


def _row_to_cells(row: RowSnapshot) -> tuple[str, str, str, str, str, str, str]:
    attempt = f"attempt={row.attempt}" if row.attempt is not None else "attempt=?"
    iteration = (
        f"iter={row.iteration}" if row.iteration is not None else "iter=?"
    )
    pos = f"{attempt} {iteration}"
    age = "—" if row.age_seconds is None else f"{row.age_seconds}s"
    cost = f"${row.cost_usd:.4f}"
    detail = row.last_detail
    action = f"{row.last_kind} {detail}".strip()
    return (
        row.task_id,
        row.status,
        pos,
        age,
        str(row.tokens),
        cost,
        action,
    )

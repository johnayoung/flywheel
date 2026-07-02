"""Textual app that renders the realtime in-flight-run dashboard.

The app polls a supplied data source (~1s in production, deterministic
in tests) and renders one :class:`DashboardSnapshot` per tick. Rows that
disappear from the live set linger dimmed for ``linger_seconds`` before
dropping (FR-7 dashboard half); a polling failure leaves the last good
frame on screen with a status-bar warning rather than crashing the app.

A persistent input bar at the bottom carries plain-text filtering and
the shared :mod:`flywheel._slash` vocabulary (``/help``, ``/status``,
``/approve``, ``/reject [feedback]``, ``/interrupt``, ``/archive``,
``/quit``). The input bar defers to the table for arrows/Enter/Escape/q/?
when unfocused so the existing keyboard navigation survives unchanged;
focus is moved to the input on demand via ``ctrl+i``.
"""

from __future__ import annotations

from collections.abc import Callable, Mapping
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import Any

from rich.text import Text
from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.widgets import DataTable, Input, Static

from flywheel_core.invoker_client import (
    CONTROL_COMMAND_APPROVE,
    CONTROL_COMMAND_INTERRUPT,
    CONTROL_COMMAND_REJECT,
)
from flywheel_core.lifecycle import Status

from flywheel._quit_prompt import (
    QUIT_DETACH,
    QUIT_STOP,
    QuitPromptScreen,
)
from flywheel._history_screen import HistoryScreen
from flywheel._session_screen import ArchiveAction, SessionScreen
from flywheel._slash import (
    HELP_TEXT,
    SLASH_APPROVE,
    SLASH_ARCHIVE,
    SLASH_AUTOPILOT,
    SLASH_EXIT,
    SLASH_HELP,
    SLASH_HISTORY,
    SLASH_INTERRUPT,
    SLASH_QUIT,
    SLASH_REJECT,
    SLASH_STATUS,
    SLASH_WORKER,
    is_slash,
    parse_slash,
    unknown_command_notice,
)
from flywheel._snapshot import DashboardSnapshot, RowSnapshot, SummaryData
from flywheel._autopilot_supervisor import AutopilotState, AutopilotStatus
from flywheel_orchestrator._autopilot_activity import (
    PHASE_IDLE,
    PHASE_RUNNING,
    PHASE_STARTING,
    AutopilotActivity,
)
from flywheel._worker_supervisor import WorkerState, WorkerStatus

# How long a row that just left the active set stays dimmed on screen.
DEFAULT_LINGER_SECONDS: int = 30

# How often the dashboard re-polls the store. ~1s matches the spec; tests
# override to a very small interval (or drive ticks manually).
DEFAULT_POLL_INTERVAL_SECONDS: float = 1.0

_HELP_LINES: tuple[str, ...] = (
    "key bindings",
    "  up/down     move row selection",
    "  enter       open session view for the selected run",
    "  escape      close the session view (back to dashboard)",
    "  h           open the finished-run history view",
    "  ctrl+i      focus the input bar (filter + slash commands)",
    "  q / ctrl+c  quit",
    "  ?           toggle this help footer",
    "",
    "input bar: plain text filters rows; /help lists slash commands.",
)

_HELP_TEXT: str = "\n".join(_HELP_LINES)

# ``(key, label)`` pairs: keys are the stable cell identifiers (the
# filter haystack and tests address cells by position), labels are what
# the header row shows.
_TABLE_COLUMNS: tuple[tuple[str, str], ...] = (
    ("task_id", "task"),
    ("status", "status"),
    ("pos", "attempt/iter"),
    ("age", "age"),
    ("tokens", "tokens"),
    ("cost", "cost"),
    ("last_action", "last action"),
)

# Status -> cell style for the active rows. Anything unknown renders
# unstyled; departed rows are dimmed wholesale regardless of status.
_STATUS_STYLES: Mapping[str, str] = {
    Status.RUNNING.value: "bold green",
    Status.VALIDATING.value: "bold yellow",
    Status.AWAITING_APPROVAL.value: "bold magenta",
}


# Producer seam for the dashboard's slash commands. Matches the shape
# the session screen uses so the supervising CLI can pass the same
# ``enqueue_command``-bound closure to both.
EnqueueForRun = Callable[[str, str, Mapping[str, Any]], int]
"""``(run_id, kind, payload) -> command_id`` -- store enqueue seam."""


# Producer seams the dashboard threads through to the worker supervisor
# so the supervision module stays a hard dependency of the wiring layer
# only -- the app sees four small callables it can mock in tests
# without dragging in subprocess machinery. Production callers bind
# them to ``WorkerSupervisor`` instance methods; tests pass stubs that
# record the calls and return scripted statuses.
WorkerStatusFn = Callable[[], WorkerStatus]
"""``() -> WorkerStatus`` -- read-only status poll for the status bar."""
WorkerStartFn = Callable[[], WorkerStatus]
"""``() -> WorkerStatus`` -- ``/worker start`` and post-DEAD respawn."""
WorkerStopFn = Callable[[], bool]
"""``() -> bool`` -- ``/worker stop`` and quit-prompt 's' branch."""
WorkerDetachFn = Callable[[], None]
"""``() -> None`` -- quit-prompt Enter branch (idempotent forget)."""

# The autopilot supervisor's seams, mirroring the worker's. Autopilot is an
# independent supervised child (decision D-6); the console shows its status and
# drives ``/autopilot start|stop`` through these callables, bound to an
# ``AutopilotSupervisor`` in production and stubbed in tests.
AutopilotStatusFn = Callable[[], AutopilotStatus]
"""``() -> AutopilotStatus`` -- read-only status poll for the status surface."""
AutopilotStartFn = Callable[[], AutopilotStatus]
"""``() -> AutopilotStatus`` -- ``/autopilot start``."""
AutopilotStopFn = Callable[[], bool]
"""``() -> bool`` -- ``/autopilot stop``."""
AutopilotDetachFn = Callable[[], None]
"""``() -> None`` -- console-exit detach (idempotent forget)."""


# Statuses against which the in-process watcher will apply ``interrupt``.
# Matches :mod:`flywheel._session_screen` so the dashboard never
# enqueues a command the session would have refused.
_STEERABLE_STATUSES: frozenset[Status] = frozenset(
    {Status.RUNNING, Status.VALIDATING}
)
_APPROVABLE_STATUSES: frozenset[Status] = frozenset(
    {Status.AWAITING_APPROVAL}
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
    raises is contained -- the last good frame stays on screen, the
    status bar carries the error, and the next tick retries.

    The optional ``enqueue`` / ``archive`` seams are what the slash-
    command vocabulary touches the store through; passing ``None`` makes
    the corresponding verbs degrade to an inline "not wired" notice so
    snapshot-only tests do not have to stand a store up.
    """

    CSS = """
    Screen {
        layout: vertical;
    }

    #summary {
        height: auto;
        padding: 0 1;
        background: $boost;
    }

    #worker_bar {
        height: auto;
        padding: 0 1;
        background: $boost;
    }

    #rows {
        height: 1fr;
        scrollbar-gutter: stable;
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

    #dashboard_notice {
        height: auto;
        padding: 0 1;
        color: $warning;
    }

    #dashboard_input {
        height: auto;
        border: tall $primary 60%;
    }

    #dashboard_input:focus {
        border: tall $accent;
    }

    .hidden {
        display: none;
    }
    """

    BINDINGS = [
        Binding("up", "cursor_up", "Up", show=True),
        Binding("down", "cursor_down", "Down", show=True),
        # ``enter`` is documented for the help footer; the actual
        # dispatch lives in :meth:`on_data_table_row_selected` because
        # the DataTable widget already owns ``enter`` and fires a
        # ``RowSelected`` message we listen for there. Non-priority
        # so the supervised-quit prompt's own ``enter`` binding (on
        # top of the screen stack) is the one that fires when the
        # modal is active -- a priority Enter on the App would
        # pre-empt the prompt and silently no-op the detach choice.
        Binding("enter", "open_session", "Open", show=True),
        Binding("q", "quit", "Quit", show=True),
        # Terminal muscle memory: ctrl+c routes through the same
        # supervised-quit prompt as ``q``. Textual's default binds
        # ctrl+c to a "press ctrl+q to quit" hint, but ctrl+q is
        # swallowed by VS Code's terminal, leaving no way out.
        # ``priority=True`` so it also fires while the input bar has
        # focus.
        Binding("ctrl+c", "quit", "Quit", show=False, priority=True),
        Binding("question_mark", "toggle_help", "Help", show=True),
        # Finished-run history. Non-priority so typing ``h`` into the
        # focused input bar stays a filter keystroke; with the table
        # focused (the default) the binding fires.
        Binding("h", "open_history", "History", show=True),
        # Focus shortcut for the persistent input bar. ``priority=True``
        # so the chord wins even when focus has bounced to a child
        # widget; the input itself does not consume ``ctrl+i`` so
        # toggling back out by re-focusing the table also works.
        Binding(
            "ctrl+i", "focus_input", "Input", show=True, priority=True
        ),
    ]

    def __init__(
        self,
        poll: Callable[[], DashboardSnapshot],
        *,
        poll_interval_seconds: float = DEFAULT_POLL_INTERVAL_SECONDS,
        linger_seconds: int = DEFAULT_LINGER_SECONDS,
        clock: Callable[[], datetime] | None = None,
        open_session: Callable[[str, str], SessionScreen | None] | None = None,
        open_history: Callable[[], HistoryScreen | None] | None = None,
        enqueue: EnqueueForRun | None = None,
        archive: ArchiveAction | None = None,
        worker_status: WorkerStatusFn | None = None,
        worker_start: WorkerStartFn | None = None,
        worker_stop: WorkerStopFn | None = None,
        worker_detach: WorkerDetachFn | None = None,
        autopilot_status: AutopilotStatusFn | None = None,
        autopilot_start: AutopilotStartFn | None = None,
        autopilot_stop: AutopilotStopFn | None = None,
        autopilot_detach: AutopilotDetachFn | None = None,
    ) -> None:
        super().__init__()
        self._poll = poll
        self._poll_interval_seconds = poll_interval_seconds
        self._linger_seconds = linger_seconds
        self._clock = clock or _utcnow
        # Factory the CLI threads through so the dashboard can construct
        # a session screen for the selected run without owning the store
        # handle. ``None`` (the default) disables the Enter binding --
        # useful for the existing snapshot-only Pilot tests so they do
        # not need to seed a transcript every time.
        self._open_session = open_session
        # Factory for the finished-run history screen (``h`` /
        # ``/history``). ``None`` disables the binding -- the same
        # degrade shape as ``open_session``.
        self._open_history = open_history
        self._enqueue = enqueue
        self._archive = archive
        # Worker supervision seams. ``None`` (the default) disables the
        # worker status bar and degrades ``/worker`` to a "not wired"
        # notice -- the same shape the dashboard's existing ``enqueue``
        # / ``archive`` seams take so snapshot-only tests stay light.
        self._worker_status = worker_status
        self._worker_start = worker_start
        self._worker_stop = worker_stop
        self._worker_detach = worker_detach
        # Autopilot supervision seams, the independent second supervised child
        # (decision D-6). Same optional shape as the worker seams: ``None``
        # degrades ``/autopilot`` to a "not wired" notice.
        self._autopilot_status = autopilot_status
        self._autopilot_start = autopilot_start
        self._autopilot_stop = autopilot_stop
        self._autopilot_detach = autopilot_detach
        # Latest worker status snapshot; cached so the status bar can
        # re-render between polls without re-querying the supervisor.
        self._last_worker_status: WorkerStatus | None = None
        self._last_autopilot_status: AutopilotStatus | None = None
        self._memo: dict[str, _RowMemo] = {}
        self._last_snapshot: DashboardSnapshot | None = None
        self._last_error: str | None = None
        # Exposed for tests so they can assert what the table is showing
        # without poking widget internals.
        self._visible_run_order: list[str] = []
        self._help_visible: bool = False
        # Input bar state: ``_filter`` is the active row-filter string,
        # ``_notice`` carries one-shot slash-command output (``/help``,
        # unknown command, archive summary).
        self._filter: str = ""
        self._notice: str | None = None
        # ``True`` once a quit-prompt modal is on the screen stack so
        # rapid double-q does not stack multiple prompts; cleared in the
        # prompt's dismiss callback.
        self._quit_prompt_active: bool = False

    # ----- Public test seams ---------------------------------------------------

    @property
    def filter_text(self) -> str:
        """The active row-filter substring (lowercased)."""

        return self._filter

    @property
    def notice(self) -> str | None:
        """The current inline notice text, if any."""

        return self._notice

    @property
    def worker_status(self) -> WorkerStatus | None:
        """The most recent worker status snapshot, or ``None`` if not wired."""

        return self._last_worker_status

    # ----- Textual lifecycle -------------------------------------------------

    def compose(self) -> ComposeResult:
        yield Static("", id="summary")
        yield Static("", id="worker_bar", classes="hidden")
        yield DataTable(id="rows")
        yield Static("", id="empty_state", classes="hidden")
        yield Static("", id="status_bar", classes="hidden")
        yield Static("", id="dashboard_notice", classes="hidden")
        yield Static(_HELP_TEXT, id="help_footer", classes="hidden")
        # Persistent input bar. Plain text filters the rows above; a
        # leading ``/`` switches into slash-command mode. Default focus
        # sits on the DataTable (in ``on_mount``) so arrows/Enter/q/?
        # keep their pre-input meaning when the bar is unfocused.
        yield Input(
            placeholder="filter rows, or /help for slash commands",
            id="dashboard_input",
        )

    def on_mount(self) -> None:
        table = self.query_one(DataTable)
        table.cursor_type = "row"
        table.zebra_stripes = True
        for key, label in _TABLE_COLUMNS:
            table.add_column(label, key=key)
        # Pin focus on the table so arrows/Enter/q/? do not get
        # intercepted by the input bar by default.
        table.focus()
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

        # Worker bar -- queried each render so a dying child surfaces
        # within one tick. ``None`` (no supervisor wired) hides the bar
        # entirely so snapshot-only tests are unaffected.
        worker_bar = self.query_one("#worker_bar", Static)
        worker_text: Text | None = None
        if self._worker_status is not None:
            try:
                status = self._worker_status()
            except Exception as exc:  # noqa: BLE001 - boundary against supervisor errors
                status = None
                self._last_error = f"worker status read failed: {exc}"
            if status is not None:
                self._last_worker_status = status
            if self._last_worker_status is not None:
                worker_text = _format_worker_status(self._last_worker_status)

        # Autopilot is an independent supervised child shown on the same bar
        # (decision D-6): the console surfaces its live/none status the way it
        # shows the worker's.
        autopilot_text: Text | None = None
        if self._autopilot_status is not None:
            try:
                ap_status = self._autopilot_status()
            except Exception as exc:  # noqa: BLE001 - boundary against supervisor errors
                ap_status = None
                self._last_error = f"autopilot status read failed: {exc}"
            if ap_status is not None:
                self._last_autopilot_status = ap_status
            if self._last_autopilot_status is not None:
                autopilot_text = _format_autopilot_status(
                    self._last_autopilot_status
                )

        parts = [t for t in (worker_text, autopilot_text) if t is not None]
        if parts:
            combined = parts[0]
            for extra in parts[1:]:
                combined = combined + Text("   ") + extra
            worker_bar.update(combined)
            worker_bar.remove_class("hidden")
        else:
            worker_bar.update("")
            worker_bar.add_class("hidden")

        # Compute the visible ordering after the filter is applied so
        # empty-state / row rendering / cursor-bound calculations all
        # consult one canonical list.
        ordered_items = sorted(
            self._memo.items(),
            key=lambda kv: (kv[1].snapshot.task_id, kv[0]),
        )
        filtered_items = [
            (run_id, memo)
            for run_id, memo in ordered_items
            if self._row_matches_filter(memo.snapshot)
        ]

        # Empty-state line: distinguish "no in-flight runs at all" from
        # "filter excludes everything" so the operator can tell which
        # one they hit (Edge Cases row "filter text matching zero rows").
        empty = self.query_one("#empty_state", Static)
        if not self._memo:
            empty.update("(no in-flight runs)")
            empty.remove_class("hidden")
        elif not filtered_items:
            empty.update(f"(no rows match filter {self._filter!r})")
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

        # Inline notice (slash-command output / unknown command / etc).
        notice_widget = self.query_one("#dashboard_notice", Static)
        if self._notice:
            notice_widget.update(self._notice)
            notice_widget.remove_class("hidden")
        else:
            notice_widget.update("")
            notice_widget.add_class("hidden")

        # Rows: clear and re-add in task_id order so layout is stable.
        # Filtered-out runs are simply omitted (their memos persist so a
        # filter clear restores them without re-poll).
        #
        # ``DataTable.clear()`` resets the cursor to row 0, and this
        # method runs on every poll tick -- so the selection must be
        # captured before the clear and restored after the rebuild, or
        # the operator's cursor snaps back to the first row once a
        # second. Restore targets the previously-selected run id (rows
        # may have shifted), falling back to the old index clamped into
        # the new bounds when that run left the visible set.
        table = self.query_one(DataTable)
        selected_run_id = self._selected_run_id()
        previous_row = table.cursor_row
        table.clear()
        self._visible_run_order = []
        for run_id, memo in filtered_items:
            cells = _row_to_cells(memo.snapshot)
            if memo.active:
                table.add_row(*_style_active_cells(cells), key=run_id)
            else:
                dimmed = tuple(Text(c, style="dim") for c in cells)
                table.add_row(*dimmed, key=run_id)
            self._visible_run_order.append(run_id)
        if self._visible_run_order:
            if selected_run_id in self._visible_run_order:
                target = self._visible_run_order.index(selected_run_id)
            else:
                target = max(
                    0, min(previous_row, len(self._visible_run_order) - 1)
                )
            table.move_cursor(row=target, animate=False)

    def _row_matches_filter(self, row: RowSnapshot) -> bool:
        """Whether ``row`` survives the current plain-text row filter.

        Empty filter (the default) lets every row through. A non-empty
        filter is matched case-insensitively against the cell strings
        the table would render -- the operator sees what they typed
        against the same shape they see on screen.
        """

        if not self._filter:
            return True
        needle = self._filter.lower()
        haystack = " ".join(_row_to_cells(row)).lower()
        return needle in haystack

    # ----- Bindings ----------------------------------------------------------

    def action_cursor_up(self) -> None:
        self.query_one(DataTable).action_cursor_up()

    def action_cursor_down(self) -> None:
        self.query_one(DataTable).action_cursor_down()

    def action_quit(self) -> None:
        """Quit through the supervised-child handoff prompt when applicable.

        Override of :meth:`App.action_quit`. When the supervisor owns
        a live child, push the prompt screen and decide based on the
        operator's choice; otherwise exit immediately. The prompt is
        a one-shot modal -- ``_quit_prompt_active`` guards against a
        rapid double-q stacking two prompts.
        """

        self.request_quit()

    def request_quit(self) -> None:
        """Shared entry point for ``q`` binding, ``/quit`` slash, and SIGINT.

        Routes through the supervised-child prompt when the supervisor
        owns a live child, exits silently otherwise. Public so the TUI
        wrapper can call it on a signal handler without going through
        a keypress.
        """

        if self._quit_prompt_active:
            return
        worker_live = self._worker_is_supervised()
        autopilot_live = self._autopilot_is_supervised()
        if not worker_live and not autopilot_live:
            # No supervised children (or status unreadable) -> exit silently;
            # never block quit on a status hiccup, never kill what we do not own.
            self.exit()
            return
        label = _running_daemons_label(worker_live, autopilot_live)
        self._quit_prompt_active = True
        self.push_screen(QuitPromptScreen(label), self._handle_quit_choice)

    def _worker_is_supervised(self) -> bool:
        if self._worker_status is None:
            return False
        try:
            return self._worker_status().state == WorkerState.SUPERVISED
        except Exception:  # noqa: BLE001 - boundary; treat as not-live
            return False

    def _autopilot_is_supervised(self) -> bool:
        if self._autopilot_status is None:
            return False
        try:
            return self._autopilot_status().state == AutopilotState.SUPERVISED
        except Exception:  # noqa: BLE001 - boundary; treat as not-live
            return False

    def _handle_quit_choice(self, result: str | None) -> None:
        """Apply the operator's quit-prompt choice across BOTH daemons.

        ``Enter`` -> detach every supervised child (they keep running, console
                     exits). ``s`` -> stop every supervised child (SIGTERM +
                     wait), exit. ``Esc`` / ``None`` -> cancel; no exit, no
                     supervisor touched (never silently kill a child).
        """

        self._quit_prompt_active = False
        if result == QUIT_DETACH:
            self._for_each_supervisor(self._worker_detach, self._autopilot_detach)
            self.exit()
            return
        if result == QUIT_STOP:
            self._for_each_supervisor(self._worker_stop, self._autopilot_stop)
            self.exit()
            return
        # QUIT_CANCEL (or unknown) -- stay on the dashboard.

    @staticmethod
    def _for_each_supervisor(*actions: Callable[[], object] | None) -> None:
        """Invoke each non-None supervisor action, containing any error.

        Used by the quit handoff and ``/exit`` to stop (or detach) every
        supervised child; a failure on one never blocks the others or the exit.
        """
        for action in actions:
            if action is None:
                continue
            try:
                action()
            except Exception:  # noqa: BLE001 - boundary against supervisor errors
                pass

    def handle_exit_slash(self) -> None:
        """``/exit``: stop every supervised daemon and exit immediately.

        The decisive "I'm done -- take everything down" verb, distinct from
        ``/quit`` (which prompts detach/stop/cancel). Stops the worker and the
        autopilot daemon (whichever are supervised) and exits with no prompt.
        """
        self._for_each_supervisor(self._worker_stop, self._autopilot_stop)
        self.exit()

    def action_toggle_help(self) -> None:
        widget = self.query_one("#help_footer", Static)
        if widget.has_class("hidden"):
            widget.remove_class("hidden")
            self._help_visible = True
        else:
            widget.add_class("hidden")
            self._help_visible = False

    def action_focus_input(self) -> None:
        """Move keyboard focus to the persistent input bar."""

        self.query_one("#dashboard_input", Input).focus()

    def on_data_table_row_selected(
        self, event: DataTable.RowSelected
    ) -> None:
        """Open the per-run session screen when DataTable emits RowSelected.

        ``DataTable`` consumes ``enter`` itself and surfaces a
        ``RowSelected`` message; the dashboard's ``enter`` binding
        is intentionally non-priority so the supervised-quit prompt's
        own ``enter`` wins while the prompt is on top of the screen
        stack -- this handler is the dispatch site for the normal
        case (no modal active).
        """

        if self._quit_prompt_active:
            return
        del event  # row id is read from ``_visible_run_order`` for stability
        self.action_open_session()

    def action_open_session(self) -> None:
        """Push the session screen for the currently-selected row.

        No-op when no session factory was supplied (snapshot-only Pilot
        tests) or when the cursor sits on an empty table. The selected
        row id is looked up in ``_visible_run_order`` so we never trust
        the table widget's internal index past a refresh. Skips when
        a modal (the quit prompt) is on top so an Enter that should
        confirm the prompt does not also push a session screen.
        """

        # The Enter binding is ``priority=True`` for bottom-bar display,
        # but that priority means it fires even while the quit prompt
        # is the active screen; short-circuit so the prompt's own
        # Enter binding is the one that wins on top of the stack.
        if self._quit_prompt_active:
            return
        if self._open_session is None:
            return
        run_id = self._selected_run_id()
        if run_id is None:
            return
        memo = self._memo.get(run_id)
        if memo is None:
            return
        screen = self._open_session(run_id, memo.snapshot.task_id)
        if screen is None:
            return
        self.push_screen(screen)

    def action_open_history(self) -> None:
        """Push the finished-run history screen (``h`` / ``/history``).

        No-op when no history factory was supplied (snapshot-only Pilot
        tests) or while a modal is on top of the stack.
        """

        if self._quit_prompt_active:
            return
        if self._open_history is None:
            return
        screen = self._open_history()
        if screen is None:
            return
        self.push_screen(screen)

    def _selected_run_id(self) -> str | None:
        """Resolve the currently-selected run id from the visible order.

        Returns ``None`` when the table is empty or the cursor sits past
        the visible window (post-filter row removal). Used by both the
        Enter binding and the slash-command dispatch.
        """

        table = self.query_one(DataTable)
        row_index = table.cursor_row
        if row_index is None or row_index < 0:
            return None
        if row_index >= len(self._visible_run_order):
            return None
        return self._visible_run_order[row_index]

    # ----- Input bar ----------------------------------------------------------

    def on_input_changed(self, event: Input.Changed) -> None:
        """Live-update the row filter as the operator types.

        Slash-command keystrokes do NOT filter (the leading ``/`` is
        the signal that the line is a command, not a query); plain text
        re-renders the table inline so the operator sees results as
        they type.
        """

        if event.input.id != "dashboard_input":
            return
        value = event.value
        if value.startswith("/"):
            # Don't apply the line as a filter while the operator is
            # still typing a slash command; the previous filter (if any)
            # stays in effect until Enter or a non-slash edit.
            return
        self._filter = value
        if self._last_snapshot is not None:
            self._render(self._last_snapshot)

    def on_input_submitted(self, event: Input.Submitted) -> None:
        """Compose-bar submit -- slash command, or set / clear filter."""

        if event.input.id != "dashboard_input":
            return
        raw = event.value
        if is_slash(raw):
            self._handle_slash(raw, event.input)
            return
        # Plain-text Enter just confirms the filter (already applied via
        # ``on_input_changed``); leave the input populated so the
        # operator can edit it without re-typing.
        self._filter = raw
        if self._last_snapshot is not None:
            self._render(self._last_snapshot)

    def _handle_slash(self, raw: str, input_widget: Input) -> None:
        """Route one slash command from the dashboard input bar.

        Commands needing a target run id (``/approve``, ``/reject``,
        ``/interrupt``) read the cursor's selected run; missing or
        invalid targets surface an inline notice and leave the input
        populated for editing (Error Handling row in spec 00021).
        """

        command = parse_slash(raw)
        verb = command.verb
        if not verb:
            self._set_notice(unknown_command_notice(""))
            return
        if verb == SLASH_HELP:
            self._set_notice(HELP_TEXT)
            input_widget.value = ""
            return
        if verb == SLASH_STATUS:
            self._set_notice(self._status_text())
            input_widget.value = ""
            return
        if verb == SLASH_APPROVE:
            self._dispatch_run_command(
                kind=CONTROL_COMMAND_APPROVE,
                payload={},
                permitted=_APPROVABLE_STATUSES,
                input_widget=input_widget,
            )
            return
        if verb == SLASH_REJECT:
            payload: dict[str, Any] = {}
            if command.argument:
                payload["feedback"] = command.argument
            self._dispatch_run_command(
                kind=CONTROL_COMMAND_REJECT,
                payload=payload,
                permitted=_APPROVABLE_STATUSES,
                input_widget=input_widget,
            )
            return
        if verb == SLASH_INTERRUPT:
            self._dispatch_run_command(
                kind=CONTROL_COMMAND_INTERRUPT,
                payload={},
                permitted=_STEERABLE_STATUSES,
                input_widget=input_widget,
            )
            return
        if verb == SLASH_ARCHIVE:
            self._handle_archive()
            input_widget.value = ""
            return
        if verb == SLASH_HISTORY:
            if self._open_history is None:
                self._set_notice("/history is not wired on this screen")
                return
            input_widget.value = ""
            self.action_open_history()
            return
        if verb == SLASH_WORKER:
            notice = self.handle_worker_slash(command.argument)
            if notice:
                self._set_notice(notice)
            input_widget.value = ""
            return
        if verb == SLASH_AUTOPILOT:
            notice = self.handle_autopilot_slash(command.argument)
            if notice:
                self._set_notice(notice)
            input_widget.value = ""
            return
        if verb == SLASH_QUIT:
            input_widget.value = ""
            self.request_quit()
            return
        if verb == SLASH_EXIT:
            input_widget.value = ""
            self.handle_exit_slash()
            return
        # Unknown verb: keep the typed line so the operator can fix
        # the typo without re-entering the rest.
        self._set_notice(unknown_command_notice(verb))

    def _dispatch_run_command(
        self,
        *,
        kind: str,
        payload: Mapping[str, Any],
        permitted: frozenset[Status],
        input_widget: Input,
    ) -> None:
        """Enqueue a control command against the cursor-selected run.

        Inline notices cover the four failure shapes: no enqueue seam,
        nothing selected, a run that left the active set between render
        and submit, and a status that does not permit the command.
        Successful enqueues clear the input so the operator can move on.
        """

        if self._enqueue is None:
            self._set_notice(f"/{kind} is not wired on this screen")
            return
        run_id = self._selected_run_id()
        if run_id is None:
            self._set_notice(f"/{kind}: no run selected")
            return
        memo = self._memo.get(run_id)
        if memo is None:
            self._set_notice(
                f"/{kind}: run {run_id} no longer in active set"
            )
            return
        status_value = memo.snapshot.status
        current_status = _status_from_value(status_value)
        if current_status is None or current_status not in permitted:
            self._set_notice(
                f"run is not steerable for {kind!r} (status={status_value})"
            )
            return
        try:
            self._enqueue(run_id, kind, dict(payload))
        except Exception as exc:  # noqa: BLE001 - boundary against store errors
            self._set_notice(f"/{kind} failed: {exc}")
            return
        self._set_notice(f"/{kind} enqueued for {run_id}")
        input_widget.value = ""

    def _handle_archive(self) -> None:
        """Run ``/archive`` and surface the moved phase list inline."""

        if self._archive is None:
            self._set_notice("/archive is not wired on this screen")
            return
        try:
            moved = self._archive()
        except Exception as exc:  # noqa: BLE001 - boundary against archive errors
            self._set_notice(f"/archive failed: {exc}")
            return
        if not moved:
            self._set_notice("/archive: no phases archived")
        else:
            self._set_notice("/archive: moved " + ", ".join(moved))

    def handle_worker_slash(self, argument: str) -> str:
        """Dispatch ``/worker start`` and ``/worker stop`` against the supervisor.

        Returns the inline notice the caller should surface
        (dashboard's ``#dashboard_notice``, session screen's
        ``#session_notice``). The four observable outcomes per
        sub-verb -- no supervisor wired, unknown sub-verb, and the
        success / failure of the underlying
        :class:`WorkerSupervisor` call -- are all returned through
        the same string so both screens reach the operator with one
        vocabulary.

        Public (no leading underscore) because the
        :class:`SessionScreen` calls it via ``getattr(self.app, ...)``
        when the operator types ``/worker`` on the per-run view.
        """

        sub = argument.strip().lower()
        if sub == "start":
            if self._worker_start is None:
                return "/worker start is not wired on this screen"
            try:
                status = self._worker_start()
            except Exception as exc:  # noqa: BLE001 - boundary against supervisor errors
                return f"/worker start failed: {exc}"
            self._last_worker_status = status
            if status.state == WorkerState.SUPERVISED:
                return f"/worker start: supervised (pid={status.pid})"
            if status.state == WorkerState.DETACHED:
                return (
                    "/worker start: a worker already holds a live lease "
                    "(detached); no spawn"
                )
            if status.state == WorkerState.ERROR:
                return (
                    f"/worker start failed: {status.message or 'unknown'}"
                )
            return f"/worker start: state={status.state.value}"
        if sub == "stop":
            if self._worker_stop is None:
                return "/worker stop is not wired on this screen"
            # Edge case: only signal a child this console owns. A
            # detached or external worker shows an inline notice
            # instead -- the spec calls this out explicitly so the
            # operator cannot accidentally kill someone else's run.
            current = (
                self._worker_status() if self._worker_status is not None else None
            )
            if current is None or current.state != WorkerState.SUPERVISED:
                state_name = current.state.value if current is not None else "unknown"
                return (
                    f"/worker stop: no supervised child to stop "
                    f"(state={state_name})"
                )
            try:
                stopped = self._worker_stop()
            except Exception as exc:  # noqa: BLE001 - boundary against supervisor errors
                return f"/worker stop failed: {exc}"
            if self._worker_status is not None:
                try:
                    self._last_worker_status = self._worker_status()
                except Exception:  # noqa: BLE001 - re-read is opportunistic
                    pass
            if stopped:
                return "/worker stop: worker terminated gracefully"
            return (
                "/worker stop: worker did not exit within the wait window"
            )
        return (
            f"/worker: unknown sub-verb {sub!r}; try 'start' or 'stop'"
        )

    def handle_autopilot_slash(self, argument: str) -> str:
        """Dispatch ``/autopilot start`` and ``/autopilot stop``.

        Mirrors :meth:`handle_worker_slash` against the independent autopilot
        supervisor (decision D-6): ``start`` spawns the neverending autopilot
        daemon as a detached supervised child when none is owned (idempotent
        otherwise), ``stop`` SIGTERMs the supervised child. Returns the inline
        notice the caller surfaces. Public so the session screen can reach it
        the way it reaches ``handle_worker_slash``.
        """
        sub = argument.strip().lower()
        if sub == "start":
            if self._autopilot_start is None:
                return "/autopilot start is not wired on this screen"
            try:
                status = self._autopilot_start()
            except Exception as exc:  # noqa: BLE001 - boundary against supervisor errors
                return f"/autopilot start failed: {exc}"
            self._last_autopilot_status = status
            if status.state == AutopilotState.SUPERVISED:
                return f"/autopilot start: supervised (pid={status.pid})"
            if status.state == AutopilotState.ERROR:
                return (
                    f"/autopilot start failed: {status.message or 'unknown'}"
                )
            return f"/autopilot start: state={status.state.value}"
        if sub == "stop":
            if self._autopilot_stop is None:
                return "/autopilot stop is not wired on this screen"
            current = (
                self._autopilot_status()
                if self._autopilot_status is not None
                else None
            )
            if current is None or current.state != AutopilotState.SUPERVISED:
                state_name = (
                    current.state.value if current is not None else "unknown"
                )
                return (
                    f"/autopilot stop: no supervised autopilot to stop "
                    f"(state={state_name})"
                )
            try:
                stopped = self._autopilot_stop()
            except Exception as exc:  # noqa: BLE001 - boundary against supervisor errors
                return f"/autopilot stop failed: {exc}"
            if self._autopilot_status is not None:
                try:
                    self._last_autopilot_status = self._autopilot_status()
                except Exception:  # noqa: BLE001 - re-read is opportunistic
                    pass
            if stopped:
                return "/autopilot stop: autopilot terminated gracefully"
            return (
                "/autopilot stop: autopilot did not exit within the wait window"
            )
        return (
            f"/autopilot: unknown sub-verb {sub!r}; try 'start' or 'stop'"
        )

    def autopilot_status(self) -> AutopilotStatus | None:
        """The most recent autopilot status snapshot, for the status surface."""
        return self._last_autopilot_status

    def _status_text(self) -> str:
        """Render the selected run's status for ``/status``.

        With no selection the line reports a summary aggregate so the
        operator still gets something useful from the verb on an empty
        cursor.
        """

        run_id = self._selected_run_id()
        if run_id is None or run_id not in self._memo:
            if self._last_snapshot is None:
                return "/status: no snapshot yet"
            summary = self._last_snapshot.summary
            return (
                f"/status: active={summary.active_workers} "
                f"tokens={summary.tokens_total} "
                f"cost=${summary.cost_usd_total:.4f}"
            )
        snapshot = self._memo[run_id].snapshot
        line = (
            f"run {run_id}: task={snapshot.task_id} "
            f"status={snapshot.status}"
        )
        if snapshot.awaiting_instruction:
            line += f"; gate={snapshot.awaiting_instruction}"
        return line

    def _set_notice(self, message: str) -> None:
        """Record an inline notice and re-render so it shows immediately."""

        self._notice = message
        if self._last_snapshot is not None:
            self._render(self._last_snapshot)


def _utcnow() -> datetime:
    return datetime.now(timezone.utc)


def _status_from_value(value: str) -> Status | None:
    """Map a snapshot's ``status`` string back to the ``Status`` enum.

    Returns ``None`` when the value does not correspond to a known
    enum member -- treat unknown statuses as not steerable.
    """

    try:
        return Status(value)
    except ValueError:
        return None


def _format_worker_status(status: WorkerStatus) -> Text:
    """Render one ``worker:`` line for the status bar.

    Follows the spec's status-bar vocabulary verbatim
    (``worker: supervised`` / ``detached`` / ``none`` / ``dead`` /
    ``error: <reason>``) so a docs grep on the labels lands on this
    function. The ``none`` line embeds the ``/worker start`` hint
    explicitly called out in the Error Handling row for
    ``--no-worker`` with nothing live. Returned as a styled
    :class:`rich.text.Text` -- healthy states render calm, dead/error
    render loud.
    """

    state = status.state
    if state == WorkerState.SUPERVISED:
        pid = f" pid={status.pid}" if status.pid is not None else ""
        return Text(f"worker: supervised{pid}", style="green")
    if state == WorkerState.DETACHED:
        return Text(
            "worker: detached (this console did not spawn it)",
            style="yellow",
        )
    if state == WorkerState.NONE:
        return Text(
            "worker: none -- type '/worker start' to spawn one",
            style="dim",
        )
    if state == WorkerState.DEAD:
        detail = f" ({status.message})" if status.message else ""
        return Text(
            f"worker: dead -- type '/worker start' to respawn{detail}",
            style="bold red",
        )
    if state == WorkerState.DEAD_AFTER_BUDGET:
        detail = f" ({status.message})" if status.message else ""
        return Text(
            "worker: dead (crash-loop budget exhausted) -- "
            f"type '/worker start' to re-arm{detail}",
            style="bold red",
        )
    if state == WorkerState.ERROR:
        return Text(
            f"worker: error: {status.message or 'unknown'}",
            style="bold red",
        )
    return Text(f"worker: {state.value}")


def _running_daemons_label(worker_live: bool, autopilot_live: bool) -> str:
    """The quit prompt's first line, naming the supervised daemons at risk."""
    if worker_live and autopilot_live:
        return "supervised worker and autopilot are running"
    if autopilot_live:
        return "supervised autopilot is running"
    return "supervised worker is running"


def _format_autopilot_activity(activity: AutopilotActivity, now: float) -> str:
    """Summarize one activity snapshot for the supervised status line.

    Reads as ``<phase> | last: E emitted, D dropped``. The countdown
    (``next in Xm Ys``) is derived from ``next_cycle_at`` against ``now`` so it
    ticks down each render. The last-cycle summary appears only once a cycle has
    actually completed (idle, or running a cycle past the first).
    """
    parts: list[str] = []
    if activity.phase == PHASE_RUNNING:
        parts.append(f"cycle {activity.cycle_index} running")
    elif activity.phase == PHASE_IDLE:
        if activity.next_cycle_at is not None:
            remaining = _format_duration(int(round(activity.next_cycle_at - now)))
            parts.append(f"idle, next in {remaining}")
        else:
            parts.append("idle")
    elif activity.phase == PHASE_STARTING:
        parts.append("starting")
    else:
        parts.append(activity.phase)

    completed = activity.phase == PHASE_IDLE or (
        activity.phase == PHASE_RUNNING and activity.cycle_index > 1
    )
    if completed:
        parts.append(
            f"last: {activity.last_emitted} emitted, "
            f"{activity.last_dropped} dropped"
        )
    return " | ".join(parts)


def _format_autopilot_status(
    status: AutopilotStatus, now: float | None = None
) -> Text:
    """Render one ``autopilot:`` line for the status bar.

    Mirrors :func:`_format_worker_status`'s vocabulary against the autopilot
    supervisor's states (``supervised`` / ``none`` / ``dead`` / ``error``);
    autopilot has no ``detached`` state (it writes no lease). When the
    supervised daemon has recorded live activity, its per-cycle summary
    (current cycle, last emitted/dropped, time-to-next-cycle) is appended.
    """
    state = status.state
    if state == AutopilotState.SUPERVISED:
        pid = f" pid={status.pid}" if status.pid is not None else ""
        text = f"autopilot: supervised{pid}"
        if status.activity is not None:
            clock = (
                now
                if now is not None
                else datetime.now(timezone.utc).timestamp()
            )
            text += f" -- {_format_autopilot_activity(status.activity, clock)}"
        return Text(text, style="green")
    if state == AutopilotState.NONE:
        return Text(
            "autopilot: none -- type '/autopilot start' to spawn one",
            style="dim",
        )
    if state == AutopilotState.DEAD:
        detail = f" ({status.message})" if status.message else ""
        return Text(
            f"autopilot: dead -- type '/autopilot start' to respawn{detail}",
            style="bold red",
        )
    if state == AutopilotState.DEAD_AFTER_BUDGET:
        detail = f" ({status.message})" if status.message else ""
        return Text(
            "autopilot: dead (crash-loop budget exhausted) -- "
            f"type '/autopilot start' to re-arm{detail}",
            style="bold red",
        )
    if state == AutopilotState.ERROR:
        return Text(
            f"autopilot: error: {status.message or 'unknown'}",
            style="bold red",
        )
    return Text(f"autopilot: {state.value}")


def _format_summary(summary: SummaryData) -> Text:
    """Render the summary header — single line so it stays readable on
    narrow terminals; counts roll up to match ``flywheel status``
    aggregated by state. Zero counts render dim so the populated ones
    carry the eye; ``failed`` goes red the moment it is non-zero."""
    counts = summary.task_counts
    queued = counts.get("fresh", 0)
    done = counts.get("done", 0)
    failed = counts.get("retryable", 0)
    runtime = _format_duration(summary.runtime_seconds)
    text = Text()
    text.append(
        f"active={summary.active_workers}",
        "bold green" if summary.active_workers else "dim",
    )
    text.append("  ")
    text.append(f"queued={queued}", "cyan" if queued else "dim")
    text.append("  ")
    text.append(f"done={done}", "green" if done else "dim")
    text.append("  ")
    text.append(f"failed={failed}", "bold red" if failed else "dim")
    text.append("  ")
    text.append(f"tokens={_format_tokens(summary.tokens_total)}")
    text.append("  ")
    text.append(f"cost={_format_cost(summary.cost_usd_total)}")
    text.append("  ")
    text.append(f"runtime={runtime}")
    return text


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
    attempt = str(row.attempt) if row.attempt is not None else "?"
    iteration = str(row.iteration) if row.iteration is not None else "?"
    pos = f"{attempt}/{iteration}"
    age = (
        "—"
        if row.age_seconds is None
        else _format_duration(row.age_seconds)
    )
    detail = _truncate(row.last_detail, _MAX_DETAIL_WIDTH)
    action = f"{row.last_kind} {detail}".strip()
    return (
        row.task_id,
        row.status,
        pos,
        age,
        _format_tokens(row.tokens),
        _format_cost(row.cost_usd),
        action,
    )


def _style_active_cells(
    cells: tuple[str, str, str, str, str, str, str],
) -> tuple[str | Text, ...]:
    """Apply the status colour to an active row's status cell.

    Styling happens here rather than in :func:`_row_to_cells` so the
    plain-string cells stay the single source for the filter haystack.
    """

    style = _STATUS_STYLES.get(cells[1])
    if style is None:
        return cells
    return (
        cells[0],
        Text(cells[1], style=style),
        *cells[2:],
    )


# Last-action details are agent text/tool summaries that can run long;
# cap the cell so one chatty run does not push every other column off
# a narrow terminal.
_MAX_DETAIL_WIDTH: int = 60


def _truncate(text: str, width: int) -> str:
    if len(text) <= width:
        return text
    return text[: width - 1] + "…"


def _format_tokens(tokens: int) -> str:
    """Humanize token counts: ``980`` / ``56k`` / ``36.0M``."""

    if tokens >= 1_000_000:
        return f"{tokens / 1_000_000:.1f}M"
    if tokens >= 10_000:
        return f"{tokens / 1_000:.0f}k"
    return str(tokens)


def _format_cost(cost_usd: float) -> str:
    """Render dollars: cents-precision once past $1, tenths-of-a-cent
    below it (early-run costs are fractions of a cent)."""

    if cost_usd >= 1:
        return f"${cost_usd:.2f}"
    return f"${cost_usd:.4f}"

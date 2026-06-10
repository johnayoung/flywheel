"""Textual screen that renders the live transcript of one run.

Pressing ``Enter`` on a dashboard row opens this screen. It pulls the
selected run's merged audit stream through a
:class:`flywheel_tui._session.TranscriptTailer`, renders each entry
chat-style, and tail-follows until either the operator scrolls up
(follow pauses, a new-activity indicator appears) or the run reaches a
terminal status (a banner pins the terminal state and the transcript
stays scrollable). ``Escape`` returns to the dashboard with the row
selection preserved.

The screen takes a ``fetch`` callable and a ``status`` callable rather
than a store handle so Pilot tests can drive deterministic frames
without timer races; the production wiring threads
:meth:`TranscriptTailer.fetch` and a ``load_lifecycle``-backed status
poll through these seams.
"""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass

from rich.text import Text
from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import VerticalScroll
from textual.screen import Screen
from textual.widgets import Static

from flywheel.lifecycle import Status
from flywheel_tui._session import (
    EntryKind,
    TranscriptEntry,
    is_terminal,
)


# How often the screen pulls new transcript entries. ~250ms matches the
# audit CLI's default --follow cadence so the session view's responsiveness
# is on par with `python -m flywheel.audit --follow`.
DEFAULT_SESSION_POLL_INTERVAL_SECONDS: float = 0.25


_HEADER_STYLES: dict[EntryKind, str] = {
    EntryKind.AGENT_TEXT: "bold cyan",
    EntryKind.TOOL_CALL: "bold magenta",
    EntryKind.TOOL_RESULT: "magenta",
    EntryKind.USER_TEXT: "bold green",
    EntryKind.OPERATOR_SAY: "bold yellow",
    EntryKind.SYSTEM: "blue",
    EntryKind.RESULT: "blue",
    EntryKind.LIFECYCLE: "dim",
    EntryKind.GATE: "bold red",
}

_BODY_STYLES: dict[EntryKind, str] = {
    EntryKind.AGENT_TEXT: "",
    EntryKind.TOOL_CALL: "",
    EntryKind.TOOL_RESULT: "dim",
    EntryKind.USER_TEXT: "",
    EntryKind.OPERATOR_SAY: "yellow",
    EntryKind.SYSTEM: "dim",
    EntryKind.RESULT: "dim",
    EntryKind.LIFECYCLE: "dim",
    EntryKind.GATE: "red",
}


@dataclass(frozen=True, kw_only=True)
class SessionStatus:
    """Snapshot of the viewed run's lifecycle, fed to the screen each tick.

    The screen never touches the store directly; it polls a caller-
    supplied ``status`` callable that returns one :class:`SessionStatus`
    per call. Consolidating the four observable shapes here (current
    status, terminal flag, awaiting-gate instruction, missing-lifecycle
    sentinel) keeps the screen's render path branch-light.
    """

    status: Status | None
    awaiting_instruction: str | None
    missing: bool = False


class SessionScreen(Screen[None]):
    """Drill-down view of one run's transcript.

    The screen owns three observable bits of state:

    * ``_follow`` -- the tail-follow flag. ``True`` on open and after
      the operator presses ``End``; toggled to ``False`` by ``Page Up``,
      ``Home``, or ``Up`` so a scrolled-up view is sticky and new
      messages do not yank the viewport (FR-4 + edge case).
    * ``_new_activity`` -- whether at least one transcript entry has
      arrived while ``_follow`` is ``False``. Drives the new-activity
      indicator at the bottom of the screen so the operator can see
      that the run is still moving even when scrolled back through
      history.
    * ``_last_status`` -- the most recent :class:`SessionStatus`. Read
      by the banner / gate widgets so a transient store-read failure
      keeps the last good banner on screen rather than blanking it.

    Exposed for tests: ``run_id``, ``entries`` (rendered so far),
    ``_follow``, ``_new_activity`` -- the same shape ``DashboardApp``
    uses for ``_visible_run_order`` so Pilot tests can assert state
    transitions without poking at widget internals.
    """

    CSS = """
    Screen {
        layout: vertical;
    }

    #session_header {
        height: auto;
        padding: 0 1;
    }

    #session_banner {
        height: auto;
        padding: 0 1;
        color: $warning;
    }

    #session_gate {
        height: auto;
        padding: 0 1;
        background: $error;
        color: $text;
    }

    #session_transcript {
        height: 1fr;
        padding: 0 1;
    }

    #session_indicator {
        height: auto;
        padding: 0 1;
        background: $boost;
        color: $accent;
    }

    .hidden {
        display: none;
    }
    """

    BINDINGS = [
        Binding("escape", "back", "Back", show=True),
        Binding("end", "follow_end", "Follow", show=True),
        Binding("home", "scroll_top", "Top", show=True),
        Binding("pageup", "page_up", "Page up", show=True),
        Binding("pagedown", "page_down", "Page down", show=True),
        Binding("up", "scroll_up", "Up", show=True),
        Binding("down", "scroll_down", "Down", show=True),
    ]

    def __init__(
        self,
        *,
        run_id: str,
        task_id: str,
        fetch: Callable[[], list[TranscriptEntry]],
        status: Callable[[], SessionStatus],
        poll_interval_seconds: float = DEFAULT_SESSION_POLL_INTERVAL_SECONDS,
    ) -> None:
        super().__init__()
        self._run_id = run_id
        self._task_id = task_id
        self._fetch = fetch
        self._status = status
        self._poll_interval_seconds = poll_interval_seconds
        # Observable state -- kept on the instance so tests can assert
        # transitions without scraping widget render output.
        self._follow: bool = True
        self._new_activity: bool = False
        self._last_status: SessionStatus | None = None
        self._last_error: str | None = None
        self.entries: list[TranscriptEntry] = []

    # ----- Public test seams --------------------------------------------

    @property
    def run_id(self) -> str:
        """The run this screen is bound to."""

        return self._run_id

    @property
    def task_id(self) -> str:
        """The task id of the viewed run (used by the header)."""

        return self._task_id

    @property
    def follow(self) -> bool:
        """Whether tail-follow is currently active."""

        return self._follow

    @property
    def new_activity(self) -> bool:
        """Whether new entries have arrived while follow was paused."""

        return self._new_activity

    # ----- Textual lifecycle --------------------------------------------

    def compose(self) -> ComposeResult:
        yield Static("", id="session_header")
        yield Static("", id="session_gate", classes="hidden")
        yield Static("", id="session_banner", classes="hidden")
        yield VerticalScroll(id="session_transcript")
        yield Static("", id="session_indicator", classes="hidden")

    def on_mount(self) -> None:
        self._render_header()
        self.refresh_now()
        self.set_interval(self._poll_interval_seconds, self.refresh_now)

    # ----- Polling + render ---------------------------------------------

    def refresh_now(self) -> None:
        """Pull one tailer tick and re-render banner / gate / indicator.

        Exposed (rather than the internal handler name) so Pilot tests
        can synchronously drive a tick without waiting on the timer.
        A failure inside the tailer or status callable is contained:
        the last good frame stays on screen and the banner widget
        carries the error.
        """

        try:
            new_entries = self._fetch()
            self._last_error = None
        except Exception as exc:  # noqa: BLE001 - boundary against store errors
            self._last_error = f"transcript read failed: {exc}"
            new_entries = []
        try:
            status = self._status()
            if self._last_error is None:
                # Only clear the warning when the status call also
                # succeeded; otherwise keep the most descriptive error.
                pass
        except Exception as exc:  # noqa: BLE001 - same boundary as above
            self._last_error = f"status read failed: {exc}"
            status = self._last_status
        if status is not None:
            self._last_status = status
        if new_entries:
            self._append_entries(new_entries)
        self._render_status_widgets()

    def _append_entries(self, new_entries: list[TranscriptEntry]) -> None:
        """Append rendered lines to the transcript and update follow state.

        When follow is active, the new lines are inserted and the
        viewport scrolls to the bottom; when paused, the lines still
        land (so the operator sees them on End-resume) but the
        viewport is left alone and ``_new_activity`` flips so the
        indicator shows.
        """

        transcript = self.query_one("#session_transcript", VerticalScroll)
        for entry in new_entries:
            self.entries.append(entry)
            transcript.mount(_render_entry_widget(entry))
        if self._follow:
            # ``animate=False`` keeps the viewport pinned in tests --
            # ``run_test`` does not advance an animation clock.
            transcript.scroll_end(animate=False)
            self._new_activity = False
        else:
            self._new_activity = True

    def _render_header(self) -> None:
        """One-line header showing run + task id; updated on mount only."""

        self.query_one("#session_header", Static).update(
            f"run={self._run_id}  task={self._task_id}"
        )

    def _render_status_widgets(self) -> None:
        """Refresh banner, gate, and indicator from ``_last_status``."""

        banner = self.query_one("#session_banner", Static)
        gate = self.query_one("#session_gate", Static)
        indicator = self.query_one("#session_indicator", Static)
        status = self._last_status

        # Banner: prefer the read-error message; else terminal state;
        # else a missing-lifecycle notice; else hidden so the live
        # transcript has the full viewport.
        if self._last_error is not None:
            banner.update(self._last_error)
            banner.remove_class("hidden")
        elif status is not None and status.missing:
            banner.update(
                f"run {self._run_id!r} not found in store (was it dropped?)"
            )
            banner.remove_class("hidden")
        elif (
            status is not None
            and status.status is not None
            and is_terminal(status.status)
        ):
            banner.update(
                f"-- terminal status: {status.status.value} -- "
                "transcript scrollable; steering disabled."
            )
            banner.remove_class("hidden")
        else:
            banner.update("")
            banner.add_class("hidden")

        # Gate: surface the awaiting-approval instruction prominently
        # when the run is parked, hidden otherwise (FR-7 / Edge Cases).
        if status is not None and status.awaiting_instruction:
            gate.update(
                "manual gate awaiting approval: "
                + status.awaiting_instruction
            )
            gate.remove_class("hidden")
        else:
            gate.update("")
            gate.add_class("hidden")

        # New-activity indicator: only meaningful while follow is paused.
        if not self._follow and self._new_activity:
            indicator.update(
                "new activity below -- press End to resume follow"
            )
            indicator.remove_class("hidden")
        else:
            indicator.update("")
            indicator.add_class("hidden")

    # ----- Bindings -----------------------------------------------------

    def action_back(self) -> None:
        """Pop this screen so the dashboard regains focus.

        The dashboard's row cursor is intrinsically preserved because
        the dashboard screen instance is never re-composed -- pushing
        and popping a screen on top of it leaves its widgets intact.
        """

        self.app.pop_screen()

    def action_follow_end(self) -> None:
        """Resume tail-follow and snap the viewport to the bottom."""

        self._follow = True
        self._new_activity = False
        transcript = self.query_one("#session_transcript", VerticalScroll)
        transcript.scroll_end(animate=False)
        self._render_status_widgets()

    def action_page_up(self) -> None:
        self._pause_follow()
        self.query_one("#session_transcript", VerticalScroll).action_page_up()

    def action_page_down(self) -> None:
        transcript = self.query_one("#session_transcript", VerticalScroll)
        transcript.action_page_down()

    def action_scroll_up(self) -> None:
        self._pause_follow()
        self.query_one("#session_transcript", VerticalScroll).action_scroll_up()

    def action_scroll_down(self) -> None:
        self.query_one(
            "#session_transcript", VerticalScroll
        ).action_scroll_down()

    def action_scroll_top(self) -> None:
        self._pause_follow()
        transcript = self.query_one("#session_transcript", VerticalScroll)
        transcript.scroll_home(animate=False)

    def _pause_follow(self) -> None:
        """Flip ``_follow`` off without re-rendering the transcript.

        Called from every "scroll up" binding so the operator's intent
        ("hold position, I'm reading") is honoured regardless of which
        key they used.
        """

        if self._follow:
            self._follow = False
            # Re-render so the indicator picks up the state change on
            # the same tick; ``_new_activity`` stays False until the
            # next ``_append_entries`` so the indicator only fires when
            # there is actual new activity to point at.
            self._render_status_widgets()


# --- Rendering helpers -----------------------------------------------------


def _render_entry_widget(entry: TranscriptEntry) -> Static:
    """Build the :class:`Static` widget for one transcript entry.

    Each row is a single :class:`Text` so colour styling (header bold,
    body dimmed for tool results) survives ``run_test`` without needing
    a separate widget tree per entry.
    """

    text = render_entry_text(entry)
    return Static(text, expand=True)


def render_entry_text(entry: TranscriptEntry) -> Text:
    """Render one entry as a ``rich.text.Text`` line.

    Public so tests can assert the rendered shape (header tag, body
    text) without mounting a Textual app. Newlines inside the body
    have already been collapsed by :mod:`flywheel_tui._session`.
    """

    header_style = _HEADER_STYLES.get(entry.kind, "")
    body_style = _BODY_STYLES.get(entry.kind, "")
    text = Text()
    text.append(entry.header, style=header_style)
    if entry.body:
        text.append("  ")
        text.append(entry.body, style=body_style)
    return text


__all__ = [
    "DEFAULT_SESSION_POLL_INTERVAL_SECONDS",
    "SessionScreen",
    "SessionStatus",
    "render_entry_text",
]

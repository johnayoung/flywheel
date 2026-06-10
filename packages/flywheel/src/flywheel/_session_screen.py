"""Textual screen that renders the live transcript of one run.

Pressing ``Enter`` on a dashboard row opens this screen. It pulls the
selected run's merged audit stream through a
:class:`flywheel._session.TranscriptTailer`, renders each entry
chat-style, and tail-follows until either the operator scrolls up
(follow pauses, a new-activity indicator appears) or the run reaches a
terminal status (a banner pins the terminal state and the transcript
stays scrollable). ``Escape`` returns to the dashboard with the row
selection preserved.

The compose box doubles as the persistent input bar: plain text is a
``say`` (the shipped behaviour); a leading ``/`` switches the line into
a slash command from the shared :mod:`flywheel._slash` vocabulary
(``/help``, ``/status``, ``/approve``, ``/reject [feedback]``,
``/interrupt``, ``/archive``, ``/quit``). Slash commands target the
viewed run with the exact store-mediated semantics of their CLI twins.

The screen takes a ``fetch`` callable and a ``status`` callable rather
than a store handle so Pilot tests can drive deterministic frames
without timer races; the production wiring threads
:meth:`TranscriptTailer.fetch` and a ``load_lifecycle``-backed status
poll through these seams.
"""

from __future__ import annotations

from collections.abc import Callable, Mapping
from dataclasses import dataclass
from typing import Any

from rich.text import Text
from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import VerticalScroll
from textual.screen import Screen
from textual.widgets import Input, Static

from flywheel_core.invoker_client import (
    CONTROL_COMMAND_APPROVE,
    CONTROL_COMMAND_INTERRUPT,
    CONTROL_COMMAND_REJECT,
    CONTROL_COMMAND_SAY,
)
from flywheel_core.lifecycle import Status

from flywheel._session import (
    EntryKind,
    TranscriptEntry,
    is_terminal,
)
from flywheel._slash import (
    HELP_TEXT,
    SLASH_APPROVE,
    SLASH_ARCHIVE,
    SLASH_HELP,
    SLASH_INTERRUPT,
    SLASH_QUIT,
    SLASH_REJECT,
    SLASH_STATUS,
    SLASH_WORKER,
    is_slash,
    parse_slash,
    unknown_command_notice,
)


# How often the screen pulls new transcript entries. ~250ms matches the
# audit CLI's default --follow cadence so the session view's responsiveness
# is on par with `python -m flywheel_core.audit --follow`.
DEFAULT_SESSION_POLL_INTERVAL_SECONDS: float = 0.25


# Lifecycle statuses against which the live in-session watcher can apply
# ``say`` / ``interrupt``. Mirrors :func:`flywheel_core.workflow._enqueue_control_command`
# (``RUNNING`` + ``VALIDATING``): outside this set the SDK session is no
# longer being driven so an enqueued command would sit pending forever.
_STEERABLE_STATUSES: frozenset[Status] = frozenset(
    {Status.RUNNING, Status.VALIDATING}
)

# Lifecycle status at which a parked manual gate accepts approve/reject.
# Single-element set kept as a frozenset so the steerability check shares
# the same "is in set" shape as ``_STEERABLE_STATUSES``.
_APPROVABLE_STATUSES: frozenset[Status] = frozenset(
    {Status.AWAITING_APPROVAL}
)


# Per-command kind a session-screen submit produces. Used both to build the
# ``control_commands`` row and to drive the pending-list rendering.
EnqueueCommand = Callable[[str, Mapping[str, Any]], int]
"""Producer seam: takes (kind, payload), returns the store-assigned command id.

Production callers wire this to a closure over ``ControlCommandStore.enqueue_command``
that fills in the run_id and a fresh timestamp; tests pass a stub that
records the call and returns a monotonically-increasing fake id.
"""


# Side-effect seam for ``/archive``: returns the list of moved phase
# directories so the screen can summarise the outcome inline. Kept as a
# callable so tests can stub it without standing up a directory layout.
ArchiveAction = Callable[[], list[str]]


@dataclass
class _PendingCommand:
    """One outstanding operator-issued command awaiting watcher feedback.

    ``status`` is one of ``"pending"`` / ``"failed"`` -- an applied command
    is removed from the pending list outright because the matching
    transcript entry (OPERATOR_SAY for ``say``, control / gate(...) lines
    for the rest) already announces the apply in the transcript. A failed
    command stays visible so the operator can see the error_detail inline.
    """

    command_id: int
    kind: str
    summary: str
    status: str = "pending"
    error_detail: str | None = None


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

    The screen owns four observable bits of state:

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
    * ``_notice`` -- the inline operator notice line. Populated when
      steering is refused (run no longer steerable, store raise),
      ``/help`` / ``/status`` print a one-shot message, or an unknown
      slash command is typed. The compose input is preserved on the
      unknown-command branch so the operator can edit and resubmit
      (Error Handling row in spec 00021).

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

    #session_pending {
        height: auto;
        padding: 0 1;
        color: $accent;
    }

    #session_notice {
        height: auto;
        padding: 0 1;
        color: $warning;
    }

    #session_steering_help {
        height: auto;
        padding: 0 1;
        color: $text-muted;
    }

    #session_compose {
        height: auto;
        padding: 0 1;
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
        # Steering bindings. ``priority=True`` so they still fire when
        # focus sits inside the compose ``Input`` widget. Bare letter
        # keys would clash with typing in the compose box, so each
        # verb is on a ctrl-chord that the Input does not consume.
        Binding(
            "ctrl+x", "interrupt", "Interrupt", show=True, priority=True
        ),
        Binding("ctrl+y", "approve", "Approve", show=True, priority=True),
        Binding("ctrl+r", "reject", "Reject", show=True, priority=True),
    ]

    def __init__(
        self,
        *,
        run_id: str,
        task_id: str,
        fetch: Callable[[], list[TranscriptEntry]],
        status: Callable[[], SessionStatus],
        poll_interval_seconds: float = DEFAULT_SESSION_POLL_INTERVAL_SECONDS,
        enqueue: EnqueueCommand | None = None,
        archive: ArchiveAction | None = None,
    ) -> None:
        super().__init__()
        self._run_id = run_id
        self._task_id = task_id
        self._fetch = fetch
        self._status = status
        self._poll_interval_seconds = poll_interval_seconds
        self._enqueue = enqueue
        self._archive = archive
        # Observable state -- kept on the instance so tests can assert
        # transitions without scraping widget render output.
        self._follow: bool = True
        self._new_activity: bool = False
        self._last_status: SessionStatus | None = None
        self._last_error: str | None = None
        self.entries: list[TranscriptEntry] = []
        # Steering state -- ordered by enqueue id so the pending list
        # always renders in submit order, matching the watcher's claim
        # order.
        self.pending_commands: dict[int, _PendingCommand] = {}
        self._notice: str | None = None

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

    @property
    def notice(self) -> str | None:
        """The current inline notice text, if any (Pilot-test seam)."""

        return self._notice

    # ----- Textual lifecycle --------------------------------------------

    def compose(self) -> ComposeResult:
        yield Static("", id="session_header")
        yield Static("", id="session_gate", classes="hidden")
        yield Static("", id="session_banner", classes="hidden")
        yield VerticalScroll(id="session_transcript")
        yield Static("", id="session_indicator", classes="hidden")
        yield Static("", id="session_pending", classes="hidden")
        yield Static("", id="session_notice", classes="hidden")
        yield Static("", id="session_steering_help", classes="hidden")
        # The compose box doubles as the persistent input bar: plain
        # text is a ``say``, a leading ``/`` switches into slash-command
        # mode, Ctrl+R submits the current value as a reject's optional
        # feedback. The placeholder advertises the primary actions plus
        # the slash escape so the operator can discover the vocabulary
        # without leaving the screen.
        yield Input(
            placeholder=(
                "say message or /help (enter=say, ctrl+x=interrupt, "
                "ctrl+y=approve, ctrl+r=reject)"
            ),
            id="session_compose",
        )

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
            self._reconcile_pending(entry)
        if self._follow:
            # ``animate=False`` keeps the viewport pinned in tests --
            # ``run_test`` does not advance an animation clock.
            transcript.scroll_end(animate=False)
            self._new_activity = False
        else:
            self._new_activity = True

    def _reconcile_pending(self, entry: TranscriptEntry) -> None:
        """Flip a pending steering command to applied / failed.

        Match is by ``control_command_id`` on the transcript entry --
        the classifier exposes that field on
        ``harness.control_command_applied`` and
        ``harness.control_command_failed`` events only. A command
        enqueued by another producer (CLI) lands as an entry too but
        carries an id this screen never enqueued, so it slides past
        without disturbing the local pending state.

        Applied commands are dropped from the pending list because the
        same record produces a transcript entry (OPERATOR_SAY for
        ``say``, control / gate(...) lines for the rest) that already
        announces the apply. Failed commands stay visible so the
        operator sees the error_detail inline.
        """

        command_id = entry.control_command_id
        if command_id is None:
            return
        pending = self.pending_commands.get(command_id)
        if pending is None:
            return
        if entry.kind == EntryKind.LIFECYCLE and entry.header == "control":
            if entry.control_command_error is not None:
                pending.status = "failed"
                pending.error_detail = entry.control_command_error
                return
            # Non-say apply: the transcript already carries
            # "control  applied {kind}", so drop the pending marker.
            self.pending_commands.pop(command_id, None)
            return
        if entry.kind == EntryKind.OPERATOR_SAY:
            # Apply for ``say``: the OPERATOR_SAY line itself replaces
            # the pending marker.
            self.pending_commands.pop(command_id, None)
            return

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

        # Steering widgets: pending list, inline notice, and the help
        # line that advertises the available verbs. The compose box
        # itself stays mounted (it owns the slash-command vocabulary)
        # but loses its placeholder and goes read-only-ish on terminal
        # so the operator cannot type say/interrupt into a dead run.
        self._render_pending_widget()
        self._render_notice_widget()
        self._render_compose_widget()

    def _render_pending_widget(self) -> None:
        """Render the pending-commands list above the compose box.

        Each line is one outstanding command: ``[#id] kind: summary``
        for a pending entry, or ``[#id] kind failed: <error>`` for a
        failure. The widget is hidden when no commands are pending so
        the transcript reclaims the screen real estate.
        """

        pending_widget = self.query_one("#session_pending", Static)
        if not self.pending_commands:
            pending_widget.update("")
            pending_widget.add_class("hidden")
            return
        lines: list[str] = []
        for command_id in sorted(self.pending_commands.keys()):
            command = self.pending_commands[command_id]
            if command.status == "failed":
                error = command.error_detail or "(no detail)"
                lines.append(
                    f"[#{command_id}] {command.kind} failed: {error}"
                )
            else:
                lines.append(
                    f"[#{command_id}] {command.kind} pending"
                    + (f": {command.summary}" if command.summary else "")
                )
        pending_widget.update("\n".join(lines))
        pending_widget.remove_class("hidden")

    def _render_notice_widget(self) -> None:
        """Render the inline operator notice (e.g. not-steerable)."""

        notice_widget = self.query_one("#session_notice", Static)
        if self._notice:
            notice_widget.update(self._notice)
            notice_widget.remove_class("hidden")
        else:
            notice_widget.update("")
            notice_widget.add_class("hidden")

    def _render_compose_widget(self) -> None:
        """Show / hide the steering help footer; the compose box stays.

        The compose Input remains mounted for the whole life of the
        screen so the slash-command vocabulary (``/help``, ``/quit``,
        ...) is always reachable -- even on a terminal run where
        ``say`` would be a no-op. The help footer only advertises the
        verbs that would actually succeed for the current status so the
        operator never sees "press ctrl+y to approve" against a DONE
        run.
        """

        help_widget = self.query_one("#session_steering_help", Static)
        if self._enqueue is None:
            help_widget.add_class("hidden")
            help_widget.update("")
            return
        status = self._last_status
        can_say_or_interrupt = (
            status is not None
            and status.status is not None
            and status.status in _STEERABLE_STATUSES
        )
        can_approve_or_reject = (
            status is not None
            and status.status is not None
            and status.status in _APPROVABLE_STATUSES
        )
        if not can_say_or_interrupt and not can_approve_or_reject:
            help_widget.add_class("hidden")
            help_widget.update("")
            return
        help_lines: list[str] = []
        if can_say_or_interrupt:
            help_lines.append("enter=say  ctrl+x=interrupt")
        if can_approve_or_reject:
            help_lines.append(
                "ctrl+y=approve  ctrl+r=reject (input value = feedback)"
            )
        help_widget.update("  ".join(help_lines))
        help_widget.remove_class("hidden")

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

    # ----- Steering -----------------------------------------------------

    def on_input_submitted(self, event: Input.Submitted) -> None:
        """Compose-box submit -- slash command, or fall through to ``say``.

        A leading ``/`` switches into the shared slash vocabulary; the
        input clears on success and stays populated on the unknown-
        command branch so the operator can correct the typo without
        retyping the rest of the line.
        """

        if event.input.id != "session_compose":
            return
        raw = event.value
        if is_slash(raw):
            self._handle_slash(raw, event.input)
            return
        text = raw.strip()
        if not text:
            self._set_notice("say message must be non-empty")
            self._render_status_widgets()
            return
        self.submit_say(text)
        event.input.value = ""

    def _handle_slash(self, raw: str, input_widget: Input) -> None:
        """Dispatch one slash command line from the compose box.

        The slash command set mirrors the dashboard's so the operator
        moves between the two surfaces with one vocabulary. Errors
        leave the input populated (Error Handling row in spec 00021)
        so the operator can edit and resubmit; successes clear it.
        """

        command = parse_slash(raw)
        verb = command.verb
        if not verb:
            self._set_notice(unknown_command_notice(""))
            self._render_status_widgets()
            return
        if verb == SLASH_HELP:
            self._set_notice(HELP_TEXT)
            self._render_status_widgets()
            input_widget.value = ""
            return
        if verb == SLASH_STATUS:
            self._set_notice(self._status_text())
            self._render_status_widgets()
            input_widget.value = ""
            return
        if verb == SLASH_APPROVE:
            self.submit_approve()
            input_widget.value = ""
            return
        if verb == SLASH_REJECT:
            feedback = command.argument or None
            self.submit_reject(feedback)
            input_widget.value = ""
            return
        if verb == SLASH_INTERRUPT:
            self.submit_interrupt()
            input_widget.value = ""
            return
        if verb == SLASH_ARCHIVE:
            self._handle_archive()
            input_widget.value = ""
            return
        if verb == SLASH_WORKER:
            self._handle_worker_slash(command.argument)
            input_widget.value = ""
            return
        if verb == SLASH_QUIT:
            input_widget.value = ""
            # Route through the dashboard's supervised-child prompt so
            # quit from the session screen takes the same detach-or-stop
            # path as quit from the dashboard (spec FR-3: prompt appears
            # only when this console owns a supervised child).
            request_quit = getattr(self.app, "request_quit", None)
            if callable(request_quit):
                request_quit()
            else:
                self.app.exit()
            return
        # Unknown verb: preserve the typed line for editing.
        self._set_notice(unknown_command_notice(verb))
        self._render_status_widgets()

    def _status_text(self) -> str:
        """Render the viewed run's lifecycle status for ``/status``."""

        status = self._last_status
        if status is None:
            return f"run {self._run_id}: status unknown"
        if status.missing:
            return f"run {self._run_id}: not found in store"
        if status.status is None:
            return f"run {self._run_id}: lifecycle in unresolved state"
        line = f"run {self._run_id}: status={status.status.value}"
        if status.awaiting_instruction:
            line += f"; gate={status.awaiting_instruction}"
        return line

    def _handle_worker_slash(self, argument: str) -> None:
        """Forward ``/worker start|stop`` to the dashboard app's handler.

        The session screen does not own the worker supervisor (the
        :class:`DashboardApp` does); routing through the app keeps a
        single dispatch site so the inline notice and status-bar
        update flow remain consistent across screens. When the app
        lacks the seam (snapshot-only tests, ``--no-worker``), the
        notice mirrors the dashboard's "not wired" message.
        """

        handler = getattr(self.app, "handle_worker_slash", None)
        if not callable(handler):
            self._set_notice("/worker is not wired on this screen")
            self._render_status_widgets()
            return
        notice = handler(argument)
        if isinstance(notice, str) and notice:
            self._set_notice(notice)
            self._render_status_widgets()

    def _handle_archive(self) -> None:
        """Run the ``/archive`` action and surface its outcome inline."""

        if self._archive is None:
            self._set_notice(
                "/archive is not wired on this screen (no archive seam)"
            )
            self._render_status_widgets()
            return
        try:
            moved = self._archive()
        except Exception as exc:  # noqa: BLE001 - boundary against archive errors
            self._set_notice(f"/archive failed: {exc}")
            self._render_status_widgets()
            return
        if not moved:
            self._set_notice("/archive: no phases archived")
        else:
            self._set_notice(
                "/archive: moved " + ", ".join(moved)
            )
        self._render_status_widgets()

    def action_interrupt(self) -> None:
        """Key binding: enqueue an interrupt command."""

        self.submit_interrupt()

    def action_approve(self) -> None:
        """Key binding: enqueue an approve command (AWAITING_APPROVAL only)."""

        self.submit_approve()

    def action_reject(self) -> None:
        """Key binding: enqueue a reject command, lifting the optional
        feedback from the compose box's current value."""

        feedback: str | None = None
        try:
            compose = self.query_one("#session_compose", Input)
        except Exception:  # noqa: BLE001 - widget not mounted yet
            compose = None
        if compose is not None:
            raw = compose.value.strip()
            if raw:
                feedback = raw
                compose.value = ""
        self.submit_reject(feedback)

    def submit_say(self, text: str) -> int | None:
        """Enqueue a ``say`` command; returns the command id or ``None``.

        ``None`` is returned when steering is disabled (no enqueue seam
        wired) or when the run left the steerable set between render
        and submit. In the latter case an inline notice surfaces; the
        store is not touched. Re-checking status at submit time is the
        Edge Cases row "run left active set between render and submit"
        requirement.
        """

        return self._enqueue_steering(
            kind=CONTROL_COMMAND_SAY,
            payload={"text": text},
            summary=_short(text, 60),
            permitted=_STEERABLE_STATUSES,
        )

    def submit_interrupt(self) -> int | None:
        """Enqueue an ``interrupt`` command; returns the command id."""

        return self._enqueue_steering(
            kind=CONTROL_COMMAND_INTERRUPT,
            payload={},
            summary="",
            permitted=_STEERABLE_STATUSES,
        )

    def submit_approve(self) -> int | None:
        """Enqueue an ``approve`` command (AWAITING_APPROVAL only)."""

        return self._enqueue_steering(
            kind=CONTROL_COMMAND_APPROVE,
            payload={},
            summary="",
            permitted=_APPROVABLE_STATUSES,
        )

    def submit_reject(self, feedback: str | None) -> int | None:
        """Enqueue a ``reject`` command; ``feedback`` is optional.

        An empty / whitespace-only feedback collapses to ``None`` so
        the payload matches the watcher's expected shape (``feedback``
        present only when truthy) -- mirrors
        :func:`flywheel_core.workflow._cmd_reject` which omits the field
        when no ``--feedback`` flag is supplied.
        """

        payload: dict[str, Any] = {}
        if feedback is not None and feedback.strip():
            payload["feedback"] = feedback
        summary = _short(feedback, 60) if feedback else ""
        return self._enqueue_steering(
            kind=CONTROL_COMMAND_REJECT,
            payload=payload,
            summary=summary,
            permitted=_APPROVABLE_STATUSES,
        )

    def _enqueue_steering(
        self,
        *,
        kind: str,
        payload: Mapping[str, Any],
        summary: str,
        permitted: frozenset[Status],
    ) -> int | None:
        """Single enqueue path shared by the four steering verbs.

        Centralises the submit-time status re-check so a stale viewed
        status (one that changed between render and submit) is caught
        before the store is touched. On success: records the
        :class:`_PendingCommand`, clears the previous notice, refreshes
        widgets so the pending list shows immediately. On failure (no
        seam wired / status not permitted / store raised): records an
        inline notice, no store touch.
        """

        if self._enqueue is None:
            return None
        status = self._last_status
        current_status = status.status if status is not None else None
        if current_status is None or current_status not in permitted:
            self._set_notice(
                f"run is not steerable for {kind!r} "
                f"(status={current_status.value if current_status else 'unknown'})"
            )
            self._render_status_widgets()
            return None
        try:
            command_id = self._enqueue(kind, dict(payload))
        except Exception as exc:  # noqa: BLE001 - boundary against store errors
            self._set_notice(f"enqueue failed: {exc}")
            self._render_status_widgets()
            return None
        self.pending_commands[command_id] = _PendingCommand(
            command_id=command_id,
            kind=kind,
            summary=summary,
        )
        self._notice = None
        self._render_status_widgets()
        return command_id

    def _set_notice(self, message: str) -> None:
        """Record an inline notice; ``_render_notice_widget`` picks it up."""

        self._notice = message


# --- Rendering helpers -----------------------------------------------------


def _short(value: object, limit: int = 60) -> str:
    """Collapse ``value`` to a single line capped at ``limit`` characters.

    Used to summarise the operator's say / reject feedback text in the
    pending-commands widget. Mirrors :func:`flywheel._session._short`
    so the dashboard and session view collapse identically.
    """

    text = str(value).replace("\n", " ").replace("\r", " ")
    if len(text) <= limit:
        return text
    keep = max(limit - 1, 1)
    return text[:keep] + "…"


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
    have already been collapsed by :mod:`flywheel._session`.
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
    "ArchiveAction",
    "DEFAULT_SESSION_POLL_INTERVAL_SECONDS",
    "EnqueueCommand",
    "SessionScreen",
    "SessionStatus",
    "render_entry_text",
]

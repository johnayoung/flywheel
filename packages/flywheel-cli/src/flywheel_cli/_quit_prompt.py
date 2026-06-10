"""Modal quit-prompt screen for the supervised-worker handoff.

When the operator quits a console that owns a supervised worker, the
spec requires a single one-shot prompt: ``Enter`` detaches (worker
keeps running, console exits), ``s`` stops the worker gracefully
(SIGTERM; the existing in-flight finalization makes the run land at
``interrupted``). ``Escape`` cancels the quit so the operator can keep
working without disturbing the worker.

The prompt is intentionally minimal -- one :class:`Static` with the
three lines of instructions, three key bindings. It is *not* shown
when the console did not spawn the worker (detached / external /
already stopped); the dashboard's ``action_quit`` short-circuits in
that case.
"""

from __future__ import annotations

from textual.app import ComposeResult
from textual.binding import Binding
from textual.screen import ModalScreen
from textual.widgets import Static


# The three possible operator decisions surfaced as the dismiss result.
# String literal sentinels so the dashboard's callback can match them
# without importing this module beyond the screen class.
QUIT_DETACH = "detach"
QUIT_STOP = "stop"
QUIT_CANCEL = "cancel"


_PROMPT_LINES: tuple[str, ...] = (
    "supervised worker is running",
    "  enter  detach (worker keeps running, console exits)",
    "  s      stop gracefully (SIGTERM; in-flight run finalizes to interrupted)",
    "  esc    cancel quit",
)


class QuitPromptScreen(ModalScreen[str]):
    """Single-purpose modal asking detach / stop / cancel on quit.

    Returns the chosen action via :meth:`Screen.dismiss`. The
    dashboard pushes this screen with a callback that interprets the
    result: ``"detach"`` invokes :meth:`WorkerSupervisor.detach`
    before exit, ``"stop"`` invokes :meth:`WorkerSupervisor.stop`,
    ``"cancel"`` pops back to the dashboard without touching the
    supervisor.

    ``priority=True`` on each binding so the modal's keys win over
    any parent screen bindings while it is on top of the screen
    stack.
    """

    CSS = """
    QuitPromptScreen {
        align: center middle;
    }

    #quit_prompt {
        width: 64;
        height: auto;
        padding: 1 2;
        background: $boost;
        border: solid $accent;
    }
    """

    BINDINGS = [
        Binding("enter", "detach", "Detach", priority=True, show=True),
        Binding("s", "stop", "Stop", priority=True, show=True),
        Binding("escape", "cancel", "Cancel", priority=True, show=True),
    ]

    def compose(self) -> ComposeResult:
        yield Static("\n".join(_PROMPT_LINES), id="quit_prompt")

    def action_detach(self) -> None:
        self.dismiss(QUIT_DETACH)

    def action_stop(self) -> None:
        self.dismiss(QUIT_STOP)

    def action_cancel(self) -> None:
        self.dismiss(QUIT_CANCEL)


__all__ = [
    "QUIT_CANCEL",
    "QUIT_DETACH",
    "QUIT_STOP",
    "QuitPromptScreen",
]

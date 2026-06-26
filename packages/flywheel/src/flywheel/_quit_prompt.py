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


#: The default running-daemons label (back-compat: a worker-only console).
DEFAULT_RUNNING_LABEL = "supervised worker is running"


def _prompt_lines(running_label: str) -> tuple[str, ...]:
    return (
        running_label,
        "  enter  detach (daemons keep running, console exits)",
        "  s      stop all gracefully (SIGTERM; in-flight run finalizes to interrupted)",
        "  esc    cancel quit",
    )


class QuitPromptScreen(ModalScreen[str]):
    """Single-purpose modal asking detach / stop / cancel on quit.

    Returns the chosen action via :meth:`Screen.dismiss`. The
    dashboard pushes this screen with a callback that interprets the
    result: ``"detach"`` detaches every supervised child before exit,
    ``"stop"`` stops every supervised child (SIGTERM), ``"cancel"``
    pops back to the dashboard without touching any supervisor.

    ``running_label`` names which daemons are live (worker, autopilot,
    or both) so the operator sees exactly what ``stop`` will terminate.

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

    def __init__(self, running_label: str = DEFAULT_RUNNING_LABEL) -> None:
        super().__init__()
        self._running_label = running_label

    def compose(self) -> ComposeResult:
        yield Static(
            "\n".join(_prompt_lines(self._running_label)), id="quit_prompt"
        )

    def action_detach(self) -> None:
        self.dismiss(QUIT_DETACH)

    def action_stop(self) -> None:
        self.dismiss(QUIT_STOP)

    def action_cancel(self) -> None:
        self.dismiss(QUIT_CANCEL)


__all__ = [
    "DEFAULT_RUNNING_LABEL",
    "QUIT_CANCEL",
    "QUIT_DETACH",
    "QUIT_STOP",
    "QuitPromptScreen",
]

"""Slash-command vocabulary shared by the dashboard and session screens.

Both screens carry a persistent input bar; a leading ``/`` switches the
input into command mode. The seven verbs in :data:`SLASH_COMMANDS`
(``/help``, ``/status``, ``/approve``, ``/reject``, ``/interrupt``,
``/archive``, ``/quit``) reuse the exact store-mediated semantics of
their pre-existing CLI/key-binding twins -- the spec calls this an
"alternate input surface, not a new channel".

This module is deliberately UI-agnostic: it parses the typed line, names
the seven verbs, builds help text, and surfaces the unknown-command
error message. The screens own the actual dispatch (enqueue against the
selected/viewed run, pop the screen, exit the app, ...) because the
shape of those side-effects differs between dashboard and session.
"""

from __future__ import annotations

from dataclasses import dataclass


# The eight verbs the input bar honours. ``/worker start|stop`` is
# the supervision handle for the engine-on-launch child the console
# manages -- ``start`` (re)spawns a worker when none is live, ``stop``
# sends SIGTERM to the supervised child.
SLASH_HELP = "help"
SLASH_STATUS = "status"
SLASH_APPROVE = "approve"
SLASH_REJECT = "reject"
SLASH_INTERRUPT = "interrupt"
SLASH_ARCHIVE = "archive"
SLASH_WORKER = "worker"
SLASH_QUIT = "quit"

SLASH_COMMANDS: tuple[str, ...] = (
    SLASH_HELP,
    SLASH_STATUS,
    SLASH_APPROVE,
    SLASH_REJECT,
    SLASH_INTERRUPT,
    SLASH_ARCHIVE,
    SLASH_WORKER,
    SLASH_QUIT,
)


HELP_LINES: tuple[str, ...] = (
    "available slash commands",
    "  /help               show this list",
    "  /status             show the selected/viewed run's lifecycle status",
    "  /approve            approve the parked manual gate",
    "  /reject [feedback]  reject the parked manual gate (optional feedback)",
    "  /interrupt          interrupt the run",
    "  /archive            archive completed phases",
    "  /worker start|stop  spawn or gracefully stop the supervised worker",
    "  /quit               exit the console",
)

HELP_TEXT: str = "\n".join(HELP_LINES)


@dataclass(frozen=True, kw_only=True)
class SlashCommand:
    """One parsed slash-command invocation.

    ``verb`` is the bare name (no leading ``/``); ``argument`` is the
    rest of the line after the verb, stripped of surrounding whitespace.
    ``raw`` is the original typed line minus the leading ``/`` so a
    re-render of the input on error can preserve the operator's
    keystrokes exactly.
    """

    verb: str
    argument: str
    raw: str


def is_slash(text: str) -> bool:
    """Whether ``text`` begins with the slash-command sigil.

    Whitespace-only / empty strings return False so a stray ``/`` is
    rejected at the parser rather than treated as a verb.
    """

    return text.startswith("/") and len(text.strip()) > 1


def parse_slash(text: str) -> SlashCommand:
    """Split a leading-slash line into ``(verb, argument)``.

    The verb is the first whitespace-delimited token after ``/``; the
    argument is everything after the first whitespace, stripped. An
    isolated ``/verb`` carries an empty argument.
    """

    stripped = text.lstrip()
    assert stripped.startswith("/"), "parse_slash requires a leading slash"
    body = stripped[1:]
    if not body or body[0].isspace():
        return SlashCommand(verb="", argument="", raw=body)
    head, _, tail = body.partition(" ")
    return SlashCommand(verb=head.strip(), argument=tail.strip(), raw=body)


def unknown_command_notice(verb: str) -> str:
    """The inline error rendered when a slash verb is not recognised.

    Mirrors the spec's Error Handling row: name the bad verb, point at
    ``/help``.
    """

    return f"unknown slash command /{verb}; try /help"


__all__ = [
    "HELP_LINES",
    "HELP_TEXT",
    "SLASH_APPROVE",
    "SLASH_ARCHIVE",
    "SLASH_COMMANDS",
    "SLASH_HELP",
    "SLASH_INTERRUPT",
    "SLASH_QUIT",
    "SLASH_REJECT",
    "SLASH_STATUS",
    "SLASH_WORKER",
    "SlashCommand",
    "is_slash",
    "parse_slash",
    "unknown_command_notice",
]

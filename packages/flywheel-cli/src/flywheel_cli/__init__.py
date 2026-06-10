"""Unified ``fw`` / ``flywheel`` command + console package.

This package owns the operator console (Textual dashboard + per-run
session screen + persistent input bar) and the verb router that
dispatches every other subcommand:

* ``init``, ``status``, ``live``, ``archive``, ``recover``,
  ``recheck-blocked`` -> :func:`flywheel_orchestrator._workflow.main`.
* ``interrupt``, ``approve``, ``reject``, ``say`` (-> ``steer``) ->
  :func:`flywheel.workflow.main`.
* ``worker`` -> :func:`flywheel_worktree.worker.main` (the
  git-worktree daemon loop, in-process -- no shell-out).
* ``audit`` -> :func:`flywheel.audit._cli.main`.
* bare ``fw`` (TTY) or ``fw --json`` / non-TTY stdout ->
  :func:`flywheel_cli._tui.main` (Textual console or JSON snapshot).

The console code (``_dashboard.py``, ``_session*.py``, ``_snapshot.py``,
``_slash.py``, ``_tui.py``) was absorbed from the deleted ``flywheel-tui``
package per spec 00021; the import paths now live under
:mod:`flywheel_cli` and there is no transitional shim.
"""

from flywheel_cli._cli import main
from flywheel_cli._session import (
    EntryKind,
    TranscriptEntry,
    TranscriptTailer,
    classify,
    is_terminal,
)
from flywheel_cli._session_screen import SessionScreen, SessionStatus
from flywheel_cli._snapshot import (
    DashboardSnapshot,
    RowSnapshot,
    SummaryData,
    build_snapshot,
    snapshot_to_dict,
)
from flywheel_cli._tui import main as tui_main

__all__ = [
    "DashboardSnapshot",
    "EntryKind",
    "RowSnapshot",
    "SessionScreen",
    "SessionStatus",
    "SummaryData",
    "TranscriptEntry",
    "TranscriptTailer",
    "build_snapshot",
    "classify",
    "is_terminal",
    "main",
    "snapshot_to_dict",
    "tui_main",
]

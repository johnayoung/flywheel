"""Unified ``flywheel`` / ``fw`` command + console package.

This is the product shell: dist name ``flywheel``, import module
``flywheel_cli``, console scripts ``flywheel`` and ``fw`` (one
implementation, two byte-identical entries). It owns the operator
console (Textual dashboard + per-run session screen + persistent input
bar) and the verb router that dispatches every other subcommand:

* ``init``, ``status``, ``live``, ``archive``, ``recover``,
  ``recheck-blocked`` -> :func:`flywheel_orchestrator._workflow.main`.
* ``interrupt``, ``approve``, ``reject``, ``say`` (-> ``steer``) ->
  :func:`flywheel.workflow.main`.
* ``worker`` -> :func:`flywheel_worktree.worker.main` (the
  git-worktree daemon loop, in-process -- no shell-out).
* ``audit`` -> :func:`flywheel.audit._cli.main`.
* bare ``flywheel`` / ``fw`` (TTY) or ``--json`` / non-TTY stdout ->
  :func:`flywheel_cli._tui.main` (Textual console or JSON snapshot).

The console code (``_dashboard.py``, ``_session*.py``, ``_snapshot.py``,
``_slash.py``, ``_tui.py``) was absorbed from the prior standalone TUI
package per spec 00021; the import paths now live under :mod:`flywheel_cli`
and there is no transitional shim.
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
from flywheel_cli._worker_supervisor import (
    WorkerState,
    WorkerStatus,
    WorkerSupervisor,
)

__all__ = [
    "DashboardSnapshot",
    "EntryKind",
    "RowSnapshot",
    "SessionScreen",
    "SessionStatus",
    "SummaryData",
    "TranscriptEntry",
    "TranscriptTailer",
    "WorkerState",
    "WorkerStatus",
    "WorkerSupervisor",
    "build_snapshot",
    "classify",
    "is_terminal",
    "main",
    "snapshot_to_dict",
    "tui_main",
]

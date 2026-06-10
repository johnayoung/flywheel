"""Unified ``flywheel`` / ``fw`` command + console package.

This is the product shell: dist name ``flywheel``, import module
``flywheel``, console scripts ``flywheel`` and ``fw`` (one
implementation, two byte-identical entries). It owns the operator
console (Textual dashboard + per-run session screen + persistent input
bar) and the verb router that dispatches every other subcommand:

* ``init``, ``status``, ``live``, ``archive``, ``recover``,
  ``recheck-blocked`` -> :func:`flywheel_orchestrator._workflow.main`.
* ``interrupt``, ``approve``, ``reject``, ``say`` (-> ``steer``) ->
  :func:`flywheel_core.workflow.main`.
* ``worker`` -> :func:`flywheel_worktree.worker.main` (the
  git-worktree daemon loop, in-process -- no shell-out).
* ``audit`` -> :func:`flywheel_core.audit._cli.main`.
* bare ``flywheel`` / ``fw`` (TTY) or ``--json`` / non-TTY stdout ->
  :func:`flywheel._tui.main` (Textual console or JSON snapshot).

The console code (``_dashboard.py``, ``_session*.py``, ``_snapshot.py``,
``_slash.py``, ``_tui.py``) was absorbed from the prior standalone TUI
package per spec 00021; the import paths now live under :mod:`flywheel`
and there is no transitional shim.
"""

from flywheel._cli import main
from flywheel._session import (
    EntryKind,
    TranscriptEntry,
    TranscriptTailer,
    classify,
    is_terminal,
)
from flywheel._session_screen import SessionScreen, SessionStatus
from flywheel._snapshot import (
    DashboardSnapshot,
    RowSnapshot,
    SummaryData,
    build_snapshot,
    snapshot_to_dict,
)
from flywheel._tui import main as tui_main
from flywheel._worker_supervisor import (
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

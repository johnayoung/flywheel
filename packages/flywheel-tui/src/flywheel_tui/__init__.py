"""Interactive terminal console for flywheel-orchestrator.

A Textual app over the orchestrator's read plane: bare ``flywheel-tui``
opens a keyboard-navigable realtime dashboard of in-flight runs; with
``--json`` (or a non-TTY stdout) it prints one machine-readable snapshot
and exits. Depends on ``flywheel-orchestrator`` and nothing depends on
it; the Textual dependency lives only inside this package.
"""

from flywheel_tui._cli import main
from flywheel_tui._session import (
    EntryKind,
    TranscriptEntry,
    TranscriptTailer,
    classify,
    is_terminal,
)
from flywheel_tui._session_screen import SessionScreen, SessionStatus
from flywheel_tui._snapshot import (
    DashboardSnapshot,
    RowSnapshot,
    SummaryData,
    build_snapshot,
    snapshot_to_dict,
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
    "build_snapshot",
    "classify",
    "is_terminal",
    "main",
    "snapshot_to_dict",
]

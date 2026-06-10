"""Interactive terminal console for flywheel-orchestrator.

A Textual app over the orchestrator's read plane: bare ``flywheel-tui``
opens a keyboard-navigable realtime dashboard of in-flight runs; with
``--json`` (or a non-TTY stdout) it prints one machine-readable snapshot
and exits. Depends on ``flywheel-orchestrator`` and nothing depends on
it; the Textual dependency lives only inside this package.
"""

from flywheel_tui._cli import main
from flywheel_tui._snapshot import (
    DashboardSnapshot,
    RowSnapshot,
    SummaryData,
    build_snapshot,
    snapshot_to_dict,
)

__all__ = [
    "DashboardSnapshot",
    "RowSnapshot",
    "SummaryData",
    "build_snapshot",
    "main",
    "snapshot_to_dict",
]

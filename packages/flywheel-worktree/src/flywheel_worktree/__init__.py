"""Git-worktree submit strategy + reference worker for flywheel-orchestrator.

The concrete "between agent-finished and result-merged" layer: each task runs
in its own git worktree on a per-task branch; on success the branch is
fast-forward merged into the base, otherwise it is parked for forensics. This
is one worked example of the orchestrator's pluggable submit seam — swap it for
your own strategy in another codebase. It depends on ``flywheel-orchestrator``;
the daemon is launched through the unified product shell as
``flywheel worker``, which calls :func:`flywheel_worktree.worker.main`
in-process.
"""

from flywheel_worktree.worker import (
    GitWorktreeSubmitter,
    PrepareSandboxError,
    main,
    phase_of_task_file,
    run_once,
)

__all__ = [
    "GitWorktreeSubmitter",
    "PrepareSandboxError",
    "main",
    "phase_of_task_file",
    "run_once",
]

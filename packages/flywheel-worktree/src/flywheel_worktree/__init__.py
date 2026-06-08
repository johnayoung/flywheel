"""Git-worktree submit strategy + reference worker for flywheel-orchestrator.

The concrete "between agent-finished and result-merged" layer: each task runs
in its own git worktree on a per-task branch; on success the branch is
fast-forward merged into the base, otherwise it is parked for forensics. This
is one worked example of the orchestrator's pluggable submit seam — swap it for
your own strategy in another codebase. It depends on ``flywheel-orchestrator``.

Skeleton: the worktree submitter + daemon loop are being relocated here from
``.workflow/worker.py`` in a later phase of the core/consumer split.
"""

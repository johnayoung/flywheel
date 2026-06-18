"""The registry of submit (landing) strategies.

Names the landing strategies an operator can select in ``[submit] strategy``
and binds each to a builder beside its implementation:
:func:`~flywheel_worktree.worker.build_merge_submitter` and
:func:`~flywheel_worktree.pr.build_pr_submitter`. The builders share one
signature so the worker dispatches by name with no per-strategy branch.

Resolving ``pr`` lazily imports :mod:`flywheel_worktree.pr` (which imports
``worker``), so routing through :meth:`Registry.resolve` is what dissolves the
old hand-written late import in ``worker.main`` — the registry is the seam
that breaks the cycle. Neither shipped strategy needs an optional extra. A
third-party landing strategy is discovered and selectable by advertising a
``flywheel.submit_strategies`` entry point, with no fork (built-ins win a name
collision).
"""

from __future__ import annotations

from flywheel_core._registry import PluginSpec, Registry

SUBMIT_STRATEGIES = Registry("submit strategy", "flywheel.submit_strategies")

SUBMIT_STRATEGIES.register(
    PluginSpec(
        name="merge",
        target="flywheel_worktree.worker:build_merge_submitter",
        summary="fast-forward merge into the phase base (default)",
    )
)
SUBMIT_STRATEGIES.register(
    PluginSpec(
        name="pr",
        target="flywheel_worktree.pr:build_pr_submitter",
        summary="push the branch and open a pull request",
    )
)

__all__ = ["SUBMIT_STRATEGIES"]

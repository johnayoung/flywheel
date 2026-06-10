"""Unified ``fw`` command router (interim package).

This package owns no execution logic of its own. Every subcommand is a
thin dispatch into the existing implementations:

* ``init``, ``status``, ``live``, ``archive``, ``recover``,
  ``recheck-blocked`` -> :func:`flywheel_orchestrator._workflow.main`.
* ``interrupt``, ``approve``, ``reject``, ``say`` (-> ``steer``) ->
  :func:`flywheel.workflow.main`.
* ``worker`` -> :func:`flywheel_worktree.worker.main` (the
  git-worktree daemon loop, in-process -- no shell-out).
* ``audit`` -> :func:`flywheel.audit._cli.main`.
* bare ``fw`` (TTY) or ``fw --json`` / non-TTY stdout ->
  :func:`flywheel_tui._cli.main` (Textual console or JSON snapshot).

The package's distribution name is the interim ``flywheel-cli`` (the
core package still owns the ``flywheel`` script until the cli-cutover
task flips both). The console script installed here is ``fw`` ONLY.
"""

from flywheel_cli._cli import main

__all__ = ["main"]

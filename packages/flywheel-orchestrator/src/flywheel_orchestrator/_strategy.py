"""The landing seam: how finished work leaves the loop.

A *submit strategy* is the consumer code between "agent finished" and
"result landed" — branches, worktrees, merges, pull requests, review gates,
or nothing at all. It stays out of flywheel by design (``docs/strategy.md``):
the orchestrator owns selection and execution order; what a terminal status
*means* for the repository is the strategy's call.

The seam is two hooks ``orchestrate`` invokes around every run:

* ``prepare_sandbox`` — provision the directory the task runs in (a git
  worktree, a container mount, a plain dir). May raise; the task is skipped
  for the session and peers are never starved.
* ``submit`` — act on the run's terminal status while the task's lease is
  still held, so two workers never land the same task concurrently. MUST
  NOT raise: it records its own outcome and swallows its errors, so a
  landing failure never unwinds the orchestrator and abandons peer tasks.

Both hooks exist standalone (:data:`SandboxProvider` / :data:`Submitter`)
and as the bundled :class:`SubmitStrategy` protocol — one object per landing
policy (merge to base, open a PR, emit a patch), passed to
``orchestrate(strategy=...)``. The reference implementation is
``flywheel_worktree.worker.GitWorktreeSubmitter``.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Literal, Protocol, runtime_checkable

from flywheel_core.lifecycle import Status
from flywheel_core.task import Task


@dataclass(frozen=True, kw_only=True)
class SandboxRequest:
    """What the orchestrator needs a consumer to provision a run's sandbox.

    Handed to a :data:`SandboxProvider` so a git-aware consumer can create or
    reuse a worktree (and derive the branch from ``task_file``'s phase) and
    return the directory the task should run in. ``run_id`` is ``None`` for a
    fresh run; for a resume/recheck it is the lifecycle being continued.
    ``mode`` distinguishes the two so the provider can rebase a parked
    worktree on resume.

    ``task_file`` is populated for file-backed work sources and is an empty
    ``Path()`` otherwise; ``source_ref`` always carries the source's opaque
    item handle. Path-deriving providers must treat an empty path as "no
    file".
    """

    task_id: str
    task_file: Path
    run_id: str | None
    mode: Literal["fresh", "resume"]
    source_ref: str = ""


@dataclass(frozen=True, kw_only=True)
class SubmitRequest:
    """The terminal outcome of one run, handed to a consumer's submit step.

    A git-aware consumer uses this to FF-merge the task branch on ``done`` or
    park the ``sandbox`` worktree on a non-done terminal status. ``sandbox``
    is exactly the path the matching :data:`SandboxProvider` returned.
    ``task_file``/``source_ref`` follow the same convention as
    :class:`SandboxRequest`.

    ``task`` is the validated core task the run was driven with. It carries
    everything a submit strategy needs to re-verify the work against the
    tree it is about to land on (the graders) or to render the outcome
    elsewhere (the goal), without assuming a file-backed source.
    """

    task_id: str
    task_file: Path
    task: Task
    run_id: str
    status: Status
    sandbox: Path
    source_ref: str = ""


# A consumer maps a SandboxRequest to the directory the task runs in (default:
# ``sandbox_root/<task-id>``), and is handed a SubmitRequest after each run
# finalizes — while the lease is still held — to merge or park. ``submit`` MUST
# NOT raise: it records its own park/merge outcome and swallows git errors, so
# a submit failure never unwinds the orchestrator and abandons peer tasks.
SandboxProvider = Callable[[SandboxRequest], Path]
Submitter = Callable[[SubmitRequest], None]


@runtime_checkable
class SubmitStrategy(Protocol):
    """One landing policy, bundled: provision the sandbox, land the outcome.

    Structural: any object with conforming ``prepare_sandbox`` / ``submit``
    methods satisfies it — no registration, no base class. The two methods
    carry the same contracts as the standalone callables above
    (``prepare_sandbox`` may raise to skip the task; ``submit`` must not
    raise). Pass one to ``orchestrate(strategy=...)`` instead of wiring the
    callables individually.
    """

    def prepare_sandbox(self, request: SandboxRequest, /) -> Path: ...

    def submit(self, request: SubmitRequest, /) -> None: ...

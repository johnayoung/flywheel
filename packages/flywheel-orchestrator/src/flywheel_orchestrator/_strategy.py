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

from collections.abc import Mapping
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable, Literal, Protocol, runtime_checkable

from flywheel_core import InvokeFunc
from flywheel_core.lifecycle import Status
from flywheel_core.task import Task

from flywheel_orchestrator._sources import GraderReceipt


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
    ``receipts`` are the final attempt's grader verdicts — the same
    projection the work source's report receives — so a strategy can
    render "how done was decided" (e.g. into a PR body). Best-effort:
    empty when the projection failed.
    """

    task_id: str
    task_file: Path
    task: Task
    run_id: str
    status: Status
    sandbox: Path
    source_ref: str = ""
    receipts: tuple[GraderReceipt, ...] = ()


@dataclass(frozen=True, kw_only=True)
class SandboxHandle:
    """A provisioned sandbox plus how a run should attach to it.

    The richer return shape of a :data:`SandboxProvider` (spec 00043,
    increment F of 00036). The git-worktree backend provisions a plain
    directory and needs only ``path``, so a provider may still return a bare
    ``Path`` — the orchestrator adapts it via :func:`_as_handle` to
    ``SandboxHandle(path=...)`` with empty contributions, byte-identical to
    the pre-handle behavior. A non-worktree backend (e.g. a container)
    populates the optional fields so the orchestrator runs the agent *inside*
    the provisioned environment:

    * ``env_contribution`` — extra environment merged onto the policy-resolved
      ``[sandbox.env]`` agent env (the handle wins on key collision), for env
      a backend must inject (a container's ``PATH``, a forwarded socket).
    * ``invoke_wrapper`` — wraps the run's :data:`~flywheel_core.InvokeFunc`
      so the agent iteration executes in the backend (e.g. ``docker exec``
      into the container) instead of the worker process. ``None`` runs
      in-process exactly as today. It is handed the base invoke, which is
      ``None`` in normal operation (orchestrate is driven with ``invoke=None``
      and the SDK invoker is built downstream); a replacing wrapper ignores
      it, an augmenting one composes it.
    * ``teardown`` — disposes the provisioned environment after the run lands
      (e.g. ``docker stop``/``rm`` the container). The orchestrator calls it
      best-effort after ``submit`` and before releasing the lease; it MUST NOT
      raise (a teardown failure never unwinds the worker). Per-task by
      construction — the provider captures the container/handle identity in the
      closure at ``prepare_sandbox`` time — which is why it lives here rather
      than on :class:`SubmitStrategy` (a single shared strategy instance could
      not key teardown to one task's sandbox). ``None`` (every worktree
      backend) means there is nothing to dispose.

    Frozen and kw-only, mirroring :class:`SandboxRequest`/:class:`SubmitRequest`.
    """

    path: Path
    env_contribution: Mapping[str, str] = field(default_factory=dict)
    invoke_wrapper: Callable[[InvokeFunc | None], InvokeFunc] | None = None
    teardown: Callable[[], None] | None = None


def _as_handle(result: Path | SandboxHandle) -> SandboxHandle:
    """Adapt a provider return to a :class:`SandboxHandle`.

    Back-compat seam: a provider that returns a bare ``Path`` (every shipped
    worktree provider) becomes a handle with empty contributions, so the
    orchestrator's handle-aware path stays byte-identical for it.
    """
    if isinstance(result, SandboxHandle):
        return result
    return SandboxHandle(path=result)


# A consumer maps a SandboxRequest to the directory the task runs in (default:
# ``sandbox_root/<task-id>``) — or a :class:`SandboxHandle` describing it — and
# is handed a SubmitRequest after each run finalizes — while the lease is still
# held — to merge or park. ``submit`` MUST NOT raise: it records its own
# park/merge outcome and swallows git errors, so a submit failure never unwinds
# the orchestrator and abandons peer tasks.
SandboxProvider = Callable[[SandboxRequest], "Path | SandboxHandle"]
Submitter = Callable[[SubmitRequest], None]


@runtime_checkable
class SubmitStrategy(Protocol):
    """One landing policy, bundled: provision the sandbox, land the outcome.

    Structural: any object with conforming ``prepare_sandbox`` / ``submit``
    methods satisfies it — no registration, no base class. The two methods
    carry the same contracts as the standalone callables above
    (``prepare_sandbox`` may raise to skip the task; ``submit`` must not
    raise). ``prepare_sandbox`` may return either a bare ``Path`` (the
    worktree backend) or a :class:`SandboxHandle` (a container backend); the
    orchestrator adapts a ``Path`` to a handle. Pass one to
    ``orchestrate(strategy=...)`` instead of wiring the callables individually.
    """

    def prepare_sandbox(
        self, request: SandboxRequest, /
    ) -> Path | SandboxHandle: ...

    def submit(self, request: SubmitRequest, /) -> None: ...

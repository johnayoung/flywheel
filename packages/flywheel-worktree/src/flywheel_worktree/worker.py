#!/usr/bin/env python3
"""Git-worktree worker: the reference consumer that drives flywheel tasks.

The ``flywheel-worktree`` package — one worked example of building on top of
``flywheel-orchestrator``. Library only: the daemon is launched through the
unified product shell as ``flywheel worker``, which calls :func:`main` in
this module in-process. It is the strategy layer of ``docs/strategy.md``:
the code between "agent finished" and "result merged". It owns the two
concerns flywheel deliberately does not:

* **Git submit** — each task runs in its own worktree on branch
  ``flywheel/<phase>/<task-id>``; on ``done`` the branch is FF-merged into the
  base and the worktree removed, otherwise it is parked for forensics. When
  the base advanced under a finished task, the branch is rebased once and its
  command graders re-run against the rebased tree before the merge — nothing
  lands that was not verified against the exact base it lands on. Lives in
  :class:`GitWorktreeSubmitter`, injected into ``orchestrate`` through its
  ``prepare_sandbox`` / ``submit`` seam. No git lives in flywheel.
* **Daemon poll loop** — ``orchestrate`` drains every eligible task to
  quiescence and exits; this loop re-invokes it after recording phase base
  refs and archiving completed phases. The worker never creates commits on
  the operator's branch.

Selection, prerequisites, reactive unblock/resume, leases + heartbeat,
stranded recovery, and graceful-shutdown finalization are flywheel's, reused
as-is. Replaces the former 858-line ``.workflow/task-worker.sh``.

Parallelism is per-process: run several workers against one store — leases keep
them off the same task, a repo-level merge flock serializes their base merges.
A graceful SIGTERM/SIGINT finalizes the in-flight lifecycle to ``interrupted``
(inside flywheel's ``run_task_object``) and stops the loop; SIGKILL/OOM/reboot
are caught by orchestrate's startup recovery sweep on the next run.

    flywheel worker [--once] [--tasks-dir DIR] [--db PATH] ...
"""

from __future__ import annotations

import argparse
import asyncio
import contextlib
import fcntl
import os
import shutil
import signal
import subprocess
import sys
import threading
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path, PurePosixPath
from collections.abc import Mapping
from typing import IO, Callable, Iterator, Protocol, Sequence, TextIO
from uuid import uuid4

from flywheel_core import (
    CommandGrader,
    FaultClass,
    GraderResultRecord,
    InvokeFunc,
    Lifecycle,
    Status,
    Task,
    classify_fault,
    invoke_iteration,
    run_command_graders,
)
from flywheel_core.deadline import DeadlineExceeded, run_with_deadline
from flywheel_core.events import (
    GATE_EXCERPT_MAX_BYTES,
    LANDING_STRATEGY_MERGE,
    PARK_KIND_DIVERGENT_BASE,
    PARK_KIND_HELD_OUT_GATE,
    PARK_KIND_MERGE_CONFLICT,
    PARK_KIND_PROTECTED_PATHS,
    PARK_KIND_STANDING_VERIFY,
    PARK_KIND_SUBMIT_ERROR,
    RUNG_AGENT_RESOLVED,
    RUNG_FAST_FORWARD,
    RUNG_MERGE_FALLBACK,
    RUNG_REBASE,
    DomainEvent,
    GateGraderReceipt,
    Landed,
    LandingParked,
)
from flywheel_orchestrator import (
    DEFAULT_LANDING_REDRIVE_BOUND,
    DEFAULT_SESSION_PAUSE_CEILING_SECONDS,
    DEFAULT_SWEEP_SECONDS,
    DEFAULT_WORKER_CONCURRENCY,
    FilesystemHeldOutGraderSource,
    GhRunner,
    HeldOutGraderSource,
    LandabilityVerdict,
    OrchestratorReport,
    PolicyError,
    RespawnDecision,
    SandboxRequest,
    SubmitRequest,
    SubmitStrategy,
    SupervisionBudget,
    SupervisionPolicy,
    WorkPolicy,
    evaluate_held_out_gate,
    load_effective_policy,
    open_sqlite_bound_store,
    orchestrate,
    resolve_grader_env,
    resolve_sandbox_root,
)
from flywheel_core.deadline_config import DeadlineConfig
from flywheel_core.workflow import (
    DEFAULT_MAX_RETRIES,
    DEFAULT_MAX_TURNS,
)
from flywheel_orchestrator import (
    LiveRunRow,
    archive_completed_phases,
    build_claim_store,
    collect_live_rows,
    iter_active_phase_dirs,
    write_phase_base_if_missing,
)

from flywheel_worktree._disk_preflight import DiskPreflight
from flywheel_worktree._submit_registry import SUBMIT_STRATEGIES
from flywheel_worktree._trailers import (
    provenance_trailers,
    stamp_commit_messages,
)

DEFAULT_RETENTION_DAYS = 7
# Default cap on per-run telemetry JSONL files kept under
# .flywheel/logs/runs/. The harness's sink writes one <run_id>.jsonl per run
# and never rotates them, so without an active bound the directory grows
# without limit. The worker reclaims the oldest files past this cap each cycle;
# the most-recent runs (including the one in flight) always survive. Magnitude
# is a default, not a contract.
DEFAULT_RUN_LOG_RETENTION = 500
DEFAULT_HEARTBEAT_SECONDS = 10
DEFAULT_POLL_INTERVAL_SECONDS = 5
# Consecutive whole-cycle failures (orchestrate raising unexpectedly) before
# the daemon gives up so an operator can inspect, rather than hot-looping. The
# per-task starvation guard lives in orchestrate (attempted_fresh); this is the
# cross-cycle backstop that replaces the bash SPAWN_FAILURES circuit breaker.
MAX_CONSECUTIVE_CYCLE_FAILURES = 5
CYCLE_FAILURE_BACKOFF_SECONDS = 10
# Distinct exit code for a permanent-stop: a cycle fault that can never succeed
# on retry (a schema-version mismatch reopening the store every cycle). It stops
# the loop after a single cycle -- separate from the transient give-up exit (1)
# so an operator/grader can tell "wrong on-disk schema" from "five flaky cycles".
PERMANENT_STOP_EXIT_CODE = 2

# Worker-pool supervision (spec 00060). How long the pool's group shutdown
# gives each member's SIGTERM window before escalating to SIGKILL, and how
# often the supervise loop polls members for exit. The stop timeout mirrors the
# console worker supervisor (commit 36a0622); the poll interval is short so a
# stop signal or a member crash is noticed promptly without busy-spinning.
DEFAULT_POOL_STOP_TIMEOUT_SECONDS = 10.0
POOL_SUPERVISE_POLL_SECONDS = 0.2
# Per-slot windowed crash-loop budget (spec 00070, decisions D-B/D-C). A member
# that keeps crashing is respawned at most this many times within a rolling
# window; the next death inside the window RETIRES that one slot in isolation
# and the pool keeps supervising every other live member -- one bad slot never
# group-kills the healthy fleet. Windowed, not lifetime: a slot that dips once
# after a long healthy interval replenishes its budget rather than carrying an
# old burst forever. Mirrors the console supervisors' default budget.
DEFAULT_POOL_RESTARTS_PER_SLOT = 5
DEFAULT_POOL_RESTART_WINDOW_SECONDS = 300.0


Logger = Callable[[str], None]


# Fixed, deterministic commit identity established on every worktree the worker
# provisions, so the agent's own in-sandbox ``git commit`` resolves an
# author/committer even on a host with no global or system git identity
# (``GIT_CONFIG_NOSYSTEM=1``, empty ``HOME``). Constant across every worktree in
# a repo (never random/UUID/timestamp/per-run): the worker authors no commit
# itself; this only lets the agent's commit succeed.
WORKTREE_COMMIT_IDENTITY_NAME = "Flywheel Worker"
WORKTREE_COMMIT_IDENTITY_EMAIL = "worker@flywheel.invalid"


@dataclass(frozen=True)
class ConflictResolutionRequest:
    """Inputs a bounded conflict-resolution session runs against (spec 00076,
    criterion 4).

    ``prompt`` scopes the agent to resolving the in-progress merge's conflict
    markers; ``worktree`` is the conflicted tree it works in (the session's
    cwd); ``max_turns`` / ``max_wall_seconds`` are the session's hard bounds
    (``[submit] recovery_agent_max_turns`` / ``recovery_agent_max_wall_seconds``);
    ``model`` is the SDK model id (``None`` => the SDK default).
    """

    prompt: str
    worktree: Path
    max_turns: int
    max_wall_seconds: float
    model: str | None = None


@dataclass(frozen=True)
class ConflictResolutionReport:
    """Usage a bounded conflict-resolution session recorded (spec 00076, D-4).

    ``turns`` is the number of agent turns spent (bounded by
    ``max_turns``); ``wall_seconds`` is the wall-clock elapsed (bounded by
    ``max_wall_seconds``). The driver reports usage only -- never a resolution
    verdict, since the agent's claim is untrusted and the worker decides
    whether the tree is resolved from git state alone.
    """

    turns: int
    wall_seconds: float


#: The injectable conflict-resolution seam: given a request it drives one
#: bounded agent session inside the conflicted worktree and returns the recorded
#: usage. The default is :func:`_default_resolve_conflict` (a real SDK run behind
#: the lazy ``flywheel_core._sdk`` boundary); tests inject a synchronous stub so
#: the rung is exercised offline without touching the SDK, mirroring
#: ``build_repo_invoker``'s seam.
ConflictResolver = Callable[
    [ConflictResolutionRequest], ConflictResolutionReport
]


def _default_resolve_conflict(
    request: ConflictResolutionRequest,
) -> ConflictResolutionReport:
    """Production conflict-resolution session: a bounded Claude run rooted in the
    conflicted worktree (SDK behind the lazy ``flywheel_core._sdk`` boundary).

    Drives exactly one bounded agent iteration inside ``request.worktree`` -- an
    in-progress merge with conflict markers in place -- under both a turn budget
    (``ClaudeAgentOptions.max_turns``) and a wall-clock ceiling
    (:func:`~flywheel_core.deadline.run_with_deadline`). Returns the recorded
    usage; it never inspects or reports whether the tree was resolved (the
    worker owns that). Synchronous by design so the ``submit`` call chain (itself
    synchronous) drives it via :func:`asyncio.run`, exactly one session per
    re-driver pass.
    """
    return asyncio.run(_drive_conflict_resolution(request))


async def _drive_conflict_resolution(
    request: ConflictResolutionRequest,
) -> ConflictResolutionReport:
    """Run one bounded agent iteration for :func:`_default_resolve_conflict`.

    The SDK is imported lazily here so importing this module never requires the
    ``claude`` extra -- the seam is only exercised when the merge-fallback rung
    actually escalates. On a wall-clock timeout the call is cancelled and the
    usage is reported at the bound (the true turn count is unavailable after
    cancellation), so a slow session parks within its bounds rather than raising.
    """
    from flywheel_core._sdk import ClaudeAgentOptions

    options = ClaudeAgentOptions(
        cwd=str(request.worktree),
        add_dirs=[str(request.worktree)],
        permission_mode="bypassPermissions",
        max_turns=request.max_turns,
        model=request.model,
    )
    started = time.monotonic()
    call = invoke_iteration(prompt=request.prompt, options=options)
    try:
        result = await run_with_deadline(call, request.max_wall_seconds)
    except DeadlineExceeded:
        return ConflictResolutionReport(
            turns=request.max_turns,
            wall_seconds=request.max_wall_seconds,
        )
    turns = result.signals.num_turns or 0
    wall = min(time.monotonic() - started, request.max_wall_seconds)
    return ConflictResolutionReport(turns=turns, wall_seconds=wall)


def _render_conflict_resolution_prompt(
    req: SubmitRequest, *, branch: str, base: str
) -> str:
    """The instruction a bounded conflict-resolution session runs under.

    Names the in-progress merge, scopes the agent strictly to resolving the
    conflict markers (never re-doing the task or editing beyond the conflict),
    and asks it to stage the resolutions. The worker authors the merge commit
    and owns every landing gate afterwards, so the prompt does not ask the agent
    to commit, verify, or judge -- its claim is untrusted.
    """
    return (
        f"You are resolving a git merge conflict, nothing more.\n\n"
        f"The branch `{branch}` implements this already-completed and verified "
        f"task:\n\n{req.task.goal}\n\n"
        f"A merge of the base branch `{base}` into `{branch}` is in progress in "
        f"this worktree and has left conflict markers. Your ONLY job is to "
        f"resolve every conflicted file so the merged result preserves the "
        f"task's completed work and the base's changes together, then stage the "
        f"resolved files with `git add`.\n\n"
        f"Strict rules:\n"
        f"- Resolve only the merge conflicts. Do NOT re-implement the task, add "
        f"features, or edit anything unrelated to a conflict.\n"
        f"- Leave no conflict markers (<<<<<<<, =======, >>>>>>>).\n"
        f"- Do NOT run `git commit`, `git merge --abort`, or `git reset`; the "
        f"harness commits and verifies the result.\n"
        f"- Do NOT touch verification config, graders, or CI.\n"
    )


class LandingLedger(Protocol):
    """The store surface the submitter needs to record a landing-parked
    outcome: read the run's current version, then append the audit-witness
    :class:`~flywheel_core.events.LandingParked` event under optimistic
    concurrency. Both ``SqliteStore``/``PostgresStore``/``InMemoryStore``
    satisfy it."""

    def load_lifecycle(self, run_id: str) -> Lifecycle | None: ...

    def append_domain_event(
        self, event: DomainEvent, *, expected_version: int
    ) -> Lifecycle: ...


class GitError(RuntimeError):
    """A git invocation the worker expected to succeed did not."""


class PrepareSandboxError(RuntimeError):
    """A task's worktree could not be provisioned. ``orchestrate`` skips that
    task for the session (never starving peers), as the old worker did."""


# --- git plumbing -----------------------------------------------------------


def _git(
    cwd: Path, *args: str, check: bool = False
) -> subprocess.CompletedProcess[str]:
    """Run ``git -C <cwd> <args>``. ``check=False`` by default: most callers
    branch on ``returncode`` (a non-zero exit is data); ``check=True`` raises
    :class:`GitError` for the few calls whose failure is unexpected."""
    proc = subprocess.run(
        ["git", "-C", str(cwd), *args],
        text=True,
        capture_output=True,
        check=False,
    )
    if check and proc.returncode != 0:
        raise GitError(
            f"git {' '.join(args)} failed in {cwd}: {proc.stderr.strip()}"
        )
    return proc


def _git_progress_probe(sandbox: Path) -> Callable[[], object]:
    """Build the checkpoint-nudge progress probe bound to ``sandbox``.

    Git awareness lives ONLY here (the landability-probe discipline, spec
    00061): the orchestrator and core treat the returned closure as an opaque
    token source. The token is the sandbox branch's resolved HEAD revision, so
    it changes IFF a new commit lands -- the harness captures it at iteration
    start and an equal token at nudge-check time means "no new commit since the
    iteration began" (nudge-eligible). A fresh worktree with zero commits past
    its base holds a stable HEAD, so it stays nudge-eligible until the agent
    commits.

    The probe RAISES on any git failure (a deleted/missing worktree, a non-repo
    dir): the harness's ``_safe_probe`` contains that raise and simply skips the
    nudge, so a broken probe never fires on a false "no progress" signal and
    never unwinds the run. Returning a stable sentinel instead would read as
    "no progress" and wrongly arm the nudge -- so failure must raise.

    Bound per-run over one sandbox, so two concurrent pool members' probes read
    only their own sandbox's HEAD, never a peer's.
    """

    def _probe() -> object:
        return _git(sandbox, "rev-parse", "HEAD", check=True).stdout.strip()

    return _probe


@contextlib.contextmanager
def merge_lock(lock_path: Path) -> Iterator[None]:
    """Exclusive cross-process lock around base-branch mutations. Per-task
    leases keep two workers off the same task, but two workers finishing
    *different* tasks would both fast-forward the one shared base. This flock
    serializes those merges (and the task-file commit) — finer-grained than the
    old ``.worker.lock``: peers still run tasks, only the merge is exclusive."""
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    fd = os.open(str(lock_path), os.O_CREAT | os.O_RDWR, 0o644)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX)
        yield
    finally:
        fcntl.flock(fd, fcntl.LOCK_UN)
        os.close(fd)


def phase_of_task_file(task_file: Path, tasks_dir: Path) -> str:
    """Phase = the directory under ``active/`` a task lives in (``_root`` when
    directly under ``active/``). Drives the ``flywheel/<phase>/<task-id>``
    branch name."""
    active_root = (tasks_dir / "active").resolve()
    try:
        rel = task_file.resolve().relative_to(active_root)
    except ValueError:
        return "_root"
    parent = rel.parent
    return "_root" if str(parent) == "." else str(parent)


# --- git submit strategy ----------------------------------------------------


def _bounded_output_excerpt(
    *segments: str, bound: int = GATE_EXCERPT_MAX_BYTES
) -> str:
    """Combine a deciding check's captured output into one raw tail bounded to
    ``bound`` bytes, mirroring the held-out gate's excerpt builder (spec 00073,
    criterion 11): non-empty segments join with newlines, then the combined
    text is re-capped as a *tail* so the final content survives when the check
    emitted more than the bound. Stored raw -- redaction is a render-time
    concern, never applied here (spec 00074, D-3 / spec 00073, D-2)."""
    combined = "\n".join(s for s in segments if s)
    encoded = combined.encode("utf-8")
    if len(encoded) <= bound:
        return combined
    return encoded[-bound:].decode("utf-8", errors="replace")


def _payload_excerpt(payload: Mapping[str, object]) -> str:
    """Bounded output tail for a re-verify command grader's receipt, read from
    the same ``stdout_tail`` / ``stderr_tail`` / ``spawn_error`` payload keys
    the command runner records (identical to the held-out gate's source)."""
    segments = [
        value
        for key in ("stdout_tail", "stderr_tail", "spawn_error")
        for value in (payload.get(key),)
        if isinstance(value, str) and value
    ]
    return _bounded_output_excerpt(*segments)


class _ReverifyRecorder:
    """Minimal in-memory ``GraderResultStore`` for submit-time re-verification.

    Post-rebase grader runs are a merge gate, not lifecycle history: the
    run's authoritative receipts were persisted in-run, and the store's
    ``(run_id, attempt_number, ordinal)`` key has no slot for a re-check.
    Outcomes are logged instead.
    """

    def __init__(self) -> None:
        self.records: list[GraderResultRecord] = []

    def append_grader_result(
        self, result: GraderResultRecord
    ) -> GraderResultRecord:
        self.records.append(result)
        return result

    def list_grader_results(
        self, run_id: str, attempt_number: int
    ) -> list[GraderResultRecord]:
        return [
            r
            for r in self.records
            if r.run_id == run_id and r.attempt_number == attempt_number
        ]


class GitWorktreeSubmitter:
    """Provisions per-task worktrees and merges/parks them on completion.

    The reference :class:`flywheel_orchestrator.SubmitStrategy`:
    :meth:`prepare_sandbox` may raise :class:`PrepareSandboxError` (skips
    that task); :meth:`submit` never raises (records its own park/merge
    outcome). Passed whole to ``orchestrate(strategy=...)``.
    """

    def __init__(
        self,
        *,
        repo_root: Path,
        tasks_dir: Path,
        worktrees_dir: Path,
        phase_base: str,
        lock_path: Path,
        log: Logger,
        protected_paths: Sequence[str] = (),
        setup_command: str | None = None,
        on_done: str = "destroy",
        on_failure: str = "park",
        store: LandingLedger | None = None,
        grader_env: Mapping[str, str] | None = None,
        verify_command: str | None = None,
        held_out_source: HeldOutGraderSource | None = None,
        disk_preflight: DiskPreflight | None = None,
        recovery_agent_max_turns: int = 0,
        recovery_agent_max_wall_seconds: float = 900.0,
        recovery_agent_model: str | None = None,
        resolve_conflict: ConflictResolver | None = None,
    ) -> None:
        self.repo_root = repo_root
        self.tasks_dir = tasks_dir
        self.worktrees_dir = worktrees_dir
        self.phase_base = phase_base
        self.lock_path = lock_path
        self.log = log
        self.protected_paths = tuple(protected_paths)
        self.setup_command = setup_command
        # The full environment submit-time re-verification runs command graders
        # with (resolved [sandbox.env]); None inherits the worker's environment,
        # byte-identical to before. Shares the in-run grader env so a post-rebase
        # re-verify builds against the same cache as the run it re-checks.
        self.grader_env = grader_env
        # Standing build invariant ([submit] verify, spec 00064): a repo-wide
        # command re-run under the merge lock against the exact tree about to
        # become the base, on every land path, independent of the task's own
        # graders. None => no gate (back-compat). Runs with grader_env so the
        # build shares the same cache/toolchain as the in-run graders.
        self.verify_command = verify_command
        # Declared held-out landing gate (spec 00051), re-run against the MERGED
        # candidate tree on the merge-fallback recovery rung (spec 00076, D-2):
        # the orchestrator gates the pre-merge tree, but a merge produces a tree
        # no grader saw, so nothing lands on the new rung that the held-out gate
        # would block. None => no gate (byte-identical to an ungated land); the
        # clean-FF and rebase rungs are unaffected (their gating is unchanged).
        self.held_out_source = held_out_source
        # Submit-time retention ([sandbox.retention], spec 00041). Defaults
        # reproduce today's hardcoded behavior: a DONE branch's worktree is
        # destroyed after the merge, a non-DONE worktree is parked for
        # forensics. ``on_done="preserve"`` keeps a merged worktree for
        # inspection; ``on_failure="destroy"`` removes a failed one (no
        # forensics).
        self.on_done = on_done
        self.on_failure = on_failure
        # When present, submit() records a queryable LANDING_PARKED event on
        # the run's ledger for a park outcome (uncommitted-work or
        # divergent-base). None (e.g. a bare direct construction) degrades to
        # the log-only park the worktree still preserves.
        self.store = store
        # Disk/inode preflight guarding the authoritative ledger write below
        # (_record_landing_park). Default-on: a bare construction still probes
        # free space/inodes ahead of the append so a near-full disk yields a
        # queryable degraded-space record instead of an ENOSPC crash that tears
        # the store row. Threshold is configurable by injecting a DiskPreflight;
        # on a healthy host the defaults never trip, so landing proceeds as
        # before.
        self.disk_preflight = (
            disk_preflight
            if disk_preflight is not None
            else DiskPreflight(log=log)
        )
        # Bounded agentic conflict-resolution rung ([submit]
        # recovery_agent_max_turns / recovery_agent_max_wall_seconds, spec 00076
        # criterion 4). When the merge-fallback merge itself conflicts and
        # max_turns > 0, a single bounded agent session (the injected resolver,
        # defaulting to a real SDK run behind the lazy flywheel_core._sdk
        # boundary) resolves the conflict; the resolved tree lands only after the
        # SAME merged-tree re-verification bar the merge-fallback rung enforces.
        # max_turns == 0 (the default for a bare construction) disables the rung
        # entirely -- a merge conflict parks exactly as merge-fallback does. The
        # resolver is injectable so tests never touch the real SDK (D-4).
        self.recovery_agent_max_turns = recovery_agent_max_turns
        self.recovery_agent_max_wall_seconds = recovery_agent_max_wall_seconds
        self.recovery_agent_model = recovery_agent_model
        self._resolve_conflict = resolve_conflict or _default_resolve_conflict

    def _branch(self, task_id: str, phase: str) -> str:
        return f"flywheel/{phase}/{task_id}"

    def _worktree(self, task_id: str) -> Path:
        return self.worktrees_dir / task_id

    def _landing_base(self, phase: str) -> str:
        """The ref a task in ``phase`` verifies against and lands onto,
        resolved per submit request.

        The merge/pr strategies land onto the single configured base regardless
        of phase, so this returns :attr:`phase_base` unchanged. The phase
        strategy overrides it to derive a per-phase integration branch (spec
        00079, D-1). Read-only: it never materializes a ref, so a task that
        parks never creates one; use :meth:`_materialize_landing_base` at the
        actual fast-forward to fork the branch on the phase's first landing.
        """
        return self.phase_base

    def _materialize_landing_base(self, phase: str, base: str) -> str:
        """Ensure the ref a passing task fast-forwards onto exists and return
        it, under the caller's merge lock.

        A no-op for the merge/pr strategies (the configured base always exists),
        so it returns ``base`` unchanged and landing is byte-identical to today.
        The phase strategy overrides it to fork ``flywheel/phase/<phase>`` from
        the then-current true base on the phase's first landing; the true base
        is never advanced (spec 00079, criterion 1).
        """
        return base

    def _branch_exists(self, branch: str) -> bool:
        return (
            _git(
                self.repo_root,
                "show-ref",
                "--verify",
                "--quiet",
                f"refs/heads/{branch}",
            ).returncode
            == 0
        )

    def _is_registered_worktree(self, worktree: Path) -> bool:
        listing = _git(
            self.repo_root, "worktree", "list", "--porcelain"
        ).stdout
        registered = {
            line[len("worktree ") :]
            for line in listing.splitlines()
            if line.startswith("worktree ")
        }
        return str(worktree) in registered or str(worktree.resolve()) in registered

    def scrub_worktree_locks(self, worktree: Path, branch: str) -> None:
        """Clear stale git state a SIGKILL'd run leaves in a reused worktree
        (``index.lock``/``HEAD.lock``, a half-finished rebase/merge/cherry-pick)
        so the retry's first git call doesn't fail confusingly."""
        res = _git(worktree, "rev-parse", "--git-dir")
        if res.returncode != 0:
            return
        worktree_git_dir = Path(res.stdout.strip())
        if not worktree_git_dir.is_absolute():
            worktree_git_dir = (worktree / worktree_git_dir).resolve()

        main = _git(self.repo_root, "rev-parse", "--git-dir")
        main_git_dir: Path | None = None
        if main.returncode == 0:
            main_git_dir = Path(main.stdout.strip())
            if not main_git_dir.is_absolute():
                main_git_dir = (self.repo_root / main_git_dir).resolve()

        for name in ("index.lock", "HEAD.lock"):
            (worktree_git_dir / name).unlink(missing_ok=True)
        if main_git_dir is not None:
            (main_git_dir / "refs" / "heads" / f"{branch}.lock").unlink(
                missing_ok=True
            )

        if (worktree_git_dir / "rebase-merge").is_dir() or (
            worktree_git_dir / "rebase-apply"
        ).is_dir():
            self.log(f"Scrub: aborting in-progress rebase in {worktree}")
            _git(worktree, "rebase", "--abort")
        if (worktree_git_dir / "MERGE_HEAD").is_file():
            self.log(f"Scrub: aborting in-progress merge in {worktree}")
            _git(worktree, "merge", "--abort")
        if (worktree_git_dir / "CHERRY_PICK_HEAD").is_file():
            self.log(f"Scrub: aborting in-progress cherry-pick in {worktree}")
            _git(worktree, "cherry-pick", "--abort")

    def _rebase_parked_branch(
        self, worktree: Path, branch: str, base: str | None = None
    ) -> bool:
        """Rebase a parked branch onto the current base (so its flywheel source
        matches the live DB schema). ``True`` if up to date or rebased cleanly,
        ``False`` on conflict.

        ``base`` is the ref to rebase onto; ``None`` falls back to
        :attr:`phase_base` (the merge/pr landing base), so the phase strategy
        rebases a reused worktree onto its integration branch instead."""
        base = self.phase_base if base is None else base
        res = _git(
            self.repo_root,
            "rev-list",
            "--count",
            f"{branch}..{base}",
        )
        behind = (
            int(res.stdout.strip())
            if res.returncode == 0 and res.stdout.strip().isdigit()
            else 0
        )
        if behind == 0:
            self.log(
                f"Reusing parked worktree on {branch}; prior commits carry "
                f"forward."
            )
            return True
        self.log(
            f"Parked {branch} is {behind} commit(s) behind {base}; "
            f"rebasing..."
        )
        if _git(worktree, "rebase", base).returncode == 0:
            self.log("Rebase clean; prior commits carry forward.")
            return True
        _git(worktree, "rebase", "--abort")
        self.log(
            f"Rebase failed against {base}; discarding parked "
            f"worktree+branch and starting fresh."
        )
        return False

    def _add_worktree(self, worktree: Path, *args: str) -> None:
        proc = _git(self.repo_root, "worktree", "add", str(worktree), *args)
        if proc.returncode != 0:
            raise PrepareSandboxError(
                f"git worktree add failed for {worktree}: {proc.stderr.strip()}"
            )
        self._set_commit_identity(worktree)

    def _set_commit_identity(self, worktree: Path) -> None:
        """Establish the fixed commit identity on a freshly provisioned
        worktree so an in-sandbox ``git commit`` succeeds with no global or
        system git identity present. Written to the worktree's repo-local
        config (read by the in-worktree git process regardless of ``HOME`` /
        ``GIT_CONFIG_NOSYSTEM``); a reused parked worktree already carries it.
        The worker itself still authors no commit (D-1)."""
        _git(worktree, "config", "user.name", WORKTREE_COMMIT_IDENTITY_NAME)
        _git(worktree, "config", "user.email", WORKTREE_COMMIT_IDENTITY_EMAIL)

    def _discard_and_recreate(
        self, worktree: Path, branch: str, base: str | None = None
    ) -> None:
        base = self.phase_base if base is None else base
        _git(self.repo_root, "worktree", "remove", "--force", str(worktree))
        _git(self.repo_root, "branch", "-D", branch)
        self._add_worktree(worktree, "-b", branch, base)

    def _guard_done_branch(
        self,
        run_id: str | None,
        branch: str,
        worktree: Path,
        base: str | None = None,
    ) -> None:
        """Refuse to discard a parked worktree+branch that belongs to a DONE
        run (spec 00076, D-3): automation never deletes verified work.

        A DONE run's branch that could not fast-forward or rebase carries
        verified commits pending an unattended landing re-drive; the reuse
        path's discard-on-failed-rebase must not throw that away. When the run
        owning ``branch`` finalized ``DONE`` this raises
        :class:`PrepareSandboxError` so the task is skipped for the session with
        its branch ref and parked worktree intact. A non-DONE retry-reuse (the
        run is being resumed, or a fresh run with no resolvable status) is
        untouched -- discard proceeds exactly as before. Best-effort: no store
        handle, no ``run_id``, or a store read error reads as not-done, so the
        pre-existing discard behavior is unchanged wherever DONE-ness cannot be
        established."""
        base = self.phase_base if base is None else base
        if not self._run_is_done(run_id):
            return
        self.log(
            f"parked branch {branch} belongs to a DONE run and cannot rebase "
            f"onto {base}; preserving branch+worktree for landing "
            f"recovery instead of discarding verified work"
        )
        raise PrepareSandboxError(
            f"{branch} is a DONE run's branch that cannot rebase onto "
            f"{base}; refusing to discard verified work, worktree "
            f"preserved at {worktree}"
        )

    def _run_is_done(self, run_id: str | None) -> bool:
        """Whether the run identified by ``run_id`` finalized ``DONE``.

        Best-effort read through the ledger handle: ``None`` store/``run_id`` or
        any store error yields ``False`` so a status read never wedges
        :meth:`prepare_sandbox` (which must still provision or skip cleanly)."""
        if self.store is None or run_id is None:
            return False
        try:
            lifecycle = self.store.load_lifecycle(run_id)
        except Exception:  # noqa: BLE001 - a status read must not wedge prepare
            return False
        return lifecycle is not None and lifecycle.status is Status.DONE

    def prepare_sandbox(self, req: SandboxRequest) -> Path:
        """Provision (or reuse) the worktree a task runs in; return its path.
        Reuses a parked worktree+branch on retry (rebasing onto base first),
        recreates it when only the branch survived a sweep, and refuses to
        clobber half-present operator state (:class:`PrepareSandboxError`).
        Every freshly created directory gets the policy's sandbox setup
        command; a reused parked worktree skips it (its environment
        survived with it)."""
        task_id = req.task_id
        phase = phase_of_task_file(req.task_file, self.tasks_dir)
        worktree = self._worktree(task_id)
        branch = self._branch(task_id, phase)
        # The ref a fresh/reused worktree branches from. Merge/pr: the single
        # configured base. Phase strategy: the phase's integration branch when
        # it exists (so tasks stack), else the true base (spec 00079).
        base = self._landing_base(phase)

        worktree_present = worktree.is_dir()
        branch_present = self._branch_exists(branch)

        if worktree_present and branch_present:
            if not self._is_registered_worktree(worktree):
                raise PrepareSandboxError(
                    f"{worktree} exists but is not a registered worktree; "
                    f"refusing to clobber"
                )
            self.scrub_worktree_locks(worktree, branch)
            if self._rebase_parked_branch(worktree, branch, base):
                return worktree
            self._guard_done_branch(req.run_id, branch, worktree, base)
            self._discard_and_recreate(worktree, branch, base)
            self._run_setup(worktree)
            return worktree

        if (not worktree_present) and branch_present:
            self.log(
                f"Recreating worktree on existing branch {branch} (directory "
                f"was removed; ref survived)."
            )
            self._add_worktree(worktree, branch)
            if not self._rebase_parked_branch(worktree, branch, base):
                self._guard_done_branch(req.run_id, branch, worktree, base)
                self._discard_and_recreate(worktree, branch, base)
            self._run_setup(worktree)
            return worktree

        if worktree_present and (not branch_present):
            raise PrepareSandboxError(
                f"{worktree} exists but no branch {branch}; refusing to "
                f"clobber. Remove the directory manually."
            )

        self._add_worktree(worktree, "-b", branch, base)
        self._run_setup(worktree)
        return worktree

    def _run_setup(self, worktree: Path) -> None:
        """Run the policy's ``[sandbox] setup`` command (shell) inside a
        newly provisioned worktree — dependency install, codegen — before
        the agent enters, so tasks never pay discovery cost for a bare
        checkout. Failure raises :class:`PrepareSandboxError`: the task is
        skipped this session rather than run in a half-provisioned sandbox.
        """
        if self.setup_command is None:
            return
        self.log(f"sandbox setup: {self.setup_command!r} in {worktree}")
        proc = subprocess.run(
            self.setup_command,
            shell=True,
            cwd=worktree,
            text=True,
            capture_output=True,
            check=False,
        )
        if proc.returncode != 0:
            tail = (proc.stderr or proc.stdout).strip()[-2000:]
            raise PrepareSandboxError(
                f"sandbox setup {self.setup_command!r} failed in {worktree} "
                f"(exit {proc.returncode}): {tail}"
            )

    def submit(self, req: SubmitRequest) -> None:
        """Merge a DONE task's branch into the base, or park the worktree.
        Never raises (orchestrate calls this under the task's lease); any git
        failure is logged and the worktree left parked for inspection."""
        try:
            self._submit(req)
        except Exception as exc:  # noqa: BLE001 - must not escape into orchestrate
            worktree = self._worktree(req.task_id)
            self.log(
                f"submit error for {req.task_id} ({type(exc).__name__}: "
                f"{exc}); worktree left parked at {worktree}"
            )
            # Audit-witness for the swallowed error: the land is still
            # suppressed (the exception stays swallowed, the worktree stays
            # parked); this only records why. _record_landing_park never
            # raises, so submit() still cannot escape into orchestrate.
            self._record_landing_park(
                req.run_id,
                park_kind=PARK_KIND_SUBMIT_ERROR,
                detail=(
                    f"submit raised {type(exc).__name__}: {exc}; worktree "
                    f"left parked at {worktree}"
                ),
            )

    def is_landable(self, req: SubmitRequest) -> LandabilityVerdict:
        """Report whether the run produced a committed, non-empty change.

        The :data:`~flywheel_orchestrator.LandabilityProbe` hook (spec 00061,
        decision D-3): a change is landable iff the worktree is clean (no
        uncommitted modifications or untracked files) and there is at least one
        commit with a non-empty diff against the branch's base. It reuses the
        exact ``git status --porcelain`` + ``git rev-list --count base..branch``
        inspections :meth:`_submit` lands on.

        Strictly read-only: it inspects the worktree/branch and returns a
        verdict; it never mutates, merges, parks, rebases, or cleans up — the
        landing decisions stay in :meth:`submit`. The orchestrator calls this at
        the post-run ``Status.DONE`` site, before ``submit``.
        """
        phase = phase_of_task_file(req.task_file, self.tasks_dir)
        worktree = self._worktree(req.task_id)
        branch = self._branch(req.task_id, phase)
        base = self._landing_base(phase)

        porcelain = _git(worktree, "status", "--porcelain").stdout
        if porcelain.strip():
            return LandabilityVerdict(
                landable=False,
                reason=(
                    f"uncommitted changes in the worktree on {branch}: the "
                    f"agent finished without committing its edits"
                ),
            )

        if self._commit_count(branch, base) == 0:
            return LandabilityVerdict(
                landable=False,
                reason=(
                    f"no commits beyond {base} on {branch}: the "
                    f"run produced an empty diff against its base"
                ),
            )

        return LandabilityVerdict(landable=True)

    def _submit(self, req: SubmitRequest) -> None:
        task_id = req.task_id
        phase = phase_of_task_file(req.task_file, self.tasks_dir)
        worktree = self._worktree(task_id)
        branch = self._branch(task_id, phase)

        if req.status != Status.DONE:
            # failed / interrupted / any non-done terminal: park for forensics
            # (default), or destroy when [sandbox.retention] on_failure asks.
            self._teardown_on_failure(worktree, branch, req.status)
            return

        # All base-branch mutations are serialized across worker processes.
        with merge_lock(self.lock_path):
            # The ref this task lands onto, resolved per request under the lock.
            # Merge/pr: the single configured base. Phase strategy: the phase's
            # integration branch (materialized at the fast-forward, not here, so
            # a task that parks never creates one) -- spec 00079.
            base = self._landing_base(phase)
            porcelain = _git(worktree, "status", "--porcelain").stdout
            if porcelain.strip():
                self.log(
                    f"DONE with uncommitted changes on {branch}; parking "
                    f"worktree at {worktree}"
                )
                self._record_landing_park(
                    req.run_id,
                    park_kind="uncommitted-work",
                    detail=(
                        f"DONE with an uncommitted tree on {branch}; worktree "
                        f"preserved at {worktree}"
                    ),
                )
                return

            commit_count = self._commit_count(branch, base)
            if commit_count == 0:
                # Legitimate no-op: work already on base, or inspection-only
                # graders with no diff. The lifecycle row is the source of
                # truth; just clean up the empty branch+worktree.
                self.log(
                    f"{task_id} reached DONE with no commits beyond "
                    f"{base}; nothing to merge"
                )
                self._cleanup(worktree, branch)
                return

            violations = self._protected_violations(branch, base)
            if violations:
                self.log(
                    f"{task_id} touches protected path(s) "
                    f"{', '.join(violations)}; refusing to merge, parking "
                    f"worktree at {worktree}"
                )
                self._record_landing_park(
                    req.run_id,
                    park_kind=PARK_KIND_PROTECTED_PATHS,
                    detail=(
                        f"{branch} touches protected path(s) "
                        f"{', '.join(violations)}; refusing to merge, worktree "
                        f"preserved at {worktree}"
                    ),
                )
                return

            # Clean-FF path: when the branch already contains the current base
            # its tree IS the tree that will become the base, so the standing
            # build invariant ([submit] verify) gates it here, before the FF.
            # Under the merge lock the base cannot move, so this ancestry check
            # predicts the FF outcome exactly.
            if self._is_ancestor(base, branch):
                verify_passed, verify_receipts = self._standing_verify(
                    req, worktree
                )
                if not verify_passed:
                    self.log(
                        f"standing verify failed for {branch}; refusing to "
                        f"merge, parking worktree at {worktree}"
                    )
                    self._record_landing_park(
                        req.run_id,
                        park_kind="standing-verify",
                        detail=(
                            f"{branch} failed the standing build invariant "
                            f"([submit] verify) against the tree it would land; "
                            f"worktree preserved at {worktree}"
                        ),
                        receipts=verify_receipts,
                    )
                    return
                # Only now that the tree has passed do we materialize the ref it
                # lands onto (a no-op for merge/pr; the phase strategy creates the
                # integration branch at the current base on the phase's first
                # landing -- never on a park -- spec 00079). Stamp the exact tree
                # just verified against that ref, then FF it (spec 00078).
                land_ref = self._materialize_landing_base(phase, base)
                self._stamp_trailers(req, worktree, branch, base=land_ref)
                if self._ff_merge(branch, land_ref):
                    landed_ref = _git(
                        self.repo_root, "rev-parse", land_ref
                    ).stdout.strip()
                    self.log(
                        f"Merged {branch} into {land_ref} "
                        f"({commit_count} commit(s))"
                    )
                    self._record_landing(
                        req.run_id,
                        strategy=LANDING_STRATEGY_MERGE,
                        landed_ref=landed_ref,
                        rung=RUNG_FAST_FORWARD,
                    )
                    self._teardown_on_done(worktree, branch)
                    return

            # FF failed (base advanced): rebase once, re-verify, retry FF, and
            # when the rebase itself conflicts fall through to the merge-fallback
            # recovery rung (spec 00076, D-1) rather than parking here.
            self.log(f"FF failed for {branch}; rebasing onto {base}")
            if _git(worktree, "rebase", base).returncode != 0:
                _git(worktree, "rebase", "--abort")
                self._merge_fallback(
                    req, worktree, branch, commit_count, base=base
                )
                return
            reverify_passed, reverify_receipts = self._reverify(req, worktree)
            if not reverify_passed:
                self.log(
                    f"post-rebase re-verification failed for {branch}; "
                    f"parking worktree at {worktree}"
                )
                # A failed re-verify is a divergent-base park: the branch cannot
                # land because it no longer verifies against the advanced base
                # it rebased onto. Record the deciding graders' output so the
                # park is diagnosable from the store alone (spec 00074).
                self._record_landing_park(
                    req.run_id,
                    park_kind="divergent-base",
                    detail=(
                        f"{branch} failed post-rebase re-verification against "
                        f"the advanced {base}; its command graders no "
                        f"longer pass on the rebased tree; worktree preserved at "
                        f"{worktree}"
                    ),
                    receipts=reverify_receipts,
                )
                return
            # The rebased worktree is now the exact post-merge tree: gate it on
            # the standing build invariant before the FF, same as the clean path.
            verify_passed, verify_receipts = self._standing_verify(
                req, worktree
            )
            if not verify_passed:
                self.log(
                    f"post-rebase standing verify failed for {branch}; "
                    f"parking worktree at {worktree}"
                )
                self._record_landing_park(
                    req.run_id,
                    park_kind="standing-verify",
                    detail=(
                        f"{branch} failed the standing build invariant "
                        f"([submit] verify) against the rebased tree it would "
                        f"land; worktree preserved at {worktree}"
                    ),
                    receipts=verify_receipts,
                )
                return
            # Materialize the landing ref (first-landing create for the phase
            # strategy; no-op for merge/pr), then the same message-only stamp on
            # the rebased tree before its FF: the rebased worktree is the exact
            # post-merge tree (spec 00078).
            land_ref = self._materialize_landing_base(phase, base)
            self._stamp_trailers(req, worktree, branch, base=land_ref)
            if self._ff_merge(branch, land_ref):
                landed_ref = _git(
                    self.repo_root, "rev-parse", land_ref
                ).stdout.strip()
                self.log(
                    f"Merged {branch} into {land_ref} after rebase "
                    f"({commit_count} commit(s))"
                )
                self._record_landing(
                    req.run_id,
                    strategy=LANDING_STRATEGY_MERGE,
                    landed_ref=landed_ref,
                    rung=RUNG_REBASE,
                )
                self._teardown_on_done(worktree, branch)
                return
            self.log(
                f"post-rebase FF failed for {branch}; parking worktree at "
                f"{worktree}"
            )
            self._record_landing_park(
                req.run_id,
                park_kind="divergent-base",
                detail=(
                    f"{branch} cannot fast-forward {land_ref} even after "
                    f"a clean rebase + re-verify; worktree preserved at "
                    f"{worktree}"
                ),
            )

    def _merge_fallback(
        self,
        req: SubmitRequest,
        worktree: Path,
        branch: str,
        commit_count: int,
        *,
        base: str,
    ) -> None:
        """Merge-fallback recovery rung (spec 00076, D-1/D-2): land a DONE
        branch that can neither fast-forward nor cleanly rebase onto the
        advanced base by merging the base into the branch (``--no-ff``) and
        re-verifying the MERGED candidate tree at rebase parity before the
        fast-forward.

        The full landing bar runs against the merged tree -- the task's command
        graders, the standing build invariant (``[submit] verify``), then the
        declared held-out gate -- so nothing lands that was not verified against
        the exact base it lands on (D-2). Any failure -- the merge itself
        conflicts, or any check fails -- resets the branch to its pre-merge tip
        (leaving the base ref byte-identical), records no ``Landed`` event, and
        parks the branch+worktree under a queryable park kind (D-3). On success
        the base fast-forwards to the merge commit (whose parent is the original
        branch tip, so the branch's commits become ancestors of the advanced
        base -- criterion 1) and a ``Landed`` event names the merge-fallback
        rung.

        Runs entirely under the merge lock the caller holds, so the base cannot
        move between the merge and the fast-forward. Never raises: like every
        other landing leaf it records its own outcome. When the merge itself
        conflicts and the bounded agentic resolution rung is armed ([submit]
        ``recovery_agent_max_turns`` > 0), this escalates to :meth:`_agent_resolve`
        with the conflicted worktree left in place (spec 00076, criterion 4);
        with the rung disabled (``max_turns`` == 0, the default) it parks under
        ``merge-conflict`` exactly as before.
        """
        self.log(
            f"rebase failed for {branch}; attempting merge-fallback of "
            f"{base} into {branch}"
        )
        pre_merge = _git(worktree, "rev-parse", "HEAD").stdout.strip()
        # The worker authors this merge commit (its own landing bookkeeping, not
        # a change to the agent's work -- D-1); a reused parked worktree may lack
        # a commit identity, so (re)establish the fixed one before committing.
        self._set_commit_identity(worktree)
        merge = _git(
            worktree,
            "merge",
            "--no-ff",
            "-m",
            f"flywheel: merge-fallback land of {branch} onto {base}",
            base,
        )
        if merge.returncode != 0:
            # The merge itself conflicts: there is no clean candidate tree to
            # re-verify. When the bounded agentic resolution rung is armed
            # ([submit] recovery_agent_max_turns > 0), escalate to it with the
            # conflicted, in-progress merge left in the worktree for the session
            # to resolve (spec 00076, criterion 4). Otherwise abort back to the
            # pre-merge tip (no in-progress merge state remains) and park.
            if self.recovery_agent_max_turns > 0:
                self._agent_resolve(
                    req, worktree, branch, commit_count, pre_merge, base=base
                )
                return
            _git(worktree, "merge", "--abort")
            self.log(
                f"merge-fallback of {base} into {branch} "
                f"conflicted; parking worktree at {worktree}"
            )
            self._record_landing_park(
                req.run_id,
                park_kind=PARK_KIND_MERGE_CONFLICT,
                detail=(
                    f"{branch} cannot fast-forward or rebase onto "
                    f"{base}; the merge-fallback of the base into "
                    f"the branch conflicted; worktree preserved at {worktree}"
                ),
            )
            return

        # The merged worktree is the exact tree that would become the base.
        # Re-verify at rebase parity, each check against the merged tree: task
        # command graders, then the standing build invariant, then the declared
        # held-out gate. Any failure resets to the pre-merge tip (base untouched)
        # and parks with the deciding check's receipts.
        reverify_passed, reverify_receipts = self._reverify(req, worktree)
        if not reverify_passed:
            _git(worktree, "reset", "--hard", pre_merge)
            self.log(
                f"merge-fallback re-verification failed for {branch}; parking "
                f"worktree at {worktree}"
            )
            self._record_landing_park(
                req.run_id,
                park_kind=PARK_KIND_DIVERGENT_BASE,
                detail=(
                    f"{branch} failed post-merge re-verification against the "
                    f"merged {base}; its command graders no longer "
                    f"pass on the merged tree; worktree preserved at {worktree}"
                ),
                receipts=reverify_receipts,
            )
            return
        verify_passed, verify_receipts = self._standing_verify(req, worktree)
        if not verify_passed:
            _git(worktree, "reset", "--hard", pre_merge)
            self.log(
                f"merge-fallback standing verify failed for {branch}; parking "
                f"worktree at {worktree}"
            )
            self._record_landing_park(
                req.run_id,
                park_kind=PARK_KIND_STANDING_VERIFY,
                detail=(
                    f"{branch} failed the standing build invariant "
                    f"([submit] verify) against the merged tree it would land; "
                    f"worktree preserved at {worktree}"
                ),
                receipts=verify_receipts,
            )
            return
        gate_passed, gate_receipts = self._held_out_gate(req, worktree)
        if not gate_passed:
            _git(worktree, "reset", "--hard", pre_merge)
            self.log(
                f"merge-fallback held-out gate blocked {branch}; parking "
                f"worktree at {worktree}"
            )
            self._record_landing_park(
                req.run_id,
                park_kind=PARK_KIND_HELD_OUT_GATE,
                detail=(
                    f"{branch} was blocked by the declared held-out landing "
                    f"gate on the merged tree it would land; worktree preserved "
                    f"at {worktree}"
                ),
                receipts=gate_receipts,
            )
            return

        # Every rung passed against the merged tree. Materialize the landing ref
        # (create the phase integration branch at the base on first landing; a
        # no-op for merge/pr), then FF it. The base is an ancestor of the merge
        # commit, so under the lock the fast-forward is exact.
        phase = phase_of_task_file(req.task_file, self.tasks_dir)
        land_ref = self._materialize_landing_base(phase, base)
        if not self._ff_merge(branch, land_ref):
            _git(worktree, "reset", "--hard", pre_merge)
            self.log(
                f"merge-fallback FF of {branch} onto {land_ref} was "
                f"refused; parking worktree at {worktree}"
            )
            self._record_landing_park(
                req.run_id,
                park_kind=PARK_KIND_DIVERGENT_BASE,
                detail=(
                    f"{branch} passed merge-fallback re-verification but the "
                    f"fast-forward onto {land_ref} was refused; worktree "
                    f"preserved at {worktree}"
                ),
            )
            return
        landed_ref = _git(
            self.repo_root, "rev-parse", land_ref
        ).stdout.strip()
        self.log(
            f"Merged {branch} into {land_ref} via merge-fallback "
            f"({commit_count} commit(s))"
        )
        self._record_landing(
            req.run_id,
            strategy=LANDING_STRATEGY_MERGE,
            landed_ref=landed_ref,
            rung=RUNG_MERGE_FALLBACK,
        )
        self._teardown_on_done(worktree, branch)

    def _agent_resolve(
        self,
        req: SubmitRequest,
        worktree: Path,
        branch: str,
        commit_count: int,
        pre_merge: str,
        *,
        base: str,
    ) -> None:
        """Bounded agentic conflict-resolution rung (spec 00076, criterion 4).

        Reached only when [submit] ``recovery_agent_max_turns`` > 0 and the
        merge-fallback merge itself conflicted, leaving an in-progress merge with
        conflict markers in ``worktree``. Drives exactly one bounded agent
        session (the injected resolver, defaulting to a real SDK run behind the
        lazy ``flywheel_core._sdk`` boundary) to resolve the conflict, then treats
        the result exactly like the merge-fallback rung: the worker authors the
        merge commit (its own landing bookkeeping -- D-1) and the resolved tree
        re-runs the task command graders, the standing build invariant, and the
        declared held-out gate before the base fast-forwards.

        Any non-landing outcome -- the session crashes or errors, the bound is
        exhausted without a resolved tree, or a re-verification check fails --
        resets the branch to its pre-merge tip (leaving the base ref
        byte-identical, no ``Landed`` record) and parks the branch+worktree,
        recording the session's turn/wall usage on the park. On success the base
        fast-forwards to the resolved merge commit (whose first parent is the
        original branch tip, so the branch's commits become ancestors of the
        advanced base -- criterion 1) and a ``Landed`` event names the
        ``agent-resolved`` rung with the same usage.

        Never raises: a session crash or SDK error is contained here so submit
        parks preserved rather than unwinding (criterion 7). At most one session
        per re-driver pass (D-4).
        """
        self.log(
            f"merge-fallback of {base} into {branch} conflicted; "
            f"escalating to a bounded agent session "
            f"(max_turns={self.recovery_agent_max_turns}, "
            f"max_wall_seconds={self.recovery_agent_max_wall_seconds})"
        )
        prompt = _render_conflict_resolution_prompt(
            req, branch=branch, base=base
        )
        started = time.monotonic()
        try:
            report = self._resolve_conflict(
                ConflictResolutionRequest(
                    prompt=prompt,
                    worktree=worktree,
                    max_turns=self.recovery_agent_max_turns,
                    max_wall_seconds=self.recovery_agent_max_wall_seconds,
                    model=self.recovery_agent_model,
                )
            )
            turns, wall = report.turns, report.wall_seconds
        except Exception as exc:  # noqa: BLE001 - a session crash must not
            # unwind submit (criterion 7): abort the in-progress merge, reset to
            # the pre-merge tip (base ref untouched), and park preserved with the
            # wall we observed. The turn count is unavailable after a crash.
            wall = min(
                time.monotonic() - started,
                self.recovery_agent_max_wall_seconds,
            )
            _git(worktree, "merge", "--abort")
            _git(worktree, "reset", "--hard", pre_merge)
            self.log(
                f"conflict-resolution session for {branch} crashed "
                f"({type(exc).__name__}: {exc}); parking worktree at {worktree}"
            )
            self._record_landing_park(
                req.run_id,
                park_kind=PARK_KIND_MERGE_CONFLICT,
                detail=(
                    f"{branch} merge-fallback conflicted and the bounded "
                    f"conflict-resolution session crashed "
                    f"({type(exc).__name__}: {exc}); worktree preserved at "
                    f"{worktree}"
                ),
                agent_turns=0,
                agent_wall_seconds=wall,
            )
            return

        # The agent's claim is untrusted: decide from git state whether the merge
        # is resolved and, if so, author the merge commit ourselves.
        if not self._complete_agent_merge(worktree):
            _git(worktree, "merge", "--abort")
            _git(worktree, "reset", "--hard", pre_merge)
            self.log(
                f"bounded agent session did not resolve the merge conflict on "
                f"{branch} within its bounds ({turns} turn(s)/{wall:.1f}s); "
                f"parking worktree at {worktree}"
            )
            self._record_landing_park(
                req.run_id,
                park_kind=PARK_KIND_MERGE_CONFLICT,
                detail=(
                    f"{branch} merge-fallback conflicted and the bounded "
                    f"conflict-resolution session did not produce a resolved "
                    f"tree within {turns} turn(s)/{wall:.1f}s; worktree preserved "
                    f"at {worktree}"
                ),
                agent_turns=turns,
                agent_wall_seconds=wall,
            )
            return

        # A resolved, committed merge tree: run the SAME merged-tree landing bar
        # the merge-fallback rung enforces before the base moves, recording the
        # session usage on whichever outcome (land or park) results.
        self._reverify_gate_and_land(
            req,
            worktree,
            branch,
            commit_count,
            pre_merge,
            base=base,
            turns=turns,
            wall=wall,
        )

    def _complete_agent_merge(self, worktree: Path) -> bool:
        """Decide from git state alone whether the bounded session resolved the
        in-progress merge -- and, if so, author the merge commit.

        The agent's claim is untrusted, so resolution is judged mechanically: a
        resolved merge has no unmerged index entries left (every conflicted file
        was resolved *and* staged with ``git add``). Any remaining unmerged path
        -- the session gave up, ran out of turns, or left a conflict unstaged --
        means unresolved and returns ``False``. When the merge is still in
        progress (``MERGE_HEAD`` present) the worker authors the merge commit now
        that the conflicts are staged (its own landing bookkeeping -- D-1); a
        session that committed the merge itself is accepted as-is. A leftover
        dirty working tree after the commit is rejected too, so the tree that is
        re-verified is byte-identical to the tree that lands.
        """
        if _git(worktree, "ls-files", "--unmerged").stdout.strip():
            return False
        merge_in_progress = (
            _git(
                worktree, "rev-parse", "-q", "--verify", "MERGE_HEAD"
            ).returncode
            == 0
        )
        if merge_in_progress:
            commit = _git(
                worktree,
                "commit",
                "--no-edit",
            )
            if commit.returncode != 0:
                return False
        # The verified tree must equal the landed tree: reject any leftover
        # working-tree change (a stray edit or untracked file the session left).
        if _git(worktree, "status", "--porcelain").stdout.strip():
            return False
        return True

    def _reverify_gate_and_land(
        self,
        req: SubmitRequest,
        worktree: Path,
        branch: str,
        commit_count: int,
        pre_merge: str,
        *,
        base: str,
        turns: int,
        wall: float,
    ) -> None:
        """Run the merged-tree landing bar for the agent-resolved rung and land
        or park.

        Applies the same checks the merge-fallback rung runs against a merged
        tree -- task command graders, then the standing build invariant, then the
        declared held-out gate, then a fast-forward-only advance of the base --
        but records the resolution session's ``turns``/``wall`` usage on whichever
        outcome results. Any failure resets the branch to ``pre_merge`` (base ref
        untouched, no ``Landed`` record) and parks with the deciding check's
        receipts; a clean pass fast-forwards the base to the resolved merge
        commit and records a ``Landed`` at the ``agent-resolved`` rung.
        """
        reverify_passed, reverify_receipts = self._reverify(req, worktree)
        if not reverify_passed:
            _git(worktree, "reset", "--hard", pre_merge)
            self.log(
                f"agent-resolved re-verification failed for {branch}; parking "
                f"worktree at {worktree}"
            )
            self._record_landing_park(
                req.run_id,
                park_kind=PARK_KIND_DIVERGENT_BASE,
                detail=(
                    f"{branch} was resolved by a bounded agent session but "
                    f"failed post-merge re-verification against the merged "
                    f"{base}; its command graders no longer pass on "
                    f"the resolved tree; worktree preserved at {worktree}"
                ),
                receipts=reverify_receipts,
                agent_turns=turns,
                agent_wall_seconds=wall,
            )
            return
        verify_passed, verify_receipts = self._standing_verify(req, worktree)
        if not verify_passed:
            _git(worktree, "reset", "--hard", pre_merge)
            self.log(
                f"agent-resolved standing verify failed for {branch}; parking "
                f"worktree at {worktree}"
            )
            self._record_landing_park(
                req.run_id,
                park_kind=PARK_KIND_STANDING_VERIFY,
                detail=(
                    f"{branch} was resolved by a bounded agent session but "
                    f"failed the standing build invariant ([submit] verify) "
                    f"against the resolved tree it would land; worktree preserved "
                    f"at {worktree}"
                ),
                receipts=verify_receipts,
                agent_turns=turns,
                agent_wall_seconds=wall,
            )
            return
        gate_passed, gate_receipts = self._held_out_gate(req, worktree)
        if not gate_passed:
            _git(worktree, "reset", "--hard", pre_merge)
            self.log(
                f"agent-resolved held-out gate blocked {branch}; parking "
                f"worktree at {worktree}"
            )
            self._record_landing_park(
                req.run_id,
                park_kind=PARK_KIND_HELD_OUT_GATE,
                detail=(
                    f"{branch} was resolved by a bounded agent session but was "
                    f"blocked by the declared held-out landing gate on the "
                    f"resolved tree it would land; worktree preserved at "
                    f"{worktree}"
                ),
                receipts=gate_receipts,
                agent_turns=turns,
                agent_wall_seconds=wall,
            )
            return
        phase = phase_of_task_file(req.task_file, self.tasks_dir)
        land_ref = self._materialize_landing_base(phase, base)
        if not self._ff_merge(branch, land_ref):
            _git(worktree, "reset", "--hard", pre_merge)
            self.log(
                f"agent-resolved FF of {branch} onto {land_ref} was "
                f"refused; parking worktree at {worktree}"
            )
            self._record_landing_park(
                req.run_id,
                park_kind=PARK_KIND_DIVERGENT_BASE,
                detail=(
                    f"{branch} passed agent-resolved re-verification but the "
                    f"fast-forward onto {land_ref} was refused; worktree "
                    f"preserved at {worktree}"
                ),
                agent_turns=turns,
                agent_wall_seconds=wall,
            )
            return
        landed_ref = _git(
            self.repo_root, "rev-parse", land_ref
        ).stdout.strip()
        self.log(
            f"Merged {branch} into {land_ref} via agent-resolved "
            f"conflict resolution ({commit_count} commit(s); {turns} "
            f"turn(s)/{wall:.1f}s)"
        )
        self._record_landing(
            req.run_id,
            strategy=LANDING_STRATEGY_MERGE,
            landed_ref=landed_ref,
            rung=RUNG_AGENT_RESOLVED,
            agent_turns=turns,
            agent_wall_seconds=wall,
        )
        self._teardown_on_done(worktree, branch)

    def _held_out_gate(
        self, req: SubmitRequest, worktree: Path
    ) -> tuple[bool, tuple[GateGraderReceipt, ...]]:
        """Run the declared held-out landing gate against the merged candidate
        tree (spec 00076, D-2), returning ``(passed, receipts)``.

        The orchestrator gates the *pre-merge* tree; a merge produces a tree no
        grader saw, so the recovery rung re-runs the held-out gate against the
        merged worktree before it can land -- nothing lands on the new rung that
        the held-out gate would block. No held-out source wired (the common
        case) => passes with no receipts, byte-identical to an ungated land. A
        blocking verdict (a fail, or a fail-closed evaluation error) returns
        ``False`` with one :class:`GateGraderReceipt` per executed held-out
        grader so the park it decides is diagnosable from the store alone. Never
        raises into :meth:`submit`: :func:`evaluate_held_out_gate` fails closed
        on any runner error and returns a verdict.
        """
        if self.held_out_source is None:
            return True, ()
        verdict = evaluate_held_out_gate(
            req.task,
            self.held_out_source,
            committed_tree=worktree,
            run_id=req.run_id,
            env=self.grader_env,
        )
        receipts = tuple(
            GateGraderReceipt(
                grader_name=record.grader_name
                or str(record.grader_spec.get("run", "")),
                passed=record.passed,
                output_excerpt=_payload_excerpt(record.payload),
            )
            for record in verdict.results
        )
        if verdict.blocks_landing:
            self.log(
                f"held-out gate BLOCKED {req.task_id} on the merged tree: "
                f"{verdict.reason}"
            )
        return (not verdict.blocks_landing), receipts

    def _protected_violations(
        self, branch: str, base: str | None = None
    ) -> list[str]:
        """Repo-relative paths the branch touches that match a protected
        pattern (``PurePath.full_match`` glob semantics, ``**`` crosses
        directories).

        The diff is merge-base scoped (``base...branch``) so only the
        branch's own changes count, never what the base did underneath it.
        ``base`` defaults to :attr:`phase_base` (the merge/pr landing base);
        the phase strategy passes the phase's integration branch so the diff
        is scoped to the task's own commits above that branch, not the whole
        phase. This is the merge-time half of the verification trust boundary:
        graders execute inside the tree the agent just mutated, so work
        that rewrites the verification surface itself (grader configs, CI,
        harness state) can pass its own judges — the gate refuses to land
        it regardless.
        """
        if not self.protected_paths:
            return []
        base = self.phase_base if base is None else base
        res = _git(
            self.repo_root,
            "diff",
            "--name-only",
            f"{base}...{branch}",
        )
        if res.returncode != 0:
            # Cannot establish what the branch touches: fail closed.
            return [f"<diff failed: {res.stderr.strip()}>"]
        return [
            path
            for path in res.stdout.splitlines()
            if path
            and any(
                PurePosixPath(path).full_match(pattern)
                for pattern in self.protected_paths
            )
        ]

    def _reverify(
        self, req: SubmitRequest, worktree: Path
    ) -> tuple[bool, tuple[GateGraderReceipt, ...]]:
        """Re-run the task's command graders against the rebased tree.

        A submit-time rebase moves the work onto a base the in-run graders
        never saw: a textually clean rebase can still be semantically broken
        against whatever advanced the base, so the run's receipts no longer
        describe the tree about to land. Nothing merges unless it was
        verified against the exact base it lands on — command graders re-run
        here, cwd'd to the rebased worktree and still under the merge lock so
        the base cannot move again before the FF. Transcript/rubric/manual
        graders judge the agent's work, not the tree, and stay valid across
        a clean rebase; a task with no command graders has nothing
        tree-dependent to re-check.

        Returns the pass/fail verdict paired with one :class:`GateGraderReceipt`
        per re-run command grader (name, outcome, bounded output tail), so a
        park the failing re-verification decides can carry the deciding check's
        output in its record (spec 00074). No command graders => passes with no
        receipts.
        """
        command_count = sum(
            isinstance(g, CommandGrader) for g in req.task.graders
        )
        if command_count == 0:
            self.log(
                f"{req.task_id} has no command graders; nothing to re-verify "
                f"after rebase"
            )
            return True, ()
        records = run_command_graders(
            req.task,
            _ReverifyRecorder(),
            run_id=req.run_id,
            # Merge-gate checks sit outside the lifecycle's attempt
            # numbering; receipts are logged, not persisted.
            attempt_number=0,
            cwd=worktree,
            env=self.grader_env,
        )
        receipts: list[GateGraderReceipt] = []
        for record in records:
            label = record.grader_name or str(record.grader_spec.get("run", ""))
            verdict = "pass" if record.passed else "FAIL"
            self.log(
                f"re-verify {req.task_id}: {verdict} [{label}] "
                f"({record.duration_ms}ms)"
            )
            receipts.append(
                GateGraderReceipt(
                    grader_name=record.grader_name or label,
                    passed=record.passed,
                    output_excerpt=_payload_excerpt(record.payload),
                )
            )
        passed = len(records) == command_count and all(
            r.passed for r in records
        )
        return passed, tuple(receipts)

    def _is_ancestor(self, ancestor: str, rev: str) -> bool:
        """True when ``ancestor`` is reachable from ``rev`` -- i.e. a
        fast-forward of the base to ``rev`` is possible because ``rev`` already
        contains ``ancestor``. ``git merge-base --is-ancestor`` exits 0 for
        ancestor, 1 for not; evaluated in the shared object DB so it holds for
        both the in-tree and out-of-tree FF paths. Under ``merge_lock`` the base
        cannot move, so this predicts the FF outcome exactly."""
        return (
            _git(
                self.repo_root, "merge-base", "--is-ancestor", ancestor, rev
            ).returncode
            == 0
        )

    def _standing_verify(
        self, req: SubmitRequest, worktree: Path
    ) -> tuple[bool, tuple[GateGraderReceipt, ...]]:
        """Run the policy's standing build invariant (``[submit] verify``,
        spec 00064) against the exact tree about to become the base.

        This is the repo-wide "the trunk must build" gate, run under the merge
        lock immediately before the fast-forward and independent of the task's
        own (possibly crate-scoped) command graders. It catches a semantic merge
        skew -- two independently-valid changes whose union does not build -- that
        per-task graders run in isolation cannot. Unset (``None``) => no gate.
        Runs with ``grader_env`` exactly as :meth:`_reverify` runs graders so the
        build shares the same cache/toolchain. A non-zero exit returns ``False``
        (the caller parks); the command never raises into :meth:`submit`.

        Returns the pass/fail verdict paired with a single
        :class:`GateGraderReceipt` capturing the command, its outcome, and a
        bounded tail of its combined stdout/stderr, so the park a failing gate
        decides carries the deciding check's output in its record (spec 00074).
        Unset gate => passes with no receipt."""
        if self.verify_command is None:
            return True, ()
        self.log(
            f"standing verify for {req.task_id}: {self.verify_command!r} in "
            f"{worktree}"
        )
        proc = subprocess.run(
            self.verify_command,
            shell=True,
            cwd=worktree,
            env=dict(self.grader_env) if self.grader_env is not None else None,
            text=True,
            capture_output=True,
            check=False,
        )
        passed = proc.returncode == 0
        if not passed:
            tail = (proc.stderr or proc.stdout).strip()[-2000:]
            self.log(
                f"standing verify FAILED (exit {proc.returncode}) for "
                f"{req.task_id}: {tail}"
            )
        receipt = GateGraderReceipt(
            grader_name=self.verify_command,
            passed=passed,
            output_excerpt=_bounded_output_excerpt(
                proc.stdout or "", proc.stderr or ""
            ),
        )
        return passed, (receipt,)

    def _commit_count(self, branch: str, base: str | None = None) -> int:
        base = self.phase_base if base is None else base
        res = _git(
            self.repo_root,
            "rev-list",
            "--count",
            f"{base}..{branch}",
        )
        return (
            int(res.stdout.strip())
            if res.returncode == 0 and res.stdout.strip().isdigit()
            else 0
        )

    def _stamp_trailers(
        self,
        req: SubmitRequest,
        worktree: Path,
        branch: str,
        *,
        base: str | None = None,
    ) -> None:
        """Stamp harness-authoritative provenance trailers onto the task
        branch's ``base..branch`` commits, then advance the branch and
        worktree onto the rewritten tip (spec 00078, criteria 1/3/4).

        ``base`` defaults to :attr:`phase_base` (the merge path's landing base);
        the pr strategy passes its ``pr_base`` so the stamped range is exactly
        the commits the push publishes beyond the PR base.

        Message-only: :func:`~flywheel_worktree._trailers.stamp_commit_messages`
        recreates every commit over its original tree object, so the tree that
        just passed re-verify/standing-verify is byte-identically the tree that
        lands -- stamping never changes what verification approved. Any
        agent-authored ``Flywheel-*`` trailer is stripped and replaced with the
        harness values (task id, run id, and phase directory name), so a forged
        provenance value cannot survive (D-2). Runs under the caller's merge
        lock, immediately before the fast-forward, so the base cannot move
        between the stamp and the FF; ``reset --hard`` onto the tree-identical
        tip changes no file on disk, only the branch ref the FF reads."""
        phase = phase_of_task_file(req.task_file, self.tasks_dir)
        new_tip = stamp_commit_messages(
            self.repo_root,
            base=base or self.phase_base,
            branch=branch,
            trailers=provenance_trailers(
                task_id=req.task_id, run_id=req.run_id, phase=phase
            ),
        )
        _git(worktree, "reset", "--hard", new_tip, check=True)

    def _base_is_checked_out(self, base: str | None = None) -> bool:
        """True when ``base`` is the operator's checked-out branch in
        ``repo_root`` (the back-compat default case). ``base`` defaults to
        :attr:`phase_base`; a phase integration branch is never checked out, so
        the phase strategy always takes the out-of-tree advance path."""
        base = self.phase_base if base is None else base
        return _checked_out_branch(self.repo_root) == base

    def _ff_merge(self, branch: str, base: str | None = None) -> bool:
        """Fast-forward ``base`` to ``branch``'s tip.

        When ``base`` is the operator's checked-out branch (the unconfigured
        default), advance it in-tree with ``git merge --ff-only`` as before.
        When ``base`` is NOT checked out (a configured landing base or a phase
        integration branch — the safe-landing case), advance its ref out-of-tree
        via ``git fetch . <branch>:<base>`` so the operator's working tree,
        index, and HEAD are never touched. Both paths are fast-forward-only: a
        non-FF advance returns ``False`` and the caller rebases or parks.
        ``base`` defaults to :attr:`phase_base`."""
        base = self.phase_base if base is None else base
        if self._base_is_checked_out(base):
            return (
                _git(self.repo_root, "merge", "--ff-only", branch).returncode
                == 0
            )
        return (
            _git(
                self.repo_root, "fetch", ".", f"{branch}:{base}"
            ).returncode
            == 0
        )

    def _cleanup(self, worktree: Path, branch: str) -> None:
        _git(self.repo_root, "worktree", "remove", str(worktree))
        # Force-delete: every caller has already proven ``branch`` is contained
        # in ``phase_base`` (commit_count == 0, or a successful out-of-tree
        # FF-merge). ``git branch -d`` checks mergedness against the *checked-out
        # HEAD*, not the landing base, so when the base is not the operator's
        # HEAD (the configured safe-landing case) ``-d`` refuses and silently
        # leaks the ref — which a later re-queue then "reuses", skipping sandbox
        # setup. ``-D`` deletes against the established containment instead.
        _git(self.repo_root, "branch", "-D", branch)

    def _teardown_on_done(self, worktree: Path, branch: str) -> None:
        """Dispose a worktree whose branch has just landed in the base.

        ``on_done="destroy"`` (default) removes the worktree+branch as before;
        ``on_done="preserve"`` keeps the worktree dir and branch ref for
        inspection — the work has already merged either way. The empty-branch
        (zero-commit-DONE) case is not routed here; it always cleans up.
        """
        if self.on_done == "preserve":
            self.log(
                f"on_done=preserve; keeping landed worktree at {worktree} "
                f"for inspection"
            )
            return
        self._cleanup(worktree, branch)

    def _teardown_on_failure(
        self, worktree: Path, branch: str, status: Status
    ) -> None:
        """Dispose a non-DONE terminal worktree.

        ``on_failure="park"`` (default) preserves the worktree+branch for
        forensics as before; ``on_failure="destroy"`` removes both, leaving no
        forensics behind.
        """
        if self.on_failure == "destroy":
            self.log(
                f"Lifecycle {status.value}; on_failure=destroy, removing "
                f"worktree at {worktree}"
            )
            self._cleanup(worktree, branch)
            return
        self.log(
            f"Lifecycle {status.value}; worktree preserved at {worktree}"
        )

    def _record_landing_park(
        self,
        run_id: str,
        *,
        park_kind: str,
        detail: str,
        receipts: tuple[GateGraderReceipt, ...] = (),
        agent_turns: int | None = None,
        agent_wall_seconds: float | None = None,
    ) -> None:
        """Append a queryable ``LANDING_PARKED`` audit-witness event for a
        parked DONE run (``park_kind`` is one of
        :data:`~flywheel_core.events.LANDING_PARK_KINDS`, e.g.
        ``"uncommitted-work"``, ``"divergent-base"``, ``"protected-paths"``, or
        ``"submit-error"``).

        The run already finalized ``DONE`` (terminal); this records the park
        cause on the run's ledger via ``append_domain_event`` WITHOUT any
        lifecycle transition — ``LandingParked`` folds to the identity, so the
        status stays ``DONE`` and only ``version`` advances. Best-effort: a
        missing store handle or any store error is logged, never raised, so
        ``submit()`` cannot escape into orchestrate (criterion 7).

        ``receipts`` carries the deciding check's bounded output for a park a
        grader decided -- the standing build invariant or a failing post-rebase
        re-verification (spec 00074) -- and is empty for every other cause,
        whose reason ``park_kind`` / ``detail`` already carry. Shared by both
        the merge strategy and the PR subclass that inherits this method, so a
        grader-decided park carries the same output on either land path.

        ``agent_turns`` / ``agent_wall_seconds`` record a bounded
        conflict-resolution session's usage when the park followed an
        agent-resolution attempt (spec 00076, criterion 4); both are ``None`` for
        every park that ran no session, so those records round-trip unchanged.

        The append is an authoritative ledger write, so it is gated by the
        disk/inode preflight first: when free space or inodes are below
        threshold the preflight records a queryable degraded-space witness and
        this returns WITHOUT attempting the append — declining the crashing
        write before it can tear the store row, rather than catching an
        ENOSPC failure after the fact. Above threshold the write is reached
        exactly as before."""
        if self.store is None:
            return

        store = self.store

        def _append() -> None:
            lifecycle = store.load_lifecycle(run_id)
            if lifecycle is None:
                self.log(
                    f"cannot record landing-parked event: no lifecycle for "
                    f"{run_id}"
                )
                return
            store.append_domain_event(
                LandingParked(
                    run_id=run_id,
                    ts=datetime.now(timezone.utc),
                    park_kind=park_kind,
                    detail=detail,
                    receipts=receipts,
                    agent_turns=agent_turns,
                    agent_wall_seconds=agent_wall_seconds,
                ),
                expected_version=lifecycle.version,
            )

        try:
            self.disk_preflight.guard(
                self.repo_root, _append, run_id=run_id
            )
        except Exception as exc:  # noqa: BLE001 - must not escape submit
            self.log(
                f"failed to record landing-parked event for {run_id} "
                f"({type(exc).__name__}: {exc})"
            )

    def _record_landing(
        self,
        run_id: str,
        *,
        strategy: str,
        landed_ref: str,
        rung: str = "",
        agent_turns: int | None = None,
        agent_wall_seconds: float | None = None,
    ) -> None:
        """Append a queryable ``LANDED`` audit-witness event for a run whose
        branch actually landed, carrying the landed reference (``strategy`` is
        one of :data:`~flywheel_core.events.LANDING_STRATEGIES`; ``landed_ref``
        is the landed commit sha for a merge land or the PR identifier for a PR
        land; ``rung`` names which recovery rung landed the work, one of
        :data:`~flywheel_core.events.LANDING_RUNGS`, and defaults to empty for a
        PR land where the rung concept does not apply).

        ``agent_turns`` / ``agent_wall_seconds`` record the resolution session's
        usage on an ``agent-resolved`` land (spec 00076, criterion 4); both are
        ``None`` for every other rung, so those records round-trip unchanged.

        The success counterpart to :meth:`_record_landing_park`: the caller
        invokes it only *after* the land completed, so an incomplete land leaves
        no ``Landed`` record. The run already finalized ``DONE`` (terminal); this
        records the landing on the run's ledger via ``append_domain_event``
        WITHOUT any lifecycle transition -- ``Landed`` folds to the identity, so
        the status stays ``DONE`` and only ``version`` advances. Best-effort: a
        missing store handle or any store error is logged, never raised, so
        ``submit()`` cannot escape into orchestrate. Gated by the same
        disk/inode preflight as :meth:`_record_landing_park`, since the append is
        an authoritative ledger write."""
        if self.store is None:
            return

        store = self.store

        def _append() -> None:
            lifecycle = store.load_lifecycle(run_id)
            if lifecycle is None:
                self.log(
                    f"cannot record landed event: no lifecycle for {run_id}"
                )
                return
            store.append_domain_event(
                Landed(
                    run_id=run_id,
                    ts=datetime.now(timezone.utc),
                    strategy=strategy,
                    landed_ref=landed_ref,
                    rung=rung,
                    agent_turns=agent_turns,
                    agent_wall_seconds=agent_wall_seconds,
                ),
                expected_version=lifecycle.version,
            )

        try:
            self.disk_preflight.guard(
                self.repo_root, _append, run_id=run_id
            )
        except Exception as exc:  # noqa: BLE001 - must not escape submit
            self.log(
                f"failed to record landed event for {run_id} "
                f"({type(exc).__name__}: {exc})"
            )


class PhaseBranchSubmitter(GitWorktreeSubmitter):
    """Land each task onto a per-phase integration branch (spec 00079).

    Rung derived from the merge strategy: identical provisioning and the full
    verify ladder (uncommitted-work, protected-paths, clean fast-forward,
    rebase-once-then-reverify, ``[submit] verify`` standing gate, and the
    merge-fallback recovery rungs), with one behavioral change -- the ref a
    passing task fast-forwards onto is the phase's integration branch
    ``flywheel/phase/<phase>`` derived per submit request from the task's phase
    directory, not the single configured base.

    The two overrides below are the whole of the difference (D-1). The true
    base is never advanced: the integration branch is a separate ref advanced
    out-of-tree, and the base class's out-of-tree fast-forward path already
    leaves the operator's checkout untouched for any base that is not checked
    out (a phase integration branch never is). Tasks in the same phase stack
    because :meth:`prepare_sandbox` branches a fresh worktree off the
    integration branch once it exists.
    """

    def _phase_branch(self, phase: str) -> str:
        """The integration branch a phase lands its tasks onto."""
        return f"flywheel/phase/{phase}"

    def _landing_base(self, phase: str) -> str:
        """The ref a task in ``phase`` verifies against and lands onto: the
        phase's integration branch once it exists (so tasks stack on top of
        each other and measure only their own commits), else the true base --
        the branch point for the phase's first task, before any landing has
        created the integration branch. Read-only (never materializes)."""
        integration = self._phase_branch(phase)
        if self._branch_exists(integration):
            return integration
        return self.phase_base

    def _materialize_landing_base(self, phase: str, base: str) -> str:
        """Ensure the phase integration branch exists and return it, under the
        caller's merge lock.

        Called only at an actual fast-forward (never on a park), so the first
        passing task in a phase forks ``flywheel/phase/<phase>`` from the
        then-current true base (``base`` is the true base on that first landing;
        the integration branch on every subsequent landing). The true base is
        never advanced -- only the integration ref is (criterion 1). Idempotent:
        once the branch exists this returns it unchanged."""
        integration = self._phase_branch(phase)
        if not self._branch_exists(integration):
            _git(self.repo_root, "branch", integration, base, check=True)
            self.log(
                f"created phase integration branch {integration} at {base}"
            )
        return integration


# --- daemon-side consumer concerns ------------------------------------------


def record_phase_bases(
    repo_root: Path, tasks_dir: Path, lock_path: Path, log: Logger
) -> None:
    """Capture each active phase's base SHA as a ``refs/flywheel/loop-base``
    git ref.

    Runs once per cycle, before any task branch is merged. Pure ref
    plumbing — the worker never creates commits on the operator's branch.
    Idempotent: a phase whose base is already recorded (ref, or a legacy
    committed ``.loop-base`` dotfile) is left untouched (the first-seen SHA
    is the true base; re-runs must never move it forward). Flock'd so two
    workers do not race the check-and-set.
    """
    if not (tasks_dir / "active").is_dir():
        return
    with merge_lock(lock_path):
        recorded = 0
        for phase_dir in iter_active_phase_dirs(tasks_dir):
            if write_phase_base_if_missing(repo_root, phase_dir):
                recorded += 1
        if recorded:
            log(f"Recorded base sha for {recorded} phase(s)")


def archive_phases(
    tasks_dir: Path,
    db_path: Path,
    log: Logger,
    *,
    repo_root: Path | None = None,
    landing_base: str | None = None,
    true_base: str | None = None,
    policy: WorkPolicy | None = None,
    gh: GhRunner | None = None,
) -> None:
    """Move ``active/<phase>`` dirs whose tasks are all done into ``archive/``.

    ``repo_root`` enables the loop-path archive gate
    (:func:`flywheel_core.workflow.archive_completed_phases` reads the phase's
    cumulative diff vs ``.loop-base`` to derive the marker); omitting it
    skips the gate entirely, which matches the legacy ``archive_phases``
    contract. Refusal reasons are reported via the same ``log`` callable
    that announces archived phases so a single log stream tells the
    operator everything the sweep did.

    ``landing_base`` arms the landed predicate (spec 00077): with it (and
    ``repo_root``) threaded, a DONE task archives only when its work is landed
    -- a receipt on its latest run, or its ``flywheel/<phase>/<task-id>`` branch
    head an ancestor of ``landing_base`` at sweep time -- otherwise the phase
    stays active and the blocking task id is reported via ``log``. Pass the
    worker's resolved submit base (``GitWorktreeSubmitter.phase_base``);
    ``None`` preserves the legacy DONE-only contract, which is why the archive
    seam tests that call this helper without a repo/base still archive on
    all-DONE.

    ``true_base`` arms the phase-branch merge predicate (spec 00079, criteria
    4/5/6/8): under the phase strategy a completed phase archives only when its
    ``flywheel/phase/<phase>`` integration branch tip is an ancestor of the true
    base (the phase PR merged as a merge commit). When not passed it is resolved
    from the policy -- ``policy.submit_base``, else the operator's
    currently-checked-out branch -- so the daemon's own sweep and a bare
    ``archive_phases`` caller arm it identically. The predicate arms per-phase on
    the existence of the integration branch, so ``merge``/``pr`` repos (which
    never create one) are unaffected.

    ``policy`` selects the store backend through the orchestrator's store
    factory; ``None`` keeps the historical sqlite-on-``db_path`` behavior.
    A configured ``policy.phase_verify`` runs as the phase-exit integration
    gate (spec 00035): the command runs against the merged base in
    ``repo_root`` before an eligible phase archives, and a non-zero exit
    leaves the phase active with the refusal reported via ``log``.

    The sweep threads the orchestrator claim store through so each archived
    task with a still-surfaced stop row gets a ``stop-resolved`` marker --
    archival is the verified resolution act, so the stranded/stopped status
    surface clears with the phase instead of rendering the stale stop forever.

    Under ``[submit] strategy = "phase"`` (spec 00079) the sweep does not
    archive a completed phase; it hands it to a :class:`~flywheel_worktree.pr.
    PhasePrPublisher` injected as the ``phase_completion`` seam, which evaluates
    ``[phase] verify`` against the phase-branch tree, pushes the phase branch,
    and opens or refreshes exactly one aggregate PR onto the true base. The
    phase stays ``active/`` until that PR merges. ``gh`` overrides the ``gh``
    CLI runner the publisher shells to (tests inject a fake); ``None`` uses the
    real CLI. The publisher is built only for the phase strategy, so ``merge``
    and ``pr`` callers keep today's archival behavior byte-for-byte.
    """
    resolved_true_base = true_base
    if resolved_true_base is None and policy is not None:
        resolved_true_base = policy.submit_base
    if resolved_true_base is None and repo_root is not None:
        resolved_true_base = _checked_out_branch(repo_root)
    store = open_sqlite_bound_store(policy, db_path=db_path)
    try:
        claims = build_claim_store(policy, db_path=db_path)
        # Phase-strategy completion seam (spec 00079, criterion 3): the true
        # base for the phase PR is the operator's base (``submit_pr_base`` or
        # the worker's resolved ``landing_base``), never the phase branch. The
        # publisher is imported lazily here -- ``pr`` imports ``worker`` -- so
        # the module cycle stays broken exactly as the submit-strategy registry
        # does it.
        phase_completion: Callable[[Path, list[Task]], None] | None = None
        if (
            policy is not None
            and policy.submit_strategy == "phase"
            and repo_root is not None
        ):
            pr_base = policy.submit_pr_base or landing_base
            if pr_base is not None:
                from flywheel_worktree.pr import PhasePrPublisher

                phase_completion = PhasePrPublisher(
                    store=store,
                    repo_root=repo_root,
                    tasks_dir=tasks_dir,
                    remote=policy.submit_remote,
                    pr_base=pr_base,
                    phase_verify=policy.phase_verify,
                    log=log,
                    gh=gh,
                )
        try:
            moved = archive_completed_phases(
                tasks_dir,
                store,
                repo_root=repo_root,
                log=log,
                # The phase publisher runs ``[phase] verify`` against the
                # phase-branch tree itself, so the repo-root phase-verify gate
                # is suppressed on the phase path (it would gate the wrong tree).
                phase_verify=(
                    None
                    if phase_completion is not None
                    else (policy.phase_verify if policy is not None else None)
                ),
                landing_base=landing_base,
                true_base=resolved_true_base,
                claims=claims,
                phase_completion=phase_completion,
            )
        finally:
            claims.close()
    finally:
        store.close()
    for dest in moved:
        log(f"Archived phase: {dest}")


# Per-run forensics live in the run telemetry JSONL the harness's sink writes
# at .flywheel/logs/runs/<run_id>.jsonl (spec 00025). The worker renders
# nothing from the store; it holds that directory at or under a configured
# bound via sweep_run_logs (default-on), reclaiming the oldest run files while
# the most-recent runs -- including the one in flight -- always survive.


def sweep_run_logs(runs_dir: Path, max_run_files: int, log: Logger) -> None:
    """Hold ``runs_dir`` at or under ``max_run_files`` per-run JSONL files by
    deleting the oldest (by mtime) and preserving the most-recent
    ``max_run_files``.

    Default-on companion to :func:`retention_sweep` for the telemetry stream:
    the harness's sink writes one ``<run_id>.jsonl`` per run and never rotates
    them, so without this the directory grows unbounded. The newest file -- the
    run in flight -- is always in the surviving set, so an active run is never
    reclaimed. A no-op when the directory is absent, ``max_run_files`` is
    non-positive (retention disabled), or there are already at or fewer than
    ``max_run_files`` files. Reclaim failures on individual files are skipped,
    never raised, so housekeeping cannot escape into the run loop.
    """
    if max_run_files <= 0 or not runs_dir.is_dir():
        return
    entries: list[tuple[float, str, Path]] = []
    for entry in runs_dir.iterdir():
        if entry.suffix != ".jsonl" or not entry.is_file():
            continue
        try:
            mtime = entry.stat().st_mtime
        except OSError:
            continue
        entries.append((mtime, entry.name, entry))
    if len(entries) <= max_run_files:
        return
    # Newest first: (mtime, name) descending keeps exactly the most-recent
    # files and breaks mtime ties deterministically by name. Everything past
    # the bound is the oldest tail, which is what we reclaim.
    entries.sort(key=lambda item: (item[0], item[1]), reverse=True)
    reclaimed = 0
    for _mtime, _name, path in entries[max_run_files:]:
        try:
            path.unlink()
        except OSError:
            continue
        reclaimed += 1
    if reclaimed:
        log(
            f"Run-log retention: reclaimed {reclaimed} old run file(s), "
            f"keeping the most-recent {max_run_files}"
        )


def retention_sweep(
    repo_root: Path,
    worktrees_dir: Path,
    retention_days: int,
    now_ts: float,
    log: Logger,
) -> None:
    """Remove parked worktrees older than the window and their
    ``flywheel/<phase>/<task-id>`` branches. Prunes dangling worktree entries
    first so hand-removed dirs stop blocking new adds."""
    _git(repo_root, "worktree", "prune")
    if not worktrees_dir.is_dir():
        return
    cutoff = now_ts - retention_days * 86400
    for wt in sorted(worktrees_dir.iterdir()):
        if not wt.is_dir():
            continue
        try:
            mtime = wt.stat().st_mtime
        except OSError:
            continue
        if mtime >= cutoff:
            continue
        task_id = wt.name
        log(f"Sweep: removing worktree {wt} (age > {retention_days}d)")
        if _git(repo_root, "worktree", "remove", "--force", str(wt)).returncode != 0:
            shutil.rmtree(wt, ignore_errors=True)
        refs = _git(
            repo_root,
            "for-each-ref",
            "--format=%(refname:short)",
            f"refs/heads/flywheel/*/{task_id}",
        ).stdout
        for branch in refs.splitlines():
            if branch.strip():
                _git(repo_root, "branch", "-D", branch.strip())


def retention_targets(repo_root: Path, worktrees_dir: Path) -> tuple[Path, ...]:
    """The worktree roots one retention pass covers.

    Always the active root; additionally the legacy hardcoded
    ``.flywheel/worktrees`` while it still exists on disk. ``[paths]
    sandbox_root`` used to be silently ignored on this path, so a repo that
    pinned it has in-flight/parked worktrees at the legacy location the
    moment the knob became live -- those must keep aging out of the same
    retention window instead of stranding forever.
    """
    targets = [worktrees_dir]
    legacy = repo_root / ".flywheel" / "worktrees"
    try:
        distinct = legacy.resolve() != worktrees_dir.resolve()
    except OSError:
        distinct = True
    if distinct and legacy.is_dir():
        targets.append(legacy)
    return tuple(targets)


def retention_cadence_tick(
    repo_root: Path,
    worktrees_dir: Path,
    retention_days: int,
    log: Logger,
    *,
    now: Callable[[], float] = time.time,
) -> None:
    """Run one :func:`retention_sweep` pass, reading the clock fresh.

    The daemon invokes this once per cycle -- not once at boot -- so a parked
    worktree that ages past the retention window is reclaimed mid-run without a
    restart, while a within-window worktree survives. Reading ``now`` on each
    tick is what advances the cutoff between cycles; it is injectable so a test
    can drive the cadence without wall-clock time. A single pass has exactly
    :func:`retention_sweep`'s removal semantics -- this controls only *when* the
    sweep runs, never *what* one sweep removes.
    """
    retention_sweep(repo_root, worktrees_dir, retention_days, now(), log)


def reap_container_orphans(log: Logger) -> None:
    """Startup backstop: force-remove flywheel-owned orphan containers a prior
    killed worker left behind — the SIGKILL/OOM case the container backend's
    atexit registry never covers (spec 00071 #5).

    Lazy-imports the container backend so ``flywheel-container`` stays an
    optional extra: if it is not installed this system could not have created a
    container, so the reap is a clean no-op. The scan selects strictly on the
    shared flywheel-owner label, so unrelated containers are never touched, and
    the reap excludes any container this live process still owns. Best-effort:
    a docker error (e.g. the daemon is absent) is logged and swallowed so the
    worker still starts."""
    try:
        from flywheel_container import reap_orphan_containers
    except ImportError:
        return
    try:
        reaped = reap_orphan_containers()
    except Exception as exc:  # noqa: BLE001 - startup reap is strictly best-effort
        log(f"Container orphan reap skipped ({type(exc).__name__}: {exc})")
        return
    for name in reaped:
        log(f"Reaped orphan container {name}")


# Hard ceiling on the rendered action so a runaway tool-call payload can
# never wrap the single-line heartbeat unboundedly. The per-field
# summarizers in `flywheel_core.workflow` already truncate individual values;
# this is the belt-and-braces final cap.
_HEARTBEAT_DETAIL_MAX_WIDTH: int = 100


def _truncate(text: str, limit: int) -> str:
    return text if len(text) <= limit else text[: max(limit - 1, 1)] + "…"


def _format_heartbeat(row: LiveRunRow, now: datetime) -> str:
    """Single-line per-in-flight-run progress: lifecycle-position
    breadcrumb (``attempt=N iter=K``), running tokens/cost/turns rolled
    up from the run's ``harness.iteration_completed`` events, age of the
    latest activity, and the current agent action."""
    if row.last_ts is None:
        age = "—"
    else:
        secs = int((now - row.last_ts).total_seconds())
        age = f"{max(secs, 0)}s"
    attempt_str = (
        f"attempt={row.attempt}" if row.attempt is not None else "attempt=?"
    )
    iter_str = f"iter={row.iteration}" if row.iteration is not None else "iter=?"
    if row.iterations_completed == 0:
        # No iteration has completed yet, so the per-iteration rollup
        # (tokens/cost/turns come from harness.iteration_completed events)
        # has no data. The agent is still working its first iteration --
        # render that honestly. "tokens=0 turns=0" reads as a frozen run
        # when it is in fact mid-flight; the `age` field shows how long the
        # in-progress iteration has been running.
        totals = "working (first-iteration metrics pending)"
    else:
        totals = (
            f"tokens={row.tokens_total} "
            f"cost=${row.cost_usd_total:.4f} "
            f"turns={row.turns_total}"
        )
    detail = _truncate(row.last_detail, _HEARTBEAT_DETAIL_MAX_WIDTH)
    return (
        f"{row.task_id} {row.status.value} {attempt_str} {iter_str} "
        f"{totals} age={age} {row.last_kind} {detail}"
    )


class Heartbeat:
    """Background thread printing per-in-flight-run progress lines via
    :func:`flywheel_core.workflow.collect_live_rows`, so a watcher can tell the
    agent is still moving. Quiet when nothing is in flight."""

    def __init__(
        self,
        db_path: Path,
        interval: int,
        log: Logger,
        *,
        policy: WorkPolicy | None = None,
    ) -> None:
        self._db_path = db_path
        self._policy = policy
        self._interval = interval
        self._log = log
        self._stop = threading.Event()
        self._thread = threading.Thread(
            target=self._run, name="worker-heartbeat", daemon=True
        )

    def start(self) -> Heartbeat:
        if self._interval > 0:
            self._thread.start()
        return self

    def _run(self) -> None:
        while not self._stop.wait(self._interval):
            try:
                # Rebuilt every tick through the factory — safe to call
                # repeatedly with the same policy (it only constructs).
                store = open_sqlite_bound_store(
                    self._policy, db_path=self._db_path
                )
                try:
                    rows = collect_live_rows(store)
                finally:
                    store.close()
            except Exception:  # noqa: BLE001 - transient read; retry next tick
                continue
            now = datetime.now(timezone.utc)
            for row in rows:
                self._log(_format_heartbeat(row, now))

    def stop(self) -> None:
        self._stop.set()
        if self._thread.is_alive():
            self._thread.join(timeout=self._interval + 1.0)


def make_logger(prefix: str) -> Logger:
    def log(message: str) -> None:
        print(f"{prefix} {message}", file=sys.stderr, flush=True)

    return log


# --- orchestration cycle ----------------------------------------------------


def run_once(
    submitter: GitWorktreeSubmitter,
    *,
    tasks_dir: Path,
    db_path: Path,
    worktrees_dir: Path,
    model: str | None,
    max_turns: int,
    max_retries: int,
    worker_id: str | None = None,
    lease_seconds: float = 300.0,
    reconcile_seconds: float | None = None,
    sweep_seconds: float | None = DEFAULT_SWEEP_SECONDS,
    landing_redrive_bound: int | None = DEFAULT_LANDING_REDRIVE_BOUND,
    invoke: InvokeFunc | None = None,
    stream: TextIO | None = None,
    log: Logger | None = None,
    policy: WorkPolicy | None = None,
    strategy: SubmitStrategy | None = None,
    held_out_source: HeldOutGraderSource | None = None,
) -> OrchestratorReport:
    """One cycle: record phase bases, drain every eligible task to
    quiescence through the git-submit seam, archive completed phases.
    The worker never creates commits on the operator's branch — landing
    work is the submit strategy's job, and bookkeeping lives in the
    ``refs/flywheel/`` namespace. ``invoke`` defaults to the real Claude
    Code invoker; tests inject a fake.

    Per-run forensics are the telemetry JSONL files the harness's sink
    writes under ``<db dir>/logs/runs/`` (spec 00025); the worker renders
    no ``.log`` re-render from the store.

    ``policy`` selects the store backend for every store this cycle
    constructs (orchestrate's, the archive sweep's) through the
    orchestrator's store factory; ``None`` keeps the historical
    sqlite-on-``db_path`` behavior.

    ``strategy`` is the submit strategy handed to ``orchestrate``; it defaults
    to ``submitter`` (the git-worktree backend). The container backend
    (spec 00045) passes a ``ContainerSubmitStrategy`` *wrapping* ``submitter``,
    so the worker still drives worktree provisioning/landing through
    ``submitter`` while the agent runs in a container.

    ``held_out_source`` is forwarded to ``orchestrate`` to enable the
    execute-time held-out landing gate (spec 00050): a DONE task whose
    operator-declared held-out graders fail is blocked from landing and its
    worktree parked. ``None`` (the default) leaves landing byte-identical.

    ``landing_redrive_bound`` is forwarded to ``orchestrate`` to enable the
    bounded landing re-driver (spec 00069): a run parked unlanded (a failed
    ``[submit] verify`` standing invariant, a divergent base) is re-driven
    through ``submitter``'s own rebase/reverify/standing/FF path up to this many
    times before its strand is routed to the human-review queue. It defaults to
    ``DEFAULT_LANDING_REDRIVE_BOUND`` because the worker always drives a
    landability-probing git strategy; pass ``None``/``0`` to disable.

    The git checkpoint-nudge probe factory (:func:`_git_progress_probe`) is
    always handed to ``orchestrate``, which binds it per-run over each task's
    sandbox; the harness nudges an iteration nearing its ``AGENT_ITERATION``
    deadline on a branch with no new commits (threshold from ``[worker]
    checkpoint_nudge_seconds``, default-on at 300s).
    """
    log = log or submitter.log
    record_phase_bases(
        submitter.repo_root, tasks_dir, submitter.lock_path, log
    )
    report = asyncio.run(
        orchestrate(
            tasks_dir=tasks_dir,
            policy=policy,
            db_path=db_path,
            sandbox_root=worktrees_dir,
            invoke=invoke,
            model=model,
            max_turns=max_turns,
            max_retries=max_retries,
            worker_id=worker_id,
            lease_seconds=lease_seconds,
            reconcile_seconds=reconcile_seconds,
            sweep_seconds=sweep_seconds,
            landing_redrive_bound=landing_redrive_bound,
            session_pause_ceiling_seconds=(
                policy.worker_session_pause_ceiling_seconds
                if policy is not None
                else DEFAULT_SESSION_PAUSE_CEILING_SECONDS
            ),
            strategy=strategy if strategy is not None else submitter,
            stream=stream,
            repo_root=submitter.repo_root,
            held_out_source=held_out_source,
            progress_probe_factory=_git_progress_probe,
        )
    )
    # Under the same merge_lock record_phase_bases and the FF-merge path use:
    # pool members share one repo_root/lock_path and each runs run_once every
    # cycle, so two members finishing the last task of the same phase would
    # otherwise both pass archive_completed_phases' unlocked ``if dest.exists()``
    # check and both call shutil.move -- the loser raising FileNotFoundError
    # into run_daemon_loop as a spurious cycle-failure strike.
    with merge_lock(submitter.lock_path):
        archive_phases(
            tasks_dir,
            db_path,
            log,
            repo_root=submitter.repo_root,
            landing_base=submitter.phase_base,
            policy=policy,
        )
    return report


# --- worker pool supervisor (spec 00060) ------------------------------------


@dataclass
class _PoolMember:
    """One supervised pool member: a single-task worker subprocess.

    ``proc`` is its own session leader (``start_new_session=True``), so its
    agent/MCP children share its process group and a group signal reaches the
    whole subtree (the orphan-free-shutdown guarantee, commit 36a0622).
    ``log_handle`` is the redirected stdout/stderr sink (``None`` when output is
    inherited); ``log_path`` is kept for forensics after the handle closes.
    """

    slot: int
    worker_id: str
    proc: subprocess.Popen[bytes]
    log_handle: IO[bytes] | None
    log_path: Path | None


class WorkerPool:
    """Supervise up to ``size`` single-task worker subprocesses from one
    ``flywheel worker`` invocation (spec 00060, decision D-5).

    Each pool member is its own OS process in its own session, running
    ``flywheel worker --concurrency 1`` with a distinct ``--worker-id`` against
    the one shared store. Per-task leases keep members off the same task and the
    repo merge-flock serializes their landings, so ``size`` members give up to
    ``size`` tasks executing at once and never more (criteria #2/#3). Surplus
    members find nothing claimable and idle -- or, in ``--once`` mode, drain and
    exit 0 -- without double-claiming (#7).

    Crash policy is restart-and-reclaim (D-2): a member that exits non-zero is
    respawned so the live pool returns to ``size``, and its in-flight task's
    lease lapses (or, on a same-worker-id respawn, is reclaimed immediately) so
    a live member finishes it exactly once (#8). Per-slot restarts are bounded
    by a shared *windowed* crash-loop budget (spec 00070): a slot that exhausts
    its budget is retired in isolation while every other live member keeps
    running -- one bad slot never group-kills the healthy fleet (D-C). The
    budget is windowed, not lifetime, so a slot that dips once after a long
    healthy interval is respawned, not retired.

    Shutdown reuses the group-kill supervisor model: each member is a session
    leader whose agent/MCP children share its process group, so :meth:`stop`
    signals every member's whole group (SIGTERM, then SIGKILL on the survivors
    after the stop window) and returns with nothing left alive (#9).

    ``spawn_member`` maps a member's worker id to the argv that launches it,
    keeping the production worker entrypoint and the test substitute (a trivial
    child) on the same supervision path. Construction spawns nothing; call
    :meth:`run_supervised` to drive the pool, or drive :meth:`start` /
    :meth:`_supervise_tick` / :meth:`stop` directly.
    """

    def __init__(
        self,
        *,
        size: int,
        spawn_member: Callable[[str], Sequence[str]],
        log: Logger,
        once: bool,
        prefix: str = "pool",
        log_dir: Path | None = None,
        env: Mapping[str, str] | None = None,
        stop_timeout: float = DEFAULT_POOL_STOP_TIMEOUT_SECONDS,
        poll_interval: float = POOL_SUPERVISE_POLL_SECONDS,
        max_restarts_per_slot: int = DEFAULT_POOL_RESTARTS_PER_SLOT,
        restart_window_seconds: float = DEFAULT_POOL_RESTART_WINDOW_SECONDS,
        clock: Callable[[], float] = time.monotonic,
    ) -> None:
        if size < 1:
            raise ValueError(f"worker pool size must be >= 1, got {size}")
        self._size = size
        self._spawn_member = spawn_member
        self._log = log
        self._once = once
        self._prefix = prefix
        self._log_dir = log_dir
        self._env = dict(env) if env is not None else None
        self._stop_timeout = stop_timeout
        self._poll_interval = poll_interval
        # One shared windowed crash-loop budget shape; each slot gets its own
        # SupervisionPolicy so slots decay independently (spec 00070). The
        # injected clock lets tests drive the window deterministically.
        self._budget = SupervisionBudget(
            max_respawns=max(max_restarts_per_slot, 0),
            window_seconds=restart_window_seconds,
        )
        self._clock = clock
        self._policies: dict[int, SupervisionPolicy] = {}
        self._members: dict[int, _PoolMember] = {}
        # Slots no longer supervised: cleanly drained (--once, exit 0) or
        # quarantined after exhausting their crash-loop budget (``_exhausted``).
        self._retired: set[int] = set()
        self._exhausted: set[int] = set()
        self._stop_requested = False
        self._stopped = False
        self._exit_code = 0

    @property
    def size(self) -> int:
        return self._size

    def run_supervised(self) -> int:
        """Spawn the pool, supervise it, and return the process exit code.

        Installs the SIGTERM/SIGINT shutdown handler, spawns ``size`` members,
        then loops: in ``--once`` mode it returns 0 once every member has
        drained and exited cleanly; as a daemon it runs until a stop signal.
        Either way the ``finally`` group-kills every surviving member so the
        invocation never leaves an orphan behind.
        """
        self._arm_signals()
        try:
            self.start()
            while not self._stop_requested:
                if self._once and self.is_done():
                    break
                time.sleep(self._poll_interval)
                self._supervise_tick()
            return self._exit_code
        finally:
            self.stop()

    def start(self) -> None:
        """Spawn one member into every not-yet-filled, not-retired slot."""
        for slot in range(self._size):
            if slot not in self._members and slot not in self._retired:
                self._spawn_slot(slot)

    def is_done(self) -> bool:
        """``--once`` only: every slot has drained and exited cleanly."""
        return self._once and len(self._retired) >= self._size

    def live_member_pids(self) -> list[int]:
        """PIDs of members currently alive (a running ``poll()``)."""
        return [
            m.proc.pid
            for m in self._members.values()
            if m.proc.poll() is None
        ]

    def _supervise_tick(self) -> None:
        """One supervision pass: reap exited members, retire cleanly drained
        ``--once`` members, respawn crashed ones within their windowed budget,
        and retire (in isolation) a slot that exhausts that budget.

        Budget exhaustion retires only the offending slot and leaves every
        other live member running (spec 00070, D-C) -- it never sets a pool-wide
        stop or group-kills the fleet. Once every slot is retired the pool has
        nothing left to supervise, so the loop is asked to stop rather than
        spin below-strength forever."""
        for slot in range(self._size):
            if slot in self._retired:
                continue
            member = self._members.get(slot)
            if member is None:
                continue
            code = member.proc.poll()
            if code is None:
                continue  # still running
            self._reap(member)
            if code == 0 and self._once:
                # Clean drain: this slot is done, do not respawn (else the pool
                # would never terminate in --once mode).
                self._retired.add(slot)
                self._members.pop(slot, None)
                self._log(
                    f"pool member {member.worker_id} drained (exit 0)"
                )
                continue
            # Non-zero exit, or an unexpected daemon-mode exit: charge the death
            # against this slot's windowed crash-loop budget. Inside budget it
            # is restart-and-reclaimed to restore the pool to size (D-2) -- the
            # dead member's task is reclaimed by a live worker via the existing
            # lease machinery. Past budget the slot is retired in isolation.
            policy = self._policies.setdefault(
                slot, SupervisionPolicy(self._budget, clock=self._clock)
            )
            if policy.record_death() is RespawnDecision.RESPAWN:
                self._log(
                    f"pool member {member.worker_id} exited {code}; respawning "
                    f"(death {policy.deaths_in_window} within "
                    f"{self._budget.window_seconds:g}s window, budget "
                    f"{self._budget.max_respawns})"
                )
                self._spawn_slot(slot)
                continue
            # Budget exhausted: retire THIS slot only and keep supervising the
            # rest of the fleet (D-C). No pool-wide stop, no group-kill of the
            # healthy members -- a smaller live fleet beats a dead one.
            self._retired.add(slot)
            self._exhausted.add(slot)
            self._members.pop(slot, None)
            self._exit_code = 1
            self._log(
                f"pool member {member.worker_id} exhausted its crash-loop "
                f"budget ({self._budget.max_respawns} respawns within "
                f"{self._budget.window_seconds:g}s, last exit {code}); "
                f"retiring this slot -- other members keep running"
            )
        # Every slot retired (drained or quarantined): nothing left to
        # supervise, so end the loop rather than spin below-strength forever.
        if len(self._retired) >= self._size:
            self._stop_requested = True

    def stop(self, *, timeout: float | None = None) -> None:
        """Group-kill every surviving member; idempotent.

        Sends SIGTERM to each live member's process group first (a shared
        graceful window so the pool does not pay ``timeout`` per member), waits
        out the window, then escalates the survivors to SIGKILL so the pool
        always returns with nothing left alive -- no member, no agent, no MCP
        child (#9). A force-killed member's lease simply lapses and another
        worker reclaims its task.
        """
        if self._stopped:
            return
        self._stopped = True
        window = self._stop_timeout if timeout is None else timeout
        live = [
            m for m in self._members.values() if m.proc.poll() is None
        ]
        for member in live:
            self._signal_group(member, signal.SIGTERM)
        deadline = time.monotonic() + window
        for member in live:
            remaining = max(deadline - time.monotonic(), 0.0)
            with contextlib.suppress(subprocess.TimeoutExpired):
                member.proc.wait(timeout=remaining)
        for member in live:
            if member.proc.poll() is None:
                self._signal_group(member, signal.SIGKILL)
                with contextlib.suppress(subprocess.TimeoutExpired):
                    member.proc.wait(timeout=window)
        for member in list(self._members.values()):
            self._reap(member)
        self._members.clear()

    # ----- internals --------------------------------------------------------

    def _arm_signals(self) -> None:
        """Install the SIGTERM/SIGINT shutdown handler. The pool parent runs no
        asyncio event loop, so (unlike the single-worker loop) the handler is
        installed once and stays armed for the whole invocation."""

        def _handler(signum: int, _frame: object) -> None:
            self._stop_requested = True
            self._log(
                f"pool shutdown requested (signal {signum}); stopping members."
            )

        for sig in (signal.SIGTERM, signal.SIGINT):
            with contextlib.suppress(ValueError):
                signal.signal(sig, _handler)

    def _spawn_slot(self, slot: int) -> None:
        worker_id = f"{self._prefix}-{slot}"
        argv = list(self._spawn_member(worker_id))
        log_handle, log_path = self._open_member_log(worker_id)
        try:
            proc = subprocess.Popen(
                argv,
                stdout=log_handle if log_handle is not None else None,
                stderr=(
                    subprocess.STDOUT if log_handle is not None else None
                ),
                env=(
                    self._env if self._env is not None else os.environ.copy()
                ),
                # Own session so each member's agent/MCP children share its
                # process group and a group signal takes the whole subtree
                # down; also so a Ctrl+C on the console terminal does not reach
                # members behind the pool's back.
                start_new_session=True,
                close_fds=True,
            )
        except (OSError, ValueError):
            if log_handle is not None:
                with contextlib.suppress(OSError):
                    log_handle.close()
            raise
        self._members[slot] = _PoolMember(
            slot=slot,
            worker_id=worker_id,
            proc=proc,
            log_handle=log_handle,
            log_path=log_path,
        )
        self._log(f"pool member {worker_id} spawned pid={proc.pid}")

    def _open_member_log(
        self, worker_id: str
    ) -> tuple[IO[bytes] | None, Path | None]:
        if self._log_dir is None:
            return None, None
        self._log_dir.mkdir(parents=True, exist_ok=True)
        ts = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S")
        path = self._log_dir / f"pool-{worker_id}-{ts}.log"
        return open(path, "ab", buffering=0), path

    def _signal_group(self, member: _PoolMember, sig: int) -> bool:
        """Signal a member's whole process group; ``False`` if it is gone.

        The member is a session leader, so its pgid equals its pid and the
        signal reaches every descendant it spawned.
        """
        try:
            pgid = os.getpgid(member.proc.pid)
        except ProcessLookupError:
            return False
        try:
            os.killpg(pgid, sig)
        except ProcessLookupError:
            return False
        return True

    def _reap(self, member: _PoolMember) -> None:
        if member.log_handle is not None:
            with contextlib.suppress(OSError):
                member.log_handle.close()
            member.log_handle = None


def _pool_member_argv(
    args: argparse.Namespace, worker_id: str, *, model: str | None
) -> list[str]:
    """Compose the argv for one pool member: this same worker, re-invoked at
    ``--concurrency 1`` with a distinct ``--worker-id`` (spec 00060).

    Every other run knob the operator passed is forwarded verbatim so a pool
    member behaves exactly like a hand-launched single worker against the same
    store; ``--concurrency 1`` is pinned explicitly so the member runs one task
    at a time and never recurses into another pool, regardless of config.
    """
    argv = [
        sys.executable,
        "-m",
        "flywheel_worktree.worker",
        "--concurrency",
        "1",
        "--worker-id",
        worker_id,
        "--max-turns",
        str(args.max_turns),
        "--max-retries",
        str(args.max_retries),
        "--worktree-retention-days",
        str(args.worktree_retention_days),
        "--run-log-retention",
        str(args.run_log_retention),
        "--heartbeat",
        str(args.heartbeat),
        "--poll-interval",
        str(args.poll_interval),
        "--lease-seconds",
        str(args.lease_seconds),
        "--reconcile-seconds",
        str(args.reconcile_seconds),
    ]
    if args.tasks_dir is not None:
        argv.extend(["--tasks-dir", str(args.tasks_dir)])
    if args.db is not None:
        argv.extend(["--db", str(args.db)])
    if args.sandbox_root is not None:
        argv.extend(["--sandbox-root", str(args.sandbox_root)])
    if model is not None:
        argv.extend(["--model", str(model)])
    if args.once:
        argv.append("--once")
    return argv


def _run_pool(
    args: argparse.Namespace,
    *,
    concurrency: int,
    model: str | None,
    db_path: Path,
    worker_id: str | None,
    log: Logger,
) -> int:
    """Drive a :class:`WorkerPool` of ``concurrency`` single-task members.

    The members share the one store at ``db_path`` and land through the same
    merge-flock as a hand-launched fleet would; this only supervises them from
    one invocation. The pool's worker-id prefix is the operator's
    ``--worker-id`` when given (so members read ``<id>-0``, ``<id>-1``, ...),
    else a fresh per-invocation prefix, keeping every member's id distinct so
    claims attribute to the right worker.
    """
    prefix = worker_id or f"pool-{uuid4().hex[:8]}"

    def spawn_member(member_id: str) -> list[str]:
        return _pool_member_argv(args, member_id, model=model)

    pool = WorkerPool(
        size=concurrency,
        spawn_member=spawn_member,
        log=log,
        once=bool(args.once),
        prefix=prefix,
        log_dir=db_path.parent / "logs" / "worker",
    )
    log(
        f"started worker pool size={concurrency} prefix={prefix} "
        f"once={bool(args.once)} pid={os.getpid()}"
    )
    return pool.run_supervised()


# --- CLI / daemon -----------------------------------------------------------


def _repo_root() -> Path:
    proc = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"],
        text=True,
        capture_output=True,
        check=False,
    )
    if proc.returncode != 0:
        raise SystemExit("ERROR: not inside a git repository.")
    return Path(proc.stdout.strip())


def _checked_out_branch(repo_root: Path) -> str | None:
    """The operator's currently-checked-out branch, or ``None`` on detached
    HEAD."""
    branch = _git(repo_root, "rev-parse", "--abbrev-ref", "HEAD").stdout.strip()
    if not branch or branch == "HEAD":
        return None
    return branch


def _fetch_base_fresh(
    repo_root: Path, base: str, log: Logger | None = None
) -> None:
    """Best-effort fresh fetch of the configured base ref from its remote so a
    landing targets the up-to-date base, not a stale local ref (D-3).

    The remote is ``branch.<base>.remote`` when configured, else ``origin``;
    when neither exists the fetch is a no-op (a purely local repo). The fetch
    FF-updates the local base ref (``<base>:<base>``); a base that then cannot
    fast-forward becomes the divergent-base park, never a crash here. The caller
    guarantees the base is not the operator's checked-out branch, so the refspec
    fetch into ``refs/heads/<base>`` is not refused by git."""
    remotes = _git(repo_root, "remote").stdout.split()
    if not remotes:
        return
    configured = _git(
        repo_root, "config", f"branch.{base}.remote"
    ).stdout.strip()
    remote = configured or ("origin" if "origin" in remotes else remotes[0])
    if remote not in remotes:
        return
    res = _git(repo_root, "fetch", remote, f"{base}:{base}")
    if res.returncode != 0 and log is not None:
        log(
            f"fresh fetch of base {base!r} from {remote!r} failed "
            f"({res.stderr.strip()}); landing will use the local ref"
        )


def resolve_landing_base(
    repo_root: Path,
    policy: WorkPolicy | None,
    *,
    log: Logger | None = None,
) -> str:
    """Resolve the merge-strategy landing base, fetching it fresh (D-1/D-2/D-3).

    The base is ``policy.submit_base`` when configured; otherwise it falls back
    to the operator's currently-checked-out branch (back-compat). Raises
    :class:`PolicyError` when:

    * a configured base equals the operator's currently-checked-out branch —
      refusing to land into the working checkout (D-2 belt; the load-bearing
      guarantee is that the landing advances a ref the operator does not have
      checked out), or
    * ``HEAD`` is detached and no base is configured (D-1: a detached checkout
      exposes no branch to default to).

    A configured base is fetched fresh from its remote before it is returned, so
    the landing targets the up-to-date base.
    """
    checked_out = _checked_out_branch(repo_root)
    configured = policy.submit_base if policy is not None else None

    if configured is not None:
        if checked_out is not None and configured == checked_out:
            raise PolicyError(
                f"configured landing base {configured!r} is the operator's "
                f"currently-checked-out branch; refusing to land into the "
                f"working checkout (check out a different branch or change "
                f"[submit] base)"
            )
        _fetch_base_fresh(repo_root, configured, log)
        return configured

    if checked_out is None:
        raise PolicyError(
            "worker started on detached HEAD with no [submit] base configured; "
            "set [submit] base to a landing branch or check out a branch."
        )
    return checked_out


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="flywheel worker",
        description=(
            "Git-worktree worker: drive flywheel tasks under "
            ".flywheel/tasks/active/, each in its own worktree, FF-merging on "
            "done and parking on failure."
        ),
    )
    parser.add_argument("--tasks-dir", default=None)
    parser.add_argument("--db", default=None)
    parser.add_argument(
        "--sandbox-root",
        default=None,
        help=(
            "Root under which each task's worktree is created (default: "
            "[paths] sandbox_root in flywheel.toml, else "
            ".flywheel/worktrees). Relative paths anchor at the repo root; "
            "@cache and @sibling select out-of-tree layouts."
        ),
    )
    parser.add_argument("--model", default=None)
    parser.add_argument("--max-turns", type=int, default=DEFAULT_MAX_TURNS)
    parser.add_argument("--max-retries", type=int, default=DEFAULT_MAX_RETRIES)
    parser.add_argument(
        "--worktree-retention-days", type=int, default=DEFAULT_RETENTION_DAYS
    )
    parser.add_argument(
        "--run-log-retention",
        type=int,
        default=DEFAULT_RUN_LOG_RETENTION,
        help=(
            "Cap on per-run telemetry JSONL files kept under "
            ".flywheel/logs/runs/; the oldest past the cap are reclaimed "
            "each cycle so the most-recent runs survive (0 disables)."
        ),
    )
    parser.add_argument(
        "--heartbeat",
        type=int,
        default=DEFAULT_HEARTBEAT_SECONDS,
        help="Per-in-flight-task progress line every N seconds (0 disables).",
    )
    parser.add_argument(
        "--poll-interval",
        type=int,
        default=DEFAULT_POLL_INTERVAL_SECONDS,
        help="Seconds to wait between drain cycles when idle.",
    )
    parser.add_argument("--worker-id", default=None)
    parser.add_argument(
        "--concurrency",
        default=None,
        help=(
            "Worker pool size: how many tasks this invocation drives "
            "concurrently. Overrides [worker] concurrency in flywheel.toml "
            "(default 1). Must resolve to an integer >= 1."
        ),
    )
    parser.add_argument("--lease-seconds", type=float, default=300.0)
    parser.add_argument(
        "--reconcile-seconds",
        type=float,
        default=15.0,
        help=(
            "Steering bridge: re-list the work source every N seconds and "
            "interrupt in-flight runs whose item vanished (e.g. its task "
            "file was deleted). 0 disables (default: 15)."
        ),
    )
    parser.add_argument(
        "--once",
        action="store_true",
        help="Run a single drain cycle and exit (no daemon loop).",
    )
    return parser


def _arm_signals(handler: Callable[[int, object], None]) -> None:
    """Install the shutdown-flag handler for SIGTERM/SIGINT. Re-armed each
    cycle because ``asyncio.run`` (inside orchestrate) takes these signals over
    for the run and restores their default disposition afterward."""
    for sig in (signal.SIGTERM, signal.SIGINT):
        with contextlib.suppress(ValueError):
            signal.signal(sig, handler)


def _interruptible_sleep(seconds: int, should_stop: Callable[[], bool]) -> None:
    for _ in range(max(seconds, 0)):
        if should_stop():
            return
        time.sleep(1)


def _resolve_model(
    args: argparse.Namespace, policy: WorkPolicy | None
) -> str | None:
    """Resolve the agent model id the worker invokes the SDK with.

    Precedence is exactly:

    * an explicit ``--model`` CLI flag wins;
    * else ``flywheel.toml`` ``[agent] model`` (``policy`` is the
      cwd-auto-detected ``load_effective_policy()`` result, loaded once
      by :func:`main` and shared with the store factory);
    * else ``None`` so :class:`ClaudeAgentOptions` is constructed
      without a model and the SDK falls through to the Claude Code
      default -- the historical behaviour direct ``python -m`` callers
      relied on before this feature.
    """

    if args.model:
        return args.model
    if policy is None:
        return None
    return policy.model


def _resolve_concurrency(
    args: argparse.Namespace, policy: WorkPolicy | None
) -> int:
    """Resolve the worker pool size from ``--concurrency`` over config.

    Precedence (spec 00060, D-1): an explicit ``--concurrency`` flag wins; else
    the ``[worker] concurrency`` config value (``policy.worker_concurrency``,
    default ``1``); else ``1`` when no policy is loaded. The resolved value is
    what the worker drains the queue with.

    A resolved value below ``1`` -- or a ``--concurrency`` that is not an
    integer -- is a hard error (D-4): raised as :class:`PolicyError` so
    :func:`main` exits non-zero with a message naming the concurrency setting
    BEFORE any task is claimed. A silent clamp to ``1`` would hide the operator
    mistake and is deliberately not done.
    """
    setting = "--concurrency / [worker] concurrency"
    if args.concurrency is not None:
        try:
            value = int(args.concurrency)
        except (TypeError, ValueError):
            raise PolicyError(
                f"{setting} must be an integer >= 1, got "
                f"{args.concurrency!r}"
            ) from None
    elif policy is not None:
        value = policy.worker_concurrency
    else:
        value = DEFAULT_WORKER_CONCURRENCY
    if value < 1:
        raise PolicyError(
            f"{setting} must resolve to an integer >= 1, got {value}"
        )
    return value


def build_merge_submitter(
    policy: WorkPolicy | None,
    *,
    repo_root: Path,
    tasks_dir: Path,
    worktrees_dir: Path,
    phase_base: str,
    lock_path: Path,
    log: Logger,
    protected_paths: Sequence[str],
    setup_command: str | None,
    on_done: str = "destroy",
    on_failure: str = "park",
    store: LandingLedger | None = None,
    grader_env: Mapping[str, str] | None = None,
    held_out_source: HeldOutGraderSource | None = None,
) -> GitWorktreeSubmitter:
    """Build the merge backend (the registry's ``merge`` target).

    Reads ``[submit] verify`` from ``policy`` (spec 00064) -- the standing build
    invariant the submitter re-runs under the merge lock against the exact tree
    about to land -- and otherwise takes the shared builder arguments the
    submit-strategy registry dispatches on (see
    :mod:`flywheel_worktree._submit_registry`). ``on_done``/``on_failure`` are the
    submit-time ``[sandbox.retention]`` knobs (defaults reproduce today's
    destroy/park behavior). ``store`` is the run ledger the submitter records a
    queryable ``LANDING_PARKED`` event on when it parks a DONE branch.
    ``grader_env`` is the resolved ``[sandbox.env]`` the submit-time
    re-verification (and the standing verify) run with. ``held_out_source`` is
    the declared held-out landing gate (spec 00051) the merge-fallback recovery
    rung re-runs against the merged candidate tree (spec 00076, D-2); ``None``
    when no gate is configured.

    Reads the bounded conflict-resolution rung's bounds from ``policy`` too
    ([submit] ``recovery_agent_max_turns`` / ``recovery_agent_max_wall_seconds``,
    spec 00076 criterion 4): when the merge-fallback merge itself conflicts and
    ``recovery_agent_max_turns`` > 0, a single bounded agent session resolves it
    before the same re-verification bar. ``policy is None`` (a bare construction)
    leaves the rung disabled, byte-identical to today's merge-conflict park. The
    session's SDK model is the policy's resolved agent model.
    """
    return GitWorktreeSubmitter(
        repo_root=repo_root,
        tasks_dir=tasks_dir,
        worktrees_dir=worktrees_dir,
        phase_base=phase_base,
        lock_path=lock_path,
        log=log,
        protected_paths=protected_paths,
        setup_command=setup_command,
        on_done=on_done,
        on_failure=on_failure,
        store=store,
        grader_env=grader_env,
        verify_command=policy.submit_verify if policy is not None else None,
        held_out_source=held_out_source,
        recovery_agent_max_turns=(
            policy.submit_recovery_agent_max_turns if policy is not None else 0
        ),
        recovery_agent_max_wall_seconds=(
            policy.submit_recovery_agent_max_wall_seconds
            if policy is not None
            else 900.0
        ),
        recovery_agent_model=policy.model if policy is not None else None,
    )


def build_phase_submitter(
    policy: WorkPolicy | None,
    *,
    repo_root: Path,
    tasks_dir: Path,
    worktrees_dir: Path,
    phase_base: str,
    lock_path: Path,
    log: Logger,
    protected_paths: Sequence[str],
    setup_command: str | None,
    on_done: str = "destroy",
    on_failure: str = "park",
    store: LandingLedger | None = None,
    grader_env: Mapping[str, str] | None = None,
    held_out_source: HeldOutGraderSource | None = None,
) -> PhaseBranchSubmitter:
    """Build the phase backend (the registry's ``phase`` target, spec 00079).

    Takes the same shared builder arguments the submit-strategy registry
    dispatches on as :func:`build_merge_submitter`, and reads the identical
    ``policy`` knobs -- ``[submit] verify`` (the standing build invariant), the
    bounded conflict-resolution rung's turn/wall bounds, and the resolution
    model -- because the phase strategy runs the very same verify ladder; only
    the ref each task lands onto differs (the phase's integration branch rather
    than the single configured base). Tolerates ``policy is None`` exactly as the
    merge builder does (the phase landing reads no required policy field), and
    logs the resolved landing target so an operator can see the strategy that
    armed.
    """
    submitter = PhaseBranchSubmitter(
        repo_root=repo_root,
        tasks_dir=tasks_dir,
        worktrees_dir=worktrees_dir,
        phase_base=phase_base,
        lock_path=lock_path,
        log=log,
        protected_paths=protected_paths,
        setup_command=setup_command,
        on_done=on_done,
        on_failure=on_failure,
        store=store,
        grader_env=grader_env,
        verify_command=policy.submit_verify if policy is not None else None,
        held_out_source=held_out_source,
        recovery_agent_max_turns=(
            policy.submit_recovery_agent_max_turns if policy is not None else 0
        ),
        recovery_agent_max_wall_seconds=(
            policy.submit_recovery_agent_max_wall_seconds
            if policy is not None
            else 900.0
        ),
        recovery_agent_model=policy.model if policy is not None else None,
    )
    log(
        f"landing strategy: phase (integration branch flywheel/phase/<phase> "
        f"per task's phase; true base {phase_base} never advanced)"
    )
    return submitter


def maybe_wrap_for_backend(
    submitter: GitWorktreeSubmitter,
    policy: WorkPolicy | None,
    *,
    model: str | None,
    env: Mapping[str, str],
    log: Logger,
) -> SubmitStrategy:
    """Select the run's submit strategy from ``[sandbox] backend`` (spec 00045).

    ``backend = "worktree"`` (default) returns ``submitter`` unchanged. ``backend
    = "container"`` wraps it in a ``ContainerSubmitStrategy`` (lazy-imported, so
    ``flywheel-container`` stays an optional extra) configured from
    ``[sandbox.container]`` / ``[sandbox.network]``: the worktree backend still
    provisions and lands; the agent runs in the container. A missing
    ``flywheel-container`` is a clear install error.
    """
    sandbox = policy.sandbox if policy is not None else None
    if sandbox is None or sandbox.backend != "container":
        return submitter
    try:
        from flywheel_container import build_container_strategy, resolve_auth
    except ImportError as exc:  # pragma: no cover - environment-dependent
        raise RuntimeError(
            "sandbox.backend = 'container' requires the flywheel-container "
            "package. Install it (e.g. 'flywheel-worktree[container]') or use "
            "the 'flywheel' product, which bundles it."
        ) from exc
    container = sandbox.container
    resolved_model = container.model or model
    if not resolved_model:
        raise RuntimeError(
            "sandbox.backend = 'container' needs an explicit model: set "
            "[sandbox.container] model, [agent] model, or --model (the agent "
            "CLI is invoked with --model and has no implicit default here)."
        )
    auth = resolve_auth(
        container.auth, env=env, token_env=container.auth_env or None
    )
    # Docker management calls run under the [deadlines] docker_management_seconds
    # ceiling resolved onto the policy, not the container module's import-time
    # default. (policy is non-None here -- sandbox derives from it -- but keep a
    # DeadlineConfig() default so a None policy resolves the same finite ceiling.)
    deadlines = policy.deadlines if policy is not None else DeadlineConfig()
    log(
        f"backend=container image={container.image} model={resolved_model} "
        f"auth={container.auth} network={sandbox.network.policy}"
    )
    return build_container_strategy(
        submitter,
        image=container.image,
        model=resolved_model,
        exec_timeout=container.exec_timeout,
        network_policy=sandbox.network.policy,
        allow_hosts=sandbox.network.allow_hosts,
        egress_network=container.egress_network or None,
        auth=auth,
        management_timeout=deadlines.docker_management_seconds,
    )


def build_held_out_source(
    policy: WorkPolicy | None, repo_root: Path
) -> HeldOutGraderSource | None:
    """Construct the execute-time held-out grader source from policy, or ``None``.

    Activation is opt-in (spec 00051, criterion #2, decision D-3): a source is
    built only when ``[held_out] root`` is configured. When the key is absent
    the worker supplies no source, so :func:`orchestrate` runs the gate for no
    task and landing is byte-identical to today.

    A relative ``[held_out] root`` resolves against ``repo_root`` so it points
    at ``<repo_root>/<root>`` regardless of the worker's cwd or any sandbox path
    (criterion #3); an absolute root is honored as written. The 00050
    :class:`FilesystemHeldOutGraderSource` is reused unchanged (D-5) -- this
    only supplies it, it does not re-decide gate behavior. The root is *not*
    materialized into any agent worktree; the orchestrator reads it out of band
    (D-1, criterion #7).
    """
    if policy is None or policy.held_out_root is None:
        return None
    return FilesystemHeldOutGraderSource(root=repo_root / policy.held_out_root)


def run_daemon_loop(
    *,
    run_cycle: Callable[[], object],
    once: bool,
    poll_interval: float,
    should_stop: Callable[[], bool],
    sleep: Callable[[float, Callable[[], bool]], None],
    before_cycle: Callable[[], None] | None = None,
    on_cycle_failure: Callable[[BaseException, int], None] | None = None,
    on_give_up: Callable[[int], None] | None = None,
    on_permanent_stop: Callable[[BaseException], None] | None = None,
    on_interrupt: Callable[[], None] | None = None,
    max_cycles: int | None = None,
) -> int:
    """The worker daemon's cross-cycle circuit breaker, extracted so the
    transient-vs-strike boundary is testable without a live model.

    Each pass runs one ``run_cycle`` (a full ``run_once`` in production), then
    waits ``poll_interval`` before the next; ``once`` stops after the first
    successful pass. The loop exits only on ``should_stop`` (a SIGTERM/SIGINT
    flag in production, an injected event in tests) or a breaker verdict.
    ``before_cycle`` re-arms signal handlers each iteration (``asyncio.run``
    inside a real pass reclaims them).

    The breaker sits ABOVE the store's own bounded transient retry: a TRANSIENT
    store fault that clears within the store's retry budget is absorbed BENEATH
    this loop -- ``run_cycle`` returns normally and no strike is counted, so a
    flake that self-heals costs nothing. Only a cycle that actually raises past
    that retry counts a strike (``on_cycle_failure``); after
    ``MAX_CONSECUTIVE_CYCLE_FAILURES`` consecutive strikes the loop gives up
    (``on_give_up``, returns 1) so a non-zero exit is visible. A successful
    cycle resets the count.

    A PERMANENT-classified fault (e.g. a ``StoreSchemaError`` from reopening a
    store whose ``schema_version`` row is wrong) can never succeed on retry, so
    it is NOT counted as a transient strike -- otherwise it would burn all five
    strikes with a backoff between each. Instead the loop signals the distinct
    ``on_permanent_stop`` and returns ``PERMANENT_STOP_EXIT_CODE`` after exactly
    one cycle. ``KeyboardInterrupt``/``asyncio.CancelledError`` are not counted
    either: they signal ``on_interrupt`` and stop the loop.

    ``max_cycles`` is a test-only safety bound; production leaves it ``None``.
    Returns the process exit code (0 normal, 1 gave up, permanent-stop code).
    """
    consecutive_failures = 0
    cycles = 0
    while not should_stop():
        if before_cycle is not None:
            before_cycle()
        try:
            run_cycle()
        except (KeyboardInterrupt, asyncio.CancelledError):
            if on_interrupt is not None:
                on_interrupt()
            break
        except Exception as exc:  # noqa: BLE001 - one bad cycle must not crash the daemon
            # A permanent fault (a schema-version mismatch reopening the store
            # every cycle) can never succeed on retry, so it must not ride all
            # five breaker strikes: classify PERMANENT and stop the loop after
            # this single cycle via a distinctly-labelled permanent-stop exit,
            # separate from the transient give-up path.
            if classify_fault(exc) is FaultClass.PERMANENT:
                if on_permanent_stop is not None:
                    on_permanent_stop(exc)
                return PERMANENT_STOP_EXIT_CODE
            consecutive_failures += 1
            cycles += 1
            if on_cycle_failure is not None:
                on_cycle_failure(exc, consecutive_failures)
            if consecutive_failures >= MAX_CONSECUTIVE_CYCLE_FAILURES:
                if on_give_up is not None:
                    on_give_up(consecutive_failures)
                return 1
            if max_cycles is not None and cycles >= max_cycles:
                break
            if should_stop():
                break
            sleep(CYCLE_FAILURE_BACKOFF_SECONDS, should_stop)
            continue
        consecutive_failures = 0
        cycles += 1
        if once:
            break
        if max_cycles is not None and cycles >= max_cycles:
            break
        if should_stop():
            break
        sleep(poll_interval, should_stop)
    return 0


def main(argv: Sequence[str] | None = None) -> int:
    args = _build_parser().parse_args(argv)
    repo_root = _repo_root()

    log = make_logger("[worker]")
    try:
        # Loaded once: the same policy resolves the agent model, the landing
        # base, and the store backend for every store this process constructs.
        # Base resolution reads policy.submit_base, so it must run AFTER the
        # policy loads (D-1/D-5): a configured base is fetched fresh and the
        # checked-out-base / detached-HEAD config errors surface as PolicyError.
        policy = load_effective_policy()
        phase_base = resolve_landing_base(repo_root, policy, log=log)
        # Resolve the worker pool size BEFORE any store is opened or task is
        # claimed (spec 00060, D-4): a resolved concurrency < 1 (or a
        # non-integer flag) must fail fast and loud, naming the setting, with
        # no lifecycle or claim row created.
        concurrency = _resolve_concurrency(args, policy)
        # Resolve the worktree root here too: an unknown @token or an
        # unwritable @sibling parent must fail fast, before any store opens.
        worktrees_dir = resolve_sandbox_root(
            args.sandbox_root
            or (policy.sandbox_root if policy is not None else None),
            repo_root=repo_root,
        )
    except PolicyError as exc:
        print(f"flywheel worker: policy error: {exc}", file=sys.stderr)
        return 2
    model = _resolve_model(args, policy)

    tasks_dir = (
        Path(args.tasks_dir)
        if args.tasks_dir
        else repo_root / ".flywheel" / "tasks"
    )
    db_path = (
        Path(args.db) if args.db else repo_root / ".flywheel" / "flywheel.sqlite"
    )
    lock_path = repo_root / ".flywheel" / ".merge.lock"

    worktrees_dir.mkdir(parents=True, exist_ok=True)
    db_path.parent.mkdir(parents=True, exist_ok=True)

    # Spec 00060 (D-5): a resolved concurrency > 1 means this invocation is the
    # pool supervisor, not a worker -- it spawns N single-task members (each
    # re-invoking this module at --concurrency 1) against the one shared store
    # and supervises them. The concurrency == 1 path below is the unchanged
    # single-worker daemon a pool member itself runs.
    if concurrency > 1:
        return _run_pool(
            args,
            concurrency=concurrency,
            model=model,
            db_path=db_path,
            worker_id=args.worker_id,
            log=log,
        )

    protected_paths = policy.protected_paths if policy else ()
    setup_command = policy.sandbox_setup if policy else None
    # Submit-time retention knobs ([sandbox.retention], spec 00041). Defaults
    # reproduce today's destroy-on-done / park-on-failure behavior.
    on_done = policy.sandbox.retention.on_done if policy else "destroy"
    on_failure = policy.sandbox.retention.on_failure if policy else "park"
    # Resolved [sandbox.env] for submit-time re-verification, matching the env
    # the in-run command graders (and the agent) saw. None when unconfigured.
    grader_env = resolve_grader_env(policy.sandbox.env) if policy else None
    # The submitter's own store handle: it records a queryable LANDING_PARKED
    # event on the run ledger when it parks a DONE branch. Same backing store
    # (policy-selected) as the run's lifecycle, opened on db_path.
    submit_store = open_sqlite_bound_store(policy, db_path=db_path)
    # Execute-time held-out landing gate (spec 00051): opt-in, built only when
    # [held_out] root is configured, resolved against repo_root. When unset this
    # is None and landing is byte-identical to today. Built before the submitter
    # so the merge-fallback recovery rung can re-run it against the merged
    # candidate tree (spec 00076, D-2) and orchestrate can gate the pre-merge
    # tree with the same source.
    held_out_source = build_held_out_source(policy, repo_root)
    if (
        held_out_source is not None
        and policy is not None
        and policy.held_out_root is not None
    ):
        log(f"held-out gate active root={repo_root / policy.held_out_root}")
    # The registry owns name -> builder dispatch (and lazily imports pr.py
    # for the "pr" strategy, which is what keeps that import out of this
    # module's top level). The builders share one signature.
    strategy = policy.submit_strategy if policy is not None else "merge"
    submitter: GitWorktreeSubmitter = SUBMIT_STRATEGIES.resolve(strategy)(
        policy,
        repo_root=repo_root,
        tasks_dir=tasks_dir,
        worktrees_dir=worktrees_dir,
        phase_base=phase_base,
        lock_path=lock_path,
        log=log,
        protected_paths=protected_paths,
        setup_command=setup_command,
        on_done=on_done,
        on_failure=on_failure,
        store=submit_store,
        grader_env=grader_env,
        held_out_source=held_out_source,
    )
    # Select the run's submit strategy from [sandbox] backend: worktree
    # (submitter unchanged) or container (wrap it) — spec 00045.
    run_strategy = maybe_wrap_for_backend(
        submitter, policy, model=model, env=os.environ, log=log
    )

    log(
        f"started pid={os.getpid()} base={phase_base} db={db_path} "
        f"concurrency={concurrency}"
    )
    log(
        f"tasks={tasks_dir} worktrees={worktrees_dir} "
        f"logs={db_path.parent / 'logs' / 'runs'}"
    )

    # Reap any flywheel-owned container a prior killed worker left orphaned: the
    # SIGKILL/OOM case the container backend's atexit registry cannot cover
    # (spec 00071 #5). Runs once at boot, before the main loop, and is a no-op
    # when the container backend is not installed or no orphan is present.
    reap_container_orphans(log)
    heartbeat = Heartbeat(
        db_path, args.heartbeat, make_logger("[heartbeat]"), policy=policy
    )
    heartbeat.start()

    shutdown = {"requested": False}

    def _flag(signum: int, _frame: object) -> None:
        shutdown["requested"] = True
        log(
            f"Shutdown requested (signal {signum}); stopping after the "
            f"current cycle."
        )

    run_logs_dir = db_path.parent / "logs" / "runs"

    def _run_cycle() -> None:
        # Default-on run-log retention: hold .flywheel/logs/runs/ at or under
        # the configured bound before draining, so a long-running daemon does
        # not accumulate per-run JSONL files without limit.
        sweep_run_logs(run_logs_dir, args.run_log_retention, log)
        # Worktree retention runs every cycle, not once at boot: a parked
        # worktree that ages past the window is reclaimed mid-run without a
        # restart, while a within-window worktree survives. The clock is read
        # fresh each tick so the cutoff advances between cycles. When the
        # configured root moved off the legacy .flywheel/worktrees, that
        # location is swept too until it empties out.
        for sweep_root in retention_targets(repo_root, worktrees_dir):
            retention_cadence_tick(
                repo_root, sweep_root, args.worktree_retention_days, log
            )
        run_once(
            submitter,
            tasks_dir=tasks_dir,
            db_path=db_path,
            worktrees_dir=worktrees_dir,
            model=model,
            max_turns=args.max_turns,
            max_retries=args.max_retries,
            worker_id=args.worker_id,
            lease_seconds=args.lease_seconds,
            reconcile_seconds=args.reconcile_seconds or None,
            stream=sys.stderr,
            log=log,
            policy=policy,
            strategy=run_strategy,
            held_out_source=held_out_source,
        )

    def _should_stop() -> bool:
        return shutdown["requested"]

    def _sleep(seconds: float, stop: Callable[[], bool]) -> None:
        # asyncio.run inside a cycle reclaims SIGTERM/SIGINT and restores their
        # default disposition; re-arm before every wait so a shutdown signal
        # during the backoff/poll still wakes us.
        _arm_signals(_flag)
        _interruptible_sleep(int(seconds), stop)

    try:
        return run_daemon_loop(
            run_cycle=_run_cycle,
            once=args.once,
            poll_interval=args.poll_interval,
            should_stop=_should_stop,
            sleep=_sleep,
            before_cycle=lambda: _arm_signals(_flag),
            on_cycle_failure=lambda exc, n: log(
                f"Cycle failed ({type(exc).__name__}: {exc}) "
                f"[{n}/{MAX_CONSECUTIVE_CYCLE_FAILURES}]"
            ),
            on_give_up=lambda _n: log(
                "Too many consecutive cycle failures; exiting for "
                "operator inspection."
            ),
            on_permanent_stop=lambda exc: log(
                f"Cycle hit a permanent fault "
                f"({type(exc).__name__}: {exc}); stopping the worker "
                f"loop after one cycle for operator inspection "
                f"(retrying cannot reconcile it)."
            ),
            on_interrupt=lambda: log(
                "Interrupted mid-run; in-flight task finalized to "
                "interrupted. Shutting down."
            ),
        )
    finally:
        heartbeat.stop()
        log("Shutting down.")


if __name__ == "__main__":
    raise SystemExit(main())

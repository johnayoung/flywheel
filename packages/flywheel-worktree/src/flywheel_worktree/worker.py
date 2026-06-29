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
    GraderResultRecord,
    InvokeFunc,
    Lifecycle,
    Status,
    run_command_graders,
)
from flywheel_core.events import DomainEvent, LandingParked
from flywheel_orchestrator import (
    DEFAULT_WORKER_CONCURRENCY,
    FilesystemHeldOutGraderSource,
    HeldOutGraderSource,
    OrchestratorReport,
    PolicyError,
    SandboxRequest,
    SubmitRequest,
    SubmitStrategy,
    WorkPolicy,
    load_effective_policy,
    open_sqlite_bound_store,
    orchestrate,
    resolve_grader_env,
)
from flywheel_core.workflow import (
    DEFAULT_MAX_RETRIES,
    DEFAULT_MAX_TURNS,
)
from flywheel_orchestrator import (
    LiveRunRow,
    archive_completed_phases,
    collect_live_rows,
    iter_active_phase_dirs,
    write_phase_base_if_missing,
)

from flywheel_worktree._submit_registry import SUBMIT_STRATEGIES

DEFAULT_RETENTION_DAYS = 7
DEFAULT_HEARTBEAT_SECONDS = 10
DEFAULT_POLL_INTERVAL_SECONDS = 5
# Consecutive whole-cycle failures (orchestrate raising unexpectedly) before
# the daemon gives up so an operator can inspect, rather than hot-looping. The
# per-task starvation guard lives in orchestrate (attempted_fresh); this is the
# cross-cycle backstop that replaces the bash SPAWN_FAILURES circuit breaker.
MAX_CONSECUTIVE_CYCLE_FAILURES = 5
CYCLE_FAILURE_BACKOFF_SECONDS = 10

# Worker-pool supervision (spec 00060). How long the pool's group shutdown
# gives each member's SIGTERM window before escalating to SIGKILL, and how
# often the supervise loop polls members for exit. The stop timeout mirrors the
# console worker supervisor (commit 36a0622); the poll interval is short so a
# stop signal or a member crash is noticed promptly without busy-spinning.
DEFAULT_POOL_STOP_TIMEOUT_SECONDS = 10.0
POOL_SUPERVISE_POLL_SECONDS = 0.2
# Per-slot restart budget: a member that keeps crashing (e.g. a misconfigured
# environment, not a flaky task) is respawned at most this many times before
# the pool gives up and exits non-zero for operator inspection. The per-task
# retry ceiling lives in the lease/lifecycle machinery (D-2); this is the
# cross-restart backstop that keeps a crash-loop from spinning forever.
MAX_POOL_RESTARTS_PER_SLOT = 5


Logger = Callable[[str], None]


# Fixed, deterministic commit identity established on every worktree the worker
# provisions, so the agent's own in-sandbox ``git commit`` resolves an
# author/committer even on a host with no global or system git identity
# (``GIT_CONFIG_NOSYSTEM=1``, empty ``HOME``). Constant across every worktree in
# a repo (never random/UUID/timestamp/per-run): the worker authors no commit
# itself; this only lets the agent's commit succeed.
WORKTREE_COMMIT_IDENTITY_NAME = "Flywheel Worker"
WORKTREE_COMMIT_IDENTITY_EMAIL = "worker@flywheel.invalid"


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

    def _branch(self, task_id: str, phase: str) -> str:
        return f"flywheel/{phase}/{task_id}"

    def _worktree(self, task_id: str) -> Path:
        return self.worktrees_dir / task_id

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

    def _rebase_parked_branch(self, worktree: Path, branch: str) -> bool:
        """Rebase a parked branch onto the current base (so its flywheel source
        matches the live DB schema). ``True`` if up to date or rebased cleanly,
        ``False`` on conflict."""
        res = _git(
            self.repo_root,
            "rev-list",
            "--count",
            f"{branch}..{self.phase_base}",
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
            f"Parked {branch} is {behind} commit(s) behind {self.phase_base}; "
            f"rebasing..."
        )
        if _git(worktree, "rebase", self.phase_base).returncode == 0:
            self.log("Rebase clean; prior commits carry forward.")
            return True
        _git(worktree, "rebase", "--abort")
        self.log(
            f"Rebase failed against {self.phase_base}; discarding parked "
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
        self, worktree: Path, branch: str
    ) -> None:
        _git(self.repo_root, "worktree", "remove", "--force", str(worktree))
        _git(self.repo_root, "branch", "-D", branch)
        self._add_worktree(worktree, "-b", branch, self.phase_base)

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

        worktree_present = worktree.is_dir()
        branch_present = self._branch_exists(branch)

        if worktree_present and branch_present:
            if not self._is_registered_worktree(worktree):
                raise PrepareSandboxError(
                    f"{worktree} exists but is not a registered worktree; "
                    f"refusing to clobber"
                )
            self.scrub_worktree_locks(worktree, branch)
            if self._rebase_parked_branch(worktree, branch):
                return worktree
            self._discard_and_recreate(worktree, branch)
            self._run_setup(worktree)
            return worktree

        if (not worktree_present) and branch_present:
            self.log(
                f"Recreating worktree on existing branch {branch} (directory "
                f"was removed; ref survived)."
            )
            self._add_worktree(worktree, branch)
            if not self._rebase_parked_branch(worktree, branch):
                self._discard_and_recreate(worktree, branch)
            self._run_setup(worktree)
            return worktree

        if worktree_present and (not branch_present):
            raise PrepareSandboxError(
                f"{worktree} exists but no branch {branch}; refusing to "
                f"clobber. Remove the directory manually."
            )

        self._add_worktree(worktree, "-b", branch, self.phase_base)
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
            self.log(
                f"submit error for {req.task_id} ({type(exc).__name__}: "
                f"{exc}); worktree left parked at {self._worktree(req.task_id)}"
            )

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

            commit_count = self._commit_count(branch)
            if commit_count == 0:
                # Legitimate no-op: work already on base, or inspection-only
                # graders with no diff. The lifecycle row is the source of
                # truth; just clean up the empty branch+worktree.
                self.log(
                    f"{task_id} reached DONE with no commits beyond "
                    f"{self.phase_base}; nothing to merge"
                )
                self._cleanup(worktree, branch)
                return

            violations = self._protected_violations(branch)
            if violations:
                self.log(
                    f"{task_id} touches protected path(s) "
                    f"{', '.join(violations)}; refusing to merge, parking "
                    f"worktree at {worktree}"
                )
                return

            if self._ff_merge(branch):
                self.log(
                    f"Merged {branch} into {self.phase_base} "
                    f"({commit_count} commit(s))"
                )
                self._teardown_on_done(worktree, branch)
                return

            # FF failed (base advanced): rebase once, re-verify, retry FF,
            # else park.
            self.log(f"FF failed for {branch}; rebasing onto {self.phase_base}")
            if _git(worktree, "rebase", self.phase_base).returncode != 0:
                _git(worktree, "rebase", "--abort")
                self.log(
                    f"rebase failed for {branch}; parking worktree at "
                    f"{worktree}"
                )
                self._record_landing_park(
                    req.run_id,
                    park_kind="divergent-base",
                    detail=(
                        f"{branch} cannot fast-forward {self.phase_base}: "
                        f"rebase onto the diverged base conflicted; worktree "
                        f"preserved at {worktree}"
                    ),
                )
                return
            if not self._reverify(req, worktree):
                self.log(
                    f"post-rebase re-verification failed for {branch}; "
                    f"parking worktree at {worktree}"
                )
                return
            if self._ff_merge(branch):
                self.log(
                    f"Merged {branch} into {self.phase_base} after rebase "
                    f"({commit_count} commit(s))"
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
                    f"{branch} cannot fast-forward {self.phase_base} even after "
                    f"a clean rebase + re-verify; worktree preserved at "
                    f"{worktree}"
                ),
            )

    def _protected_violations(self, branch: str) -> list[str]:
        """Repo-relative paths the branch touches that match a protected
        pattern (``PurePath.full_match`` glob semantics, ``**`` crosses
        directories).

        The diff is merge-base scoped (``base...branch``) so only the
        branch's own changes count, never what the base did underneath it.
        This is the merge-time half of the verification trust boundary:
        graders execute inside the tree the agent just mutated, so work
        that rewrites the verification surface itself (grader configs, CI,
        harness state) can pass its own judges — the gate refuses to land
        it regardless.
        """
        if not self.protected_paths:
            return []
        res = _git(
            self.repo_root,
            "diff",
            "--name-only",
            f"{self.phase_base}...{branch}",
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

    def _reverify(self, req: SubmitRequest, worktree: Path) -> bool:
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
        """
        command_count = sum(
            isinstance(g, CommandGrader) for g in req.task.graders
        )
        if command_count == 0:
            self.log(
                f"{req.task_id} has no command graders; nothing to re-verify "
                f"after rebase"
            )
            return True
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
        for record in records:
            label = record.grader_name or str(record.grader_spec.get("run", ""))
            verdict = "pass" if record.passed else "FAIL"
            self.log(
                f"re-verify {req.task_id}: {verdict} [{label}] "
                f"({record.duration_ms}ms)"
            )
        return len(records) == command_count and all(r.passed for r in records)

    def _commit_count(self, branch: str) -> int:
        res = _git(
            self.repo_root,
            "rev-list",
            "--count",
            f"{self.phase_base}..{branch}",
        )
        return (
            int(res.stdout.strip())
            if res.returncode == 0 and res.stdout.strip().isdigit()
            else 0
        )

    def _base_is_checked_out(self) -> bool:
        """True when the configured base is the operator's checked-out branch in
        ``repo_root`` (the back-compat default case)."""
        return _checked_out_branch(self.repo_root) == self.phase_base

    def _ff_merge(self, branch: str) -> bool:
        """Fast-forward the base to ``branch``'s tip.

        When the base is the operator's checked-out branch (the unconfigured
        default), advance it in-tree with ``git merge --ff-only`` as before.
        When the base is NOT checked out (a configured landing base — the
        safe-landing case), advance its ref out-of-tree via ``git fetch .
        <branch>:<base>`` so the operator's working tree, index, and HEAD are
        never touched. Both paths are fast-forward-only: a non-FF advance
        returns ``False`` and the caller rebases or parks."""
        if self._base_is_checked_out():
            return (
                _git(self.repo_root, "merge", "--ff-only", branch).returncode
                == 0
            )
        return (
            _git(
                self.repo_root, "fetch", ".", f"{branch}:{self.phase_base}"
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
        self, run_id: str, *, park_kind: str, detail: str
    ) -> None:
        """Append a queryable ``LANDING_PARKED`` audit-witness event for a
        parked DONE run (``park_kind`` ``"uncommitted-work"`` or
        ``"divergent-base"``).

        The run already finalized ``DONE`` (terminal); this records the park
        cause on the run's ledger via ``append_domain_event`` WITHOUT any
        lifecycle transition — ``LandingParked`` folds to the identity, so the
        status stays ``DONE`` and only ``version`` advances. Best-effort: a
        missing store handle or any store error is logged, never raised, so
        ``submit()`` cannot escape into orchestrate (criterion 7)."""
        if self.store is None:
            return
        try:
            lifecycle = self.store.load_lifecycle(run_id)
            if lifecycle is None:
                self.log(
                    f"cannot record landing-parked event: no lifecycle for "
                    f"{run_id}"
                )
                return
            self.store.append_domain_event(
                LandingParked(
                    run_id=run_id,
                    ts=datetime.now(timezone.utc),
                    park_kind=park_kind,
                    detail=detail,
                ),
                expected_version=lifecycle.version,
            )
        except Exception as exc:  # noqa: BLE001 - must not escape submit
            self.log(
                f"failed to record landing-parked event for {run_id} "
                f"({type(exc).__name__}: {exc})"
            )


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
    policy: WorkPolicy | None = None,
) -> None:
    """Move ``active/<phase>`` dirs whose tasks are all done into ``archive/``.

    ``repo_root`` enables the loop-path archive gate
    (:func:`flywheel_core.workflow.archive_completed_phases` reads the phase's
    cumulative diff vs ``.loop-base`` to derive the marker); omitting it
    skips the gate entirely, which matches the legacy ``archive_phases``
    contract. Refusal reasons are reported via the same ``log`` callable
    that announces archived phases so a single log stream tells the
    operator everything the sweep did.

    ``policy`` selects the store backend through the orchestrator's store
    factory; ``None`` keeps the historical sqlite-on-``db_path`` behavior.
    A configured ``policy.phase_verify`` runs as the phase-exit integration
    gate (spec 00035): the command runs against the merged base in
    ``repo_root`` before an eligible phase archives, and a non-zero exit
    leaves the phase active with the refusal reported via ``log``.
    """
    store = open_sqlite_bound_store(policy, db_path=db_path)
    try:
        moved = archive_completed_phases(
            tasks_dir,
            store,
            repo_root=repo_root,
            log=log,
            phase_verify=policy.phase_verify if policy is not None else None,
        )
    finally:
        store.close()
    for dest in moved:
        log(f"Archived phase: {dest}")


# Per-run forensics live in the run telemetry JSONL the harness's sink
# writes at .flywheel/logs/runs/<run_id>.jsonl (spec 00025). The worker
# renders nothing from the store and never deletes or rotates those
# files -- their lifecycle is operator-owned (logrotate etc.).


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
        totals = "tokens=0 cost=-- turns=0"
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
            strategy=strategy if strategy is not None else submitter,
            stream=stream,
            repo_root=submitter.repo_root,
            held_out_source=held_out_source,
        )
    )
    archive_phases(
        tasks_dir,
        db_path,
        log,
        repo_root=submitter.repo_root,
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
    so a member that crashes instantly cannot spin forever.

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
        max_restarts_per_slot: int = MAX_POOL_RESTARTS_PER_SLOT,
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
        self._max_restarts = max(max_restarts_per_slot, 0)
        self._members: dict[int, _PoolMember] = {}
        self._retired: set[int] = set()
        self._restarts: dict[int, int] = {}
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
        ``--once`` members, and respawn crashed ones (bounded per slot)."""
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
            # Non-zero exit, or an unexpected daemon-mode exit: restart-and-
            # reclaim to restore the pool to size (D-2). The dead member's task
            # is reclaimed by a live worker via the existing lease machinery.
            if self._restarts.get(slot, 0) >= self._max_restarts:
                self._log(
                    f"pool member {member.worker_id} exceeded "
                    f"{self._max_restarts} restarts (last exit {code}); "
                    f"giving up"
                )
                self._members.pop(slot, None)
                self._exit_code = 1
                self._stop_requested = True
                return
            self._restarts[slot] = self._restarts.get(slot, 0) + 1
            self._log(
                f"pool member {member.worker_id} exited {code}; respawning "
                f"(restart {self._restarts[slot]}/{self._max_restarts})"
            )
            self._spawn_slot(slot)

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
    parser.add_argument("--model", default=None)
    parser.add_argument("--max-turns", type=int, default=DEFAULT_MAX_TURNS)
    parser.add_argument("--max-retries", type=int, default=DEFAULT_MAX_RETRIES)
    parser.add_argument(
        "--worktree-retention-days", type=int, default=DEFAULT_RETENTION_DAYS
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
) -> GitWorktreeSubmitter:
    """Build the merge backend (the registry's ``merge`` target).

    The fast-forward-merge landing reads nothing extra from ``policy``; the
    argument is part of the shared builder signature the submit-strategy
    registry dispatches on (see :mod:`flywheel_worktree._submit_registry`).
    ``on_done``/``on_failure`` are the submit-time ``[sandbox.retention]``
    knobs (defaults reproduce today's destroy/park behavior). ``store`` is the
    run ledger the submitter records a queryable ``LANDING_PARKED`` event on
    when it parks a DONE branch. ``grader_env`` is the resolved ``[sandbox.env]``
    the submit-time re-verification runs command graders with.
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
    )


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
    worktrees_dir = repo_root / ".flywheel" / "worktrees"
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
    )
    # Select the run's submit strategy from [sandbox] backend: worktree
    # (submitter unchanged) or container (wrap it) — spec 00045.
    run_strategy = maybe_wrap_for_backend(
        submitter, policy, model=model, env=os.environ, log=log
    )

    # Execute-time held-out landing gate (spec 00051): opt-in, built only when
    # [held_out] root is configured, resolved against repo_root. When unset this
    # is None and landing is byte-identical to today.
    held_out_source = build_held_out_source(policy, repo_root)
    if (
        held_out_source is not None
        and policy is not None
        and policy.held_out_root is not None
    ):
        log(f"held-out gate active root={repo_root / policy.held_out_root}")

    log(
        f"started pid={os.getpid()} base={phase_base} db={db_path} "
        f"concurrency={concurrency}"
    )
    log(
        f"tasks={tasks_dir} worktrees={worktrees_dir} "
        f"logs={db_path.parent / 'logs' / 'runs'}"
    )

    retention_sweep(
        repo_root, worktrees_dir, args.worktree_retention_days, time.time(), log
    )
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

    consecutive_failures = 0
    try:
        while not shutdown["requested"]:
            _arm_signals(_flag)
            try:
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
            except (KeyboardInterrupt, asyncio.CancelledError):
                log(
                    "Interrupted mid-run; in-flight task finalized to "
                    "interrupted. Shutting down."
                )
                break
            except Exception as exc:  # noqa: BLE001 - one bad cycle must not crash the daemon
                consecutive_failures += 1
                log(
                    f"Cycle failed ({type(exc).__name__}: {exc}) "
                    f"[{consecutive_failures}/{MAX_CONSECUTIVE_CYCLE_FAILURES}]"
                )
                if consecutive_failures >= MAX_CONSECUTIVE_CYCLE_FAILURES:
                    log(
                        "Too many consecutive cycle failures; exiting for "
                        "operator inspection."
                    )
                    return 1
                _arm_signals(_flag)
                _interruptible_sleep(
                    CYCLE_FAILURE_BACKOFF_SECONDS,
                    lambda: shutdown["requested"],
                )
                continue
            else:
                consecutive_failures = 0

            if args.once:
                break
            _arm_signals(_flag)
            _interruptible_sleep(
                args.poll_interval, lambda: shutdown["requested"]
            )
    finally:
        heartbeat.stop()
        log("Shutting down.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

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
from datetime import datetime, timezone
from pathlib import Path, PurePosixPath
from typing import Callable, Iterator, Sequence, TextIO

from flywheel_core import (
    CommandGrader,
    GraderResultRecord,
    InvokeFunc,
    Status,
    run_command_graders,
)
from flywheel_orchestrator import (
    OrchestratorReport,
    PolicyError,
    SandboxRequest,
    SubmitRequest,
    WorkPolicy,
    load_effective_policy,
    open_sqlite_bound_store,
    orchestrate,
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

DEFAULT_RETENTION_DAYS = 7
DEFAULT_HEARTBEAT_SECONDS = 10
DEFAULT_POLL_INTERVAL_SECONDS = 5
# Consecutive whole-cycle failures (orchestrate raising unexpectedly) before
# the daemon gives up so an operator can inspect, rather than hot-looping. The
# per-task starvation guard lives in orchestrate (attempted_fresh); this is the
# cross-cycle backstop that replaces the bash SPAWN_FAILURES circuit breaker.
MAX_CONSECUTIVE_CYCLE_FAILURES = 5
CYCLE_FAILURE_BACKOFF_SECONDS = 10


Logger = Callable[[str], None]


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
    ) -> None:
        self.repo_root = repo_root
        self.tasks_dir = tasks_dir
        self.worktrees_dir = worktrees_dir
        self.phase_base = phase_base
        self.lock_path = lock_path
        self.log = log
        self.protected_paths = tuple(protected_paths)
        self.setup_command = setup_command

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
            # failed / interrupted / any non-done terminal: park for forensics.
            self.log(
                f"Lifecycle {req.status.value}; worktree preserved at "
                f"{worktree}"
            )
            return

        # All base-branch mutations are serialized across worker processes.
        with merge_lock(self.lock_path):
            porcelain = _git(worktree, "status", "--porcelain").stdout
            if porcelain.strip():
                self.log(
                    f"DONE with uncommitted changes on {branch}; parking "
                    f"worktree at {worktree}"
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
                self._cleanup(worktree, branch)
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
                self._cleanup(worktree, branch)
                return
            self.log(
                f"post-rebase FF failed for {branch}; parking worktree at "
                f"{worktree}"
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

    def _ff_merge(self, branch: str) -> bool:
        return (
            _git(self.repo_root, "merge", "--ff-only", branch).returncode == 0
        )

    def _cleanup(self, worktree: Path, branch: str) -> None:
        _git(self.repo_root, "worktree", "remove", str(worktree))
        _git(self.repo_root, "branch", "-d", branch)


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
    """
    store = open_sqlite_bound_store(policy, db_path=db_path)
    try:
        moved = archive_completed_phases(
            tasks_dir, store, repo_root=repo_root, log=log
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
            strategy=submitter,
            stream=stream,
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


def _phase_base(repo_root: Path) -> str:
    branch = _git(repo_root, "rev-parse", "--abbrev-ref", "HEAD").stdout.strip()
    if not branch or branch == "HEAD":
        raise SystemExit(
            "ERROR: worker started on detached HEAD; cannot resolve phase "
            "base branch."
        )
    return branch


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


def main(argv: Sequence[str] | None = None) -> int:
    args = _build_parser().parse_args(argv)
    repo_root = _repo_root()
    phase_base = _phase_base(repo_root)

    try:
        # Loaded once: the same policy resolves the agent model and selects
        # the store backend for every store this process constructs.
        policy = load_effective_policy()
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

    log = make_logger("[worker]")
    protected_paths = policy.protected_paths if policy else ()
    setup_command = policy.sandbox_setup if policy else None
    submitter: GitWorktreeSubmitter
    if policy is not None and policy.submit_strategy == "pr":
        # Late import: pr.py imports this module, so a top-level import
        # here would be circular.
        from flywheel_worktree.pr import GitPullRequestSubmitter

        submitter = GitPullRequestSubmitter(
            repo_root=repo_root,
            tasks_dir=tasks_dir,
            worktrees_dir=worktrees_dir,
            phase_base=phase_base,
            lock_path=lock_path,
            log=log,
            protected_paths=protected_paths,
            setup_command=setup_command,
            remote=policy.submit_remote,
            pr_base=policy.submit_pr_base,
        )
        log(
            f"landing strategy: pr (remote={policy.submit_remote} "
            f"base={policy.submit_pr_base or phase_base})"
        )
    else:
        submitter = GitWorktreeSubmitter(
            repo_root=repo_root,
            tasks_dir=tasks_dir,
            worktrees_dir=worktrees_dir,
            phase_base=phase_base,
            lock_path=lock_path,
            log=log,
            protected_paths=protected_paths,
            setup_command=setup_command,
        )

    worktrees_dir.mkdir(parents=True, exist_ok=True)
    db_path.parent.mkdir(parents=True, exist_ok=True)

    log(f"started pid={os.getpid()} base={phase_base} db={db_path}")
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

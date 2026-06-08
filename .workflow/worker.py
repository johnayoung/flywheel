#!/usr/bin/env python3
"""Git-worktree worker: the dogfooding consumer that drives flywheel tasks.

The strategy layer of ``docs/strategy.md`` — the code between "agent finished"
and "result merged". It owns the two concerns flywheel deliberately does not:

* **Git submit** — each task runs in its own worktree on branch
  ``flywheel/<phase>/<task-id>``; on ``done`` the branch is FF-merged into the
  base and the worktree removed, otherwise it is parked for forensics. Lives in
  :class:`GitWorktreeSubmitter`, injected into ``orchestrate`` through its
  ``prepare_sandbox`` / ``submit`` seam. No git lives in flywheel.
* **Daemon poll loop** — ``orchestrate`` drains every eligible task to
  quiescence and exits; this loop re-invokes it after committing newly-dropped
  task files and archiving completed phases.

Selection, prerequisites, reactive unblock/resume, leases + heartbeat,
stranded recovery, and graceful-shutdown finalization are flywheel's, reused
as-is. Replaces the former 858-line ``.workflow/task-worker.sh``.

Parallelism is per-process: run several workers against one store — leases keep
them off the same task, a repo-level merge flock serializes their base merges.
A graceful SIGTERM/SIGINT finalizes the in-flight lifecycle to ``interrupted``
(inside flywheel's ``run_task_object``) and stops the loop; SIGKILL/OOM/reboot
are caught by orchestrate's startup recovery sweep on the next run.

    uv run python .workflow/worker.py [--once] [--tasks-dir DIR] [--db PATH] ...
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
from pathlib import Path
from typing import Callable, Iterator, Sequence, TextIO

from flywheel import (
    InvokeFunc,
    Status,
)
from flywheel.store_sqlite import SqliteStore
from flywheel_orchestrator import (
    OrchestratorReport,
    RunRecord,
    SandboxRequest,
    SubmitRequest,
    orchestrate,
)
from flywheel.workflow import (
    DEFAULT_LOG_DIR,
    DEFAULT_MAX_RETRIES,
    DEFAULT_MAX_TURNS,
    LOOP_BASE_FILENAME,
    LiveRunRow,
    _format_event_line,
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


class GitWorktreeSubmitter:
    """Provisions per-task worktrees and merges/parks them on completion.

    :meth:`prepare` and :meth:`submit` are the ``orchestrate`` callbacks.
    ``prepare`` may raise :class:`PrepareSandboxError` (skips that task);
    ``submit`` never raises (records its own park/merge outcome).
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
    ) -> None:
        self.repo_root = repo_root
        self.tasks_dir = tasks_dir
        self.worktrees_dir = worktrees_dir
        self.phase_base = phase_base
        self.lock_path = lock_path
        self.log = log

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

    def prepare(self, req: SandboxRequest) -> Path:
        """Provision (or reuse) the worktree a task runs in; return its path.
        Reuses a parked worktree+branch on retry (rebasing onto base first),
        recreates it when only the branch survived a sweep, and refuses to
        clobber half-present operator state (:class:`PrepareSandboxError`)."""
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
            if not self._rebase_parked_branch(worktree, branch):
                self._discard_and_recreate(worktree, branch)
            return worktree

        if (not worktree_present) and branch_present:
            self.log(
                f"Recreating worktree on existing branch {branch} (directory "
                f"was removed; ref survived)."
            )
            self._add_worktree(worktree, branch)
            if not self._rebase_parked_branch(worktree, branch):
                self._discard_and_recreate(worktree, branch)
            return worktree

        if worktree_present and (not branch_present):
            raise PrepareSandboxError(
                f"{worktree} exists but no branch {branch}; refusing to "
                f"clobber. Remove the directory manually."
            )

        self._add_worktree(worktree, "-b", branch, self.phase_base)
        return worktree

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

            if self._ff_merge(branch):
                self.log(
                    f"Merged {branch} into {self.phase_base} "
                    f"({commit_count} commit(s))"
                )
                self._cleanup(worktree, branch)
                return

            # FF failed (base advanced): rebase once, retry FF, else park.
            self.log(f"FF failed for {branch}; rebasing onto {self.phase_base}")
            if _git(worktree, "rebase", self.phase_base).returncode != 0:
                _git(worktree, "rebase", "--abort")
                self.log(
                    f"rebase failed for {branch}; parking worktree at "
                    f"{worktree}"
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


def commit_task_files(
    repo_root: Path, tasks_dir: Path, lock_path: Path, log: Logger
) -> None:
    """Commit newly-dropped task JSON so worktrees (branched off the base tip)
    can see them. Stages only UNTRACKED files under ``active/`` (modified files
    may be transient rebase state). Flock'd, since it commits to the base."""
    active_dir = tasks_dir / "active"
    if not active_dir.is_dir():
        return
    with merge_lock(lock_path):
        status = _git(
            repo_root, "status", "--porcelain", "--", str(active_dir)
        ).stdout
        untracked = [
            line[3:] for line in status.splitlines() if line.startswith("?? ")
        ]
        if not untracked:
            return
        for path in untracked:
            _git(repo_root, "add", "--", path)
        if _git(repo_root, "diff", "--cached", "--quiet").returncode == 0:
            return
        _git(repo_root, "commit", "-m", "chore: stage task files for worker")
        log("Committed new task files so worktrees can access them")


def record_phase_bases(
    repo_root: Path, tasks_dir: Path, lock_path: Path, log: Logger
) -> None:
    """Capture each active phase's base SHA into a committed ``.loop-base``.

    Runs once per cycle, after :func:`commit_task_files` (so the recorded
    SHA includes any newly-dropped task JSON) and before any task branch is
    merged. For each ``active/<phase>`` lacking a ``.loop-base``, writes the
    current ``HEAD`` SHA into the dotfile and stages+commits it under the
    merge lock. Idempotent: a phase whose ``.loop-base`` already exists is
    left untouched (the first-seen SHA is the true base; re-runs must never
    move it forward).
    """
    if not (tasks_dir / "active").is_dir():
        return
    with merge_lock(lock_path):
        written: list[Path] = []
        for phase_dir in iter_active_phase_dirs(tasks_dir):
            if write_phase_base_if_missing(repo_root, phase_dir):
                written.append(phase_dir / LOOP_BASE_FILENAME)
        if not written:
            return
        for path in written:
            _git(repo_root, "add", "--", str(path))
        if _git(repo_root, "diff", "--cached", "--quiet").returncode == 0:
            return
        _git(repo_root, "commit", "-m", "chore: record phase base sha")
        log(f"Recorded base sha for {len(written)} phase(s)")


def archive_phases(
    tasks_dir: Path,
    db_path: Path,
    log: Logger,
    *,
    repo_root: Path | None = None,
) -> None:
    """Move ``active/<phase>`` dirs whose tasks are all done into ``archive/``.

    ``repo_root`` enables the loop-path archive gate
    (:func:`flywheel.workflow.archive_completed_phases` reads the phase's
    cumulative diff vs ``.loop-base`` to derive the marker); omitting it
    skips the gate entirely, which matches the legacy ``archive_phases``
    contract. Refusal reasons are reported via the same ``log`` callable
    that announces archived phases so a single log stream tells the
    operator everything the sweep did.
    """
    store = SqliteStore(db_path)
    try:
        moved = archive_completed_phases(
            tasks_dir, store, repo_root=repo_root, log=log
        )
    finally:
        store.close()
    for dest in moved:
        log(f"Archived phase: {dest}")


# --- per-run log files ------------------------------------------------------
#
# The legacy bash worker (.workflow/task-worker.sh) redirected each per-task
# subprocess to ``logs/worker/${task_id}_${hash}_$(date +%Y%m%dT%H%M%S).log``.
# The Python worker runs all tasks in-process via orchestrate and streams to
# one shared sys.stderr, so we restore the per-run forensics file by rendering
# the store's persisted telemetry to disk after each run. The filename mirrors
# the old <task>_<hash>_<ts>.log shape, keyed on the run_id (which uniquely
# identifies the lifecycle attempt) plus a UTC timestamp so multiple
# resumes/attempts of the same task never clobber each other.


def _run_id_hash(run_id: str) -> str:
    """Short, filesystem-safe slice of ``run_id`` for the log filename.

    Mirrors the ``hash`` slot in the old ``<task>_<hash>_<ts>.log`` shape.
    Run ids are of the form ``run-<32-hex>``; we keep the first 12 hex chars
    after the prefix (unique enough across a worker's lifetime, short enough
    to keep filenames greppable). Falls back to the raw value if the prefix
    is absent (forward-compat).
    """
    body = run_id[len("run-") :] if run_id.startswith("run-") else run_id
    return body[:12] or "run"


def _per_run_log_path(
    log_dir: Path, *, task_id: str, run_id: str, now: datetime
) -> Path:
    """Compose ``<log_dir>/<task_id>_<run_hash>_<utc_ts>.log``."""
    ts = now.strftime("%Y%m%dT%H%M%S")
    return log_dir / f"{task_id}_{_run_id_hash(run_id)}_{ts}.log"


def write_run_log(
    log_dir: Path,
    record: RunRecord,
    store: SqliteStore,
    *,
    now: datetime | None = None,
) -> Path:
    """Render the run's persisted telemetry into a per-run forensics file.

    Produces ``<log_dir>/<task_id>_<run_hash>_<ts>.log`` containing a header
    (task_id / run_id / mode / status) followed by one ``_format_event_line``
    per persisted telemetry event for the run, in store order. The directory
    is created on demand so a fresh checkout's first run does not fail.

    Called for every :class:`RunRecord` the orchestrator returns — including
    failed and interrupted runs, since those are exactly the runs an operator
    needs forensics for.
    """
    log_dir.mkdir(parents=True, exist_ok=True)
    moment = now if now is not None else datetime.now(timezone.utc)
    path = _per_run_log_path(
        log_dir, task_id=record.task_id, run_id=record.run_id, now=moment
    )
    events = store.list_events(record.run_id)
    lines: list[str] = [
        f"# task_id={record.task_id}",
        f"# run_id={record.run_id}",
        f"# mode={record.mode}",
        f"# status={record.status.value}",
        f"# worker_id={record.worker_id}",
        f"# written_at={moment.isoformat()}",
        "",
    ]
    lines.extend(_format_event_line(event) for event in events)
    path.write_text("\n".join(lines) + "\n")
    return path


def write_run_logs(
    log_dir: Path, report: OrchestratorReport, db_path: Path, log: Logger
) -> list[Path]:
    """Persist a per-run log for every run the orchestrator drove this cycle.

    Opens its own short-lived :class:`SqliteStore` (the orchestrate call has
    already closed its handle by the time this runs). Returns the list of
    written paths, in the same order as ``report.runs``. Any per-run write
    error is logged and skipped — losing a forensics file must never abort
    the daemon loop.
    """
    written: list[Path] = []
    if not report.runs:
        return written
    store = SqliteStore(db_path)
    try:
        for record in report.runs:
            try:
                path = write_run_log(log_dir, record, store)
            except OSError as exc:
                log(
                    f"failed to write per-run log for {record.run_id} "
                    f"({type(exc).__name__}: {exc})"
                )
                continue
            written.append(path)
            log(f"wrote run log {path} ({record.status.value})")
    finally:
        store.close()
    return written


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
# summarizers in `flywheel.workflow` already truncate individual values;
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
    :func:`flywheel.workflow.collect_live_rows`, so a watcher can tell the
    agent is still moving. Quiet when nothing is in flight."""

    def __init__(self, db_path: Path, interval: int, log: Logger) -> None:
        self._db_path = db_path
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
                store = SqliteStore(self._db_path)
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
    invoke: InvokeFunc | None = None,
    stream: TextIO | None = None,
    log: Logger | None = None,
    log_dir: Path | None = None,
) -> OrchestratorReport:
    """One cycle: commit new task files, drain every eligible task to
    quiescence through the git-submit seam, write a per-run forensics log for
    each run the orchestrator drove, archive completed phases. ``invoke``
    defaults to the real Claude Code invoker; tests inject a fake.

    ``log_dir`` defaults to ``submitter.repo_root / DEFAULT_LOG_DIR`` so the
    files always land at ``logs/worker/`` under the active repo regardless of
    the caller's cwd. Pass an explicit path to override (e.g. tests, the
    ``--log-dir`` CLI flag).
    """
    log = log or submitter.log
    resolved_log_dir = (
        log_dir if log_dir is not None else submitter.repo_root / DEFAULT_LOG_DIR
    )
    commit_task_files(submitter.repo_root, tasks_dir, submitter.lock_path, log)
    record_phase_bases(
        submitter.repo_root, tasks_dir, submitter.lock_path, log
    )
    report = asyncio.run(
        orchestrate(
            tasks_dir=tasks_dir,
            db_path=db_path,
            sandbox_root=worktrees_dir,
            invoke=invoke,
            model=model,
            max_turns=max_turns,
            max_retries=max_retries,
            worker_id=worker_id,
            lease_seconds=lease_seconds,
            prepare_sandbox=submitter.prepare,
            submit=submitter.submit,
            stream=stream,
        )
    )
    write_run_logs(resolved_log_dir, report, db_path, log)
    archive_phases(tasks_dir, db_path, log, repo_root=submitter.repo_root)
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
        prog="worker.py",
        description=(
            "Git-worktree worker: drive flywheel tasks under "
            ".workflow/tasks/active/, each in its own worktree, FF-merging on "
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
        "--log-dir",
        default=None,
        help=(
            "Directory for per-run forensics logs. Defaults to "
            "<repo_root>/logs/worker/."
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


def main(argv: Sequence[str] | None = None) -> int:
    args = _build_parser().parse_args(argv)
    repo_root = _repo_root()
    phase_base = _phase_base(repo_root)

    tasks_dir = (
        Path(args.tasks_dir)
        if args.tasks_dir
        else repo_root / ".workflow" / "tasks"
    )
    db_path = (
        Path(args.db) if args.db else repo_root / ".workflow" / "flywheel.sqlite"
    )
    worktrees_dir = repo_root / ".workflow" / "worktrees"
    lock_path = repo_root / ".workflow" / ".merge.lock"
    log_dir = Path(args.log_dir) if args.log_dir else repo_root / DEFAULT_LOG_DIR

    log = make_logger("[worker]")
    submitter = GitWorktreeSubmitter(
        repo_root=repo_root,
        tasks_dir=tasks_dir,
        worktrees_dir=worktrees_dir,
        phase_base=phase_base,
        lock_path=lock_path,
        log=log,
    )

    worktrees_dir.mkdir(parents=True, exist_ok=True)
    db_path.parent.mkdir(parents=True, exist_ok=True)
    log_dir.mkdir(parents=True, exist_ok=True)

    log(f"started pid={os.getpid()} base={phase_base} db={db_path}")
    log(f"tasks={tasks_dir} worktrees={worktrees_dir} logs={log_dir}")

    retention_sweep(
        repo_root, worktrees_dir, args.worktree_retention_days, time.time(), log
    )
    heartbeat = Heartbeat(db_path, args.heartbeat, make_logger("[heartbeat]"))
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
                    model=args.model,
                    max_turns=args.max_turns,
                    max_retries=args.max_retries,
                    worker_id=args.worker_id,
                    lease_seconds=args.lease_seconds,
                    stream=sys.stderr,
                    log=log,
                    log_dir=log_dir,
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

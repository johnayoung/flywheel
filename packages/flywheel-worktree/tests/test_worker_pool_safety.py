"""Concurrency-safety proofs for the worker pool (spec 00060, criteria #1/#4/#10).

The pool supervisor (``test_worker_pool.py``) proves the supervision mechanics
-- spawn N, retire/restart members, group-kill on stop. These tests prove the
*safety spine* the pool inherits from the orchestrator's lease/claim layer and
the git-worktree merge-flock landing path: exactly-once execution, serialized
landing with submit-time re-verification, and byte-for-byte single-worker
behavior at concurrency 1.

A pool member is a single-task worker (``flywheel worker --concurrency 1``)
draining the shared store via :func:`worker.run_once` / ``orchestrate``; the
merge-flock serializes their base merges and per-task leases keep them off the
same task. A pool of N is therefore behaviorally N such members against one
store -- modeled here as N in-process workers (threads), each with a distinct
worker id, against one shared sqlite store and (for landing) one git repo. This
is deterministic (a barrier induces real overlap; it does not rely on machine
load) and exercises the exact claim + merge-flock code a subprocess pool runs.
"""

from __future__ import annotations

import asyncio
import io
import json
import subprocess
import threading
from collections import Counter
from pathlib import Path

from claude_agent_sdk import AssistantMessage, ResultMessage, TextBlock

from flywheel_core import (
    Intent,
    InvocationRequest,
    InvocationSignals,
    IterationResult,
    Status,
    ValidEnvelope,
)
from flywheel_core.store_sqlite import SqliteStore
from flywheel_orchestrator import orchestrate
from flywheel_worktree import worker


# --- git / store helpers (mirror test_worker.py) ----------------------------


def _git(cwd: Path, *args: str) -> str:
    proc = subprocess.run(
        ["git", "-C", str(cwd), *args],
        text=True,
        capture_output=True,
        check=True,
    )
    return proc.stdout.strip()


def _init_repo(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True)
    _git(path, "init", "-b", "main")
    _git(path, "config", "user.email", "worker-test@example.com")
    _git(path, "config", "user.name", "worker test")
    _git(path, "config", "commit.gpgsign", "false")
    (path / "README.md").write_text("base\n")
    _git(path, "add", "-A")
    _git(path, "commit", "-m", "init")


def _rev(repo: Path, ref: str) -> str:
    return _git(repo, "rev-parse", ref)


def _commit(worktree: Path, filename: str, body: str, message: str) -> None:
    (worktree / filename).write_text(body)
    _git(worktree, "add", "-A")
    _git(worktree, "commit", "-m", message)


def _task_file(
    repo: Path, phase: str, task_id: str, *, grader: str = "true"
) -> Path:
    tf = repo / ".flywheel" / "tasks" / "active" / phase / f"{task_id}.json"
    tf.parent.mkdir(parents=True, exist_ok=True)
    tf.write_text(
        json.dumps(
            {
                "id": task_id,
                "goal": f"Goal for {task_id}.",
                "graders": [{"type": "command", "run": grader}],
            }
        )
    )
    return tf


def _submitter(
    repo: Path, *, log: "worker.Logger" = lambda _m: None
) -> "worker.GitWorktreeSubmitter":
    worktrees = repo / ".flywheel" / "worktrees"
    worktrees.mkdir(parents=True, exist_ok=True)
    return worker.GitWorktreeSubmitter(
        repo_root=repo,
        tasks_dir=repo / ".flywheel" / "tasks",
        worktrees_dir=worktrees,
        phase_base="main",
        lock_path=repo / ".flywheel" / ".merge.lock",
        log=log,
    )


def _signals() -> InvocationSignals:
    return InvocationSignals(
        stop_reason="end_turn",
        num_turns=1,
        total_cost_usd=0.0,
        result_is_error=False,
        result_subtype="success",
        api_error_status=None,
        session_id="sess",
    )


def _messages() -> tuple[object, ...]:
    return (
        AssistantMessage(
            content=[TextBlock(text="done")],
            model="claude-test",
            stop_reason="end_turn",
            session_id="sess",
            usage=None,
        ),
        ResultMessage(
            subtype="success",
            duration_ms=1,
            duration_api_ms=1,
            is_error=False,
            num_turns=1,
            session_id="sess",
            stop_reason="end_turn",
            total_cost_usd=0.0,
            usage=None,
        ),
    )


def _verify_result() -> IterationResult:
    return IterationResult(
        transcript="ok",
        messages=_messages(),  # type: ignore[arg-type]
        envelope=ValidEnvelope(intent=Intent.VERIFY),
        signals=_signals(),
        failure=None,
    )


class _ConcurrencyProbe:
    """Counts how many fake-agent invokes are in flight at once, thread-safely.

    The peak it records is the observable 'tasks simultaneously executing'
    signal the concurrency criteria assert against -- measured at the invoke
    boundary, so it captures *every* instant a task is mid-execution rather
    than sampling the store and risking a miss.
    """

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self.in_flight = 0
        self.peak = 0

    def enter(self) -> None:
        with self._lock:
            self.in_flight += 1
            self.peak = max(self.peak, self.in_flight)

    def leave(self) -> None:
        with self._lock:
            self.in_flight -= 1


# --- #4: exactly-once under induced overlap ---------------------------------


def test_pool_runs_each_task_exactly_once_under_induced_overlap(
    tmp_path: Path,
) -> None:
    """The pool analog of
    ``test_fresh_selection_rechecks_terminal_state_under_claim`` /
    ``test_two_workers_run_each_task_exactly_once``: M independent tasks driven
    by N concurrent members with *induced* (not load-dependent) overlap.

    A barrier of width N gates the first invoke of every member, so N tasks are
    provably mid-execution at one instant before any completes -- real overlap,
    not N serial runs. Under that contention every task must execute to exactly
    one DONE run, and the union of executed tasks must cover all M with none
    twice: the exactly-once invariant the lease/claim layer (4dc477b) owns,
    asserted under the pool.
    """
    repo = tmp_path / "repo"
    _init_repo(repo)
    n_members = 3
    m_tasks = 9
    task_ids = [f"t{i:02d}" for i in range(m_tasks)]
    for tid in task_ids:
        _task_file(repo, "01-phase", tid)
    db_path = repo / ".flywheel" / "flywheel.sqlite"
    sandbox_root = repo / ".flywheel" / "sandboxes"

    probe = _ConcurrencyProbe()
    # A single N-wide rendezvous: the first invoke from each of the N members
    # blocks here until all N have arrived, guaranteeing a moment of true N-way
    # overlap. Later invokes skip it (set once it fires) so the final, smaller
    # batch of tasks cannot deadlock waiting for an Nth party that never comes.
    barrier = threading.Barrier(n_members, timeout=30)
    overlap_reached = threading.Event()

    def _make_invoke():
        async def _invoke(_request: InvocationRequest) -> IterationResult:
            probe.enter()
            try:
                if not overlap_reached.is_set():
                    try:
                        barrier.wait()
                        overlap_reached.set()
                    except threading.BrokenBarrierError:
                        pass
                else:
                    await asyncio.sleep(0.01)
            finally:
                probe.leave()
            return _verify_result()

        return _invoke

    results: dict[str, object] = {}

    def _member(worker_id: str) -> None:
        results[worker_id] = asyncio.run(
            orchestrate(
                tasks_dir=repo / ".flywheel" / "tasks",
                db_path=db_path,
                sandbox_root=sandbox_root / worker_id,
                invoke=_make_invoke(),
                worker_id=worker_id,
                max_retries=0,
                max_turns=4,
                lease_seconds=60,
                stream=io.StringIO(),
            )
        )

    member_ids = [f"pool-{i}" for i in range(n_members)]
    threads = [
        threading.Thread(target=_member, args=(wid,)) for wid in member_ids
    ]
    for t in threads:
        t.start()
    for t in threads:
        t.join(60)
    assert all(not t.is_alive() for t in threads), "a member hung"

    # Real overlap was induced: N tasks executed at the same instant. A pool
    # whose tasks were too fast to overlap (or that serialized on a shared lock)
    # never reaches N here, so this is not a load-dependent pass.
    assert probe.peak == n_members
    assert overlap_reached.is_set()

    all_runs = [r for wid in member_ids for r in results[wid].runs]  # type: ignore[attr-defined]
    runs_by_task = Counter(r.task_id for r in all_runs)
    # Exactly one DONE run per task; union covers all M; none ran twice.
    assert set(runs_by_task) == set(task_ids)
    assert all(count == 1 for count in runs_by_task.values()), runs_by_task
    assert len(all_runs) == m_tasks
    assert all(r.status is Status.DONE for r in all_runs)

    # The store agrees: every task is terminally DONE exactly once.
    store = SqliteStore(db_path)
    try:
        rows = store._connection.execute(  # noqa: SLF001
            "SELECT task_id, COUNT(*) AS n FROM lifecycles "
            "WHERE status = ? GROUP BY task_id",
            (Status.DONE.value,),
        ).fetchall()
    finally:
        store.close()
    done_counts = {row["task_id"]: row["n"] for row in rows}
    assert set(done_counts) == set(task_ids)
    assert all(n == 1 for n in done_counts.values()), done_counts


# --- #10: concurrent landing, serialized + re-verified ----------------------


def test_pool_concurrent_landings_serialize_and_reverify(
    tmp_path: Path,
) -> None:
    """Two members completing near-simultaneously each land only after
    re-verifying against the exact base they land on; the final base contains
    both changes with no partial/interleaved merge (criterion #10, D-3).

    Both tasks branch from the same original base and commit a distinct file,
    then rendezvous at a barrier so they finish together and contend at the
    merge-flock. The flock serializes the two base merges: one fast-forwards;
    the other now sees a moved base, so it can only land via the
    rebase-onto-base -> re-run command graders -> fast-forward path. The proof:
    both land, the post-drain base is a linear history containing both commits
    and both files, the rebase+re-verify log markers fired for the second
    lander, and the full grader suite passes on the landed base.
    """
    repo = tmp_path / "repo"
    _init_repo(repo)
    base_before = _rev(repo, "main")
    worktrees = repo / ".flywheel" / "worktrees"
    db_path = repo / ".flywheel" / "flywheel.sqlite"

    # task -> the file it commits. Each grader checks its own file exists, so a
    # post-rebase re-verify (cwd'd to the rebased tree) is a real, tree-
    # dependent check, not a no-op.
    files = {"alpha": "alpha.txt", "beta": "beta.txt"}
    for tid, fname in files.items():
        _task_file(repo, "01-phase", tid, grader=f"test -f {fname}")

    barrier = threading.Barrier(len(files), timeout=30)

    def _task_of(prompt: str) -> str:
        # The prompt embeds the task goal ("Goal for <task_id>."); identify
        # which task this invoke is driving so it commits into the right
        # per-task worktree.
        for tid in files:
            if f"Goal for {tid}." in prompt:
                return tid
        raise AssertionError(f"no known task id in prompt: {prompt[:80]!r}")

    def _make_invoke(worker_id: str):
        async def _invoke(request: InvocationRequest) -> IterationResult:
            task_id = _task_of(request.prompt)
            fname = files[task_id]
            wt = worktrees / task_id
            _commit(wt, fname, f"{task_id} body\n", f"add {fname}")
            # Finish together so both DONE branches contend at the merge-flock.
            try:
                barrier.wait()
            except threading.BrokenBarrierError:
                pass
            return _verify_result()

        return _invoke

    # Per-member captured logs: the second lander's submit() must log the
    # rebase + re-verify + merge-after-rebase sequence, proving re-verification
    # ran under contention rather than being skipped.
    log_lock = threading.Lock()
    logs: list[str] = []

    def _make_log(worker_id: str) -> "worker.Logger":
        def _log(message: str) -> None:
            with log_lock:
                logs.append(f"{worker_id} {message}")

        return _log

    results: dict[str, object] = {}

    def _member(worker_id: str) -> None:
        submitter = _submitter(repo, log=_make_log(worker_id))
        results[worker_id] = worker.run_once(
            submitter,
            tasks_dir=repo / ".flywheel" / "tasks",
            db_path=db_path,
            worktrees_dir=worktrees,
            model=None,
            max_turns=4,
            max_retries=0,
            worker_id=worker_id,
            lease_seconds=60,
            invoke=_make_invoke(worker_id),
            log=_make_log(worker_id),
        )

    member_ids = ["pool-0", "pool-1"]
    threads = [
        threading.Thread(target=_member, args=(wid,)) for wid in member_ids
    ]
    for t in threads:
        t.start()
    for t in threads:
        t.join(60)
    assert all(not t.is_alive() for t in threads), "a member hung"

    # Both tasks were executed exactly once and reached DONE (both landed).
    all_runs = [r for wid in member_ids for r in results[wid].runs]  # type: ignore[attr-defined]
    runs_by_task = Counter(r.task_id for r in all_runs)
    assert set(runs_by_task) == set(files)
    assert all(count == 1 for count in runs_by_task.values()), runs_by_task
    assert all(r.status is Status.DONE for r in all_runs)

    # The post-drain base contains BOTH changes, with no partial/interleaved
    # merge: a linear history (no merge commits) of init + the two task commits.
    for fname in files.values():
        assert (repo / fname).exists(), f"{fname} missing from landed base"
    assert _rev(repo, "main") != base_before
    merges = _git(repo, "log", "--merges", "--format=%H", "main")
    assert merges == "", f"landing produced a merge commit: {merges!r}"
    commit_subjects = _git(
        repo, "log", "--format=%s", "main"
    ).splitlines()
    assert commit_subjects == ["add beta.txt", "add alpha.txt", "init"] or (
        commit_subjects == ["add alpha.txt", "add beta.txt", "init"]
    ), commit_subjects

    # Submit-time re-verification provably ran for the second lander: its
    # branch could not fast-forward the moved base, so it landed only via the
    # rebase -> re-verify -> merge-after-rebase path.
    joined = "\n".join(logs)
    assert "rebasing onto main" in joined, joined
    assert "re-verify " in joined, joined
    assert "after rebase" in joined, joined

    # The full grader suite is green on the landed base: both tasks' command
    # graders pass against the merged tree (no broken/partial land slipped in).
    for fname in files.values():
        check = subprocess.run(
            ["test", "-f", fname],
            cwd=repo,
            check=False,
        )
        assert check.returncode == 0, f"suite red on landed base for {fname}"


# --- #1: back-compat -- concurrency 1 stays strictly serial ------------------


def test_single_worker_keeps_at_most_one_task_in_progress(
    tmp_path: Path,
) -> None:
    """A pool of size 1 (the unconfigured default member path) keeps at most one
    task in the in-progress state at any observed instant across a multi-task
    drain (criterion #1).

    The default ``flywheel worker`` (concurrency unset/1) runs a single member,
    which must execute its queue strictly serially -- never silently pooling.
    The invoke-boundary probe observes every instant a task is mid-execution; a
    size-1 member that ever ran two tasks at once would push the peak to 2 and
    fail this test. A small dwell in each invoke makes any accidental overlap
    observable rather than racing past the probe.
    """
    repo = tmp_path / "repo"
    _init_repo(repo)
    task_ids = ["s0", "s1", "s2", "s3"]
    for tid in task_ids:
        _task_file(repo, "01-phase", tid)
    db_path = repo / ".flywheel" / "flywheel.sqlite"

    probe = _ConcurrencyProbe()

    async def _invoke(_request: InvocationRequest) -> IterationResult:
        probe.enter()
        try:
            # Dwell so any concurrent entry would overlap here and lift the peak.
            await asyncio.sleep(0.02)
        finally:
            probe.leave()
        return _verify_result()

    submitter = _submitter(repo)
    report = worker.run_once(
        submitter,
        tasks_dir=repo / ".flywheel" / "tasks",
        db_path=db_path,
        worktrees_dir=repo / ".flywheel" / "worktrees",
        model=None,
        max_turns=4,
        max_retries=0,
        worker_id="solo",
        lease_seconds=60,
        invoke=_invoke,
    )

    # The single worker drained every task...
    ran = Counter(r.task_id for r in report.runs)
    assert set(ran) == set(task_ids)
    assert all(count == 1 for count in ran.values()), ran
    assert all(r.status is Status.DONE for r in report.runs)
    # ...and never had more than one task executing at once. The probe must have
    # observed activity (peak >= 1), and that peak must be exactly 1.
    assert probe.peak == 1, f"single worker overlapped tasks: peak={probe.peak}"

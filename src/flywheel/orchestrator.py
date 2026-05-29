"""Cross-task orchestrator — the consumer-side scheduler (P4 + P5).

``docs/strategy.md`` is explicit that *strategy lives in the consumer, not
the loop*: the harness owns a single task's lifecycle; deciding **which**
task runs next is a layer above it. This module is that layer. It only
*reads* authoritative lifecycle state and *decides what to run next*; it
never calls ``transition_to`` and holds no special harness privilege.

It replaces the poll loops a shell driver (``.workflow/task-worker.sh``)
otherwise runs — repeatedly shelling ``workflow next`` / ``workflow run`` /
``workflow recheck-blocked`` — with one in-process driver that re-evaluates
after each run it drives.

Scheduling, reusing the exact predicates the pull-based CLI already uses
(:func:`flywheel.workflow.select_next_task` /
:func:`~flywheel.workflow.build_status_rows`):

* **Prerequisite promotion** — a fresh/retryable task runs only once every
  task in its ``prerequisites`` has a ``DONE`` lifecycle.
* **Reactive unblocking** — a blocked-interrupted lifecycle (``INTERRUPTED``
  with a persisted ``blocked_requires`` snapshot) is re-evaluated via
  :func:`flywheel.harness.recheck_blocked_lifecycle`; when its predicates
  now hold it is unblocked and **resumed on its own run_id** (continuing its
  history), not re-run from scratch. Blocked tasks are excluded from fresh
  selection so they are never wastefully re-run while still blocked.

**Multi-worker (P5).** Several orchestrators may share one store. Before
running a task a worker must acquire its :class:`~flywheel.store_protocols.
ClaimStore` lease; a task held by a live lease is skipped, so no two workers
run the same task. A background heartbeat renews the lease while the run is
in flight, so the lease doubles as a liveness signal: if a worker crashes,
its heartbeat stops, the lease lapses, and another worker reclaims the task.
A worker that can claim nothing eligible exits — the remaining claim-holders
drain the rest and re-evaluate after each completion.

Authoritative-read discipline: every decision is made from the persisted
projection, so the worst failure mode is wasted latency, never a wrong
schedule. Termination is guaranteed by two per-session guards — at most one
fresh run per task id and at most one resume per run id.

Sandboxing: each task runs in ``sandbox_root/<task-id>``. Real
worktree/branch/merge mechanics remain a consumer "submit" concern layered
on top (out of scope here); this module owns selection and execution order.
"""

from __future__ import annotations

import threading
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable, Literal, TextIO
from uuid import uuid4

from flywheel.harness import (
    InvokeFunc,
    finalize_stranded_lifecycle,
    recheck_blocked_lifecycle,
)
from flywheel.lifecycle import Status
from flywheel.store_protocols import (
    ClaimLostError,
    OptimisticConcurrencyError,
    TaskClaim,
)
from flywheel.store_sqlite import SqliteStore
from flywheel.task import Task
from flywheel.workflow import (
    DEFAULT_MAX_RETRIES,
    DEFAULT_MAX_TURNS,
    TaskStatusRow,
    _stranded_run_ids,
    build_status_rows,
    run_task_file,
    select_next_task,
)

# Default lease window and how often the heartbeat renews it. The window is
# generous so a normal run finishes well within it; the heartbeat renews at
# roughly a third of it, so a crashed worker's lease lapses within ~one
# window of its death and the task becomes reclaimable.
#
# Lease expiry is compared against a wall clock (datetime.now), and the
# ``now`` each worker injects is its own host clock -- there is no shared or
# monotonic time source across hosts. So ``lease_seconds`` must comfortably
# exceed (max cross-host clock skew + the longest a healthy worker can go
# between heartbeat renewals); otherwise a worker with a fast clock can treat
# a live peer's lease as expired and steal it. The steal is contained (see
# _drive_or_relinquish) but wastes the preempted work. Tighten only with NTP
# discipline across the fleet.
DEFAULT_LEASE_SECONDS: float = 300.0


def _utcnow() -> datetime:
    return datetime.now(timezone.utc)


@dataclass(frozen=True, kw_only=True)
class RunRecord:
    """One task execution the orchestrator drove, in launch order."""

    task_id: str
    run_id: str
    status: Status
    mode: Literal["fresh", "resume"]
    worker_id: str


@dataclass(frozen=True, kw_only=True)
class OrchestratorReport:
    """Outcome of an :func:`orchestrate` session.

    ``recovered`` lists run ids finalized from a stranded
    ``running``/``validating`` state at entry; ``runs`` lists every task
    execution this worker launched, in order.
    """

    worker_id: str
    recovered: tuple[str, ...]
    runs: tuple[RunRecord, ...]


def _is_blocked_interrupted(row: TaskStatusRow) -> bool:
    """A lifecycle parked on a structured block (vs a bare SIGINT pause).

    Only these are routed through reactive recheck/resume; they are kept
    out of fresh selection so an unsatisfied block is never re-run blindly.
    """
    return (
        row.latest_status == Status.INTERRUPTED
        and row.blocked_requires is not None
    )


def _recover_claimable_stranded(
    control: SqliteStore,
    worker_id: str,
    *,
    lease_seconds: float,
    now: Callable[[], datetime],
) -> tuple[str, ...]:
    """Finalize stranded lifecycles whose task has no live owner.

    A lifecycle stuck in ``running``/``validating`` is *stranded* only if no
    worker is actually running it. In single-worker use that is every such
    row; with peers it is not — a peer's live run looks identical. The claim
    is the liveness signal that tells them apart: acquiring a task's claim
    proves no live worker holds it (the lease is free or lapsed), so its
    stranded lifecycle is safe to finalize. A task a peer is actively running
    cannot be claimed, so its run is left untouched.
    """
    recovered: list[str] = []
    for run_id in _stranded_run_ids(control):
        lifecycle = control.load_lifecycle(run_id)
        if lifecycle is None:
            continue
        claim = control.acquire_claim(
            lifecycle.task_id,
            worker_id,
            now=now(),
            lease_seconds=lease_seconds,
        )
        if claim is None:
            # A live peer owns this task; its run is not stranded.
            continue
        try:
            # finalize_stranded_lifecycle takes a clock callable, unlike the
            # claim methods which take a concrete ``now`` value.
            if finalize_stranded_lifecycle(control, run_id, now=now):
                recovered.append(run_id)
        finally:
            control.release_claim(claim)
    return tuple(recovered)


class _ClaimHeartbeat:
    """Renew a task lease on a timer while its run is in flight.

    The heartbeat is the worker's liveness proof: as long as the process is
    alive the lease is renewed, so another worker never reclaims a task that
    is actually running. If the process dies the thread dies with it, the
    lease lapses, and the task becomes reclaimable. A lost lease (the worker
    stalled past the window, or a clock skew let another worker steal it)
    stops renewal; the run's later ``release`` on the stale token is a no-op.
    """

    def __init__(
        self,
        *,
        store: SqliteStore,
        claim: TaskClaim,
        lease_seconds: float,
        interval: float,
        now: Callable[[], datetime],
    ) -> None:
        self._store = store
        self._claim = claim
        self._lease_seconds = lease_seconds
        self._interval = interval
        self._now = now
        self._stop = threading.Event()
        self._lock = threading.Lock()
        self._thread = threading.Thread(
            target=self._run,
            name=f"flywheel-lease-{claim.task_id}",
            daemon=True,
        )

    def start(self) -> _ClaimHeartbeat:
        self._thread.start()
        return self

    def _run(self) -> None:
        while not self._stop.wait(self._interval):
            try:
                renewed = self._store.renew_claim(
                    self._claim,
                    now=self._now(),
                    lease_seconds=self._lease_seconds,
                )
            except ClaimLostError:
                return
            except Exception:  # noqa: BLE001 - transient store error; retry
                continue
            with self._lock:
                self._claim = renewed

    def stop(self) -> TaskClaim:
        """Stop renewing and return the latest token for release."""
        self._stop.set()
        self._thread.join(timeout=self._interval + 1.0)
        with self._lock:
            return self._claim


async def orchestrate(
    *,
    tasks_dir: Path,
    db_path: Path,
    sandbox_root: Path,
    invoke: InvokeFunc | None = None,
    model: str | None = None,
    max_turns: int = DEFAULT_MAX_TURNS,
    max_retries: int = DEFAULT_MAX_RETRIES,
    worker_id: str | None = None,
    lease_seconds: float = DEFAULT_LEASE_SECONDS,
    stream: TextIO | None = None,
    now: Callable[[], datetime] | None = None,
) -> OrchestratorReport:
    """Drive every eligible task in ``tasks_dir`` to quiescence.

    Returns once this worker can make no further progress: no blocked
    lifecycle it can claim unblocks, and no fresh task is both eligible and
    claimable. Safe to run as several concurrent workers against one store —
    per-task leases keep them from double-running a task. ``invoke`` defaults
    to the real Claude Code invoker; tests inject a fake callable.
    """
    clock = now or _utcnow
    wid = worker_id or f"worker-{uuid4().hex[:8]}"
    heartbeat_interval = max(lease_seconds / 3.0, 0.001)

    db_path.parent.mkdir(parents=True, exist_ok=True)
    sandbox_root.mkdir(parents=True, exist_ok=True)

    control = SqliteStore(db_path)
    try:
        recovered = _recover_claimable_stranded(
            control, wid, lease_seconds=lease_seconds, now=clock
        )
        runs: list[RunRecord] = []
        attempted_fresh: set[str] = set()
        attempted_resume: set[str] = set()

        while True:
            rows = build_status_rows(tasks_dir, control)
            task_by_id: dict[str, Task] = {r.task.id: r.task for r in rows}
            file_by_id: dict[str, Path] = {
                r.task.id: r.task_file for r in rows
            }
            blocked_ids = frozenset(
                r.task.id for r in rows if _is_blocked_interrupted(r)
            )

            # 1. Reactive unblock + resume, claim-gated so only one worker
            #    handles a given blocked task. The first that unblocks is
            #    resumed on its own run_id; we then restart so the changed
            #    state is re-read.
            progressed = False
            for row in rows:
                if not _is_blocked_interrupted(row):
                    continue
                run_id = row.latest_run_id
                if run_id is None or run_id in attempted_resume:
                    continue
                claim = control.acquire_claim(
                    row.task.id,
                    wid,
                    now=clock(),
                    lease_seconds=lease_seconds,
                )
                if claim is None:
                    continue  # another worker owns this task right now
                try:
                    try:
                        outcome = recheck_blocked_lifecycle(
                            control,
                            run_id,
                            task_by_id[row.task.id],
                            cwd=sandbox_root / row.task.id,
                        )
                    except OptimisticConcurrencyError:
                        # Another worker transitioned it first; let go.
                        continue
                    if not outcome.applied:
                        continue
                    attempted_resume.add(run_id)
                    record = await _drive_or_relinquish(
                        control,
                        claim,
                        file_by_id[row.task.id],
                        db_path=db_path,
                        sandbox_root=sandbox_root,
                        task_id=row.task.id,
                        run_id=run_id,
                        worker_id=wid,
                        lease_seconds=lease_seconds,
                        heartbeat_interval=heartbeat_interval,
                        invoke=invoke,
                        model=model,
                        max_turns=max_turns,
                        max_retries=max_retries,
                        stream=stream,
                        now=clock,
                    )
                    # _drive_under_lease releases the lease in all cases
                    # (success or claim loss), so drop our token either way.
                    claim = None
                    if record is not None:
                        runs.append(record)
                    progressed = True
                    break
                finally:
                    if claim is not None:
                        control.release_claim(claim)
            if progressed:
                continue

            # 2. Fresh selection over the prerequisite graph. Exclude
            #    still-blocked lifecycles and tasks already run this session
            #    from candidacy while keeping the full row set so a DONE
            #    dependency still promotes its dependents. Skip — but do not
            #    consume — tasks a peer worker currently holds.
            held: set[str] = set()
            ran_fresh = False
            while True:
                exclude = attempted_fresh | blocked_ids | held
                pick = select_next_task(rows, exclude_ids=frozenset(exclude))
                if pick is None:
                    break
                claim = control.acquire_claim(
                    pick.task.id,
                    wid,
                    now=clock(),
                    lease_seconds=lease_seconds,
                )
                if claim is None:
                    held.add(pick.task.id)
                    continue
                attempted_fresh.add(pick.task.id)
                record = await _drive_or_relinquish(
                    control,
                    claim,
                    pick.task_file,
                    db_path=db_path,
                    sandbox_root=sandbox_root,
                    task_id=pick.task.id,
                    run_id=None,
                    worker_id=wid,
                    lease_seconds=lease_seconds,
                    heartbeat_interval=heartbeat_interval,
                    invoke=invoke,
                    model=model,
                    max_turns=max_turns,
                    max_retries=max_retries,
                    stream=stream,
                    now=clock,
                )
                if record is not None:
                    runs.append(record)
                ran_fresh = True
                break
            if ran_fresh:
                continue

            # Nothing to resume and nothing fresh we could claim: done.
            break

        return OrchestratorReport(
            worker_id=wid, recovered=recovered, runs=tuple(runs)
        )
    finally:
        control.close()


async def _drive_under_lease(
    control: SqliteStore,
    claim: TaskClaim,
    task_file: Path,
    *,
    db_path: Path,
    sandbox_root: Path,
    task_id: str,
    run_id: str | None,
    worker_id: str,
    lease_seconds: float,
    heartbeat_interval: float,
    invoke: InvokeFunc | None,
    model: str | None,
    max_turns: int,
    max_retries: int,
    stream: TextIO | None,
    now: Callable[[], datetime],
) -> RunRecord:
    """Run (or resume) one task while a heartbeat holds its lease, then
    release the lease.

    ``run_task_file`` opens and closes its own store, so its writes are
    committed and visible to the orchestrator's control store on the next
    query (separate SQLite connections under WAL). The control store is
    touched only by the heartbeat thread for the duration of the run, never
    concurrently with the main thread.
    """
    heartbeat = _ClaimHeartbeat(
        store=control,
        claim=claim,
        lease_seconds=lease_seconds,
        interval=heartbeat_interval,
        now=now,
    ).start()
    try:
        outcome = await run_task_file(
            task_file,
            db_path=db_path,
            sandbox=sandbox_root / task_id,
            model=model,
            max_turns=max_turns,
            max_retries=max_retries,
            invoke=invoke,
            stream=stream,
            run_id=run_id,
        )
    finally:
        latest = heartbeat.stop()
        control.release_claim(latest)
    return RunRecord(
        task_id=task_id,
        run_id=outcome.lifecycle.run_id,
        status=outcome.lifecycle.status,
        mode="resume" if run_id is not None else "fresh",
        worker_id=worker_id,
    )


async def _drive_or_relinquish(
    control: SqliteStore,
    claim: TaskClaim,
    task_file: Path,
    *,
    task_id: str,
    stream: TextIO | None,
    **kwargs: Any,
) -> RunRecord | None:
    """Drive one task, containing a mid-run claim loss to that one task.

    If a peer steals this task's lease while it runs (the worker stalled
    past the window, or clock skew let another worker reclaim it), the
    peer can finalize the lifecycle out from under us. The losing harness
    then hits the optimistic-concurrency check on its next domain append
    (or its heartbeat raises :class:`ClaimLostError`). Without this guard
    that exception unwinds all the way out of :func:`orchestrate`, killing
    the whole worker and abandoning every other task it was draining.

    Catching it here turns a lost race into a re-evaluation: we return
    ``None``, the caller records no run for it, and the main loop re-reads
    authoritative state on the next pass. Consistent with the module's
    invariant that the worst failure mode is wasted latency, never a wrong
    schedule.
    """
    try:
        return await _drive_under_lease(
            control,
            claim,
            task_file,
            task_id=task_id,
            stream=stream,
            **kwargs,
        )
    except (OptimisticConcurrencyError, ClaimLostError) as exc:
        if stream is not None:
            print(
                f"[orchestrate] {task_id}: claim lost mid-run "
                f"({type(exc).__name__}); a peer took over, re-evaluating",
                file=stream,
                flush=True,
            )
        return None


__all__ = [
    "DEFAULT_LEASE_SECONDS",
    "OrchestratorReport",
    "RunRecord",
    "orchestrate",
]

"""Cross-task orchestrator — the consumer-side scheduler (P4 + P5).

``docs/strategy.md`` is explicit that *strategy lives in the consumer, not
the loop*: the harness owns a single task's lifecycle; deciding **which**
task runs next is a layer above it. This module is that layer. It only
*reads* authoritative lifecycle state and *decides what to run next*; it
never calls ``transition_to`` and holds no special harness privilege.

It replaces the poll loops a shell driver otherwise runs — repeatedly shelling
``workflow next`` / ``workflow run`` / ``workflow recheck-blocked`` — with one
in-process driver that re-evaluates after each run it drives. The git-worktree
consumer (the ``flywheel-worktree`` package) wraps it, injecting worktree
submit through the ``prepare_sandbox`` / ``submit`` seam below.

Scheduling, reusing the exact predicates the pull-based CLI already uses
(:func:`flywheel_core.workflow.select_next_task` /
:func:`~flywheel_core.workflow.build_status_rows`):

* **Prerequisite promotion** — a fresh/retryable task runs only once every
  task in its ``prerequisites`` has a ``DONE`` lifecycle.
* **Reactive unblocking** — a blocked-interrupted lifecycle (``INTERRUPTED``
  with a persisted ``blocked_requires`` snapshot) is re-evaluated via
  :func:`flywheel_core.harness.recheck_blocked_lifecycle`; when its predicates
  now hold it is unblocked and **resumed on its own run_id** (continuing its
  history), not re-run from scratch. Blocked tasks are excluded from fresh
  selection so they are never wastefully re-run while still blocked.

**Multi-worker (P5).** Several orchestrators may share one store. Before
running a task a worker must acquire its :class:`~flywheel_core.store_protocols.
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

Sandboxing: each task runs in ``sandbox_root/<task-id>`` by default. Real
worktree/branch/merge mechanics remain a consumer "submit" concern, injected
through the optional ``prepare_sandbox`` / ``submit`` callbacks (see
:func:`orchestrate`); this module owns selection and execution order, never
git.
"""

from __future__ import annotations

import asyncio
import os
import threading
from collections.abc import Awaitable, Iterable, Mapping
from contextlib import suppress
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import TYPE_CHECKING, Any, Callable, Literal, TextIO
from uuid import uuid4

from flywheel_core.harness import (
    BUDGET_CEILING_ERROR_PREFIX,
    InvokeFunc,
    finalize_stranded_lifecycle,
    recheck_blocked_lifecycle,
    resolve_manual_approval,
)
from flywheel_core.events import (
    GATE_EXCERPT_MAX_BYTES,
    LANDING_PARK_KINDS,
    PARK_KIND_HELD_OUT_GATE,
    REDRIVE_RESULT_LANDED,
    REDRIVE_RESULT_REPARKED,
    REDRIVE_RESULT_ROUTED,
    GateGraderReceipt,
    HeldOutGateEvaluated,
    LandingParked,
    LandingRedriven,
)
from flywheel_core.invoker_client import CONTROL_COMMAND_INTERRUPT
from flywheel_core.lifecycle import Lifecycle, Outcome, Status
from flywheel_core.store_protocols import (
    OptimisticConcurrencyError,
    TelemetrySink,
)
from flywheel_core.store_sqlite import SqliteStore
from flywheel_core.telemetry_file import FileTelemetrySink

if TYPE_CHECKING:
    # Optional postgres backend, typing-only so this module never hard-depends
    # on the psycopg extra. The store factory returns SqliteStore |
    # PostgresStore and both answer these reads through the store protocol.
    from flywheel_core.store_postgres import PostgresStore
    from flywheel_orchestrator._claims_postgres import PostgresClaimStore
from flywheel_core.task import Task
from flywheel_core.validation import validate_task
from flywheel_orchestrator._claims import (
    REASON_ABORTED,
    REASON_AWAITING_APPROVAL,
    REASON_BUDGET_CEILING,
    REASON_NO_PROGRESS,
    REASON_PREREQUISITE_MISSING,
    REASON_RETRIES_EXHAUSTED_AFTER_ESCALATION,
    STOP_DANGLING_PREREQUISITE,
    STOP_NO_PROGRESS,
    STOP_NO_PROGRESS_RESET,
    STOP_RETRIES_ESCALATED,
    ClaimLostError,
    GraphSnapshotItem,
    SourceSyncRecord,
    SqliteClaimStore,
    TaskClaim,
)
from flywheel_core.workflow import (
    DEFAULT_MAX_RETRIES,
    DEFAULT_MAX_TURNS,
    _stranded_run_ids,
    run_task_object,
)
from flywheel_orchestrator._held_out_gate import (
    GateOutcome,
    GateVerdict,
    HeldOutGraderSource,
    evaluate_held_out_gate,
)
from flywheel_orchestrator._policy import (
    SandboxPolicy,
    WorkPolicy,
    resolve_grader_env,
)
from flywheel_orchestrator._sources import (
    DirectoryWorkSource,
    GraderReceipt,
    WorkItem,
    WorkReport,
    WorkSource,
)
from flywheel_orchestrator._store_factory import (
    build_claim_store,
    open_sqlite_bound_store,
    preflight_store,
)
from flywheel_orchestrator._work_graph import (
    GraphValidationIssue,
    WorkGraph,
    WorkGraphBuilder,
)
from flywheel_orchestrator._strategy import (
    SandboxHandle,
    SandboxProvider,
    SandboxRequest,
    SubmitRequest,
    SubmitStrategy,
    Submitter,
    _as_handle,
    probe_landability,
)
from flywheel_orchestrator._workflow import (
    TaskState,
    TaskStatusRow,
    _latest_lifecycle_row,
    status_rows_for_items,
    task_state,
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

# How often the source reconciler (the steering bridge) re-lists the work
# source while runs are in flight. The bridge's rule: an in-flight run whose
# item is no longer listed by its source gets an ``interrupt`` control
# command. 15s keeps tracker API traffic negligible while bounding how long
# a cancelled item keeps burning tokens.
DEFAULT_RECONCILE_SECONDS: float = 15.0

# How often the in-loop expired-lease sweep runs while the loop drives (spec
# 00069). The active counterpart to the entry-time recovery backstop: a worker
# that dies mid-task has its lapsed lease released and its stranded lifecycle
# finalized on this cadence, so the task returns to eligibility without waiting
# for another worker to happen to re-select it. 15s mirrors the reconciler --
# negligible store traffic, and well inside a healthy lease window so a live
# claim is never in the reap set.
DEFAULT_SWEEP_SECONDS: float = 15.0

# How many automatic land re-attempts the landing re-driver makes for one parked
# run before routing it to the human-review queue (spec 00069, criteria #3/#4).
# A parked DONE run -- work finished and graded green but the strategy could not
# land it (divergent base, a failed standing build invariant) -- is re-driven
# through the strategy's own rebase/reverify/standing/FF path up to this many
# times; on the bound it is routed to the queue with its park_kind as the
# machine-readable reason and no further attempt is made. Three is generous
# enough to ride out a transient base-divergence race yet bounded so a
# genuinely-stuck strand reaches a human promptly.
DEFAULT_LANDING_REDRIVE_BOUND: int = 3

# How many sanctioned escalations the retry-escalation re-driver spends before
# routing a task to the human-review queue (spec 00069, criteria #5/#6; D-A).
# On first retry-budget exhaustion the re-driver escalates exactly ONCE -- a
# stronger-model / re-decompose re-drive under the existing per-run budget
# ceilings -- and on the NEXT exhaustion (the escalated run also spent its
# budget) routes the task to the queue with
# ``retries-exhausted-after-escalation`` instead of re-escalating. One is the
# whole point: the escalation is bounded so a genuinely-stuck task reaches a
# human after a single stronger attempt, never re-escalating on every
# exhaustion.
DEFAULT_ESCALATION_BOUND: int = 1

# How many consecutive scheduling cycles a task's declared prerequisite may stay
# missing before the referencing task is routed to the human-review queue (spec
# 00069, criteria #7/#8/#13). While the prerequisite is absent the referencing
# task stays out of the ready set -- it is never dispatched against an
# unsatisfied prereq -- exactly as before; each cycle the prerequisite is still
# dangling records one ``dangling-prerequisite`` witness on the append-only stop
# ledger. On the cycle the witness count reaches this bound the task is routed to
# the queue ONCE with ``prerequisite-missing`` and the missing prerequisite id in
# the detail, and no further routing occurs. If the prerequisite instead appears
# (criterion #7) the graph rebuild resolves the edge and the task becomes
# eligible and is driven -- the bound is never reached. Three is generous enough
# to ride out a sibling source that lists its half of a cross-source dependency a
# pass or two late, yet finite so a genuinely absent prerequisite reaches a human
# promptly rather than spinning ineligible forever.
DEFAULT_PREREQ_REDRIVE_BOUND: int = 3

# How many consecutive cycles a "unit" may make NO observable progress before the
# no-progress back-off re-driver backs it off and routes it to the human-review
# queue (spec 00069, criteria #9/#13; D-C). A unit is a phase whose verify never
# passes or an autopilot repo that never authors a task -- something the loop
# keeps re-attempting with nothing to show for it. Each fruitless cycle records
# one ``no-progress-cycle`` witness on the unit's append-only ledger; on the cycle
# the consecutive count reaches this bound the unit is backed off (absent from the
# active set the next cycle) and routed ONCE with ``no-progress``. Observable
# progress (a state change, a task authored, a verify passing) appends a
# ``no-progress-reset`` marker so the streak restarts -- a unit that ever makes
# progress is never backed off. Three tolerates a couple of genuinely-slow cycles
# yet is finite, so a never-progressing unit cannot burn agent cost forever.
DEFAULT_NO_PROGRESS_BOUND: int = 3


def _utcnow() -> datetime:
    return datetime.now(timezone.utc)


def _live_run_rows(control: SqliteStore | PostgresStore) -> list[tuple[str, str]]:
    """``(run_id, task_id)`` for every lifecycle currently in flight.

    Only ``running``/``validating`` qualify: those are the statuses with a
    live worker session that an ``interrupt`` command can reach.
    ``AWAITING_APPROVAL`` is deliberately excluded — a parked gate has no
    session to interrupt, and its disposition belongs to the approve/reject
    resolver, not the steering bridge.
    """
    # Backend-agnostic read through the store protocol (not the SQLite-only
    # ``_connection``) so this works against a PostgresStore too. Sorted by
    # run_id to preserve the previous ``ORDER BY run_id`` determinism.
    live = control.list_lifecycles(
        statuses=(Status.RUNNING, Status.VALIDATING)
    )
    return sorted((lc.run_id, lc.task_id) for lc in live)


def reconcile_live_runs(
    control: SqliteStore | PostgresStore,
    wanted_task_ids: frozenset[str],
    *,
    already_signaled: set[str],
    now: datetime,
) -> tuple[str, ...]:
    """Enqueue ``interrupt`` for live runs whose item the source dropped.

    The steering bridge's core: ``wanted_task_ids`` is the current
    ``list_work()`` projection; any in-flight run whose task id is absent
    gets one ``interrupt`` control command (the spec-00013 store-routed
    path — the run's own watcher applies it, so this works for peer
    workers' runs too). ``already_signaled`` is the caller-owned dedup set:
    a run is signaled at most once per session even though it stays in
    ``running`` until the watcher acts. Returns the run ids newly signaled
    this call.

    The payload attributes the command in the audit stream; the watcher
    ignores interrupt payloads, so the attribution is free.
    """
    signaled: list[str] = []
    for run_id, task_id in _live_run_rows(control):
        if task_id in wanted_task_ids or run_id in already_signaled:
            continue
        control.enqueue_command(
            run_id,
            CONTROL_COMMAND_INTERRUPT,
            {
                "origin": "source-reconciler",
                "reason": (
                    f"task {task_id!r} is no longer listed by the work "
                    f"source"
                ),
            },
            now=now,
        )
        already_signaled.add(run_id)
        signaled.append(run_id)
    return tuple(signaled)


async def _source_reconcile_loop(
    *,
    source: WorkSource,
    control: SqliteStore | PostgresStore,
    interval: float,
    now: Callable[[], datetime],
    stream: TextIO | None,
) -> None:
    """Tick :func:`reconcile_live_runs` every ``interval`` seconds.

    Runs as a sibling asyncio task while :func:`orchestrate` awaits drives,
    so a mid-run operator edit (closed issue, pulled label, deleted task
    file) reaches the live session without waiting for the run to end.

    Failure posture: a listing failure NEVER interrupts anything — a
    tracker hiccup must not be read as "all work vanished" — and a store
    error skips the tick. Both are logged to ``stream`` and retried on the
    next tick; the loop only exits by cancellation.
    """
    already_signaled: set[str] = set()
    while True:
        await asyncio.sleep(interval)
        try:
            # Consume the validated graph: aggregate + validate the source's
            # items, then take the wanted ids from the built graph rather than
            # from a raw list_work(). A structural defect (duplicate id,
            # self-dependency, cycle) or a listing failure raises here and is
            # contained by the except below -- the tick is skipped and nothing
            # is interrupted, exactly the posture a bare list_work() failure
            # already had (a tracker hiccup is never read as "all work
            # vanished").
            graph = WorkGraphBuilder.build(source).graph
            wanted = frozenset(item.task.id for item in graph.items)
        except Exception as exc:  # noqa: BLE001 - adapter code
            if stream is not None:
                print(
                    f"[orchestrate] steering: work-source listing failed "
                    f"({type(exc).__name__}: {exc}); no runs interrupted, "
                    f"retrying next tick",
                    file=stream,
                    flush=True,
                )
            continue
        try:
            newly = reconcile_live_runs(
                control,
                wanted,
                already_signaled=already_signaled,
                now=now(),
            )
        except Exception as exc:  # noqa: BLE001 - transient store error
            if stream is not None:
                print(
                    f"[orchestrate] steering: reconcile failed "
                    f"({type(exc).__name__}: {exc}); retrying next tick",
                    file=stream,
                    flush=True,
                )
            continue
        if stream is not None:
            for run_id in newly:
                print(
                    f"[orchestrate] steering: run {run_id}'s item vanished "
                    f"from the work source; interrupt enqueued",
                    file=stream,
                    flush=True,
                )


def sync_work_source(
    source: WorkSource,
    store: SqliteClaimStore | PostgresClaimStore,
    *,
    source_kind: str,
    source_name: str,
    now: datetime,
) -> SourceSyncRecord:
    """Run one ``list_work()`` pass and persist it as a ``source_syncs`` run.

    Mirrors :func:`_source_reconcile_loop`'s posture at the storage layer
    (D-3): a sync opens a ``source_syncs`` row, runs the source's
    ``list_work()``, then settles. On success it upserts every observed item,
    replaces each item's dependency edges with the current graph, marks
    previously-seen-but-now-absent items disappeared, and finishes the run
    ``status='ok'`` with ``observed_count`` equal to the number observed. On a
    failed ``list_work()`` it finishes ``status='error'`` with a non-empty
    error and marks **nothing** disappeared — a tracker hiccup is never read as
    task disappearance (criterion #7). ``source_kind`` / ``source_name`` are the
    source's provenance/locus (D-4); read them off the adapter
    (``source.source_kind`` / ``source.source_name``). Returns the settled
    :class:`SourceSyncRecord`.
    """
    sync_id = store.record_source_sync_start(
        source_kind, source_name, now=now
    )
    try:
        items = list(source.list_work())
    except Exception as exc:  # noqa: BLE001 - adapter/transport failure
        # Failed listing: record the error and leave the catalog untouched.
        # NOTHING is marked disappeared (D-3 / criterion #7).
        store.record_source_sync_finish(
            sync_id,
            status="error",
            error=f"{type(exc).__name__}: {exc}",
            now=now,
        )
        settled = store.load_source_sync(sync_id)
        assert settled is not None  # just written above
        return settled
    for item in items:
        store.upsert_work_item(item, now=now)
        store.replace_work_item_dependencies(
            item.task.id, item.prerequisites, now=now
        )
    store.mark_work_items_disappeared(
        [item.task.id for item in items], now=now
    )
    store.record_source_sync_finish(
        sync_id, status="ok", observed_count=len(items), now=now
    )
    settled = store.load_source_sync(sync_id)
    assert settled is not None  # just written above
    return settled


def _assemble_graph_snapshot_items(
    items: list[WorkItem],
    graph: WorkGraph,
    states: dict[str, TaskState],
    *,
    worker_capabilities: frozenset[str],
    claims: SqliteClaimStore | PostgresClaimStore,
) -> list[GraphSnapshotItem]:
    """Materialize each pass work item's full cross-section for one snapshot.

    A pure read (spec 00055, D-4): per-item provenance and scheduling metadata
    come off the pass's :class:`WorkItem`s, lifecycle ``state`` and ``ready``
    membership off the same ``states`` + ``graph.ready_set`` the scheduler
    grades eligibility from, ``claim_holder`` off the live claim store, and
    ``resolved_prerequisites`` off the validated graph. It observes the
    scheduler's inputs and never mutates them, so recording a snapshot cannot
    steer dispatch.
    """
    ready_ids = {
        item.task.id
        for item in graph.ready_set(
            states, worker_capabilities=worker_capabilities
        )
    }
    holders = {
        claim.task_id: claim.worker_id for claim in claims.list_claims()
    }
    snapshot_items: list[GraphSnapshotItem] = []
    for item in items:
        task_id = item.task.id
        snapshot_items.append(
            GraphSnapshotItem(
                task_id=task_id,
                source_kind=item.source_kind,
                source_ref=item.source_ref,
                source_url=item.source_url,
                source_version=item.source_version,
                priority=item.priority,
                required_capabilities=item.required_capabilities,
                conflict_keys=item.conflict_keys,
                state=states[task_id].value,
                ready=task_id in ready_ids,
                claim_holder=holders.get(task_id),
                resolved_prerequisites=graph.resolved_prerequisites(task_id),
            )
        )
    return snapshot_items


@dataclass(frozen=True, kw_only=True)
class RunRecord:
    """One task execution the orchestrator drove, in launch order.

    ``gate`` is the held-out landing gate's verdict for this run (spec 00050,
    D-6): ``None`` when the gate did not run (no held-out source configured,
    or a non-landing terminal status), ``GateOutcome.NO_GATE``/``PASS`` when
    the task landed, and ``GateOutcome.FAIL`` when the gate blocked the land.
    This makes a gate failure distinguishable from both a clean land (``gate``
    is never ``FAIL`` there) and an agent-run failure (``status`` is not
    ``DONE`` there): only a gate-blocked land carries ``status == DONE`` with
    ``gate is GateOutcome.FAIL`` (criterion #6). ``gate_reason`` is the
    operator-readable summary behind that marker, empty when no gate ran.
    """

    task_id: str
    run_id: str
    status: Status
    mode: Literal["fresh", "resume"]
    worker_id: str
    gate: GateOutcome | None = None
    gate_reason: str = ""


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


def _is_awaiting_approval(row: TaskStatusRow) -> bool:
    """A lifecycle parked on a manual-approval gate.

    Routed through the reactive sweep alongside blocked recheck so a
    pending ``approve`` / ``reject`` command is applied on the next tick.
    Keyed off ``latest_status`` directly because ``AWAITING_APPROVAL`` is
    a status the harness owns end-to-end (unlike ``INTERRUPTED``, which
    also covers bare operator pauses and so requires the
    ``blocked_requires`` secondary check).
    """
    return row.latest_status == Status.AWAITING_APPROVAL


def _recover_claimable_stranded(
    control: SqliteStore | PostgresStore,
    claims: SqliteClaimStore | PostgresClaimStore,
    worker_id: str,
    *,
    lease_seconds: float,
    now: Callable[[], datetime],
    sink: TelemetrySink | None = None,
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
        claim = claims.acquire_claim(
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
            if finalize_stranded_lifecycle(
                control, run_id, now=now, sink=sink
            ):
                recovered.append(run_id)
        finally:
            claims.release_claim(claim)
    return tuple(recovered)


def sweep_expired_leases(
    control: SqliteStore | PostgresStore,
    claims: SqliteClaimStore | PostgresClaimStore,
    worker_id: str,
    *,
    lease_seconds: float,
    now: Callable[[], datetime],
    sink: TelemetrySink | None = None,
) -> tuple[tuple[str, ...], tuple[str, ...]]:
    """One expired-lease sweep: finalize claimable stranded lifecycles, then
    reap any remaining lapsed claim rows (spec 00069, criteria #1/#2).

    The active counterpart to the entry-time :func:`_recover_claimable_stranded`
    backstop, run on a bounded cadence *while the loop drives* (see
    :func:`_lease_sweep_loop`). Two steps, in order:

    1. :func:`_recover_claimable_stranded` finalizes every stranded
       ``running``/``validating`` lifecycle whose task the sweeper can claim --
       acquiring the claim proves no live worker holds it -- through the
       sanctioned :func:`finalize_stranded_lifecycle` path (never a direct
       status write), then releases the claim so the task returns to an
       eligible, re-selectable state. Its liveness guard is the criterion #2
       safety: a task a live peer is actively running cannot be claimed, so its
       run is left untouched.
    2. :meth:`SqliteClaimStore.sweep_expired_claims` batch-drops any *remaining*
       lapsed claim rows -- a lease that lapsed with no stranded lifecycle behind
       it -- so a dead worker's leaked lease never wedges its task's conflict
       keys. It reaps only rows with ``lease_expires_at <= now``; a still-live
       lease (criterion #2) is never touched.

    Returns ``(recovered_run_ids, released_task_ids)``: the run ids finalized in
    step 1 and the task ids reaped in step 2. Both empty on a quiet store.
    """
    recovered = _recover_claimable_stranded(
        control,
        claims,
        worker_id,
        lease_seconds=lease_seconds,
        now=now,
        sink=sink,
    )
    released = tuple(claims.sweep_expired_claims(now=now()))
    return recovered, released


async def _lease_sweep_loop(
    *,
    control: SqliteStore | PostgresStore,
    claims: SqliteClaimStore | PostgresClaimStore,
    worker_id: str,
    lease_seconds: float,
    interval: float,
    now: Callable[[], datetime],
    sink: TelemetrySink | None,
    stream: TextIO | None,
) -> None:
    """Tick :func:`sweep_expired_leases` every ``interval`` seconds (spec 00069).

    Runs as a sibling asyncio task while :func:`orchestrate` awaits drives,
    mirroring :func:`_source_reconcile_loop`: a worker that dies mid-task has its
    lapsed lease actively released and its stranded lifecycle finalized on a
    bounded cadence, so the task returns to eligibility WITHOUT waiting for some
    other worker to happen to re-select that exact task id (criterion #1). The
    live-claim safety is inherited from :func:`sweep_expired_leases` -- a future
    lease, or a task a live peer is actively running, is never swept or
    reclaimed (criterion #2).

    Failure posture matches the reconciler: a store error skips the tick, is
    logged to ``stream``, and is retried on the next tick; the loop only exits by
    cancellation. The entry-time :func:`_recover_claimable_stranded` backstop is
    unaffected -- this in-loop sweep is additive, not a replacement.
    """
    while True:
        await asyncio.sleep(interval)
        try:
            recovered, released = sweep_expired_leases(
                control,
                claims,
                worker_id,
                lease_seconds=lease_seconds,
                now=now,
                sink=sink,
            )
        except Exception as exc:  # noqa: BLE001 - transient store error
            if stream is not None:
                print(
                    f"[orchestrate] lease-sweep failed "
                    f"({type(exc).__name__}: {exc}); retrying next tick",
                    file=stream,
                    flush=True,
                )
            continue
        if stream is not None:
            for run_id in recovered:
                print(
                    f"[orchestrate] lease-sweep: finalized stranded run "
                    f"{run_id}; its task is eligible again",
                    file=stream,
                    flush=True,
                )
            for task_id in released:
                print(
                    f"[orchestrate] lease-sweep: released lapsed lease on "
                    f"task {task_id!r}",
                    file=stream,
                    flush=True,
                )


@dataclass(frozen=True, kw_only=True)
class RedriveOutcome:
    """What the landing re-driver did with one parked run in a pass.

    ``result`` is the disposition:

    * ``"landed"`` -- a re-attempt merged the branch (no fresh park appended),
      so the strand cleared.
    * ``"queued"`` -- the run exhausted its re-drive ``bound`` without landing
      and was routed to the human-review queue with ``park_kind`` as the
      machine-readable reason (or was already queued on a prior pass, in which
      case ``attempts`` is 0 -- the terminal, no-further-attempt state).
    * ``"in-progress"`` -- a re-attempt was made but the run neither landed nor
      hit the bound (e.g. a peer held the claim), so it is left for a later
      pass.
    * ``"skipped"`` -- the run is no longer a landable parked change (already
      landed, or its committed change vanished), so it was neither re-driven nor
      queued.

    ``attempts`` counts the sanctioned land re-attempts this call made for the
    run; ``park_kind`` is the last observed park cause (empty for ``landed``).
    """

    run_id: str
    task_id: str
    result: Literal["landed", "queued", "in-progress", "skipped"]
    attempts: int
    park_kind: str = ""


def _landing_parks(
    control: SqliteStore | PostgresStore, run_id: str
) -> list[LandingParked]:
    """Every :class:`LandingParked` event on ``run_id``, in ledger order.

    The re-driver counts these to bound its re-attempts: the original
    land-suppression appends the first park, and each failed re-attempt appends
    one more, so ``len(parks) - 1`` is the number of re-attempts already made
    against this run. An empty list means the run never parked (it landed
    cleanly or never finished), so there is nothing to re-drive.
    """
    return [
        event
        for event in control.list_domain_events(run_id)
        if isinstance(event, LandingParked)
    ]


def _landing_already_queued(
    claims: SqliteClaimStore | PostgresClaimStore, task_id: str, run_id: str
) -> bool:
    """True once this run's parked landing has been routed to the queue.

    The terminal guard behind criterion #4's "no (bound+1)th attempt": a run
    whose landing was routed carries an ``orchestrator_stop_events`` row keyed
    to its id whose ``kind`` is a landing park cause. Its presence stops the
    re-driver from ever re-attempting the land again OR re-queuing it, so a
    genuinely-stuck strand costs exactly ``bound`` attempts and one queue entry.
    """
    return any(
        event.run_id == run_id and event.kind in LANDING_PARK_KINDS
        for event in claims.list_subject_stop_events(task_id)
    )


def _record_landing_redrive(
    control: SqliteStore | PostgresStore,
    *,
    run_id: str,
    result: str,
    park_kind: str,
    task_id: str,
    stream: TextIO | None,
) -> None:
    """Append a :class:`LandingRedriven` record to the run's ledger, pairing one
    re-drive disposition with the outcome witness it just produced (spec 00073,
    criterion 5).

    Called only *after* a real re-attempt landed / re-parked, or the run was
    routed to the human-review queue -- so the record is never the ``redriven``
    cheat: its paired witness (a :class:`Landed`, a fresh :class:`LandingParked`,
    or the queue entry) already exists in the store. ``LandingRedriven`` folds to
    the identity, so the run stays ``Status.DONE`` and only ``version`` advances.
    Best-effort: a missing lifecycle or any store error is logged to ``stream``
    and swallowed so the re-drive loop never unwinds on a recording failure.
    """
    try:
        lifecycle = control.load_lifecycle(run_id)
        if lifecycle is None:
            return
        control.append_domain_event(
            LandingRedriven(
                run_id=run_id,
                ts=_utcnow(),
                result=result,
                park_kind=park_kind,
            ),
            expected_version=lifecycle.version,
        )
    except Exception as exc:  # noqa: BLE001 - best-effort audit witness
        if stream is not None:
            print(
                f"[orchestrate] {task_id}: failed to record landing-redrive "
                f"event ({type(exc).__name__}: {exc})",
                file=stream,
                flush=True,
            )


def redrive_parked_landings(
    control: SqliteStore | PostgresStore,
    claims: SqliteClaimStore | PostgresClaimStore,
    strategy: SubmitStrategy,
    worker_id: str,
    *,
    requests: Iterable[SubmitRequest],
    bound: int = DEFAULT_LANDING_REDRIVE_BOUND,
    held_out_source: HeldOutGraderSource | None = None,
    grader_env: Mapping[str, str] | None = None,
    lease_seconds: float,
    now: Callable[[], datetime],
    stream: TextIO | None = None,
) -> tuple[RedriveOutcome, ...]:
    """Bounded re-drive of runs parked unlanded (spec 00069, criteria #3/#4/#13).

    A run parked unlanded -- a :class:`LandingParked` witness on a ``DONE`` run
    whose work graded green but whose strategy could not land it (a divergent
    base, a failed ``[submit] verify`` standing build invariant) -- is a strand
    the loop must actively clear, not leave to accrue. For each ``request``
    describing such a run this:

    1. Skips a run that never parked, and short-circuits one already routed to
       the queue (the terminal, no-further-attempt state -- criterion #4).
    2. Confirms the run is still a *landable* parked change via the strategy's
       read-only :data:`LandabilityProbe`. A run that has since landed (its
       branch merged, no commits beyond base) or whose committed change vanished
       reports not-landable and drops out -- neither re-driven nor queued -- so a
       cleared strand is never wrongly re-parked.
    3. Re-attempts the land up to ``bound`` times under a freshly acquired claim
       (a sanctioned claim API, never a raw lifecycle write). Each re-attempt
       first clears a FRESH held-out landing gate against the content it would
       land (spec 00074, D-1), reusing the same evaluation and verdict-record
       path as the first attempt (D-2): a pass -- or no ``held_out_source`` /
       a non-landing status -- lets ``strategy.submit`` run, which re-runs the
       strategy's own rebase + command/standing re-verification against the
       exact base it lands on, so nothing lands unverified; a FAIL (or a
       fail-closed evaluation error) suppresses ``submit`` and appends a fresh
       held-out-gate park, so a gate-blocked strand can never slip in on
       re-drive through the strategy's own checks alone. An ungated re-drive
       (``held_out_source`` is ``None``) behaves byte-for-byte as before the
       gate was wired (D-6). A re-attempt that lands appends no park (the strand
       clears); one that re-parks -- a strategy re-park, a gate block, or a
       fail-closed error -- appends a fresh park, advancing the count toward the
       bound.
    4. On the bound -- ``bound`` re-attempts made without landing -- routes the
       run to the single human-review queue with its ``park_kind`` as the
       machine-readable reason, and makes no further attempt (criterion #4).

    ``requests`` may include non-parked ``DONE`` runs (the caller need not
    pre-filter); the parked-check drops them cheaply. Returns one
    :class:`RedriveOutcome` per run the re-driver actually touched, in input
    order, so a caller can react to a ``landed`` result (which advanced the
    base and may promote dependents).
    """
    outcomes: list[RedriveOutcome] = []
    for request in requests:
        run_id = request.run_id
        task_id = request.task_id
        parks = _landing_parks(control, run_id)
        if not parks:
            continue  # never parked: nothing to re-drive
        # Terminal guard (criterion #4): a run already routed to the queue is
        # never re-attempted (no bound+1) and never re-queued.
        if _landing_already_queued(claims, task_id, run_id):
            outcomes.append(
                RedriveOutcome(
                    run_id=run_id,
                    task_id=task_id,
                    result="queued",
                    attempts=0,
                    park_kind=parks[-1].park_kind,
                )
            )
            continue
        # Is this still an unlanded, landable change? A landed run's branch has
        # merged (no commits beyond base) or its worktree is gone, so the
        # strategy reports it not-landable and it drops out -- a cleared strand
        # is neither re-driven nor queued. A probe error fails open to
        # not-landable (skip): a buggy predicate must never wedge the re-driver
        # into re-parking work.
        try:
            verdict = probe_landability(strategy, request)
        except Exception as exc:  # noqa: BLE001 - probe is consumer code
            if stream is not None:
                print(
                    f"[orchestrate] {task_id}: landability probe failed during "
                    f"re-drive ({type(exc).__name__}: {exc}); skipping",
                    file=stream,
                    flush=True,
                )
            outcomes.append(
                RedriveOutcome(
                    run_id=run_id,
                    task_id=task_id,
                    result="skipped",
                    attempts=0,
                    park_kind=parks[-1].park_kind,
                )
            )
            continue
        if not verdict.landable:
            outcomes.append(
                RedriveOutcome(
                    run_id=run_id,
                    task_id=task_id,
                    result="skipped",
                    attempts=0,
                    park_kind=parks[-1].park_kind,
                )
            )
            continue
        # Bounded re-drive. ``made`` is the re-attempts already spent on this
        # run (the original suppression is park #1, not a re-attempt); each loop
        # turn is one more sanctioned land re-attempt.
        made = len(parks) - 1
        attempts = 0
        landed = False
        while made < bound:
            claim = claims.acquire_claim(
                task_id, worker_id, now=now(), lease_seconds=lease_seconds
            )
            if claim is None:
                break  # a peer holds the claim; retry on a later pass
            reattempt_landed = False
            try:
                # Fresh held-out gate re-evaluation against the content this
                # re-attempt would land (spec 00074, D-1), reusing the first
                # attempt's evaluation + verdict-record path (D-2). A pass -- or
                # no held-out source wired -- lets ``submit`` run exactly as
                # today; a FAIL (or a fail-closed evaluation error) suppresses it
                # and leaves a fresh held-out-gate park, so a gate-blocked strand
                # can never land on re-drive via the strategy's checks alone.
                if _reevaluate_landing_gate(
                    control,
                    held_out_source,
                    request,
                    grader_env=grader_env,
                    stream=stream,
                ):
                    strategy.submit(request)
                    reattempt_landed = len(
                        _landing_parks(control, run_id)
                    ) == len(parks)
            finally:
                claims.release_claim(claim)
            attempts += 1
            if reattempt_landed:
                # No fresh park: the re-attempt landed and cleared the strand.
                # Pair the redrive record with the ``Landed`` witness the submit
                # strategy just appended (criterion 5).
                landed = True
                _record_landing_redrive(
                    control,
                    run_id=run_id,
                    result=REDRIVE_RESULT_LANDED,
                    park_kind=parks[-1].park_kind,
                    task_id=task_id,
                    stream=stream,
                )
                break
            # A fresh park: the re-attempt re-parked -- the strategy could not
            # land, the gate blocked, or the evaluation failed closed. Each of
            # those paths appended a ``LandingParked``; pair the redrive record
            # with it (criterion 5) and advance the count toward the bound.
            parks = _landing_parks(control, run_id)
            made = len(parks) - 1
            _record_landing_redrive(
                control,
                run_id=run_id,
                result=REDRIVE_RESULT_REPARKED,
                park_kind=parks[-1].park_kind,
                task_id=task_id,
                stream=stream,
            )
        if landed:
            if stream is not None:
                print(
                    f"[orchestrate] {task_id}: re-drove parked landing to a "
                    f"merge after {attempts} re-attempt(s)",
                    file=stream,
                    flush=True,
                )
            outcomes.append(
                RedriveOutcome(
                    run_id=run_id,
                    task_id=task_id,
                    result="landed",
                    attempts=attempts,
                )
            )
            continue
        if made >= bound:
            park_kind = parks[-1].park_kind
            detail = (
                f"landing re-drive exhausted {bound} re-attempt(s) without "
                f"landing; last park cause {park_kind!r}: "
                f"{parks[-1].detail}"
            )
            claims.record_human_review(
                reason=park_kind,
                task_id=task_id,
                run_id=run_id,
                detail=detail,
                occurred_at=now(),
            )
            # Pair the redrive record with the human-review queue entry just
            # written (criterion 5): the run exhausted its bound and was routed.
            _record_landing_redrive(
                control,
                run_id=run_id,
                result=REDRIVE_RESULT_ROUTED,
                park_kind=park_kind,
                task_id=task_id,
                stream=stream,
            )
            if stream is not None:
                print(
                    f"[orchestrate] {task_id}: parked landing routed to the "
                    f"human-review queue after {attempts} re-attempt(s) "
                    f"(reason {park_kind!r})",
                    file=stream,
                    flush=True,
                )
            outcomes.append(
                RedriveOutcome(
                    run_id=run_id,
                    task_id=task_id,
                    result="queued",
                    attempts=attempts,
                    park_kind=park_kind,
                )
            )
            continue
        # Broke out before landing or hitting the bound (claim unavailable):
        # leave it for a later pass.
        outcomes.append(
            RedriveOutcome(
                run_id=run_id,
                task_id=task_id,
                result="in-progress",
                attempts=attempts,
                park_kind=parks[-1].park_kind,
            )
        )
    return tuple(outcomes)


# The outcomes that mark a terminal FAILED lifecycle as one reached by *retry
# exhaustion* rather than an abort. A failed validation or an internal error is
# a retry-source failure the harness re-tries until the budget runs out; an
# ``AGENT_ERROR`` is an abort (a deliberate agent/operator stop) that reaches
# FAILED without exhausting a retry budget and must NEVER be escalated -- it is
# surfaced to the queue by the D-E routing layer instead.
_RETRY_EXHAUSTION_OUTCOMES: frozenset[Outcome] = frozenset(
    {Outcome.VALIDATION_FAILED, Outcome.INTERNAL_ERROR}
)


@dataclass
class EscalationRequest:
    """A candidate the retry-escalation re-driver may escalate.

    ``run_id`` names the terminal run to inspect for retry exhaustion;
    ``task`` is re-driven (under the escalation model / a re-decompose) when the
    run is found exhausted and its single sanctioned escalation is unspent. The
    caller need not pre-filter -- a non-exhausted run (still retryable, DONE, an
    abort) is dropped cheaply by the exhaustion check.
    """

    task_id: str
    task: Task
    run_id: str


@dataclass
class EscalationOutcome:
    """What the retry-escalation re-driver did with one candidate in a pass.

    ``result`` is the disposition:

    * ``"escalated"`` -- the run exhausted its retry budget for the first time
      and the re-driver spent its single sanctioned escalation, re-driving the
      task under the escalation config; ``escalated_run_id`` names the fresh run.
    * ``"queued"`` -- the escalated run also exhausted (a second exhaustion), so
      the task was routed to the single human-review queue with
      ``retries-exhausted-after-escalation`` as the machine-readable reason (or
      was already routed on a prior pass, in which case ``escalated_run_id`` is
      empty -- the terminal, no-further-attempt state).
    * ``"in-progress"`` -- a peer held the task's claim (or the drive minted no
      run), so the escalation is left for a later pass.
    * ``"skipped"`` -- reserved; the re-driver drops a non-exhausted candidate
      before emitting an outcome, so this is not currently produced.

    ``escalations`` is the number of sanctioned escalations spent on this task
    after the call (0 before the first escalation, 1 after); ``escalated_run_id``
    is the run minted by the escalation (empty unless ``result`` is
    ``"escalated"``).
    """

    run_id: str
    task_id: str
    result: Literal["escalated", "queued", "in-progress", "skipped"]
    escalations: int
    escalated_run_id: str = ""


def _is_retry_exhausted(lifecycle: Lifecycle, max_retries: int) -> bool:
    """True when ``lifecycle`` reached a terminal FAILED by *retry exhaustion*.

    The re-driver escalates a genuine budget exhaustion only: the lifecycle is
    terminally ``FAILED``, its ``retries`` counter reached ``max_retries`` (the
    budget is spent), and its last attempt ended in a retry-source failure (a
    failed validation or an internal error -- see
    :data:`_RETRY_EXHAUSTION_OUTCOMES`). An abort (``AGENT_ERROR``) or a
    manual/budget stop reaches ``FAILED`` too but is not a retry exhaustion --
    those are surfaced to the queue directly, never escalated.
    """
    if lifecycle.status is not Status.FAILED:
        return False
    if lifecycle.retries < max_retries:
        return False
    if not lifecycle.attempts:
        return False
    return lifecycle.attempts[-1].outcome in _RETRY_EXHAUSTION_OUTCOMES


def _escalation_count(claims: SqliteClaimStore | PostgresClaimStore, task_id: str) -> int:
    """How many sanctioned escalations have been recorded for ``task_id``.

    Each escalation appends one :data:`STOP_RETRIES_ESCALATED` marker keyed to
    the task; counting them is how the re-driver enforces its bound -- one
    marker present means the single sanctioned escalation is spent, so the next
    exhaustion routes to the queue instead of re-escalating.
    """
    return sum(
        1
        for event in claims.list_subject_stop_events(task_id)
        if event.kind == STOP_RETRIES_ESCALATED
    )


def _already_queued_after_escalation(
    claims: SqliteClaimStore | PostgresClaimStore, task_id: str
) -> bool:
    """True once this task has been routed to the queue post-escalation.

    The terminal guard behind criterion #6's no-further-attempt state: a task
    routed with :data:`REASON_RETRIES_EXHAUSTED_AFTER_ESCALATION` carries that
    stop row keyed to its id. Its presence stops the re-driver from ever
    re-escalating OR re-queuing it, so a genuinely-stuck task costs exactly one
    escalation and one queue entry.
    """
    return any(
        event.kind == REASON_RETRIES_EXHAUSTED_AFTER_ESCALATION
        for event in claims.list_subject_stop_events(task_id)
    )


EscalationDriver = Callable[["EscalationRequest", "str | None"], Awaitable["str | None"]]


async def redrive_exhausted_retries(
    control: SqliteStore | PostgresStore,
    claims: SqliteClaimStore | PostgresClaimStore,
    worker_id: str,
    *,
    requests: Iterable[EscalationRequest],
    drive: EscalationDriver,
    escalation_model: str | None = None,
    max_retries: int = DEFAULT_MAX_RETRIES,
    bound: int = DEFAULT_ESCALATION_BOUND,
    lease_seconds: float,
    now: Callable[[], datetime],
    stream: TextIO | None = None,
) -> tuple[EscalationOutcome, ...]:
    """Bounded escalate-once-then-queue re-drive of exhausted retries.

    Spec 00069, criteria #5/#6/#13; decisions D-A/D-C. A task that spends its
    entire retry budget without grading green is a strand the loop must clear,
    not leave in a silent terminal ``FAILED``. For each ``request`` naming such
    a terminal run this:

    1. Drops a run that did not exhaust its retry budget (still retryable,
       ``DONE``, or an abort -- an ``AGENT_ERROR`` is not a budget exhaustion);
       the caller need not pre-filter.
    2. Short-circuits a task already routed to the queue after its escalation
       (the terminal, no-further-attempt state -- criterion #6), making no
       further attempt and never re-queuing it.
    3. On the FIRST exhaustion (no escalation marker yet) escalates exactly
       ONCE: under a freshly acquired claim (a sanctioned claim API, never a raw
       lifecycle write) it re-drives the task through ``drive`` -- a
       stronger-model / re-decompose attempt selected from existing config and
       passed ``escalation_model`` -- which mints and returns a fresh run id.
       The escalated run runs under the existing per-run budget ceilings. A
       :data:`STOP_RETRIES_ESCALATED` marker is then recorded so the escalation
       is spent exactly once; the first exhaustion is deliberately NOT routed to
       the queue (D-A: never a silent terminal FAILED, never re-escalation).
    4. On a SECOND exhaustion (the escalation marker is present, ``bound``
       reached) routes the task to the single human-review queue with
       :data:`REASON_RETRIES_EXHAUSTED_AFTER_ESCALATION` as the machine-readable
       reason, and makes no further attempt (criterion #6).

    The harness owns every lifecycle transition: this re-driver only reads
    authoritative lifecycle state, requests a fresh run through the sanctioned
    ``drive`` seam, and appends ledger rows. It never calls ``transition_to``,
    forges a status, or fabricates an agent envelope. Returns one
    :class:`EscalationOutcome` per candidate the re-driver actually touched, in
    input order.
    """
    outcomes: list[EscalationOutcome] = []
    for request in requests:
        run_id = request.run_id
        task_id = request.task_id
        lifecycle = control.load_lifecycle(run_id)
        if lifecycle is None or not _is_retry_exhausted(lifecycle, max_retries):
            continue  # not an exhausted run: nothing to escalate
        # Terminal guard (criterion #6): a task already routed post-escalation
        # is never re-escalated (no bound+1) and never re-queued.
        if _already_queued_after_escalation(claims, task_id):
            outcomes.append(
                EscalationOutcome(
                    run_id=run_id,
                    task_id=task_id,
                    result="queued",
                    escalations=_escalation_count(claims, task_id),
                )
            )
            continue
        escalations = _escalation_count(claims, task_id)
        if escalations >= bound:
            # Second exhaustion: the single sanctioned escalation is spent and
            # the escalated run also exhausted -> route to the human-review
            # queue, never a silent terminal FAILED (D-A).
            detail = (
                f"retry budget exhausted again on run {run_id!r} after the "
                f"single sanctioned escalation (retries={lifecycle.retries}, "
                f"max_retries={max_retries})"
            )
            claims.record_human_review(
                reason=REASON_RETRIES_EXHAUSTED_AFTER_ESCALATION,
                task_id=task_id,
                run_id=run_id,
                detail=detail,
                occurred_at=now(),
            )
            if stream is not None:
                print(
                    f"[orchestrate] {task_id}: retries exhausted after "
                    f"escalation; routed to the human-review queue "
                    f"(reason {REASON_RETRIES_EXHAUSTED_AFTER_ESCALATION!r})",
                    file=stream,
                    flush=True,
                )
            outcomes.append(
                EscalationOutcome(
                    run_id=run_id,
                    task_id=task_id,
                    result="queued",
                    escalations=escalations,
                )
            )
            continue
        # First exhaustion: escalate exactly once under a freshly acquired
        # claim so no peer double-escalates the same task.
        claim = claims.acquire_claim(
            task_id, worker_id, now=now(), lease_seconds=lease_seconds
        )
        if claim is None:
            outcomes.append(
                EscalationOutcome(
                    run_id=run_id,
                    task_id=task_id,
                    result="in-progress",
                    escalations=escalations,
                )
            )
            continue
        try:
            escalated_run_id = await drive(request, escalation_model)
        finally:
            claims.release_claim(claim)
        if not escalated_run_id:
            # The drive minted no run (it could not run this pass): leave the
            # escalation for a later pass, marker unrecorded so the single
            # escalation is never spent without an actual re-drive.
            outcomes.append(
                EscalationOutcome(
                    run_id=run_id,
                    task_id=task_id,
                    result="in-progress",
                    escalations=escalations,
                )
            )
            continue
        # Record the boundedness marker only after a successful re-drive, so the
        # single sanctioned escalation is spent exactly once and a later pass
        # (should the escalated run also exhaust) routes to the queue.
        detail = (
            f"escalated once after retry-budget exhaustion on run "
            f"{run_id!r}; escalated run {escalated_run_id!r}"
        )
        claims.record_stop_event(
            kind=STOP_RETRIES_ESCALATED,
            subject=task_id,
            detail=detail,
            occurred_at=now(),
        )
        if stream is not None:
            print(
                f"[orchestrate] {task_id}: retries exhausted; escalated once "
                f"to run {escalated_run_id!r}",
                file=stream,
                flush=True,
            )
        outcomes.append(
            EscalationOutcome(
                run_id=run_id,
                task_id=task_id,
                result="escalated",
                escalations=escalations + 1,
                escalated_run_id=escalated_run_id,
            )
        )
    return tuple(outcomes)


@dataclass(frozen=True, kw_only=True)
class PrereqRedriveOutcome:
    """What the dangling-prerequisite re-driver did with one issue in a pass.

    ``referencing_id`` is the task that declared the edge, ``missing_id`` the
    prerequisite that resolves to no work item this pass. ``cycles`` is the
    number of consecutive scheduling cycles the prerequisite has now been
    observed missing (the count of ``dangling-prerequisite`` witnesses on the
    referencing task's stop ledger, including this pass's). ``result`` is the
    disposition:

    * ``"waiting"`` -- the prerequisite is still missing but the bound is not yet
      reached, so the referencing task stays out of the ready set (never
      dispatched) and the re-driver waits for it to appear or the bound to cross.
    * ``"queued"`` -- the prerequisite stayed missing through the bound, so the
      referencing task was routed to the single human-review queue ONCE with
      :data:`REASON_PREREQUISITE_MISSING` and the missing id named in the detail
      (or was already routed on a prior pass, the terminal no-further-routing
      state).
    """

    referencing_id: str
    missing_id: str
    result: Literal["waiting", "queued"]
    cycles: int


def _prereq_dangling_cycles(
    claims: SqliteClaimStore | PostgresClaimStore, referencing_id: str, missing_id: str
) -> int:
    """How many cycles ``referencing_id``'s edge to ``missing_id`` has dangled.

    Counts the ``dangling-prerequisite`` witnesses the re-driver appended for
    this referencing/missing pair -- one per scheduling cycle the prerequisite
    stayed missing. The missing id is matched by its ``repr`` in the witness
    detail so a task with two distinct dangling prerequisites bounds each edge
    independently. This count is how the re-driver enforces its bound.
    """
    needle = repr(missing_id)
    return sum(
        1
        for event in claims.list_subject_stop_events(referencing_id)
        if event.kind == STOP_DANGLING_PREREQUISITE and needle in event.detail
    )


def _prereq_already_queued(
    claims: SqliteClaimStore | PostgresClaimStore, referencing_id: str, missing_id: str
) -> bool:
    """True once this referencing/missing edge has been routed to the queue.

    The terminal guard behind criterion #8's "exactly one queue entry, not an
    infinite ineligible spin": a task routed with
    :data:`REASON_PREREQUISITE_MISSING` naming this missing id carries that stop
    row keyed to its id. Its presence stops the re-driver from ever re-routing
    the same edge OR appending further witnesses, so a genuinely absent
    prerequisite costs exactly ``bound`` witnesses and one queue entry.
    """
    needle = repr(missing_id)
    return any(
        event.kind == REASON_PREREQUISITE_MISSING and needle in event.detail
        for event in claims.list_subject_stop_events(referencing_id)
    )


def redrive_missing_prerequisites(
    claims: SqliteClaimStore | PostgresClaimStore,
    *,
    issues: Iterable[GraphValidationIssue],
    bound: int = DEFAULT_PREREQ_REDRIVE_BOUND,
    now: Callable[[], datetime],
    stream: TextIO | None = None,
) -> tuple[PrereqRedriveOutcome, ...]:
    """Bounded re-drive of tasks blocked on a missing prerequisite (spec 00069,
    criteria #7/#8/#13).

    A task whose declared prerequisite resolves to no work item this pass is a
    :class:`GraphValidationIssue`: it stays out of the ready set (never
    dispatched against an unsatisfied prereq) exactly as before. This re-driver
    turns that permanent dead-end into a bounded one. For each still-dangling
    ``issue`` this pass it:

    1. Short-circuits an edge already routed to the queue (the terminal,
       no-further-routing state -- criterion #8), recording nothing further so a
       genuinely absent prerequisite produces exactly one queue entry.
    2. Appends one ``dangling-prerequisite`` witness to the referencing task's
       append-only stop ledger, advancing the cycle count -- the durable,
       recurrence-preserving record that the prerequisite is still missing (spec
       00068).
    3. On the cycle the witness count reaches ``bound``, routes the referencing
       task to the single human-review queue ONCE with
       :data:`REASON_PREREQUISITE_MISSING` and the missing prerequisite id named
       in the detail (a machine-readable reason naming the missing id --
       criterion #8). Below the bound it merely waits.

    Criterion #7 -- a once-dangling task becoming eligible when its prerequisite
    later appears -- is satisfied by the scheduler's per-pass graph rebuild, not
    here: once the prerequisite is listed and DONE the edge resolves, no issue is
    produced for it, this re-driver is never called for it, and the task enters
    the ready set and is driven. So the bound is only ever reached by a
    prerequisite that stays genuinely absent, never by one that merely arrives
    late.

    The re-driver only appends ledger rows -- it never claims, transitions a
    lifecycle, or forges a status (criterion #14). Returns one
    :class:`PrereqRedriveOutcome` per issue, in input order.
    """
    outcomes: list[PrereqRedriveOutcome] = []
    for issue in issues:
        referencing_id = issue.referencing_id
        missing_id = issue.missing_id
        # Terminal guard (criterion #8): an edge already routed to the queue is
        # never re-witnessed and never re-queued -- exactly one queue entry.
        if _prereq_already_queued(claims, referencing_id, missing_id):
            outcomes.append(
                PrereqRedriveOutcome(
                    referencing_id=referencing_id,
                    missing_id=missing_id,
                    result="queued",
                    cycles=_prereq_dangling_cycles(
                        claims, referencing_id, missing_id
                    ),
                )
            )
            continue
        # Witness this cycle's dangling edge (spec 00068): one row per pass the
        # prerequisite stays missing, never deduped -- the recurrence is the
        # signal and the count is the bound.
        claims.record_stop_event(
            kind=STOP_DANGLING_PREREQUISITE,
            subject=referencing_id,
            detail=(
                f"prerequisite {missing_id!r} resolves to no work item; "
                f"{referencing_id!r} stays out of the ready set"
            ),
            occurred_at=now(),
        )
        cycles = _prereq_dangling_cycles(claims, referencing_id, missing_id)
        if cycles >= bound:
            detail = (
                f"prerequisite {missing_id!r} stayed missing for {cycles} "
                f"cycle(s) (bound {bound}); {referencing_id!r} was never "
                f"dispatched against an unsatisfied prerequisite"
            )
            claims.record_human_review(
                reason=REASON_PREREQUISITE_MISSING,
                task_id=referencing_id,
                detail=detail,
                occurred_at=now(),
            )
            if stream is not None:
                print(
                    f"[orchestrate] {referencing_id}: prerequisite "
                    f"{missing_id!r} missing past {bound} cycle(s); routed to "
                    f"the human-review queue "
                    f"(reason {REASON_PREREQUISITE_MISSING!r})",
                    file=stream,
                    flush=True,
                )
            outcomes.append(
                PrereqRedriveOutcome(
                    referencing_id=referencing_id,
                    missing_id=missing_id,
                    result="queued",
                    cycles=cycles,
                )
            )
            continue
        outcomes.append(
            PrereqRedriveOutcome(
                referencing_id=referencing_id,
                missing_id=missing_id,
                result="waiting",
                cycles=cycles,
            )
        )
    return tuple(outcomes)


@dataclass(frozen=True, kw_only=True)
class NoProgressObservation:
    """One unit's observed progress for a single scheduling cycle.

    A "unit" is anything the loop keeps re-attempting that can make no headway --
    a phase whose verify never passes, an autopilot repo that never authors a
    task. ``unit_id`` is the stable key the re-driver counts witnesses under (the
    subject on the stop ledger); ``progressed`` is whether the unit made an
    observable state change this cycle (a task authored, a verify passing, a
    lifecycle advanced). ``detail`` is optional human-readable context folded into
    the witness / queue rows. ``task_id`` is the identifier the queue entry
    carries when the unit is backed off; it defaults to ``unit_id`` (a phase or
    repo IS its own review subject), but a unit whose review subject differs from
    its ledger key can name it explicitly.
    """

    unit_id: str
    progressed: bool
    detail: str = ""
    task_id: str = ""


@dataclass(frozen=True, kw_only=True)
class NoProgressOutcome:
    """What the no-progress back-off re-driver did with one unit in a pass.

    ``unit_id`` is the observed unit. ``cycles`` is the length of the unit's
    current consecutive no-progress streak (the count of ``no-progress-cycle``
    witnesses since its last ``no-progress-reset`` marker, including this pass's
    when it was fruitless). ``result`` is the disposition:

    * ``"progressed"`` -- the unit made observable progress this cycle; its streak
      was reset (``cycles`` is 0) and it is never backed off.
    * ``"waiting"`` -- the unit made no progress but the bound is not yet reached;
      a witness was recorded and the unit stays active for another attempt.
    * ``"queued"`` -- the unit's fruitless streak reached the bound, so it was
      backed off (it will be absent from the active set next cycle) and routed to
      the single human-review queue ONCE with :data:`REASON_NO_PROGRESS` (or was
      already routed on a prior pass, the terminal no-further-routing state).
    """

    unit_id: str
    result: Literal["progressed", "waiting", "queued"]
    cycles: int


def _no_progress_streak(claims: SqliteClaimStore | PostgresClaimStore, unit_id: str) -> int:
    """Length of ``unit_id``'s current consecutive no-progress streak.

    Walks the unit's append-only stop ledger and counts ``no-progress-cycle``
    witnesses that fall AFTER its most recent ``no-progress-reset`` marker -- so a
    unit that made progress (which appended a reset marker) starts a fresh streak
    even though no witness is ever deleted. This trailing count is how the
    re-driver enforces its bound and how progress resets the counter.
    """
    streak = 0
    for event in claims.list_subject_stop_events(unit_id):
        if event.kind == STOP_NO_PROGRESS_RESET:
            streak = 0
        elif event.kind == STOP_NO_PROGRESS:
            streak += 1
    return streak


def _no_progress_already_queued(
    claims: SqliteClaimStore | PostgresClaimStore, unit_id: str
) -> bool:
    """True once ``unit_id`` has been backed off to the human-review queue.

    The terminal guard behind criterion #9's "exactly one queue entry, not a
    re-queue every cycle": a backed-off unit carries a :data:`REASON_NO_PROGRESS`
    stop row keyed to its id. Its presence stops the re-driver from ever
    re-witnessing or re-routing the same unit, so a never-progressing unit costs
    exactly ``bound`` witnesses and one queue entry.
    """
    return any(
        event.kind == REASON_NO_PROGRESS
        for event in claims.list_subject_stop_events(unit_id)
    )


def redrive_no_progress(
    claims: SqliteClaimStore | PostgresClaimStore,
    *,
    observations: Iterable[NoProgressObservation],
    bound: int = DEFAULT_NO_PROGRESS_BOUND,
    now: Callable[[], datetime],
    stream: TextIO | None = None,
) -> tuple[NoProgressOutcome, ...]:
    """Bounded no-progress back-off of units the loop cannot advance (spec 00069,
    criteria #9/#13; D-C).

    A "unit" -- a phase whose verify never passes, an autopilot repo that never
    authors a task -- is otherwise re-attempted every cycle forever. This
    re-driver bounds that: it counts a unit's consecutive fruitless cycles and,
    after ``bound`` of them, backs the unit off (it must be absent from the active
    set the next cycle) and routes it to the single human-review queue with the
    machine-readable :data:`REASON_NO_PROGRESS` reason instead of re-running it.
    For each ``observation`` this pass it:

    1. Short-circuits a unit already backed off to the queue (the terminal,
       no-further-routing state -- criterion #9), recording nothing further so a
       never-progressing unit produces exactly one queue entry.
    2. On observable progress, appends one ``no-progress-reset`` marker (only when
       a streak is outstanding) so the unit's streak restarts -- a unit that ever
       makes progress is never backed off (the reset edge case).
    3. Otherwise appends one ``no-progress-cycle`` witness -- the durable,
       recurrence-preserving record of a fruitless cycle -- and, on the cycle the
       consecutive streak reaches ``bound``, routes the unit to the queue ONCE
       with :data:`REASON_NO_PROGRESS`. Below the bound it merely waits.

    The re-driver never re-attempts the backed-off unit and never fabricates a
    lifecycle state: it only reads observed progress and appends ledger rows
    (criterion #14). Backing a unit off is the caller's cue to drop it from the
    active set (the queue row is authoritative -- ``_no_progress_already_queued``
    keeps it out of every later pass). Returns one :class:`NoProgressOutcome` per
    observation, in input order.
    """
    outcomes: list[NoProgressOutcome] = []
    for observation in observations:
        unit_id = observation.unit_id
        # Terminal guard (criterion #9): a unit already backed off is never
        # re-witnessed and never re-queued -- exactly one queue entry, no
        # re-queue every cycle.
        if _no_progress_already_queued(claims, unit_id):
            outcomes.append(
                NoProgressOutcome(
                    unit_id=unit_id,
                    result="queued",
                    cycles=_no_progress_streak(claims, unit_id),
                )
            )
            continue
        # Observable progress resets the streak (the reset edge case): append a
        # delimiter so the trailing count restarts, but only when there is a live
        # streak to reset -- a perpetually-progressing unit never grows the
        # ledger.
        if observation.progressed:
            if _no_progress_streak(claims, unit_id) > 0:
                claims.record_stop_event(
                    kind=STOP_NO_PROGRESS_RESET,
                    subject=unit_id,
                    detail=(
                        observation.detail
                        or f"unit {unit_id!r} made progress; streak reset"
                    ),
                    occurred_at=now(),
                )
            outcomes.append(
                NoProgressOutcome(
                    unit_id=unit_id, result="progressed", cycles=0
                )
            )
            continue
        # Witness this fruitless cycle (spec 00068): one row per cycle the unit
        # makes no headway, never deduped -- the recurrence is the signal and the
        # streak length is the bound.
        claims.record_stop_event(
            kind=STOP_NO_PROGRESS,
            subject=unit_id,
            detail=(
                observation.detail
                or f"unit {unit_id!r} made no observable progress this cycle"
            ),
            occurred_at=now(),
        )
        cycles = _no_progress_streak(claims, unit_id)
        if cycles >= bound:
            detail = (
                f"unit {unit_id!r} made no observable progress for {cycles} "
                f"consecutive cycle(s) (bound {bound}); backed off and routed to "
                f"the human-review queue"
            )
            if observation.detail:
                detail = f"{detail}: {observation.detail}"
            claims.record_human_review(
                reason=REASON_NO_PROGRESS,
                task_id=observation.task_id or unit_id,
                detail=detail,
                occurred_at=now(),
            )
            if stream is not None:
                print(
                    f"[orchestrate] {unit_id}: no progress for {bound} "
                    f"consecutive cycle(s); backed off and routed to the "
                    f"human-review queue (reason {REASON_NO_PROGRESS!r})",
                    file=stream,
                    flush=True,
                )
            outcomes.append(
                NoProgressOutcome(
                    unit_id=unit_id, result="queued", cycles=cycles
                )
            )
            continue
        outcomes.append(
            NoProgressOutcome(unit_id=unit_id, result="waiting", cycles=cycles)
        )
    return tuple(outcomes)


@dataclass(frozen=True, kw_only=True)
class HumanGateRequest:
    """A parked or terminal run the human-gate re-driver inspects for an
    intentional stop (spec 00069, criteria #10/#11; D-E).

    ``run_id`` names the lifecycle to classify; ``task_id`` is the review
    subject the queue entry carries. The caller need not pre-filter -- a run
    that is not an intentional human/budget stop (still running, DONE, or a
    retry-exhausted FAILED) is dropped cheaply by the classifier.
    """

    task_id: str
    run_id: str


@dataclass(frozen=True, kw_only=True)
class HumanGateOutcome:
    """What the human-gate re-driver did with one run in a pass.

    ``reason`` is the machine-readable stop cause when the run is an intentional
    stop (``awaiting-approval`` / ``abort`` / ``budget-ceiling``), empty
    otherwise. ``result`` is the disposition:

    * ``"queued"`` -- an intentional stop surfaced to the human-review queue for
      the first time (its status left untouched).
    * ``"already-queued"`` -- an intentional stop already surfaced on a prior
      pass; nothing further recorded (the terminal, once-per-run state).
    * ``"skipped"`` -- the run is not an intentional human/budget stop, so it was
      neither surfaced nor touched.
    """

    run_id: str
    task_id: str
    result: Literal["queued", "already-queued", "skipped"]
    reason: str = ""


def _classify_human_gate(lifecycle: Lifecycle) -> str | None:
    """The human-review reason for ``lifecycle``'s intentional stop, or ``None``.

    An intentional human stop is one of exactly three durable lifecycle shapes:

    * ``AWAITING_APPROVAL`` -- parked on a manual-approval gate -> ``awaiting
      -approval``.
    * a terminal ``FAILED`` whose final attempt is ``AGENT_ERROR`` -- the shape
      both ``intent=abort`` and a budget-ceiling breach reach (both are terminal,
      non-retryable). The two are told apart by the attempt's ``error``: a budget
      kill carries :data:`BUDGET_CEILING_ERROR_PREFIX` -> ``budget-ceiling``;
      anything else is an abort -> ``abort``.

    Every other state is NOT a human gate and returns ``None``: a live
    ``RUNNING``/``VALIDATING`` run, a ``DONE`` run, and -- crucially -- a
    retry-exhausted ``FAILED`` whose final attempt is ``VALIDATION_FAILED`` /
    ``INTERNAL_ERROR`` (that is the escalation re-driver's territory, not a human
    gate), so the two failure families never cross-route.
    """
    if lifecycle.status is Status.AWAITING_APPROVAL:
        return REASON_AWAITING_APPROVAL
    if lifecycle.status is Status.FAILED and lifecycle.attempts:
        tail = lifecycle.attempts[-1]
        if tail.outcome is Outcome.AGENT_ERROR:
            if tail.error.startswith(BUDGET_CEILING_ERROR_PREFIX):
                return REASON_BUDGET_CEILING
            return REASON_ABORTED
    return None


def _human_gate_detail(
    lifecycle: Lifecycle, task_id: str, run_id: str, reason: str
) -> str:
    """Human-readable cause for the queue entry (the reason is the machine key)."""
    if reason == REASON_AWAITING_APPROVAL:
        return (
            f"human gate: {task_id!r} run {run_id!r} is awaiting manual "
            f"approval; left parked for a human (status unchanged)"
        )
    error = lifecycle.attempts[-1].error if lifecycle.attempts else ""
    if reason == REASON_BUDGET_CEILING:
        return (
            f"run {run_id!r} breached a per-run budget ceiling ({error}); "
            f"routed for a human, never re-dispatched"
        )
    return (
        f"run {run_id!r} ended via intent=abort ({error}); routed for a human, "
        f"never re-dispatched"
    )


def _human_gate_already_queued(
    claims: SqliteClaimStore | PostgresClaimStore, task_id: str, run_id: str, reason: str
) -> bool:
    """True once this run's intentional stop has been surfaced to the queue.

    The once-per-run guard: a routed stop carries an ``orchestrator_stop_events``
    row keyed to its task whose ``kind`` is the human-review ``reason`` and whose
    ``run_id`` matches. Its presence stops the re-driver from re-surfacing the
    same stop every pass, so an unresolved gate costs exactly one queue entry.
    """
    return any(
        event.run_id == run_id and event.kind == reason
        for event in claims.list_subject_stop_events(task_id)
    )


def redrive_human_gates(
    control: SqliteStore | PostgresStore,
    claims: SqliteClaimStore | PostgresClaimStore,
    *,
    requests: Iterable[HumanGateRequest],
    now: Callable[[], datetime],
    stream: TextIO | None = None,
) -> tuple[HumanGateOutcome, ...]:
    """Surface intentional human stops into the queue, never bypass them (spec
    00069, criteria #10/#11; D-E).

    AWAITING_APPROVAL, ``intent=abort``, and a budget-ceiling breach are
    *intended* stops -- a human's decision, or a deliberate budget/abort ceiling.
    Unlike a transient strand they must NOT be auto-recovered: the re-driver only
    makes them visible. For each ``request`` naming a candidate run this:

    1. Classifies the run via :func:`_classify_human_gate`. A run that is not an
       intentional stop (still running, DONE, or a retry-exhausted FAILED --
       which is the escalation re-driver's job, not a human gate) is skipped
       cleanly, so the two failure families never cross-route.
    2. Short-circuits a stop already surfaced on a prior pass (the once-per-run
       terminal state), recording nothing further so an unresolved gate produces
       exactly one queue entry rather than one every pass.
    3. Otherwise records ONE human-review entry with the machine-readable reason
       naming the cause (``awaiting-approval`` / ``abort`` / ``budget-ceiling``)
       and the offending run id.

    It NEVER transitions a lifecycle: it does not approve or reject an approval
    gate, does not re-drive an abort or budget breach, and does not write a
    status or forge an agent claim. The lifecycle's status is byte-identical
    before and after -- only a human resolves these (D-E, criterion #14). Because
    the re-driver never re-dispatches, an abort/budget stop is never re-run
    (criterion #11), and because it never resolves the gate an AWAITING_APPROVAL
    lifecycle stays AWAITING_APPROVAL across any number of re-drive/sweep cycles
    (criterion #10). Returns one :class:`HumanGateOutcome` per request, in input
    order.
    """
    outcomes: list[HumanGateOutcome] = []
    for request in requests:
        run_id = request.run_id
        task_id = request.task_id
        lifecycle = control.load_lifecycle(run_id)
        if lifecycle is None:
            continue
        reason = _classify_human_gate(lifecycle)
        if reason is None:
            outcomes.append(
                HumanGateOutcome(
                    run_id=run_id, task_id=task_id, result="skipped"
                )
            )
            continue
        if _human_gate_already_queued(claims, task_id, run_id, reason):
            outcomes.append(
                HumanGateOutcome(
                    run_id=run_id,
                    task_id=task_id,
                    result="already-queued",
                    reason=reason,
                )
            )
            continue
        claims.record_human_review(
            reason=reason,
            task_id=task_id,
            run_id=run_id,
            detail=_human_gate_detail(lifecycle, task_id, run_id, reason),
            occurred_at=now(),
        )
        if stream is not None:
            print(
                f"[orchestrate] {task_id}: intentional stop surfaced to the "
                f"human-review queue (reason {reason!r}); status left unchanged",
                file=stream,
                flush=True,
            )
        outcomes.append(
            HumanGateOutcome(
                run_id=run_id,
                task_id=task_id,
                result="queued",
                reason=reason,
            )
        )
    return tuple(outcomes)


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
        claims: SqliteClaimStore | PostgresClaimStore,
        claim: TaskClaim,
        lease_seconds: float,
        interval: float,
        now: Callable[[], datetime],
    ) -> None:
        self._claims = claims
        self._claim = claim
        self._lease_seconds = lease_seconds
        self._interval = interval
        self._now = now
        self._stop = threading.Event()
        # Set when renewal observes the lease is no longer ours (a peer stole
        # it after a sustained heartbeat stall). The driving coroutine polls
        # lost() so it can stop before landing or reporting a run whose claim
        # was taken over, instead of racing on the lifecycle version CAS.
        # See _drive_under_lease.
        self._lost = threading.Event()
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
                renewed = self._claims.renew_claim(
                    self._claim,
                    now=self._now(),
                    lease_seconds=self._lease_seconds,
                )
            except ClaimLostError:
                # Signal the driving coroutine that the lease was stolen so it
                # stops before writing a terminal result against a stale
                # version. Ending the thread silently (the prior behavior) left
                # the run to discover the loss only via the lifecycle CAS race.
                self._lost.set()
                return
            except Exception:  # noqa: BLE001 - transient store error; retry
                continue
            with self._lock:
                self._claim = renewed

    def lost(self) -> bool:
        """True once renewal has observed the lease was stolen by a peer."""
        return self._lost.is_set()

    def stop(self) -> TaskClaim:
        """Stop renewing and return the latest token for release."""
        self._stop.set()
        self._thread.join(timeout=self._interval + 1.0)
        with self._lock:
            return self._claim


def _sandbox_agent_primitives(policy: WorkPolicy | None) -> dict[str, Any]:
    """Decompose the resolved ``SandboxPolicy`` into the plain primitives the
    core option site consumes (spec 00037 SC-5/SC-6).

    Keeps the optional-SDK boundary intact: the orchestrator owns the policy
    type, but only ``str``/``tuple``/``bool`` values cross into
    ``run_task_object``, exactly as ``model`` already does. A ``None`` policy
    (library callers) decomposes the ``fast`` defaults, so the construction is
    byte-identical to today's hardcoded one.
    """
    sandbox = policy.sandbox if policy is not None else SandboxPolicy()
    cap = sandbox.capabilities
    env = sandbox.env
    # Resolve [sandbox.env]: declared ``pass`` names forwarded from os.environ
    # (present-only -- an absent name is dropped, never blank-substituted)
    # merged with the inline ``set`` literals, which win on key collision.
    # Both empty (the fast default and a None policy) resolves to {}.
    agent_env = {
        name: os.environ[name]
        for name in env.passthrough
        if name in os.environ
    }
    agent_env.update(env.set_values)
    return dict(
        permission_mode=sandbox.permission_mode,
        skills=cap.skills,
        allowed_tools=cap.allowed_tools,
        denied_tools=cap.denied_tools,
        setting_sources=cap.setting_sources,
        mcp_servers=cap.mcp_servers,
        mcp_strict=cap.mcp_strict,
        exec_enabled=sandbox.exec.enabled,
        exec_auto_allow=sandbox.exec.auto_allow,
        agent_env=agent_env,
        # The grader-side companion of agent_env: command graders run with this
        # FULL env (subprocess env replaces, not merges) so an agent build and
        # the command grader verifying it share [sandbox.env] (build-cache
        # parity). None when nothing is configured -> graders inherit as before.
        grader_env=resolve_grader_env(env),
    )


def _sandbox_limit_primitives(policy: WorkPolicy | None) -> dict[str, Any]:
    """Decompose the resolved ``SandboxLimits`` into the plain primitives the
    harness consumes (spec 00039 SC-4, increment D of 00036).

    The limits mirror of :func:`_sandbox_agent_primitives`: where that feeds
    ``build_agent_options`` capabilities, this feeds ``HarnessConfig``. Only
    plain numbers cross into ``run_task_object`` (``max_cost_usd`` float,
    ``max_tokens``/``wall_clock_seconds`` ints), keeping the optional-SDK and
    policy-type boundaries intact. A ``None`` policy (library callers) or an
    absent ``[sandbox.limits]`` section decomposes the ``fast`` default of an
    unenforced ceiling (``0.0``/``0``), so a fast run stays byte-identical.
    """
    sandbox = policy.sandbox if policy is not None else SandboxPolicy()
    return dict(
        max_cost_usd=sandbox.limits.max_cost_usd,
        max_tokens=sandbox.limits.max_tokens,
        wall_clock_seconds=sandbox.limits.wall_clock_seconds,
    )


def _apply_handle(
    handle: SandboxHandle,
    sandbox_primitives: dict[str, Any],
    invoke: InvokeFunc | None,
) -> tuple[dict[str, Any], InvokeFunc | None]:
    """Fold a :class:`SandboxHandle`'s contributions into the per-run drive
    arguments (spec 00043, increment F of 00036).

    Returns the effective ``sandbox_primitives`` (with ``env_contribution``
    merged onto the policy-resolved ``agent_env``, the handle winning on key
    collision) and the effective invoker (wrapped by ``invoke_wrapper`` when
    set, so a container backend runs the agent inside the sandbox). An
    empty-contribution handle — every worktree backend, via :func:`_as_handle`
    — returns the inputs unchanged, keeping the drive byte-identical.
    """
    if not handle.env_contribution and handle.invoke_wrapper is None:
        return sandbox_primitives, invoke
    effective = dict(sandbox_primitives)
    if handle.env_contribution:
        effective["agent_env"] = {
            **sandbox_primitives["agent_env"],
            **handle.env_contribution,
        }
    effective_invoke = invoke
    if handle.invoke_wrapper is not None:
        # The wrapper receives the base invoke, which is ``None`` in normal
        # operation: orchestrate is called with ``invoke=None`` and the SDK
        # invoker is built downstream in ``run_task_object``. A *replacing*
        # wrapper (the container backend, which execs the agent CLI) ignores
        # the base; an augmenting one would compose it. Either way orchestrate
        # does not require a base invoke to exist here.
        effective_invoke = handle.invoke_wrapper(invoke)
    return effective, effective_invoke


async def orchestrate(
    *,
    tasks_dir: Path | None = None,
    source: WorkSource | None = None,
    policy: WorkPolicy | None = None,
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
    prepare_sandbox: SandboxProvider | None = None,
    submit: Submitter | None = None,
    strategy: SubmitStrategy | None = None,
    reconcile_seconds: float | None = None,
    sweep_seconds: float | None = None,
    landing_redrive_bound: int | None = None,
    prereq_redrive_bound: int = DEFAULT_PREREQ_REDRIVE_BOUND,
    repo_root: Path | None = None,
    held_out_source: HeldOutGraderSource | None = None,
) -> OrchestratorReport:
    """Drive every eligible task from the work source to quiescence.

    Work comes from ``source`` (any
    :class:`~flywheel_orchestrator._sources.WorkSource`); passing
    ``tasks_dir`` instead wraps it in the reference
    :class:`~flywheel_orchestrator._sources.DirectoryWorkSource` — the
    historical behavior, unchanged. Exactly one of the two must be given.

    Returns once this worker can make no further progress: no blocked
    lifecycle it can claim unblocks, and no fresh task is both eligible and
    claimable. Safe to run as several concurrent workers against one store —
    per-task leases keep them from double-running a task. ``invoke`` defaults
    to the real Claude Code invoker; tests inject a fake callable.

    ``prepare_sandbox`` and ``submit`` are the optional consumer submit seam.
    By default each task runs in a plain ``sandbox_root/<task-id>`` dir and
    nothing happens on completion (the historical behavior, unchanged). A
    consumer supplies ``prepare_sandbox`` to provision the run's working
    directory (e.g. a git worktree) and ``submit`` to act on the terminal
    status (e.g. FF-merge or park). Both run while the task's lease is held,
    so two workers never merge the same task concurrently; all consumer
    git/strategy code stays in these callbacks, never in flywheel.
    ``strategy`` is the same seam as one object — anything satisfying
    :class:`~flywheel_orchestrator._strategy.SubmitStrategy` — and is
    mutually exclusive with passing the callables individually.

    After ``submit``, the run's outcome is projected back to the work
    source via :meth:`~flywheel_orchestrator._sources.WorkSource.report`
    (still under the lease). Report delivery is best-effort: a raising
    ``report`` is logged to ``stream`` and contained, never unwinding the
    scheduling loop.

    ``reconcile_seconds`` enables the steering bridge: every N seconds a
    sibling task re-lists the source and enqueues an ``interrupt`` control
    command for any in-flight run whose item is no longer listed (see
    :func:`reconcile_live_runs`). ``None``/``0`` (the default) disables it,
    preserving prior behavior for library callers; the CLI and the worktree
    daemon enable it by default.

    ``sweep_seconds`` enables the in-loop expired-lease sweep (spec 00069):
    every N seconds a sibling task releases any lapsed claim and finalizes its
    stranded lifecycle (see :func:`sweep_expired_leases`), so a worker that dies
    mid-task has its task returned to eligibility on a bounded cadence without
    waiting for another worker to re-select it. It reuses the live-claim safety
    of :func:`_recover_claimable_stranded`: a future lease, or a task a live peer
    is actively running, is never swept or reclaimed. Additive to the entry-time
    recovery, which still runs. ``None``/``0`` (the default) disables it,
    preserving prior behavior for library callers; the CLI and the worktree
    daemon enable it by default.

    ``landing_redrive_bound`` enables the bounded landing re-driver (spec
    00069): after each pass a run parked unlanded (a :class:`LandingParked`
    witness on a ``DONE`` run whose strategy could not merge it) is re-driven
    through the strategy's own rebase/reverify/standing/FF path up to this many
    times (see :func:`redrive_parked_landings`); a cleared cause lands and one
    that never clears is routed to the human-review queue with its park cause as
    the reason, with no further attempt. It requires a bundled ``strategy`` (the
    landability probe distinguishes a still-parked change from an already-landed
    one); ``None``/``0`` (the default) disables it, preserving prior behavior for
    library callers and callers that wired the bare submit callable. The CLI and
    the worktree daemon enable it by default.

    ``prereq_redrive_bound`` bounds the dangling-prerequisite re-driver (spec
    00069): a task whose declared prerequisite resolves to no work item stays out
    of the ready set (never dispatched) exactly as before, and each pass the
    prerequisite is still missing records one witness; once the prerequisite
    stays missing through this many cycles the referencing task is routed once to
    the human-review queue with ``prerequisite-missing`` naming the missing id
    (see :func:`redrive_missing_prerequisites`). If the prerequisite instead
    appears, the per-pass graph rebuild resolves the edge and the task becomes
    eligible and is driven, so the bound is only reached by a genuinely absent
    prerequisite. Always on (the dangling-prerequisite witnesses were recorded
    unconditionally before this too); the default bound is generous enough to
    tolerate a sibling source listing its half of a cross-source dependency a
    pass or two late.

    ``policy`` selects the store backend: construction routes through the
    store factory (:func:`~flywheel_orchestrator._store_factory.build_store`),
    so a postgres policy fails fast with the factory's DSN / extra errors.
    ``None`` (the default for library callers) keeps the historical
    sqlite-on-``db_path`` behavior byte-for-byte.

    ``repo_root`` enables the schedule-time static-validation gate (spec
    00034): a picked task whose graders fail :func:`validate_task` (an empty
    or unparseable command) is skipped-and-reported instead of dispatched,
    mirroring the unprovisionable-task skip so one bad task never starves
    eligible peers. (The missing-path check is tabled — see
    :mod:`flywheel_core.validation` — so a grader that references a not-yet-
    created path no longer blocks.) ``None`` (the default for library callers)
    disables the gate, preserving prior behavior.

    ``held_out_source`` enables the execute-time held-out landing gate (spec
    00050): after a task's run finalizes with the landing status (``DONE``) and
    while its lease is still held, the orchestrator loads that task's
    operator-declared held-out command graders from this source (the agent
    never saw them), runs them against the committed sandbox out of band, and
    blocks the land on any failure -- ``submit`` is not invoked, the sandbox is
    left parked for forensics, and the run's :class:`RunRecord` carries a
    distinct ``GateOutcome.FAIL`` marker. A task with no registration in the
    source lands byte-identically to today, as does every task when
    ``held_out_source`` is ``None`` (the default).
    """
    if source is None:
        if tasks_dir is None:
            raise ValueError("orchestrate requires either tasks_dir or source")
        source = DirectoryWorkSource(tasks_dir)
    elif tasks_dir is not None:
        raise ValueError("orchestrate takes tasks_dir or source, not both")
    if strategy is not None:
        if prepare_sandbox is not None or submit is not None:
            raise ValueError(
                "orchestrate takes strategy or prepare_sandbox/submit, "
                "not both"
            )
        prepare_sandbox = strategy.prepare_sandbox
        submit = strategy.submit
    # The landability probe (spec 00061) is the strategy itself: a git-aware
    # strategy implements the optional ``is_landable`` predicate, a non-git
    # strategy does not and is treated as always-landable by
    # ``probe_landability``. ``None`` when the caller wired the bare
    # prepare/submit callables (no bundled strategy), which keeps the gate off.
    landability_probe = strategy if strategy is not None else None
    clock = now or _utcnow
    wid = worker_id or f"worker-{uuid4().hex[:8]}"
    # This worker's advertised capability set (spec 00049, decision D-2):
    # ready_set withholds any item whose required_capabilities is not a
    # subset of it. Empty (the default, an absent [execution] capabilities
    # key) keeps every existing zero-requirement item runnable.
    worker_capabilities = (
        policy.execution_capabilities if policy is not None else frozenset()
    )
    heartbeat_interval = max(lease_seconds / 3.0, 0.001)
    sandbox_primitives = _sandbox_agent_primitives(policy)
    limit_primitives = _sandbox_limit_primitives(policy)

    def resolve_sandbox(
        row: TaskStatusRow,
        run_id: str | None,
        mode: Literal["fresh", "resume"],
    ) -> SandboxHandle:
        """The sandbox a task runs in: consumer-provisioned or the default
        ``sandbox_root/<task-id>``.

        Always returns a :class:`SandboxHandle`; a provider returning a bare
        ``Path`` (every worktree backend) is adapted to an empty-contribution
        handle (spec 00043), so the handle-aware drive path is byte-identical
        for it.
        """
        if prepare_sandbox is None:
            return SandboxHandle(path=sandbox_root / row.task.id)
        return _as_handle(
            prepare_sandbox(
                SandboxRequest(
                    task_id=row.task.id,
                    task_file=row.task_file,
                    run_id=run_id,
                    mode=mode,
                    source_ref=row.source_ref,
                )
            )
        )

    # Startup fail-loud gate (spec 00075, criterion 5, decision D-2): a
    # postgres backend that resolves no DSN or cannot reach its server
    # terminates here -- before any filesystem side effect, claim, or run --
    # with an error naming the backend and the DSN sources tried. This runs
    # ahead of the mkdir/store construction below so a configured-but-unusable
    # postgres never silently falls back to sqlite (no run or orchestrator
    # state is written locally). A no-op for the sqlite/unset backend.
    preflight_store(policy)

    db_path.parent.mkdir(parents=True, exist_ok=True)
    sandbox_root.mkdir(parents=True, exist_ok=True)

    # flywheel's core store (lifecycle, run state) and the orchestrator's own
    # claim store (task_claims/leases and every orchestrator ledger). Each owns
    # its tables. BOTH are built through the policy-driven factory so the
    # configured backend selects them together (and its fail-fast postgres
    # preconditions apply here too): under ``backend = "postgres"`` the run
    # record and the claims land in postgres, with nothing written to sqlite
    # (spec 00075). On the sqlite default the two stores share one file exactly
    # as before.
    control = open_sqlite_bound_store(policy, db_path=db_path)
    claims = build_claim_store(policy, db_path=db_path)
    # Control-plane telemetry (recovery sweeps, rechecks, approval
    # resolution) streams to the same per-run JSONL files the harness
    # writes (run_task_object derives the identical logs root from
    # db_path), so out-of-band interventions land in the run's timeline.
    sink = FileTelemetrySink(db_path.parent / "logs")
    reconciler: asyncio.Task[None] | None = None
    sweeper: asyncio.Task[None] | None = None
    try:
        recovered = _recover_claimable_stranded(
            control,
            claims,
            wid,
            lease_seconds=lease_seconds,
            now=clock,
            sink=sink,
        )
        if reconcile_seconds is not None and reconcile_seconds > 0:
            reconciler = asyncio.create_task(
                _source_reconcile_loop(
                    source=source,
                    control=control,
                    interval=reconcile_seconds,
                    now=clock,
                    stream=stream,
                ),
                name="flywheel-source-reconciler",
            )
        if sweep_seconds is not None and sweep_seconds > 0:
            # The in-loop sweeper MUST claim under a distinct worker id, never
            # this driver's own ``wid`` (spec 00069, criterion #2). acquire_claim
            # lets the *same* worker re-take its own live lease, so a sweeper
            # sharing ``wid`` would reclaim a task THIS worker is actively running
            # -- a live claim + RUNNING lifecycle -- and wrongly finalize it
            # mid-run. A separate id makes that live claim look like a peer's: the
            # lease is still in the future, so acquire_claim returns None and the
            # running task is left untouched. A genuinely stranded task's lease is
            # lapsed, so it is reclaimable regardless of which id sweeps it. The
            # entry-time recovery keeps ``wid`` safely -- no task is being driven
            # yet, so this worker holds no live claim to trip over.
            sweep_wid = f"{wid}-sweeper"
            sweeper = asyncio.create_task(
                _lease_sweep_loop(
                    control=control,
                    claims=claims,
                    worker_id=sweep_wid,
                    lease_seconds=lease_seconds,
                    interval=sweep_seconds,
                    now=clock,
                    sink=sink,
                    stream=stream,
                ),
                name="flywheel-lease-sweeper",
            )
        runs: list[RunRecord] = []
        attempted_fresh: set[str] = set()
        attempted_resume: set[str] = set()
        attempted_approve: set[str] = set()

        while True:
            # Source-listing containment (mirrors _source_reconcile_loop and
            # sync_work_source): a raising list_work() must degrade this pass,
            # never crash the driver. A generic-Exception guard matches the
            # reconciler/sync posture -- a tracker hiccup or adapter/transport
            # failure is contained, the in-flight state (claims, runs already
            # recorded) is left untouched, and the worker quiesces gracefully
            # (returns its report) rather than unwinding the whole session.
            try:
                items = list(source.list_work())
            except Exception as exc:  # noqa: BLE001 - adapter/transport failure
                if stream is not None:
                    print(
                        f"[orchestrate] work-source listing failed "
                        f"({type(exc).__name__}: {exc}); no work this pass, "
                        f"ending the session without touching in-flight state",
                        file=stream,
                        flush=True,
                    )
                break
            rows = status_rows_for_items(items, control)
            # Build (and structurally validate) the WorkGraph from the same
            # items the rows derive from -- one list_work() pass feeds both.
            # Fresh selection below reads runnable tasks from this validated
            # graph instead of resolving prerequisite edges ad hoc. A
            # structural defect (duplicate id, self-dependency, cycle) raises
            # here, before any task dispatches; a missing prerequisite is a
            # recorded issue and keeps its task ineligible / out of the ready
            # set exactly as today (spec 00047, decision D-1).
            validation = WorkGraph.build(items)
            graph = validation.graph
            # Bounded dangling-prerequisite re-drive (spec 00069, criteria
            # #7/#8/#13). A task whose declared prerequisite resolves to no work
            # item stays out of the ready set exactly as before -- never
            # dispatched against an unsatisfied prereq. Each such pass appends one
            # ``dangling-prerequisite`` witness to the append-only stop ledger
            # (spec 00068, naming the referencing task and the missing id; never
            # deduped, so the recurrence is the signal). When the prerequisite
            # stays missing through ``prereq_redrive_bound`` cycles the
            # referencing task is routed ONCE to the human-review queue with
            # ``prerequisite-missing`` naming the missing id; if instead the
            # prerequisite appears, the next pass's graph rebuild resolves the
            # edge, no issue is produced, and the task becomes eligible and is
            # driven (criterion #7).
            redrive_missing_prerequisites(
                claims,
                issues=validation.issues,
                bound=prereq_redrive_bound,
                now=clock,
                stream=stream,
            )
            # Human-gate routing (spec 00069, criteria #10/#11; D-E). An
            # intentional stop -- a lifecycle parked AWAITING_APPROVAL, or a run
            # terminated by ``intent=abort`` or a budget-ceiling breach -- is a
            # human's decision, never a transient strand to auto-recover. Surface
            # each into the human-review queue ONCE with a reason naming its cause
            # WITHOUT touching its status: the gate stays parked (no auto-approve /
            # auto-reject) and the abort/budget FAILED is never re-dispatched (the
            # scheduler already excludes terminal FAILED from selection). Purely a
            # read-and-append side channel -- it changes no lifecycle and no
            # scheduling decision this pass.
            redrive_human_gates(
                control,
                claims,
                requests=[
                    HumanGateRequest(
                        task_id=r.task.id, run_id=r.latest_run_id
                    )
                    for r in rows
                    if r.latest_run_id is not None
                    and r.latest_status
                    in (Status.AWAITING_APPROVAL, Status.FAILED)
                ],
                now=clock,
                stream=stream,
            )
            states: dict[str, TaskState] = {r.task.id: r.state for r in rows}
            task_by_id: dict[str, Task] = {r.task.id: r.task for r in rows}
            row_by_id: dict[str, TaskStatusRow] = {
                r.task.id: r for r in rows
            }
            # Claim-time conflict keys come off the same list_work() pass
            # (spec 00049). The dispatch and resume acquires below carry an
            # item's keys, and the claim is held through verify and landing,
            # so two items with overlapping keys never have concurrent
            # edit-to-land windows. Bookkeeping acquires (stranded finalize,
            # retry escalation, approval resolve, landing re-drive) stay
            # keyless: record-keeping on one task must not queue behind an
            # unrelated task that merely shares a file key.
            conflict_keys_by_id: dict[str, frozenset[str]] = {
                item.task.id: item.conflict_keys for item in items
            }
            blocked_ids = frozenset(
                r.task.id for r in rows if _is_blocked_interrupted(r)
            )

            # Graph snapshot (spec 00055, D-4/D-6): record one immutable
            # cross-section of the graph this pass built -- per-item provenance,
            # lifecycle state, ready-set membership, claim holder, and resolved
            # prerequisites -- at the TOP of the pass, after the graph/states/
            # readiness are known and BEFORE any task is dispatched, stamped
            # with the injected clock. A pure read-and-record side channel: it
            # observes the scheduler's inputs and never changes which task is
            # dispatched nor any claim/lease behavior. Exactly one snapshot per
            # pass -- including the terminal no-progress pass and an empty
            # source's pass (criterion #11), so a driven run leaves a sequence
            # tracking the graph evolving as tasks complete.
            claims.record_graph_snapshot(
                _assemble_graph_snapshot_items(
                    items,
                    graph,
                    states,
                    worker_capabilities=worker_capabilities,
                    claims=claims,
                ),
                captured_at=clock(),
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
                claim = claims.acquire_claim(
                    row.task.id,
                    wid,
                    now=clock(),
                    lease_seconds=lease_seconds,
                    conflict_keys=conflict_keys_by_id.get(
                        row.task.id, frozenset()
                    ),
                )
                if claim is None:
                    continue  # another worker owns this task right now
                try:
                    # Provision the sandbox before recheck so the blocked
                    # predicates are evaluated in the same (consumer-prepared,
                    # e.g. rebased-onto-base) tree the resumed run will use.
                    # Only the claim-holder reaches here, so prepares stay
                    # bounded. A failing provider skips this task for the
                    # session (it never starves peers) rather than unwinding
                    # the worker; the finally releases the claim.
                    try:
                        handle = resolve_sandbox(row, run_id, "resume")
                    except Exception as exc:  # noqa: BLE001 - consumer code
                        attempted_resume.add(run_id)
                        if stream is not None:
                            print(
                                f"[orchestrate] {row.task.id}: prepare "
                                f"failed ({type(exc).__name__}: {exc}); "
                                f"skipping",
                                file=stream,
                                flush=True,
                            )
                        # Release the claim AND witness the prepare-skip in one
                        # transaction (D-3): the control flow is unchanged (we
                        # still skip this task for the session and keep
                        # draining peers), but the dead-end is now recorded on
                        # the stop ledger. ``claim = None`` so the finally does
                        # not double-release.
                        claims.record_prepare_skip(
                            claim,
                            detail=f"{type(exc).__name__}: {exc}",
                            now=clock(),
                        )
                        claim = None
                        continue
                    sandbox = handle.path
                    drive_primitives, drive_invoke = _apply_handle(
                        handle, sandbox_primitives, invoke
                    )
                    try:
                        outcome = recheck_blocked_lifecycle(
                            control,
                            run_id,
                            task_by_id[row.task.id],
                            cwd=sandbox,
                            sink=sink,
                        )
                    except OptimisticConcurrencyError:
                        # Another worker transitioned it first; let go.
                        continue
                    if not outcome.applied:
                        continue
                    attempted_resume.add(run_id)
                    record = await _drive_or_relinquish(
                        control,
                        claims,
                        claim,
                        row_by_id[row.task.id],
                        db_path=db_path,
                        sandbox=sandbox,
                        submit=submit,
                        landability_probe=landability_probe,
                        teardown=handle.teardown,
                        work_source=source,
                        held_out_source=held_out_source,
                        run_id=run_id,
                        worker_id=wid,
                        lease_seconds=lease_seconds,
                        heartbeat_interval=heartbeat_interval,
                        invoke=drive_invoke,
                        model=model,
                        max_turns=max_turns,
                        max_retries=max_retries,
                        **drive_primitives,
                        **limit_primitives,
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
                        claims.release_claim(claim)
            if progressed:
                continue

            # 1b. Reactive resolve of AWAITING_APPROVAL gates, claim-gated
            #     so only one worker drives a given parked run. The
            #     resolver advances the lifecycle in place — no follow-on
            #     drive is needed: ``approved_next_gate`` re-parks on the
            #     next ordinal (the next loop iteration picks up any
            #     further pending approve), ``rejected_retry`` leaves the
            #     lifecycle ``READY`` for the fresh-selection pass below,
            #     and ``approved_done`` / ``rejected_failed`` are terminal.
            #     With no pending command the lifecycle stays parked and
            #     the run_id is marked so we do not tight-loop on the
            #     no-op for the rest of this session.
            for row in rows:
                if not _is_awaiting_approval(row):
                    continue
                run_id = row.latest_run_id
                if run_id is None or run_id in attempted_approve:
                    continue
                claim = claims.acquire_claim(
                    row.task.id,
                    wid,
                    now=clock(),
                    lease_seconds=lease_seconds,
                )
                if claim is None:
                    continue  # another worker owns this task right now
                try:
                    lifecycle = control.load_lifecycle(run_id)
                    if lifecycle is None:
                        attempted_approve.add(run_id)
                        continue
                    try:
                        approval_outcome = resolve_manual_approval(
                            lifecycle,
                            control,
                            task_by_id[row.task.id],
                            max_retries=max_retries,
                            now=clock,
                            sink=sink,
                        )
                    except OptimisticConcurrencyError:
                        # Another worker resolved it first; let go.
                        continue
                    if not approval_outcome.applied:
                        attempted_approve.add(run_id)
                        continue
                    # Lifecycle advanced in place; restart so the
                    # changed state is re-read on the next pass.
                    progressed = True
                    break
                finally:
                    claims.release_claim(claim)
            if progressed:
                continue

            # 1c. Bounded re-drive of parked landings (spec 00069). A run that
            #     finished and graded green but whose strategy could not land it
            #     (divergent base, failed standing verify) sits parked on an
            #     unmerged branch; the re-driver re-attempts the land through the
            #     strategy's own rebase/reverify/standing/FF path up to the bound
            #     and routes a never-clearing strand to the human-review queue.
            #     Opt-in and strategy-gated: the landability probe (a bundled
            #     strategy) is what distinguishes a still-parked change from one
            #     that has since landed, so it never runs for a bare-callable
            #     caller. A re-attempt that lands advanced the base and may
            #     promote dependents, so we re-read state on the next pass.
            if (
                landing_redrive_bound is not None
                and landing_redrive_bound > 0
                and strategy is not None
            ):
                redrive_requests = [
                    SubmitRequest(
                        task_id=row.task.id,
                        task_file=row.task_file,
                        task=row.task,
                        run_id=row.latest_run_id,
                        status=Status.DONE,
                        sandbox=sandbox_root / row.task.id,
                        source_ref=row.source_ref,
                    )
                    for row in rows
                    if row.latest_run_id is not None
                    and row.latest_status == Status.DONE
                ]
                outcomes = redrive_parked_landings(
                    control,
                    claims,
                    strategy,
                    wid,
                    requests=redrive_requests,
                    bound=landing_redrive_bound,
                    held_out_source=held_out_source,
                    grader_env=sandbox_primitives["grader_env"],
                    lease_seconds=lease_seconds,
                    now=clock,
                    stream=stream,
                )
                if any(o.result == "landed" for o in outcomes):
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
                # Highest-priority-first over the validated graph: ready_set
                # returns every runnable item (own state eligible, all
                # prerequisites DONE, required_capabilities a subset of this
                # worker's set, id not excluded) ordered by descending
                # priority with a stable walk-order tie-break, so taking the
                # first preserves select_next_task's deterministic selection
                # byte-for-byte (and, at all-default priority, the prior pure
                # walk order) while sourcing eligibility from the validated
                # graph. An excluded id drops from candidacy but still
                # satisfies a dependent's prerequisite (ready_set grades
                # prerequisites off ``states``, not ``excluded``), matching
                # exclude_ids semantics. A dangling prerequisite keeps its
                # task out of the ready set -- it never runs -- exactly as
                # before.
                ready = graph.ready_set(
                    states,
                    excluded=exclude,
                    worker_capabilities=worker_capabilities,
                )
                pick = row_by_id[ready[0].task.id] if ready else None
                if pick is None:
                    break
                # Schedule-time static-validation gate (spec 00034): refuse to
                # dispatch a task whose graders are statically broken. Skip it
                # for the session (exclude from candidacy) and surface the
                # defect, mirroring the unprovisionable-task skip below so one
                # bad task never starves eligible peers.
                if repo_root is not None:
                    defects = validate_task(pick.task, repo_root=repo_root)
                    if defects:
                        attempted_fresh.add(pick.task.id)
                        if stream is not None:
                            for defect in defects:
                                print(
                                    f"[orchestrate] {pick.task.id}: invalid "
                                    f"task definition ({defect.detail}); "
                                    f"skipping",
                                    file=stream,
                                    flush=True,
                                )
                        continue
                claim = claims.acquire_claim(
                    pick.task.id,
                    wid,
                    now=clock(),
                    lease_seconds=lease_seconds,
                    conflict_keys=conflict_keys_by_id.get(
                        pick.task.id, frozenset()
                    ),
                )
                if claim is None:
                    held.add(pick.task.id)
                    continue
                # Post-claim terminal-state recheck (closes the stale-snapshot
                # TOCTOU). ``states`` was read at the top of this pass; a peer
                # worker may have driven this task to DONE and released its
                # claim in the window before we acquired ours. ``release_claim``
                # deletes the claim row, so the claim layer alone cannot report
                # that the task already finished -- only re-reading its
                # lifecycle can. Refresh just the picked task's state under our
                # now-held claim (a DONE prerequisite is monotonic, so the rest
                # of the snapshot stays valid) and re-test readiness with the
                # exact predicate the scheduler used. If a peer finished it, the
                # task is no longer in the ready set: drop the claim and exclude
                # it rather than minting a second run. The resume paths
                # (sections 1/1b) already recheck after claiming; this gives the
                # fresh path the same exactly-once guarantee.
                states[pick.task.id] = task_state(control, pick.task).state
                if not any(
                    item.task.id == pick.task.id
                    for item in graph.ready_set(
                        states,
                        excluded=exclude,
                        worker_capabilities=worker_capabilities,
                    )
                ):
                    claims.release_claim(claim)
                    attempted_fresh.add(pick.task.id)
                    continue
                attempted_fresh.add(pick.task.id)
                # A bare-interrupted lifecycle (operator SIGINT or in-band
                # ``interrupt`` with no structured block — blocked-interrupted
                # rows are excluded from fresh selection above) must RESUME on
                # its own run_id so run_task's entry-time INTERRUPTED -> READY
                # normalization fires and the paused work continues. Minting a
                # fresh run_id instead would drive a brand-new lifecycle from
                # scratch and orphan the paused one (docs/task-lifecycle.md).
                resume_run_id = (
                    pick.latest_run_id
                    if pick.state == TaskState.INTERRUPTED
                    else None
                )
                select_mode = "resume" if resume_run_id is not None else "fresh"
                try:
                    handle = resolve_sandbox(pick, resume_run_id, select_mode)
                except Exception as exc:  # noqa: BLE001 - consumer code
                    # A failing provider skips this task for the session
                    # (already in attempted_fresh) and keeps draining the
                    # rest, rather than unwinding the whole worker. The claim
                    # release and the prepare-skip witness commit in one
                    # transaction (D-3): scheduling is unchanged, but the
                    # dead-end is now recorded on the stop ledger.
                    claims.record_prepare_skip(
                        claim,
                        detail=f"{type(exc).__name__}: {exc}",
                        now=clock(),
                    )
                    if stream is not None:
                        print(
                            f"[orchestrate] {pick.task.id}: prepare failed "
                            f"({type(exc).__name__}: {exc}); skipping",
                            file=stream,
                            flush=True,
                        )
                    continue
                sandbox = handle.path
                drive_primitives, drive_invoke = _apply_handle(
                    handle, sandbox_primitives, invoke
                )
                record = await _drive_or_relinquish(
                    control,
                    claims,
                    claim,
                    pick,
                    db_path=db_path,
                    sandbox=sandbox,
                    submit=submit,
                    landability_probe=landability_probe,
                    teardown=handle.teardown,
                    work_source=source,
                    held_out_source=held_out_source,
                    run_id=resume_run_id,
                    worker_id=wid,
                    lease_seconds=lease_seconds,
                    heartbeat_interval=heartbeat_interval,
                    invoke=drive_invoke,
                    model=model,
                    max_turns=max_turns,
                    max_retries=max_retries,
                    **drive_primitives,
                    **limit_primitives,
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
        if reconciler is not None:
            reconciler.cancel()
            with suppress(asyncio.CancelledError):
                await reconciler
        if sweeper is not None:
            sweeper.cancel()
            with suppress(asyncio.CancelledError):
                await sweeper
        control.close()
        claims.close()
        sink.close()


def _final_grader_receipts(
    control: SqliteStore | PostgresStore, run_id: str
) -> tuple[GraderReceipt, ...]:
    """Project the final attempt's grader receipts for a work report.

    Reads the run's attempts and flattens the last attempt's
    ``grader_results`` rows into :class:`GraderReceipt` values. A run that
    never reached grading (crash before validate, parked) yields an empty
    tuple — the report still carries the lifecycle status.
    """
    attempts = control.list_attempts(run_id)
    if not attempts:
        return ()
    final_number = attempts[-1].number
    return tuple(
        GraderReceipt(
            ordinal=record.ordinal,
            grader_type=str(record.grader_type),
            name=record.grader_name,
            passed=record.passed,
        )
        for record in control.list_grader_results(run_id, final_number)
    )


def _evaluate_landing_gate(
    held_out_source: HeldOutGraderSource | None,
    task: Task,
    *,
    status: Status,
    sandbox: Path,
    run_id: str,
    task_id: str,
    grader_env: Mapping[str, str] | None,
    stream: TextIO | None,
) -> GateVerdict | None:
    """Compute the held-out landing verdict for a finalized run, or ``None``.

    Returns ``None`` (the gate did not run) when no held-out source is wired or
    the run did not reach the landing status (``DONE``) -- a non-landing
    terminal is already parked by ``submit``, so there is nothing to gate, and
    the behavior is byte-identical to today (D-7). Otherwise it runs the task's
    held-out command graders against ``sandbox`` (the committed tree) via the
    engine, which fails closed on an unrunnable registration (D-3). The verdict
    is logged when it is anything other than ``NO_GATE`` so a blocked land is
    visible in the run's timeline.
    """
    if held_out_source is None or status is not Status.DONE:
        return None
    verdict = evaluate_held_out_gate(
        task,
        held_out_source,
        committed_tree=sandbox,
        run_id=run_id,
        env=grader_env,
    )
    if stream is not None and verdict.outcome is not GateOutcome.NO_GATE:
        marker = "BLOCKED" if verdict.blocks_landing else "passed"
        print(
            f"[orchestrate] {task_id}: held-out landing gate {marker} "
            f"({verdict.reason})",
            file=stream,
            flush=True,
        )
    return verdict


def _record_held_out_gate_park(
    control: SqliteStore | PostgresStore,
    *,
    run_id: str,
    detail: str,
    task_id: str,
    stream: TextIO | None,
) -> None:
    """Append a ``held-out-gate`` :class:`LandingParked` audit-witness to the
    blocked DONE run's ledger.

    The held-out gate's verdict otherwise lives only on the in-process
    ``RunRecord``; this persists it so an operator can see *why* the land was
    suppressed. Audit-witness only (D-2): the run stays ``Status.DONE`` and the
    submit is still not invoked -- ``LandingParked`` folds to the identity and
    only advances ``version``. Best-effort: a missing lifecycle or any store
    error is logged to ``stream`` and swallowed so the orchestrate loop never
    unwinds on a recording failure.
    """
    try:
        lifecycle = control.load_lifecycle(run_id)
        if lifecycle is None:
            return
        control.append_domain_event(
            LandingParked(
                run_id=run_id,
                ts=_utcnow(),
                park_kind=PARK_KIND_HELD_OUT_GATE,
                detail=detail,
            ),
            expected_version=lifecycle.version,
        )
    except Exception as exc:  # noqa: BLE001 - best-effort audit witness
        if stream is not None:
            print(
                f"[orchestrate] {task_id}: failed to record held-out-gate "
                f"landing-parked event ({type(exc).__name__}: {exc})",
                file=stream,
                flush=True,
            )


def _gate_grader_excerpt(
    payload: Mapping[str, Any], *, bound: int = GATE_EXCERPT_MAX_BYTES
) -> str:
    """Build one held-out grader's bounded output tail for a verdict receipt.

    Combines the command grader's captured stdout/stderr tails (and a spawn
    error, when the subprocess never started) into one raw excerpt, then re-caps
    it to ``bound`` bytes as a *tail* so the final content survives when a grader
    emitted more than the bound (spec 00073, criterion 11). The per-stream tails
    the runner captures are each already bounded, so the common single-stream
    case passes through unchanged. Stored raw: no redaction here -- that is a
    render-time concern (spec 00073, D-2).
    """
    segments: list[str] = []
    for key in ("stdout_tail", "stderr_tail", "spawn_error"):
        value = payload.get(key)
        if isinstance(value, str) and value:
            segments.append(value)
    combined = "\n".join(segments)
    encoded = combined.encode("utf-8")
    if len(encoded) <= bound:
        return combined
    return encoded[-bound:].decode("utf-8", errors="replace")


def _record_held_out_gate_verdict(
    control: SqliteStore | PostgresStore,
    *,
    run_id: str,
    verdict: GateVerdict,
    task_id: str,
    stream: TextIO | None,
) -> None:
    """Append a :class:`HeldOutGateEvaluated` verdict record to the run's ledger.

    Emitted for EVERY gate evaluation -- pass, fail, or no-gate (spec 00073,
    D-1) -- carrying the outcome, the operator-readable reason, and one
    :class:`GateGraderReceipt` per executed held-out grader (its name, outcome,
    and bounded raw output tail). This persists the verdict that otherwise lived
    only on the in-process ``RunRecord``, so a gate-decided park is diagnosable
    from the store alone. Audit-witness only: ``HeldOutGateEvaluated`` folds to
    the identity and only advances ``version`` -- the run stays ``Status.DONE``
    and the gate decision (what lands, blocks, or fails closed) is untouched
    (D-2). Best-effort: a missing lifecycle or any store error is logged to
    ``stream`` and swallowed so the orchestrate loop never unwinds on a
    recording failure.
    """
    try:
        lifecycle = control.load_lifecycle(run_id)
        if lifecycle is None:
            return
        receipts = tuple(
            GateGraderReceipt(
                grader_name=record.grader_name,
                passed=record.passed,
                output_excerpt=_gate_grader_excerpt(record.payload),
            )
            for record in verdict.results
        )
        control.append_domain_event(
            HeldOutGateEvaluated(
                run_id=run_id,
                ts=_utcnow(),
                outcome=verdict.outcome.value,
                reason=verdict.reason,
                receipts=receipts,
            ),
            expected_version=lifecycle.version,
        )
    except Exception as exc:  # noqa: BLE001 - best-effort audit witness
        if stream is not None:
            print(
                f"[orchestrate] {task_id}: failed to record held-out-gate "
                f"verdict event ({type(exc).__name__}: {exc})",
                file=stream,
                flush=True,
            )


def _reevaluate_landing_gate(
    control: SqliteStore | PostgresStore,
    held_out_source: HeldOutGraderSource | None,
    request: SubmitRequest,
    *,
    grader_env: Mapping[str, str] | None,
    stream: TextIO | None,
) -> bool:
    """Fresh held-out gate evaluation for one landing re-attempt (spec 00074).

    A re-driven landing must clear the SAME held-out gate a first landing does,
    against the content it is about to land (D-1) -- otherwise a park the gate
    blocked could slip in on re-drive through the strategy's own checks alone.
    This runs the identical evaluation and verdict-record path as the first
    attempt (D-2): :func:`_evaluate_landing_gate` computes the verdict against
    ``request.sandbox`` (the committed tree) and
    :func:`_record_held_out_gate_verdict` persists it for every evaluation --
    pass, fail, or no-gate.

    Returns whether the re-attempt may proceed to ``strategy.submit``:

    * ``True`` -- the gate passed, or no held-out source is wired / the run is
      not at the landing status, so an ungated re-drive lands byte-for-byte as
      today (D-6).
    * ``False`` -- the gate FAILED, or the evaluation itself errored (fail
      closed, exactly as the first attempt suppresses the land). A suppressed
      re-attempt appends a held-out-gate :class:`LandingParked` so the strand
      stays durably re-parked and the re-drive bound accounting (the park count)
      advances -- mirroring how a strategy re-park and a first-attempt gate
      block both leave a park witness.
    """
    gate_errored = False
    try:
        gate = _evaluate_landing_gate(
            held_out_source,
            request.task,
            status=request.status,
            sandbox=request.sandbox,
            run_id=request.run_id,
            task_id=request.task_id,
            grader_env=grader_env,
            stream=stream,
        )
    except Exception as exc:  # noqa: BLE001 - fail closed, suppress landing
        gate = None
        gate_errored = True
        if stream is not None:
            print(
                f"[orchestrate] {request.task_id}: held-out landing gate "
                f"re-evaluation errored ({type(exc).__name__}: {exc}); landing "
                f"suppressed (failing closed)",
                file=stream,
                flush=True,
            )
    if gate is not None:
        _record_held_out_gate_verdict(
            control,
            run_id=request.run_id,
            verdict=gate,
            task_id=request.task_id,
            stream=stream,
        )
    if (gate is not None and gate.blocks_landing) or gate_errored:
        detail = (
            gate.reason
            if gate is not None and gate.reason
            else "held-out landing gate re-evaluation errored; failing closed"
        )
        _record_held_out_gate_park(
            control,
            run_id=request.run_id,
            detail=detail,
            task_id=request.task_id,
            stream=stream,
        )
        return False
    return True


def _datapath_store(
    control: SqliteStore | PostgresStore,
) -> SqliteStore | PostgresStore | None:
    """The store ``run_task_object`` drives the run record through, or ``None``.

    Under a durable backend the run's whole record -- task version, lifecycle,
    every attempt, its domain events, and grader results -- is written to the
    orchestrator's own ``control`` store so it lands in the configured database
    and no run state leaks to sqlite (spec 00075). On the sqlite default this
    returns ``None``: ``run_task_object`` opens and closes its own
    :class:`SqliteStore` on ``db_path`` (a separate WAL connection, visible to
    ``control`` on the next query), byte-for-byte the historical behavior. The
    discriminator is the store type itself -- a ``SqliteStore`` keeps the
    separate-connection path; any other store is injected and shared.
    """
    if isinstance(control, SqliteStore):
        return None
    return control


async def _drive_under_lease(
    control: SqliteStore | PostgresStore,
    claims: SqliteClaimStore | PostgresClaimStore,
    claim: TaskClaim,
    row: TaskStatusRow,
    *,
    db_path: Path,
    sandbox: Path,
    submit: Submitter | None,
    landability_probe: object | None,
    teardown: Callable[[], None] | None,
    work_source: WorkSource,
    held_out_source: HeldOutGraderSource | None,
    run_id: str | None,
    worker_id: str,
    lease_seconds: float,
    heartbeat_interval: float,
    invoke: InvokeFunc | None,
    model: str | None,
    max_turns: int,
    max_retries: int,
    permission_mode: str,
    skills: str | tuple[str, ...],
    allowed_tools: tuple[str, ...],
    denied_tools: tuple[str, ...],
    setting_sources: tuple[str, ...] | None,
    mcp_servers: tuple[str, ...],
    mcp_strict: bool,
    exec_enabled: bool,
    exec_auto_allow: bool,
    agent_env: dict[str, str],
    grader_env: Mapping[str, str] | None,
    max_cost_usd: float,
    max_tokens: int,
    wall_clock_seconds: int,
    stream: TextIO | None,
    now: Callable[[], datetime],
) -> RunRecord:
    """Run (or resume) one task while a heartbeat holds its lease, hand the
    terminal status to ``submit``, report it to the work source, then
    release the lease.

    The run's persistence backend is chosen by :func:`_datapath_store`. On the
    sqlite default ``run_task_object`` opens and closes its own store, so its
    writes are committed and visible to the orchestrator's control store on the
    next query (separate SQLite connections under WAL). Under a durable backend
    the shared ``control`` store is injected instead, so the whole run record
    lands in the configured database and nothing is written to sqlite (spec
    00075); that store is pooled, so the concurrent reconcile/sweep tasks and
    the heartbeat borrow their own connections.

    ``submit`` (when provided) runs after the task finalizes but *before* the
    lease is released, so a consumer's merge/park acts under the same
    exclusivity that kept peers off the task. It must not raise; a graceful
    SIGTERM cancels the run before this point, so an interrupted task's
    sandbox is left untouched (parked) for the next attempt to reuse.

    ``teardown`` (the run's :class:`SandboxHandle` teardown, when the provider
    supplied one) runs after ``submit`` and before the lease is released, so a
    container backend disposes its sandbox under the same exclusivity. Like
    ``submit`` it MUST NOT raise; any error is contained here and logged, so a
    failed teardown never unwinds the worker or loses the run record.

    ``work_source.report`` runs after ``submit`` (still under the lease) so
    the external system hears about the outcome only once the consumer's
    merge/park has settled. Unlike ``submit`` it MAY raise — the exception
    is contained here and logged to ``stream``; a lost report never costs
    the schedule.
    """
    task_id = row.task.id
    # Landable-change gate (spec 00061, D-1/D-2): the orchestrator owns the
    # gate but stays git-unaware. It hands the harness a git-free closure
    # consulted at the verify-passed VALIDATING -> DONE boundary; the closure
    # asks the bundled strategy's read-only landability predicate whether the
    # finished change is landable. A non-empty reason re-drives the run via the
    # harness's own max_retries machinery (FAILED_VALIDATION, against the same
    # base) instead of landing it as DONE; the unchanged submit path runs only
    # for a landable (or no-op default) verdict. A strategy with no diff notion
    # (non-git) is always-landable, so the closure returns None and DONE lands
    # byte-identically. Probe errors fail open (return None): a buggy predicate
    # must never block landing -- submit's existing park logic is the fallback.
    def _landability_gate() -> str | None:
        request = SubmitRequest(
            task_id=task_id,
            task_file=row.task_file,
            task=row.task,
            run_id=run_id or "",
            status=Status.DONE,
            sandbox=sandbox,
            source_ref=row.source_ref,
        )
        try:
            verdict = probe_landability(landability_probe, request)
        except Exception as exc:  # noqa: BLE001 - probe is consumer code
            if stream is not None:
                print(
                    f"[orchestrate] {task_id}: landability probe failed "
                    f"({type(exc).__name__}: {exc}); treating as landable",
                    file=stream,
                    flush=True,
                )
            return None
        if verdict.landable:
            return None
        return verdict.reason or "no landable change"

    heartbeat = _ClaimHeartbeat(
        claims=claims,
        claim=claim,
        lease_seconds=lease_seconds,
        interval=heartbeat_interval,
        now=now,
    ).start()
    try:
        outcome = await run_task_object(
            row.task,
            db_path=db_path,
            store=_datapath_store(control),
            sandbox=sandbox,
            model=model,
            max_turns=max_turns,
            max_retries=max_retries,
            permission_mode=permission_mode,
            skills=skills,
            allowed_tools=allowed_tools,
            denied_tools=denied_tools,
            setting_sources=setting_sources,
            mcp_servers=mcp_servers,
            mcp_strict=mcp_strict,
            exec_enabled=exec_enabled,
            exec_auto_allow=exec_auto_allow,
            agent_env=agent_env,
            grader_env=grader_env,
            max_cost_usd=max_cost_usd,
            max_tokens=max_tokens,
            wall_clock_seconds=wall_clock_seconds,
            invoke=invoke,
            stream=stream,
            run_id=run_id,
            source=row.source_ref or None,
            landability_gate=(
                _landability_gate if landability_probe is not None else None
            ),
        )
        # A sustained heartbeat stall can let a peer's lease sweep steal this
        # claim and finalize the lifecycle out from under us while the run was
        # in flight. If renewal observed that loss, stop here: do NOT land or
        # report a run whose claim a peer now owns (the peer may already have
        # recorded its own terminal state). Raising ClaimLostError routes to
        # _drive_or_relinquish's existing containment deterministically, rather
        # than relying on the lifecycle version CAS to happen to conflict.
        if heartbeat.lost():
            raise ClaimLostError(task_id)
        # Projected once, shared by the consumer submit step and the
        # work-source report. Best-effort: a store hiccup costs the
        # receipts, never the landing or the schedule.
        try:
            receipts = _final_grader_receipts(
                control, outcome.lifecycle.run_id
            )
        except Exception as exc:  # noqa: BLE001 - projection is best-effort
            receipts = ()
            if stream is not None:
                print(
                    f"[orchestrate] {task_id}: grader receipt projection "
                    f"failed ({type(exc).__name__}: {exc}); continuing",
                    file=stream,
                    flush=True,
                )
        # Execute-time held-out landing gate (spec 00050, D-1): while the lease
        # is still held and BEFORE submit, grade the committed sandbox with the
        # task's operator-declared held-out command graders (the agent never
        # saw them). Only the landing status (DONE) is gated -- a non-landing
        # terminal already parks via submit, so there is nothing to block. The
        # verdict is the out-of-band grader exit code, never the agent's
        # self-report (D-4); a registered-but-unrunnable grader fails closed
        # (D-3). A blocked gate suppresses the submit landing effect: submit is
        # not invoked, so the worktree backend leaves the sandbox parked for
        # forensics and no merge/PR happens.
        # Call-site containment, fail closed: gate evaluation must never unwind
        # the worker, and an evaluation that ERRORED must suppress landing
        # rather than fall through to a merge/PR off an unevaluated gate. The
        # engine already fails closed on a registration/runner error (returns a
        # FAIL verdict); this guard covers the orthogonal case where the
        # evaluation itself raised an unexpected exception (e.g. a buggy custom
        # source). Mirrors the gate engine's discipline and the program
        # decision that intentional gates are never auto-bypassed: an errored
        # gate blocks the land just like a FAIL verdict.
        gate_errored = False
        try:
            gate = _evaluate_landing_gate(
                held_out_source,
                row.task,
                status=outcome.lifecycle.status,
                sandbox=sandbox,
                run_id=outcome.lifecycle.run_id,
                task_id=task_id,
                grader_env=grader_env,
                stream=stream,
            )
        except Exception as exc:  # noqa: BLE001 - fail closed, suppress landing
            gate = None
            gate_errored = True
            if stream is not None:
                print(
                    f"[orchestrate] {task_id}: held-out landing gate "
                    f"evaluation errored ({type(exc).__name__}: {exc}); "
                    f"landing suppressed (failing closed)",
                    file=stream,
                    flush=True,
                )
        if gate is not None:
            # Persist the gate verdict for EVERY evaluation -- pass, fail, or
            # no-gate (spec 00073, D-1) -- so the decision and its per-grader
            # receipts (name, outcome, bounded output tail) are diagnosable from
            # the store alone, not only from the in-process RunRecord. Recorded
            # before the park witness below so a blocked run carries both the
            # verdict record and the landing-parked fact. Audit-witness only:
            # this never changes whether the land is suppressed (D-2).
            _record_held_out_gate_verdict(
                control,
                run_id=outcome.lifecycle.run_id,
                verdict=gate,
                task_id=task_id,
                stream=stream,
            )
        gate_blocked = gate is not None and gate.blocks_landing
        if gate is not None and gate.blocks_landing:
            # The gate FAILed and is suppressing the land (submit is not called
            # below). Persist the verdict as a held-out-gate LandingParked so the
            # blocked strand is visible on the run's ledger; this does not change
            # whether the land is suppressed (D-2).
            _record_held_out_gate_park(
                control,
                run_id=outcome.lifecycle.run_id,
                detail=gate.reason or "held-out landing gate blocked the land",
                task_id=task_id,
                stream=stream,
            )
        if submit is not None and not gate_blocked and not gate_errored:
            submit(
                SubmitRequest(
                    task_id=task_id,
                    task_file=row.task_file,
                    task=row.task,
                    run_id=outcome.lifecycle.run_id,
                    status=outcome.lifecycle.status,
                    sandbox=sandbox,
                    source_ref=row.source_ref,
                    receipts=receipts,
                )
            )
        try:
            work_source.report(
                WorkReport(
                    task_id=task_id,
                    source_ref=row.source_ref,
                    run_id=outcome.lifecycle.run_id,
                    status=outcome.lifecycle.status,
                    error=outcome.lifecycle.error or "",
                    graders=receipts,
                )
            )
        except Exception as exc:  # noqa: BLE001 - adapter code
            if stream is not None:
                print(
                    f"[orchestrate] {task_id}: work-source report failed "
                    f"({type(exc).__name__}: {exc}); continuing",
                    file=stream,
                    flush=True,
                )
    finally:
        # Dispose the run's sandbox (e.g. docker stop/rm) on EVERY exit path —
        # clean finalize, grader failure, crash, in-band interrupt, or
        # claim-loss — still under the lease, so a container is never leaked
        # when run_task_object raises before submit. Best-effort and must not
        # raise (a teardown failure leaks at worst, never costs the schedule);
        # a None teardown (every worktree backend) is a no-op. The worktree
        # backend keeps parking its dir via submit, unaffected.
        if teardown is not None:
            try:
                teardown()
            except Exception as exc:  # noqa: BLE001 - consumer code
                if stream is not None:
                    print(
                        f"[orchestrate] {task_id}: sandbox teardown failed "
                        f"({type(exc).__name__}: {exc}); continuing",
                        file=stream,
                        flush=True,
                    )
        latest = heartbeat.stop()
        claims.release_claim(latest)
    return RunRecord(
        task_id=task_id,
        run_id=outcome.lifecycle.run_id,
        status=outcome.lifecycle.status,
        mode="resume" if run_id is not None else "fresh",
        worker_id=worker_id,
        gate=gate.outcome if gate is not None else None,
        gate_reason=gate.reason if gate is not None else "",
    )


async def _drive_or_relinquish(
    control: SqliteStore | PostgresStore,
    claims: SqliteClaimStore | PostgresClaimStore,
    claim: TaskClaim,
    row: TaskStatusRow,
    *,
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

    The same containment applies to an **in-band interrupt of this one
    run**: when a control-command ``interrupt`` (e.g. the steering bridge
    reacting to a vanished work item) cancels the live SDK stream, the
    harness finalizes the lifecycle to ``INTERRUPTED`` and re-raises
    ``CancelledError`` — correct for whole-worker shutdown, fatal for a
    multi-task session. The discriminator is two-sided: the orchestrate
    task itself must NOT be the one being cancelled
    (``current_task().cancelling() == 0`` — a SIGTERM/SIGINT teardown
    re-raises so the worker actually stops), AND the store must confirm
    the harness finalized this task's lifecycle to ``INTERRUPTED`` (an
    unexplained cancellation re-raises rather than being swallowed). A
    contained interrupt yields a real ``RunRecord`` so the session report
    still carries the run.
    """
    try:
        return await _drive_under_lease(
            control,
            claims,
            claim,
            row,
            stream=stream,
            **kwargs,
        )
    except (OptimisticConcurrencyError, ClaimLostError) as exc:
        if stream is not None:
            print(
                f"[orchestrate] {row.task.id}: claim lost mid-run "
                f"({type(exc).__name__}); a peer took over, re-evaluating",
                file=stream,
                flush=True,
            )
        return None
    except asyncio.CancelledError:
        current = asyncio.current_task()
        if current is not None and current.cancelling() > 0:
            raise  # the worker itself is being torn down
        latest = _latest_lifecycle_row(control, row.task.id)
        if latest is None or latest[1] is not Status.INTERRUPTED:
            raise  # not a finalized in-band interrupt; do not swallow
        interrupted_run_id = latest[0]
        if stream is not None:
            print(
                f"[orchestrate] {row.task.id}: run {interrupted_run_id} "
                f"interrupted in-band (control command); continuing with "
                f"remaining tasks",
                file=stream,
                flush=True,
            )
        return RunRecord(
            task_id=row.task.id,
            run_id=interrupted_run_id,
            status=Status.INTERRUPTED,
            mode="resume" if kwargs.get("run_id") is not None else "fresh",
            worker_id=kwargs["worker_id"],
        )


__all__ = [
    "DEFAULT_ESCALATION_BOUND",
    "DEFAULT_LANDING_REDRIVE_BOUND",
    "DEFAULT_LEASE_SECONDS",
    "DEFAULT_RECONCILE_SECONDS",
    "DEFAULT_SWEEP_SECONDS",
    "EscalationDriver",
    "EscalationOutcome",
    "EscalationRequest",
    "HumanGateOutcome",
    "HumanGateRequest",
    "OrchestratorReport",
    "RedriveOutcome",
    "RunRecord",
    "SandboxProvider",
    "SandboxRequest",
    "SubmitRequest",
    "Submitter",
    "orchestrate",
    "reconcile_live_runs",
    "redrive_exhausted_retries",
    "redrive_human_gates",
    "redrive_parked_landings",
    "sweep_expired_leases",
]

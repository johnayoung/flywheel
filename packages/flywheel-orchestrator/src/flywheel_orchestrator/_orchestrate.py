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
from contextlib import suppress
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import TYPE_CHECKING, Any, Callable, Literal, TextIO
from uuid import uuid4

from flywheel_core.harness import (
    InvokeFunc,
    finalize_stranded_lifecycle,
    recheck_blocked_lifecycle,
    resolve_manual_approval,
)
from flywheel_core.invoker_client import CONTROL_COMMAND_INTERRUPT
from flywheel_core.lifecycle import Status
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
from flywheel_core.task import Task
from flywheel_core.validation import validate_task
from flywheel_orchestrator._claims import (
    ClaimLostError,
    SqliteClaimStore,
    TaskClaim,
)
from flywheel_core.workflow import (
    DEFAULT_MAX_RETRIES,
    DEFAULT_MAX_TURNS,
    _stranded_run_ids,
    run_task_object,
)
from flywheel_orchestrator._policy import SandboxPolicy, WorkPolicy
from flywheel_orchestrator._sources import (
    DirectoryWorkSource,
    GraderReceipt,
    WorkReport,
    WorkSource,
)
from flywheel_orchestrator._store_factory import open_sqlite_bound_store
from flywheel_orchestrator._strategy import (
    SandboxHandle,
    SandboxProvider,
    SandboxRequest,
    SubmitRequest,
    SubmitStrategy,
    Submitter,
    _as_handle,
)
from flywheel_orchestrator._workflow import (
    TaskState,
    TaskStatusRow,
    _latest_lifecycle_row,
    select_next_task,
    status_rows_for_items,
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
            wanted = frozenset(item.task.id for item in source.list_work())
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
    claims: SqliteClaimStore,
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
        claims: SqliteClaimStore,
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
        if invoke is None:
            raise ValueError(
                "SandboxHandle.invoke_wrapper requires a base invoke; "
                "orchestrate was given none"
            )
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
    repo_root: Path | None = None,
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

    ``policy`` selects the store backend: construction routes through the
    store factory (:func:`~flywheel_orchestrator._store_factory.build_store`),
    so a postgres policy fails fast with the factory's DSN / extra errors.
    ``None`` (the default for library callers) keeps the historical
    sqlite-on-``db_path`` behavior byte-for-byte.

    ``repo_root`` enables the schedule-time static-validation gate (spec
    00034): a picked task whose graders fail :func:`validate_task` (an
    unparseable command, or a repo-relative path that does not exist) is
    skipped-and-reported instead of dispatched, mirroring the
    unprovisionable-task skip so one bad task never starves eligible peers.
    ``None`` (the default for library callers) disables the gate, preserving
    prior behavior.
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
    clock = now or _utcnow
    wid = worker_id or f"worker-{uuid4().hex[:8]}"
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

    db_path.parent.mkdir(parents=True, exist_ok=True)
    sandbox_root.mkdir(parents=True, exist_ok=True)

    # Two stores on one file: flywheel's core store (lifecycle, run state) and
    # the orchestrator's own claim store (task_claims). Each owns its tables.
    # The core store is built through the factory so the policy's backend
    # selection (and its fail-fast postgres preconditions) apply here too.
    control = open_sqlite_bound_store(policy, db_path=db_path)
    claims = SqliteClaimStore(db_path)
    # Control-plane telemetry (recovery sweeps, rechecks, approval
    # resolution) streams to the same per-run JSONL files the harness
    # writes (run_task_object derives the identical logs root from
    # db_path), so out-of-band interventions land in the run's timeline.
    sink = FileTelemetrySink(db_path.parent / "logs")
    reconciler: asyncio.Task[None] | None = None
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
        runs: list[RunRecord] = []
        attempted_fresh: set[str] = set()
        attempted_resume: set[str] = set()
        attempted_approve: set[str] = set()

        while True:
            rows = status_rows_for_items(source.list_work(), control)
            task_by_id: dict[str, Task] = {r.task.id: r.task for r in rows}
            row_by_id: dict[str, TaskStatusRow] = {
                r.task.id: r for r in rows
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
                claim = claims.acquire_claim(
                    row.task.id,
                    wid,
                    now=clock(),
                    lease_seconds=lease_seconds,
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
                        teardown=handle.teardown,
                        work_source=source,
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
                )
                if claim is None:
                    held.add(pick.task.id)
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
                    # rest, rather than unwinding the whole worker.
                    claims.release_claim(claim)
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
                    teardown=handle.teardown,
                    work_source=source,
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


async def _drive_under_lease(
    control: SqliteStore | PostgresStore,
    claims: SqliteClaimStore,
    claim: TaskClaim,
    row: TaskStatusRow,
    *,
    db_path: Path,
    sandbox: Path,
    submit: Submitter | None,
    teardown: Callable[[], None] | None,
    work_source: WorkSource,
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
    max_cost_usd: float,
    max_tokens: int,
    wall_clock_seconds: int,
    stream: TextIO | None,
    now: Callable[[], datetime],
) -> RunRecord:
    """Run (or resume) one task while a heartbeat holds its lease, hand the
    terminal status to ``submit``, report it to the work source, then
    release the lease.

    ``run_task_object`` opens and closes its own store, so its writes are
    committed and visible to the orchestrator's control store on the next
    query (separate SQLite connections under WAL). The control store is
    touched only by the heartbeat thread for the duration of the run, never
    concurrently with the main thread.

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
            max_cost_usd=max_cost_usd,
            max_tokens=max_tokens,
            wall_clock_seconds=wall_clock_seconds,
            invoke=invoke,
            stream=stream,
            run_id=run_id,
            source=row.source_ref or None,
        )
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
        if submit is not None:
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
        if teardown is not None:
            # Dispose the run's sandbox (e.g. docker stop/rm) after landing,
            # still under the lease. Best-effort and must not raise: a teardown
            # failure leaks a container at worst, never costs the run record or
            # the schedule.
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
        latest = heartbeat.stop()
        claims.release_claim(latest)
    return RunRecord(
        task_id=task_id,
        run_id=outcome.lifecycle.run_id,
        status=outcome.lifecycle.status,
        mode="resume" if run_id is not None else "fresh",
        worker_id=worker_id,
    )


async def _drive_or_relinquish(
    control: SqliteStore | PostgresStore,
    claims: SqliteClaimStore,
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
    "DEFAULT_LEASE_SECONDS",
    "DEFAULT_RECONCILE_SECONDS",
    "OrchestratorReport",
    "RunRecord",
    "SandboxProvider",
    "SandboxRequest",
    "SubmitRequest",
    "Submitter",
    "orchestrate",
    "reconcile_live_runs",
]

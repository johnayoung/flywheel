"""Per-task lease store for multi-worker mutual exclusion.

The orchestrator's *own* persistence, independent of flywheel's core store. A
claim is transient coordination state — a worker holds a lease while running a
task and releases it on completion — not audit history, so it lives here rather
than in the flywheel-core schema. The store owns only the ``task_claims`` table
(and its own ``orchestrator_schema_version`` sentinel) and can be pointed at
the same backend (one SQLite file, one Postgres) as the flywheel store; each
layer manages its own tables and never references the other's.

``task_id`` is a bare string with no foreign key by design: a claim is taken
before the task definition is recorded and deleted on completion.
"""

from __future__ import annotations

import json
import sqlite3
from collections.abc import Iterable, Iterator
from contextlib import contextmanager
from dataclasses import dataclass
from datetime import datetime, timedelta
from pathlib import Path
from typing import TYPE_CHECKING, Protocol, runtime_checkable

from flywheel_core.events import LANDING_PARK_KINDS
from flywheel_core.loaders import task_digest
from flywheel_core.store_protocols import SchemaMismatchError

if TYPE_CHECKING:
    from flywheel_orchestrator._sources import WorkItem

# Bump if the orchestrator's persisted schema gains a backwards-incompatible
# change. Versioned independently of flywheel-core's schema_version so the two
# can share one backend without colliding.
#
# v2 adds the additive WorkGraph persistence tables (``work_items``,
# ``work_item_dependencies``, and ``source_syncs``); the 1 -> 2 bump is a pure
# forward migration (``CREATE TABLE IF NOT EXISTS`` on open), so a pre-existing
# v1 store keeps its ``task_claims`` rows. All three tables are part of the same
# v2 DDL block, so a store opened at v2 by an earlier build that predates the
# ``source_syncs`` table gains it on the next open without a further bump.
#
# v3 adds the additive ``task_claims.conflict_keys_json`` column (spec 00049,
# D-4/D-5): each live claim records its item's conflict keys so ``acquire_claim``
# can refuse an item overlapping a different live claim. The bump is additive --
# pre-existing v1/v2 stores gain the column via ``ALTER TABLE ADD COLUMN`` (the
# column defaults to ``'[]'``, so every surviving claim row keeps its data) and
# converge their sentinel forward; no drop-and-recreate, no hard mismatch.
#
# v4 adds the additive append-only ``orchestrator_events`` ledger (spec 00054,
# D-1/D-2/D-5): one immutable row per committed claim-lease transition
# (``acquired``/``stolen``/``renewed``/``released``/``expired``), written in the
# same transaction as the ``task_claims`` mutation it describes. The bump is
# additive -- ``CREATE TABLE IF NOT EXISTS`` materializes the table on open, so a
# pre-existing v1/v2/v3 store keeps every ``task_claims``/``work_items`` row and
# simply gains an empty ledger; the sentinel converges forward, no
# drop-and-recreate, no hard mismatch.
#
# v5 adds the additive append-only WorkGraph snapshot record (spec 00055,
# D-1/D-2/D-5): a ``graph_snapshots`` header table plus a
# ``graph_snapshot_items`` table holding one row per captured work item, written
# atomically (header + all item rows in one transaction) and stamped with the
# live ``orchestrator_events`` high-water mark. The bump is additive --
# ``CREATE TABLE IF NOT EXISTS`` materializes both tables on open, so a
# pre-existing v1/v2/v3/v4 store keeps every existing row and simply gains an
# empty snapshot stream; the sentinel converges forward, no drop-and-recreate,
# no hard mismatch.
#
# v6 adds the additive append-only ``orchestrator_stop_events`` ledger (spec
# per-task-stop-records): one immutable row per pre-run dead-end that produced
# no run_id -- a dangling prerequisite, a no-op autopilot cycle, a recurring
# container prepare-preflight skip, a source-listing truncation, or a
# zero-grader item drop -- naming the stop's ``kind`` (a member of
# :data:`ORCHESTRATOR_STOP_EVENT_KINDS`), its ``subject`` (the task id or the
# source name), and its ``detail`` (the cause). The bump is additive --
# ``CREATE TABLE IF NOT EXISTS`` materializes the table on open, so a
# pre-existing v1..v5 store keeps every existing row and simply gains an empty
# stop-event ledger; the sentinel converges forward, no drop-and-recreate, no
# hard mismatch.
#
# v7 adds the additive ``orchestrator_stop_events.run_id`` column (spec
# 00069-work-redriver, queue-surface): the single human-review queue is built by
# reusing this same stop-event ledger -- a routed unit that could not be
# auto-recovered is appended with a machine-readable ``reason`` (its ``kind``)
# and, for a run-keyed stop (landing park, retry exhaustion, abort, budget
# ceiling), the ``run_id`` of the offending run, so a queue entry carries both
# task and run identity. The bump is additive -- a pre-existing v1..v6 store
# gains the column via ``ALTER TABLE ADD COLUMN`` (default ``''``, so every
# surviving stop row keeps its data) and converges its sentinel forward; no new
# silo table, no drop-and-recreate, no hard mismatch.
CURRENT_ORCH_SCHEMA_VERSION: int = 7

# The five event types the ledger records, one per committed ``task_claims``
# insert/update/delete (spec 00054 D-2). ``stolen`` is deliberately distinct from
# ``acquired`` so a reclaim over a different worker's lapsed lease is observable;
# there is no ``lost`` type because a rejected renew commits no state change.
EVENT_ACQUIRED: str = "acquired"
EVENT_STOLEN: str = "stolen"
EVENT_RENEWED: str = "renewed"
EVENT_RELEASED: str = "released"
EVENT_EXPIRED: str = "expired"

ORCHESTRATOR_EVENT_TYPES: frozenset[str] = frozenset(
    {
        EVENT_ACQUIRED,
        EVENT_STOLEN,
        EVENT_RENEWED,
        EVENT_RELEASED,
        EVENT_EXPIRED,
    }
)

# The five stop-event kinds the orchestrator records for a pre-run dead-end that
# produced no run_id (spec per-task-stop-records). A CLOSED taxonomy mirroring
# the claim-event taxonomy above: audit-witness rows that name a stop and its
# cause without changing which work is (or is not) scheduled. Recurrence is the
# signal, so the ledger never dedupes -- one row per occurrence.
#
# * ``dangling-prerequisite`` -- a task whose declared prerequisite resolves to
#   no work item this pass; the task stays out of the ready set.
# * ``no-op-cycle`` -- an autopilot refill cycle that emitted zero tasks.
# * ``no-progress`` (witness) -- a cycle in which a unit (a never-passing phase,
#   an autopilot repo that never authors) made no observable progress; one row
#   per fruitless cycle, counted by the no-progress back-off re-driver to enforce
#   its bound.
# * ``prepare-skip`` -- a sandbox prepare/preflight failure that releases the
#   claim and continues without minting a run.
# * ``source-truncation`` -- a work-source listing that filled its one-page cap,
#   so some candidate items were not read this pass.
# * ``zero-grader-drop`` -- a candidate item a source dropped as not runnable
#   because it resolved to no graders.
STOP_DANGLING_PREREQUISITE: str = "dangling-prerequisite"
STOP_NO_OP_CYCLE: str = "no-op-cycle"
STOP_NO_PROGRESS: str = "no-progress-cycle"
STOP_PREPARE_SKIP: str = "prepare-skip"
STOP_SOURCE_TRUNCATION: str = "source-truncation"
STOP_ZERO_GRADER_DROP: str = "zero-grader-drop"

ORCHESTRATOR_STOP_EVENT_KINDS: frozenset[str] = frozenset(
    {
        STOP_DANGLING_PREREQUISITE,
        STOP_NO_OP_CYCLE,
        STOP_NO_PROGRESS,
        STOP_PREPARE_SKIP,
        STOP_SOURCE_TRUNCATION,
        STOP_ZERO_GRADER_DROP,
    }
)

# The no-progress back-off re-driver's streak delimiter (spec 00069, criteria
# #9/#13). The re-driver counts consecutive ``no-progress-cycle`` witnesses on a
# unit's ledger to enforce its bound; when the unit instead makes observable
# progress the re-driver appends one ``no-progress-reset`` marker, and the streak
# is counted only from AFTER the last such marker -- so progress genuinely resets
# the counter even though the append-only ledger never deletes a witness. Like
# ``retries-escalated`` this marker is DELIBERATELY neither a pre-run stop
# (:data:`ORCHESTRATOR_STOP_EVENT_KINDS`) nor a queue reason
# (:data:`HUMAN_REVIEW_QUEUE_REASONS`) -- it is internal bookkeeping and never
# leaks into the human-review queue read.
STOP_NO_PROGRESS_RESET: str = "no-progress-reset"

# The retry-escalation re-driver's boundedness marker (spec 00069, criteria
# #5/#6; D-A). When a task exhausts its retry budget the re-driver escalates
# exactly ONCE -- a stronger-model / re-decompose re-drive -- and appends one
# ``retries-escalated`` stop row keyed to the task id as the durable witness
# that its single sanctioned escalation is spent. The re-driver counts these
# rows (``list_subject_stop_events``) to enforce the bound: with one present, a
# SECOND exhaustion routes the task to the human-review queue with
# ``retries-exhausted-after-escalation`` instead of re-escalating. The marker is
# DELIBERATELY neither a pre-run stop (:data:`ORCHESTRATOR_STOP_EVENT_KINDS`) nor
# a queue reason (:data:`HUMAN_REVIEW_QUEUE_REASONS`) -- it is an internal
# bookkeeping witness, so it never leaks into the human-review queue read.
STOP_RETRIES_ESCALATED: str = "retries-escalated"

# The stop-surface supersession marker. The ledger is append-only and never
# deletes a row, so once a unit's dead-end is actually serviced -- its phase
# archived after every task landed (the phase-exit gates are the verified
# witness), or an operator resolved a queued strand by hand -- the resolution
# is recorded the same way progress resets a no-progress streak: one appended
# marker row keyed to the subject. Readers that surface "the latest stop per
# subject" (the ``status`` stranded view) treat the marker as clearing the
# subject; a stop appended AFTER the marker is a fresh recurrence and surfaces
# again. Like ``no-progress-reset`` this marker is DELIBERATELY neither a
# pre-run stop (:data:`ORCHESTRATOR_STOP_EVENT_KINDS`) nor a queue reason
# (:data:`HUMAN_REVIEW_QUEUE_REASONS`), so it never renders as a stop and
# never enters the human-review queue read.
STOP_RESOLVED: str = "stop-resolved"

# The archive sweep's indeterminate-landing marker (spec 00077, criterion 7 /
# D-4). Landed means a receipt or ancestry (D-1); when the sweep can prove
# neither -- no ``Landed`` receipt on a DONE task's latest run AND neither its
# ``flywheel/<phase>/<task>`` branch nor any recorded head resolves for the
# ancestry probe -- the landing state is genuinely UNKNOWN, so the sweep fails
# closed (D-4): the phase stays active and one row with this stable kind is
# appended, keyed to the task id, so ``flywheel status`` surfaces the strand
# instead of the sweep silently blessing retention-destroyed evidence as
# landed. The sweep appends at most one unresolved row per subject (it checks
# the subject's latest row first), so repeated sweeps over a blocked phase do
# not flood the ledger. Like ``no-progress-reset`` / ``retries-escalated`` /
# ``stop-resolved`` this is DELIBERATELY neither a pre-run stop
# (:data:`ORCHESTRATOR_STOP_EVENT_KINDS`) nor a queue reason
# (:data:`HUMAN_REVIEW_QUEUE_REASONS`): the ``status`` stranded view surfaces it
# by subject regardless of kind, and a later :data:`STOP_RESOLVED` (the phase
# archived once the task landed) supersedes it.
STOP_INDETERMINATE_LANDING: str = "indeterminate-landing"


# The human-review queue vocabulary (spec 00069-work-redriver, queue-surface).
#
# The single visible human-review queue is NOT a new silo: it is a routed view
# over this same append-only stop-event ledger. When a routing layer (landing
# re-driver, retry escalation, prereq re-driver, no-progress back-off, human-gate
# routing) cannot auto-recover a unit, it appends one stop row whose ``kind`` is
# a member of :data:`HUMAN_REVIEW_QUEUE_REASONS` -- a stable, machine-readable
# token, never a free-text detail string -- and ``list_human_review_queue`` reads
# every such row back as a :class:`HumanReviewQueueEntry` carrying its task/run
# identity and that reason. The reason tokens are deliberately DISJOINT from the
# pre-run :data:`ORCHESTRATOR_STOP_EVENT_KINDS` above, so the two logical streams
# coexist in one table without colliding and a reader tells them apart by
# membership alone.
#
# * ``retries-exhausted-after-escalation`` -- a task that exhausted its retry
#   budget a second time, after its one sanctioned escalation (D-A; criterion #6).
# * ``prerequisite-missing`` -- a task whose declared prerequisite stayed absent
#   past the re-drive bound (criterion #8); the missing prerequisite id rides in
#   ``detail``.
# * ``no-progress`` -- a unit backed off after a bound of fruitless cycles
#   (criterion #9).
# * ``awaiting-approval`` -- a lifecycle parked on a manual gate, surfaced without
#   being transitioned (D-E; criterion #10).
# * ``abort`` / ``budget-ceiling`` -- an intentional human/budget stop, surfaced
#   and never re-dispatched (D-E; criterion #11).
#
# A parked landing (criterion #4) is routed with its park cause AS the reason --
# one of :data:`flywheel_core.events.LANDING_PARK_KINDS` -- so the queue entry's
# machine-readable reason names the ``park_kind`` directly. Those tokens are
# folded into the queue vocabulary below.
REASON_RETRIES_EXHAUSTED_AFTER_ESCALATION: str = (
    "retries-exhausted-after-escalation"
)
REASON_PREREQUISITE_MISSING: str = "prerequisite-missing"
REASON_NO_PROGRESS: str = "no-progress"
REASON_AWAITING_APPROVAL: str = "awaiting-approval"
REASON_ABORTED: str = "abort"
REASON_BUDGET_CEILING: str = "budget-ceiling"

HUMAN_REVIEW_QUEUE_REASONS: frozenset[str] = (
    frozenset(
        {
            REASON_RETRIES_EXHAUSTED_AFTER_ESCALATION,
            REASON_PREREQUISITE_MISSING,
            REASON_NO_PROGRESS,
            REASON_AWAITING_APPROVAL,
            REASON_ABORTED,
            REASON_BUDGET_CEILING,
        }
    )
    | LANDING_PARK_KINDS
)


def _require_review_reason(reason: str) -> None:
    """Guard that a human-review routing carries a stable, machine-readable
    token (a member of :data:`HUMAN_REVIEW_QUEUE_REASONS`), never a free-text
    detail string. Raises :class:`ValueError` on an unknown reason so a routing
    layer cannot smuggle prose into the ``reason`` field (edge case: the reason
    must be machine-readable, not only a human-readable detail)."""
    if reason not in HUMAN_REVIEW_QUEUE_REASONS:
        raise ValueError(
            f"unknown human-review reason {reason!r}; must be one of "
            f"{sorted(HUMAN_REVIEW_QUEUE_REASONS)}"
        )


class ClaimLostError(Exception):
    """Raised when ``renew_claim`` finds the claim is no longer the caller's.

    Either the lease lapsed and another worker stole the task, or the claim was
    released, so the version/worker no longer match. The caller must stop
    acting on the task — another worker now owns it.
    """

    def __init__(self, task_id: str) -> None:
        super().__init__(f"claim on task {task_id!r} lost")
        self.task_id = task_id


@dataclass(frozen=True, kw_only=True)
class TaskClaim:
    """A worker's lease on a task, mirroring one ``task_claims`` row.

    Immutable snapshot: ``acquire_claim`` / ``renew_claim`` return a fresh
    instance with the bumped ``version`` and extended ``lease_expires_at``.
    ``version`` and ``worker_id`` together are the optimistic-concurrency key
    for renew/release — a stale token (wrong version, or a different worker
    stole the task) is rejected.
    """

    task_id: str
    worker_id: str
    claimed_at: datetime
    lease_expires_at: datetime
    version: int


@dataclass(frozen=True, kw_only=True)
class WorkItemRecord:
    """A persisted ``work_items`` row read back from the orchestrator store.

    Immutable snapshot of one observed work item's catalog entry.
    ``first_seen_at`` is stamped once on the first observation and never
    moves; ``last_seen_at`` advances on every observation; ``disappeared_at``
    is set when a *successful* sync no longer observes the item and is cleared
    (back to ``None``) the moment it is observed again.

    ``priority`` / ``required_capabilities_json`` / ``conflict_keys_json``
    carry the item's scheduling metadata (spec 00049): an integer priority
    and two canonical (sorted) JSON-array string sets, written from the
    matching ``WorkItem`` fields. ``metadata_json`` remains a forward-compat
    column carried at its default (``'{}'``); nothing populates it yet. The
    ``*_json`` fields are canonical JSON strings on both backends.
    """

    task_id: str
    source_kind: str | None
    source_ref: str | None
    source_url: str | None
    source_version: str | None
    task_content_hash: str | None
    priority: int
    required_capabilities_json: str
    conflict_keys_json: str
    first_seen_at: datetime
    last_seen_at: datetime
    disappeared_at: datetime | None
    metadata_json: str


@dataclass(frozen=True, kw_only=True)
class SourceSyncRecord:
    """A persisted ``source_syncs`` row read back from the orchestrator store.

    Immutable snapshot of one sync run over a :class:`WorkSource`. A row is
    written at the start of a pass (``status='running'``, ``finished_at`` NULL)
    and finished when the pass settles: ``status='ok'`` with ``observed_count``
    equal to the number of items the pass observed, or ``status='error'`` with
    a non-empty ``error`` when ``list_work()`` failed. ``source_name`` is the
    source's locus (D-4): the ``tasks_dir`` path for a directory source, the
    ``owner/repo`` for a GitHub source. ``metadata_json`` is a canonical JSON
    string carried at its default (``'{}'``); nothing populates it this spec.
    """

    id: int
    source_kind: str
    source_name: str
    started_at: datetime
    finished_at: datetime | None
    status: str
    observed_count: int
    error: str | None
    metadata_json: str


@dataclass(frozen=True, kw_only=True)
class OrchestratorEventRecord:
    """One immutable ``orchestrator_events`` row -- a committed claim-lease
    transition (spec 00054).

    Append-only: a row is written in the same store transaction as the
    ``task_claims`` insert/update/delete it describes, never updated or deleted.
    ``id`` is the monotonic insertion id (backend-assigned), so the global
    stream and a per-task timeline both read back in ``id`` order.
    ``event_type`` is one of :data:`ORCHESTRATOR_EVENT_TYPES`; ``worker_id`` is
    the worker the event pertains to (for ``expired`` it is the reaped holder);
    ``version`` is the claim version after the transition; ``lease_expires_at``
    is the lease the transition left in place (or, for a deletion, the lease of
    the row removed); ``occurred_at`` is the injected ``now`` -- never a wall
    clock, so the ledger is deterministic and testable.
    """

    id: int
    task_id: str
    worker_id: str
    event_type: str
    version: int
    lease_expires_at: datetime
    occurred_at: datetime


@dataclass(frozen=True, kw_only=True)
class OrchestratorStopEventRecord:
    """One immutable ``orchestrator_stop_events`` row -- a pre-run dead-end
    that produced no run_id (spec per-task-stop-records).

    Append-only, never updated or deleted, and never deduped: recurrence is the
    signal, so a stop observed on N passes is N rows. ``id`` is the monotonic,
    backend-assigned insertion id, so both the global stream and a per-subject
    timeline read back in ``id`` order. ``kind`` is one member of
    :data:`ORCHESTRATOR_STOP_EVENT_KINDS`; ``subject`` names what the stop is
    about (a task id for ``dangling-prerequisite`` / ``prepare-skip``, a source
    name for ``no-op-cycle`` / ``source-truncation`` / ``zero-grader-drop``);
    ``detail`` carries the cause (the missing prerequisite id, the no-op reason
    plus observed/target depth, the prepare failure text, the truncation or
    zero-grader specifics). ``occurred_at`` is the injected timestamp -- never a
    wall clock, so the ledger is deterministic and testable.

    ``run_id`` (schema v7) is the offending run for a routed human-review entry
    (spec 00069): a landing park / retry exhaustion / abort / budget stop carries
    the run whose lifecycle produced it. Pre-run dead-ends have no run and leave
    it ``''``.
    """

    id: int
    kind: str
    subject: str
    detail: str
    occurred_at: datetime
    run_id: str = ""


@dataclass(frozen=True, kw_only=True)
class HumanReviewQueueEntry:
    """One entry in the single visible human-review queue (spec 00069).

    A routed view over the append-only stop-event ledger: every unit that a
    routing layer could not auto-recover surfaces here exactly once per routing,
    carrying its task/run identity and a machine-readable ``reason``. The queue
    is NOT a new persistence silo -- an entry is one ``orchestrator_stop_events``
    row whose ``kind`` is a member of :data:`HUMAN_REVIEW_QUEUE_REASONS`; the
    read simply projects those rows.

    ``task_id`` is the unit the human must look at; ``run_id`` is the offending
    run when the stop is run-keyed (a landing park, a retry exhaustion, an abort
    or budget breach) and ``''`` for a task-keyed stop with no run (a missing
    prerequisite, a no-progress back-off). ``reason`` is the stable token
    (never a free-text string); ``detail`` is the human-readable cause;
    ``occurred_at`` is the injected routing timestamp; ``id`` is the underlying
    ledger row id, so the queue reads back in insertion order.
    """

    id: int
    task_id: str
    run_id: str
    reason: str
    detail: str
    occurred_at: datetime


@runtime_checkable
class StopEventStore(Protocol):
    """The narrow sink a stop-event producer records through.

    Every :class:`ClaimStore` backend also satisfies this protocol, so the
    work-source stop sink, the orchestrator loop (dangling prerequisite,
    prepare skip), and the autopilot pass (no-op cycle) all record through the
    same append-only ledger without depending on a concrete backend type.
    """

    def record_stop_event(
        self,
        *,
        kind: str,
        subject: str,
        detail: str,
        occurred_at: datetime,
    ) -> None: ...


@dataclass(frozen=True, kw_only=True)
class GraphSnapshotItem:
    """One captured work item inside a WorkGraph snapshot (spec 00055, D-1).

    The full materialized state of a single item as a scheduling pass saw it:
    its source provenance (``source_kind``/``source_ref``/``source_url``/
    ``source_version``), scheduling metadata (``priority``,
    ``required_capabilities``, ``conflict_keys``), lifecycle ``state``, a
    ``ready`` flag (was it in the pass's ready set), the ``claim_holder`` worker
    id (or ``None`` when unheld), and the ``resolved_prerequisites`` ids.

    Used both as the input the loop assembles and passes to
    :meth:`ClaimStore.record_graph_snapshot` and as the read-back row, so a
    recorded item round-trips to an equal value. The three set fields are stored
    canonically (sorted JSON) on the durable backends -- the same
    encode/decode parity ``conflict_keys_json`` already uses -- so they read
    back as equal sets regardless of insertion order.
    """

    task_id: str
    source_kind: str | None = None
    source_ref: str | None = None
    source_url: str | None = None
    source_version: str | None = None
    priority: int = 0
    required_capabilities: frozenset[str] = frozenset()
    conflict_keys: frozenset[str] = frozenset()
    state: str
    ready: bool
    claim_holder: str | None = None
    resolved_prerequisites: frozenset[str] = frozenset()


@dataclass(frozen=True, kw_only=True)
class GraphSnapshotRecord:
    """One ``graph_snapshots`` header row -- a recorded WorkGraph snapshot.

    Append-only (spec 00055, D-3): a header and all its item rows commit in one
    store transaction, never updated or deleted. ``id`` is the monotonic,
    backend-assigned snapshot id (so the stream reads back in insertion order).
    ``captured_at`` is the injected capture timestamp -- never a wall clock, so
    the record is deterministic and testable. ``item_count`` is the number of
    captured items (equal to the number of item rows read back, criterion #3).
    ``last_event_id`` is the maximum ``orchestrator_events`` id at the instant
    of the write (0 on an empty ledger), computed by the store inside the
    snapshot's own transaction (D-2), so the cursor cannot drift from the
    ledger it points at.
    """

    id: int
    captured_at: datetime
    item_count: int
    last_event_id: int


@runtime_checkable
class ClaimStore(Protocol):
    """Per-task lease contract for multi-worker mutual exclusion.

    At most one live claim exists per ``task_id``. A worker acquires it before
    running the task and releases it on completion; the lease's expiry lets
    another worker reclaim a task whose worker crashed.

    * ``acquire_claim`` returns a :class:`TaskClaim` when the task is free, the
      existing lease has expired (the new claim *steals* it), or the caller
      already holds it (idempotent re-acquire). It returns ``None`` when a
      *live* lease is held by a different worker, or when the item's
      ``conflict_keys`` overlap those of a *different* live claim (so two
      conflicting items never hold concurrent claims; spec 00049 D-3/D-4). The
      refusal clears once the conflicting claim is released or its lease lapses.
      ``conflict_keys`` defaults to empty, in which case acquisition is never
      refused on a conflict basis. The check-and-write is atomic.
    * ``renew_claim`` extends the lease, bumping ``version``; it raises
      :class:`ClaimLostError` when the caller's token no longer matches.
    * ``release_claim`` drops the claim when the token still matches; a no-op
      if it was already stolen or released.
    * ``load_claim`` returns the current claim for a task, or ``None``.
    * ``list_claims`` enumerates every currently-held claim — one
      :class:`TaskClaim` per held row — so an operator surface can see who
      holds what without knowing task ids up front. Released claims are
      absent; expiry is not filtered (an expired-but-not-yet-stolen lease
      still appears, consistent with ``load_claim``).
    * ``sweep_expired_claims`` batch-releases every claim whose lease lapsed
      at the injected ``now`` (``lease_expires_at <= now``) in a single pass,
      returning those task ids to the acquirable pool and dropping them from
      ``list_claims`` — reaping all of one dead worker's lapsed claims at
      once, not one task at a time. A claim still valid at ``now`` stays held
      and owned. Returns the released task ids. ``acquire_claim`` /
      ``renew_claim`` / ``release_claim`` semantics for non-lapsed claims are
      unchanged (the sweep only drops already-lapsed rows).

    ``now`` is injected (not read from a clock) so lease expiry is
    deterministic and testable.
    """

    def acquire_claim(
        self,
        task_id: str,
        worker_id: str,
        *,
        now: datetime,
        lease_seconds: float,
        conflict_keys: frozenset[str] = frozenset(),
    ) -> TaskClaim | None: ...

    def renew_claim(
        self,
        claim: TaskClaim,
        *,
        now: datetime,
        lease_seconds: float,
    ) -> TaskClaim: ...

    def release_claim(self, claim: TaskClaim) -> None: ...

    def load_claim(self, task_id: str) -> TaskClaim | None: ...

    def list_claims(self) -> list[TaskClaim]: ...

    def sweep_expired_claims(self, *, now: datetime) -> list[str]: ...


def _iso(ts: datetime) -> str:
    return ts.isoformat()


def encode_str_set(values: frozenset[str]) -> str:
    """Canonical JSON-array encoding of a string set (order-insensitive).

    Sorted so the persisted ``*_json`` value is deterministic regardless of
    set iteration order; the read-back decodes back to the same set. Shared by
    both backends so SQLite TEXT and Postgres JSONB store identical content.
    """
    return json.dumps(sorted(values))


def decode_str_set(value: str) -> frozenset[str]:
    """Inverse of :func:`encode_str_set` -- a canonical JSON array to a set.

    Shared by both backends so the conflict-key overlap check reads identical
    content whether the column was stored as SQLite TEXT or Postgres JSONB.
    """
    return frozenset(json.loads(value))


def _parse_iso(value: str) -> datetime:
    return datetime.fromisoformat(value)


def _row_to_work_item_record(row: sqlite3.Row) -> WorkItemRecord:
    disappeared = row["disappeared_at"]
    return WorkItemRecord(
        task_id=row["task_id"],
        source_kind=row["source_kind"],
        source_ref=row["source_ref"],
        source_url=row["source_url"],
        source_version=row["source_version"],
        task_content_hash=row["task_content_hash"],
        priority=int(row["priority"]),
        required_capabilities_json=row["required_capabilities_json"],
        conflict_keys_json=row["conflict_keys_json"],
        first_seen_at=_parse_iso(row["first_seen_at"]),
        last_seen_at=_parse_iso(row["last_seen_at"]),
        disappeared_at=(
            _parse_iso(disappeared) if disappeared is not None else None
        ),
        metadata_json=row["metadata_json"],
    )


def _row_to_source_sync_record(row: sqlite3.Row) -> SourceSyncRecord:
    finished = row["finished_at"]
    return SourceSyncRecord(
        id=int(row["id"]),
        source_kind=row["source_kind"],
        source_name=row["source_name"],
        started_at=_parse_iso(row["started_at"]),
        finished_at=_parse_iso(finished) if finished is not None else None,
        status=row["status"],
        observed_count=int(row["observed_count"]),
        error=row["error"],
        metadata_json=row["metadata_json"],
    )


def _row_to_orchestrator_event_record(
    row: sqlite3.Row,
) -> OrchestratorEventRecord:
    return OrchestratorEventRecord(
        id=int(row["id"]),
        task_id=row["task_id"],
        worker_id=row["worker_id"],
        event_type=row["event_type"],
        version=int(row["version"]),
        lease_expires_at=_parse_iso(row["lease_expires_at"]),
        occurred_at=_parse_iso(row["occurred_at"]),
    )


def _row_to_orchestrator_stop_event_record(
    row: sqlite3.Row,
) -> OrchestratorStopEventRecord:
    return OrchestratorStopEventRecord(
        id=int(row["id"]),
        kind=row["kind"],
        subject=row["subject"],
        detail=row["detail"],
        occurred_at=_parse_iso(row["occurred_at"]),
        run_id=row["run_id"],
    )


def _row_to_human_review_entry(row: sqlite3.Row) -> HumanReviewQueueEntry:
    return HumanReviewQueueEntry(
        id=int(row["id"]),
        task_id=row["subject"],
        run_id=row["run_id"],
        reason=row["kind"],
        detail=row["detail"],
        occurred_at=_parse_iso(row["occurred_at"]),
    )


def _row_to_graph_snapshot_record(row: sqlite3.Row) -> GraphSnapshotRecord:
    return GraphSnapshotRecord(
        id=int(row["id"]),
        captured_at=_parse_iso(row["captured_at"]),
        item_count=int(row["item_count"]),
        last_event_id=int(row["last_event_id"]),
    )


def _row_to_graph_snapshot_item(row: sqlite3.Row) -> GraphSnapshotItem:
    holder = row["claim_holder"]
    return GraphSnapshotItem(
        task_id=row["task_id"],
        source_kind=row["source_kind"],
        source_ref=row["source_ref"],
        source_url=row["source_url"],
        source_version=row["source_version"],
        priority=int(row["priority"]),
        required_capabilities=decode_str_set(
            row["required_capabilities_json"]
        ),
        conflict_keys=decode_str_set(row["conflict_keys_json"]),
        state=row["state"],
        ready=bool(row["ready"]),
        claim_holder=holder if holder is not None else None,
        resolved_prerequisites=decode_str_set(
            row["resolved_prerequisites_json"]
        ),
    )


def _normalize_graph_snapshot_item(
    item: GraphSnapshotItem,
) -> GraphSnapshotItem:
    """Round-trip an item's set fields through the durable encode/decode.

    The in-memory store mirrors the durable backends bit-for-bit by storing
    each item exactly as SQLite/Postgres would read it back -- canonical sets,
    a coerced bool ``ready``. Keeps the in-memory substrate from masking a
    parity bug.
    """
    return GraphSnapshotItem(
        task_id=item.task_id,
        source_kind=item.source_kind,
        source_ref=item.source_ref,
        source_url=item.source_url,
        source_version=item.source_version,
        priority=item.priority,
        required_capabilities=decode_str_set(
            encode_str_set(item.required_capabilities)
        ),
        conflict_keys=decode_str_set(encode_str_set(item.conflict_keys)),
        state=item.state,
        ready=bool(item.ready),
        claim_holder=item.claim_holder,
        resolved_prerequisites=decode_str_set(
            encode_str_set(item.resolved_prerequisites)
        ),
    )


class InMemoryClaimStore:
    """In-memory :class:`ClaimStore`. Not durable; the test substrate.

    Records the same append-only ``orchestrator_events`` ledger as the durable
    backends (spec 00054): one immutable event per committed claim-lease
    transition, readable as a global stream and a per-task timeline.
    """

    def __init__(self) -> None:
        self._claims: dict[str, TaskClaim] = {}
        self._conflict_keys: dict[str, frozenset[str]] = {}
        self._events: list[OrchestratorEventRecord] = []
        self._stop_events: list[OrchestratorStopEventRecord] = []
        self._snapshots: list[GraphSnapshotRecord] = []
        self._snapshot_items: dict[int, list[GraphSnapshotItem]] = {}

    def _append_event(
        self,
        *,
        task_id: str,
        worker_id: str,
        event_type: str,
        version: int,
        lease_expires_at: datetime,
        occurred_at: datetime,
    ) -> None:
        # Append-only: a fresh monotonic id (1-based) per committed transition,
        # never deduped or overwritten. Stored in insertion order, so both read
        # accessors return events in id order without sorting.
        self._events.append(
            OrchestratorEventRecord(
                id=len(self._events) + 1,
                task_id=task_id,
                worker_id=worker_id,
                event_type=event_type,
                version=version,
                lease_expires_at=lease_expires_at,
                occurred_at=occurred_at,
            )
        )

    def _append_stop_event(
        self,
        *,
        kind: str,
        subject: str,
        detail: str,
        occurred_at: datetime,
        run_id: str = "",
    ) -> None:
        # Append-only, never deduped: a fresh monotonic id (1-based) per
        # recorded stop, stored in insertion order so both read accessors
        # return in id order without sorting.
        self._stop_events.append(
            OrchestratorStopEventRecord(
                id=len(self._stop_events) + 1,
                kind=kind,
                subject=subject,
                detail=detail,
                occurred_at=occurred_at,
                run_id=run_id,
            )
        )

    def acquire_claim(
        self,
        task_id: str,
        worker_id: str,
        *,
        now: datetime,
        lease_seconds: float,
        conflict_keys: frozenset[str] = frozenset(),
    ) -> TaskClaim | None:
        existing = self._claims.get(task_id)
        free = (
            existing is None
            or existing.lease_expires_at <= now
            or existing.worker_id == worker_id
        )
        if not free:
            return None
        incoming = frozenset(conflict_keys)
        if incoming and self._has_conflicting_live_claim(
            task_id, incoming, now=now
        ):
            return None
        version = existing.version + 1 if existing is not None else 1
        # A fresh insert or a same-worker re-acquire is ``acquired``; taking a
        # *different* worker's lapsed lease is ``stolen`` (D-2) -- the "free"
        # guard above means a different-worker existing row can only be lapsed.
        if existing is not None and existing.worker_id != worker_id:
            event_type = EVENT_STOLEN
        else:
            event_type = EVENT_ACQUIRED
        claim = TaskClaim(
            task_id=task_id,
            worker_id=worker_id,
            claimed_at=now,
            lease_expires_at=now + timedelta(seconds=lease_seconds),
            version=version,
        )
        self._claims[task_id] = claim
        self._conflict_keys[task_id] = incoming
        self._append_event(
            task_id=task_id,
            worker_id=worker_id,
            event_type=event_type,
            version=version,
            lease_expires_at=claim.lease_expires_at,
            occurred_at=now,
        )
        return claim

    def _has_conflicting_live_claim(
        self,
        task_id: str,
        incoming: frozenset[str],
        *,
        now: datetime,
    ) -> bool:
        # A *different* live claim (another task whose lease has not lapsed)
        # whose conflict keys overlap the incoming set blocks the acquire.
        for other_id, other_claim in self._claims.items():
            if other_id == task_id:
                continue
            if other_claim.lease_expires_at <= now:
                continue
            if self._conflict_keys.get(other_id, frozenset()) & incoming:
                return True
        return False

    def renew_claim(
        self,
        claim: TaskClaim,
        *,
        now: datetime,
        lease_seconds: float,
    ) -> TaskClaim:
        existing = self._claims.get(claim.task_id)
        if (
            existing is None
            or existing.version != claim.version
            or existing.worker_id != claim.worker_id
        ):
            raise ClaimLostError(claim.task_id)
        renewed = TaskClaim(
            task_id=claim.task_id,
            worker_id=claim.worker_id,
            claimed_at=existing.claimed_at,
            lease_expires_at=now + timedelta(seconds=lease_seconds),
            version=existing.version + 1,
        )
        self._claims[claim.task_id] = renewed
        self._append_event(
            task_id=renewed.task_id,
            worker_id=renewed.worker_id,
            event_type=EVENT_RENEWED,
            version=renewed.version,
            lease_expires_at=renewed.lease_expires_at,
            occurred_at=now,
        )
        return renewed

    def release_claim(
        self, claim: TaskClaim, *, now: datetime | None = None
    ) -> None:
        existing = self._claims.get(claim.task_id)
        if (
            existing is not None
            and existing.version == claim.version
            and existing.worker_id == claim.worker_id
        ):
            del self._claims[claim.task_id]
            self._conflict_keys.pop(claim.task_id, None)
            # Only an actual row deletion records ``released`` (D-1/D-2); a
            # stale/no-op release falls through the guard and writes nothing.
            self._append_event(
                task_id=claim.task_id,
                worker_id=claim.worker_id,
                event_type=EVENT_RELEASED,
                version=claim.version,
                lease_expires_at=claim.lease_expires_at,
                occurred_at=now if now is not None else claim.lease_expires_at,
            )

    def load_claim(self, task_id: str) -> TaskClaim | None:
        return self._claims.get(task_id)

    def list_claims(self) -> list[TaskClaim]:
        # Live held rows only; released claims were removed from the dict.
        # Expiry is not filtered, matching load_claim. TaskClaim is frozen,
        # so the stored instances are safe to return directly.
        return list(self._claims.values())

    def sweep_expired_claims(self, *, now: datetime) -> list[str]:
        # Drop every lapsed claim in one pass: those task ids leave the held
        # set (so they vanish from list_claims and become acquirable), while
        # claims still valid at now are untouched. Materialize the doomed ids
        # before mutating so the iteration is not over a changing dict.
        doomed = [
            claim
            for claim in self._claims.values()
            if claim.lease_expires_at <= now
        ]
        for claim in doomed:
            del self._claims[claim.task_id]
            self._conflict_keys.pop(claim.task_id, None)
            # One ``expired`` event per reaped claim, carrying the reaped
            # holder's worker id (D-2); a still-valid claim is left untouched
            # and writes nothing.
            self._append_event(
                task_id=claim.task_id,
                worker_id=claim.worker_id,
                event_type=EVENT_EXPIRED,
                version=claim.version,
                lease_expires_at=claim.lease_expires_at,
                occurred_at=now,
            )
        return [claim.task_id for claim in doomed]

    def list_events(self) -> list[OrchestratorEventRecord]:
        # Global stream: every recorded event in id (insertion) order.
        return list(self._events)

    def list_task_events(self, task_id: str) -> list[OrchestratorEventRecord]:
        # Per-task timeline: one task's events in id order.
        return [event for event in self._events if event.task_id == task_id]

    def record_stop_event(
        self,
        *,
        kind: str,
        subject: str,
        detail: str,
        occurred_at: datetime,
    ) -> None:
        # Audit-witness only: append one row naming the stop and its cause.
        # Never dedupes -- recurrence is the signal.
        self._append_stop_event(
            kind=kind, subject=subject, detail=detail, occurred_at=occurred_at
        )

    def record_prepare_skip(
        self,
        claim: TaskClaim,
        *,
        detail: str,
        now: datetime,
    ) -> None:
        # A sandbox prepare/preflight failure: release the claim AND record the
        # stop event together (D-3). The release records ``released`` only when
        # the token still owns a row (mirroring release_claim); the stop event
        # is recorded unconditionally, since the dead-end happened regardless.
        existing = self._claims.get(claim.task_id)
        if (
            existing is not None
            and existing.version == claim.version
            and existing.worker_id == claim.worker_id
        ):
            del self._claims[claim.task_id]
            self._conflict_keys.pop(claim.task_id, None)
            self._append_event(
                task_id=claim.task_id,
                worker_id=claim.worker_id,
                event_type=EVENT_RELEASED,
                version=claim.version,
                lease_expires_at=claim.lease_expires_at,
                occurred_at=now,
            )
        self._append_stop_event(
            kind=STOP_PREPARE_SKIP,
            subject=claim.task_id,
            detail=detail,
            occurred_at=now,
        )

    def list_stop_events(self) -> list[OrchestratorStopEventRecord]:
        # Global stream: every recorded stop in id (insertion) order.
        return list(self._stop_events)

    def list_subject_stop_events(
        self, subject: str
    ) -> list[OrchestratorStopEventRecord]:
        # Per-subject timeline: one subject's stops in id order.
        return [e for e in self._stop_events if e.subject == subject]

    def record_human_review(
        self,
        *,
        reason: str,
        task_id: str,
        occurred_at: datetime,
        run_id: str = "",
        detail: str = "",
    ) -> None:
        # Route one unit into the single human-review queue by appending a stop
        # row whose ``kind`` is the machine-readable ``reason``. Reusing the
        # ledger keeps the queue on the existing surface (no new silo); the
        # reason MUST be a stable token from HUMAN_REVIEW_QUEUE_REASONS, never a
        # free-text detail string. Append-only, never deduped.
        _require_review_reason(reason)
        self._append_stop_event(
            kind=reason,
            subject=task_id,
            detail=detail,
            occurred_at=occurred_at,
            run_id=run_id,
        )

    def list_human_review_queue(self) -> list[HumanReviewQueueEntry]:
        # The single queue read: every routed unit across all kinds, in id
        # (insertion) order. An empty queue returns an empty list, not an error.
        # Pre-run stop rows (dangling-prereq, no-op, prepare-skip, ...) are NOT
        # queue entries -- their kinds are disjoint from the review vocabulary.
        return [
            HumanReviewQueueEntry(
                id=e.id,
                task_id=e.subject,
                run_id=e.run_id,
                reason=e.kind,
                detail=e.detail,
                occurred_at=e.occurred_at,
            )
            for e in self._stop_events
            if e.kind in HUMAN_REVIEW_QUEUE_REASONS
        ]

    def record_graph_snapshot(
        self,
        items: Iterable[GraphSnapshotItem],
        *,
        captured_at: datetime,
    ) -> GraphSnapshotRecord:
        # Append-only (D-3): a fresh monotonic id (1-based) per recorded
        # snapshot, never overwritten. The cursor is store-computed from the
        # in-memory ledger -- the max event id, 0 when empty (D-2) -- mirroring
        # the durable backends' MAX(id) read inside the write transaction.
        materialized = list(items)
        last_event_id = self._events[-1].id if self._events else 0
        snapshot_id = len(self._snapshots) + 1
        record = GraphSnapshotRecord(
            id=snapshot_id,
            captured_at=captured_at,
            item_count=len(materialized),
            last_event_id=last_event_id,
        )
        self._snapshots.append(record)
        # Normalize each item through the same encode/decode the durable
        # backends use (so set fields read back identically) and store in
        # task_id order to mirror the SQLite ``ORDER BY task_id`` read-back.
        self._snapshot_items[snapshot_id] = sorted(
            (_normalize_graph_snapshot_item(item) for item in materialized),
            key=lambda item: item.task_id,
        )
        return record

    def list_graph_snapshots(self) -> list[GraphSnapshotRecord]:
        # Snapshot stream: every recorded header in id (insertion) order.
        return list(self._snapshots)

    def list_graph_snapshot_items(
        self, snapshot_id: int
    ) -> list[GraphSnapshotItem]:
        # One snapshot's item rows, in task_id order. Unknown id -> empty.
        return list(self._snapshot_items.get(snapshot_id, []))

    def latest_graph_snapshot(self) -> GraphSnapshotRecord | None:
        # Most recently recorded snapshot; None on an empty store.
        return self._snapshots[-1] if self._snapshots else None

    def close(self) -> None:  # parity with the durable stores
        pass


_SCHEMA_SQL = """
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS task_claims (
  task_id            TEXT PRIMARY KEY,
  worker_id          TEXT NOT NULL,
  claimed_at         DATETIME NOT NULL,
  lease_expires_at   DATETIME NOT NULL,
  version            INTEGER NOT NULL,
  conflict_keys_json TEXT NOT NULL DEFAULT '[]'
);

CREATE TABLE IF NOT EXISTS orchestrator_schema_version (
  id      INTEGER PRIMARY KEY CHECK (id = 1),
  version INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS work_items (
  task_id                    TEXT PRIMARY KEY,
  source_kind                TEXT,
  source_ref                 TEXT,
  source_url                 TEXT,
  source_version             TEXT,
  task_content_hash          TEXT,
  priority                   INTEGER NOT NULL DEFAULT 0,
  required_capabilities_json TEXT NOT NULL DEFAULT '[]',
  conflict_keys_json         TEXT NOT NULL DEFAULT '[]',
  first_seen_at              TEXT NOT NULL,
  last_seen_at               TEXT NOT NULL,
  disappeared_at             TEXT,
  metadata_json              TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS work_item_dependencies (
  task_id              TEXT NOT NULL,
  prerequisite_task_id TEXT NOT NULL,
  created_at           TEXT NOT NULL,
  PRIMARY KEY (task_id, prerequisite_task_id)
);

CREATE INDEX IF NOT EXISTS idx_work_item_dependencies_prerequisite
  ON work_item_dependencies (prerequisite_task_id);

CREATE TABLE IF NOT EXISTS source_syncs (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  source_kind    TEXT NOT NULL,
  source_name    TEXT NOT NULL,
  started_at     TEXT NOT NULL,
  finished_at    TEXT,
  status         TEXT NOT NULL,
  observed_count INTEGER NOT NULL DEFAULT 0,
  error          TEXT,
  metadata_json  TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS orchestrator_events (
  id               INTEGER PRIMARY KEY AUTOINCREMENT,
  task_id          TEXT NOT NULL,
  worker_id        TEXT NOT NULL,
  event_type       TEXT NOT NULL,
  version          INTEGER NOT NULL,
  lease_expires_at TEXT NOT NULL,
  occurred_at      TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_orchestrator_events_task
  ON orchestrator_events (task_id, id);

CREATE TABLE IF NOT EXISTS orchestrator_stop_events (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  kind        TEXT NOT NULL,
  subject     TEXT NOT NULL,
  detail      TEXT NOT NULL,
  occurred_at TEXT NOT NULL,
  run_id      TEXT NOT NULL DEFAULT ''
);

CREATE INDEX IF NOT EXISTS idx_orchestrator_stop_events_subject
  ON orchestrator_stop_events (subject, id);

CREATE TABLE IF NOT EXISTS graph_snapshots (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  captured_at   TEXT NOT NULL,
  item_count    INTEGER NOT NULL,
  last_event_id INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS graph_snapshot_items (
  snapshot_id                 INTEGER NOT NULL,
  task_id                     TEXT NOT NULL,
  source_kind                 TEXT,
  source_ref                  TEXT,
  source_url                  TEXT,
  source_version              TEXT,
  priority                    INTEGER NOT NULL DEFAULT 0,
  required_capabilities_json  TEXT NOT NULL DEFAULT '[]',
  conflict_keys_json          TEXT NOT NULL DEFAULT '[]',
  state                       TEXT NOT NULL,
  ready                       INTEGER NOT NULL,
  claim_holder                TEXT,
  resolved_prerequisites_json TEXT NOT NULL DEFAULT '[]',
  PRIMARY KEY (snapshot_id, task_id)
);

CREATE INDEX IF NOT EXISTS idx_graph_snapshot_items_snapshot
  ON graph_snapshot_items (snapshot_id);
"""


class OrchestratorSchemaError(SchemaMismatchError):
    """Raised when the orchestrator's on-disk schema version mismatches.

    Subclasses the shared :class:`~flywheel_core.store_protocols.SchemaMismatchError`
    so the fault classifier buckets it PERMANENT alongside the core store's
    ``StoreSchemaError``.
    """

    def __init__(self, *, observed: int | None, expected: int) -> None:
        super().__init__(
            "orchestrator store must be re-created: "
            f"observed orchestrator_schema_version={observed!r}, "
            f"expected {expected}"
        )
        self.observed = observed
        self.expected = expected


class SqliteClaimStore:
    """SQLite :class:`ClaimStore`. Owns ``task_claims`` and its own schema
    sentinel; safe to point at the same file as ``flywheel.SqliteStore`` (each
    touches only its own tables)."""

    def __init__(self, path: str | Path = ":memory:") -> None:
        self._connection = sqlite3.connect(
            str(path), check_same_thread=False, isolation_level=None
        )
        self._connection.row_factory = sqlite3.Row
        self._bootstrap()

    def _bootstrap(self) -> None:
        conn = self._connection
        conn.executescript(_SCHEMA_SQL)
        conn.execute("PRAGMA foreign_keys = ON;")
        conn.execute("PRAGMA busy_timeout = 5000;")
        # Additive v3 migration: a pre-existing v1/v2 store has a task_claims
        # table that predates the conflict-keys column. CREATE TABLE IF NOT
        # EXISTS leaves that older table untouched, so add the column in place
        # (defaulting to '[]', preserving every existing claim row). New stores
        # already have it from the CREATE TABLE above, so the ALTER is skipped.
        columns = {
            str(r["name"])
            for r in conn.execute("PRAGMA table_info(task_claims)").fetchall()
        }
        if "conflict_keys_json" not in columns:
            conn.execute(
                "ALTER TABLE task_claims "
                "ADD COLUMN conflict_keys_json TEXT NOT NULL DEFAULT '[]'"
            )
        # Additive v7 migration: a pre-existing v6 store has an
        # orchestrator_stop_events table predating the run_id column. CREATE
        # TABLE IF NOT EXISTS leaves that older table untouched, so add the
        # column in place (defaulting to '', preserving every existing stop
        # row). New stores already have it from the CREATE TABLE above.
        stop_columns = {
            str(r["name"])
            for r in conn.execute(
                "PRAGMA table_info(orchestrator_stop_events)"
            ).fetchall()
        }
        if "run_id" not in stop_columns:
            conn.execute(
                "ALTER TABLE orchestrator_stop_events "
                "ADD COLUMN run_id TEXT NOT NULL DEFAULT ''"
            )
        conn.execute(
            "INSERT OR IGNORE INTO orchestrator_schema_version (id, version) "
            "VALUES (1, ?)",
            (CURRENT_ORCH_SCHEMA_VERSION,),
        )
        row = conn.execute(
            "SELECT version FROM orchestrator_schema_version WHERE id = 1"
        ).fetchone()
        observed = int(row["version"]) if row is not None else None
        # Additive forward migration v1..v6 -> v7: the WorkGraph tables (v2),
        # the conflict-keys column (v3), the orchestrator_events ledger (v4),
        # the graph_snapshots/graph_snapshot_items tables (v5), the
        # orchestrator_stop_events ledger (v6), and its run_id column (v7) were
        # all materialized above (CREATE TABLE IF NOT EXISTS plus the additive
        # ALTERs), so a pre-existing store keeps its
        # task_claims/work_items/source_syncs/orchestrator_events/snapshot/stop
        # rows intact and simply gains the run_id column. Converge the sentinel
        # forward rather than refusing the store; a newer-than-current version
        # still trips the mismatch guard below.
        if observed is not None and observed < CURRENT_ORCH_SCHEMA_VERSION:
            conn.execute(
                "UPDATE orchestrator_schema_version SET version = ? "
                "WHERE id = 1",
                (CURRENT_ORCH_SCHEMA_VERSION,),
            )
            observed = CURRENT_ORCH_SCHEMA_VERSION
        if observed != CURRENT_ORCH_SCHEMA_VERSION:
            raise OrchestratorSchemaError(
                observed=observed, expected=CURRENT_ORCH_SCHEMA_VERSION
            )

    @contextmanager
    def _transaction(self) -> Iterator[None]:
        self._connection.execute("BEGIN IMMEDIATE")
        try:
            yield
        except BaseException:
            self._connection.execute("ROLLBACK")
            raise
        else:
            self._connection.execute("COMMIT")

    def _append_event(
        self,
        *,
        task_id: str,
        worker_id: str,
        event_type: str,
        version: int,
        lease_expires_at: datetime,
        occurred_at: datetime,
    ) -> None:
        # Append one ledger row. MUST be called inside an open ``_transaction``
        # so the event commits atomically with the ``task_claims`` mutation it
        # describes (D-1): a rolled-back transition takes its event with it.
        self._connection.execute(
            "INSERT INTO orchestrator_events (task_id, worker_id, event_type, "
            "version, lease_expires_at, occurred_at) VALUES (?, ?, ?, ?, ?, ?)",
            (
                task_id,
                worker_id,
                event_type,
                version,
                _iso(lease_expires_at),
                _iso(occurred_at),
            ),
        )

    def acquire_claim(
        self,
        task_id: str,
        worker_id: str,
        *,
        now: datetime,
        lease_seconds: float,
        conflict_keys: frozenset[str] = frozenset(),
    ) -> TaskClaim | None:
        lease_expires = now + timedelta(seconds=lease_seconds)
        incoming = frozenset(conflict_keys)
        keys_json = encode_str_set(incoming)
        with self._transaction():
            row = self._connection.execute(
                "SELECT worker_id, lease_expires_at, version "
                "FROM task_claims WHERE task_id = ?",
                (task_id,),
            ).fetchone()
            if (
                row is not None
                and _parse_iso(row["lease_expires_at"]) > now
                and row["worker_id"] != worker_id
            ):
                return None
            # Refuse on conflict-key overlap with a *different* live claim. The
            # task's own row (re-acquire / expiry-steal of the same task_id) is
            # excluded, and lapsed claims do not block -- so the refusal clears
            # once the conflicting claim is released or its lease expires.
            if incoming and self._has_conflicting_live_claim(
                task_id, incoming, now=now
            ):
                return None
            if row is None:
                self._connection.execute(
                    "INSERT INTO task_claims (task_id, worker_id, "
                    "claimed_at, lease_expires_at, version, "
                    "conflict_keys_json) VALUES (?, ?, ?, ?, 1, ?)",
                    (
                        task_id,
                        worker_id,
                        _iso(now),
                        _iso(lease_expires),
                        keys_json,
                    ),
                )
                self._append_event(
                    task_id=task_id,
                    worker_id=worker_id,
                    event_type=EVENT_ACQUIRED,
                    version=1,
                    lease_expires_at=lease_expires,
                    occurred_at=now,
                )
                return TaskClaim(
                    task_id=task_id,
                    worker_id=worker_id,
                    claimed_at=now,
                    lease_expires_at=lease_expires,
                    version=1,
                )
            new_version = int(row["version"]) + 1
            # A same-worker re-acquire is ``acquired``; taking over a *different*
            # worker's row is ``stolen`` (D-2). The live-different-worker case
            # already returned None above, so a surviving different-worker row
            # here is necessarily a lapsed lease being reclaimed.
            event_type = (
                EVENT_ACQUIRED
                if row["worker_id"] == worker_id
                else EVENT_STOLEN
            )
            self._connection.execute(
                "UPDATE task_claims SET worker_id = ?, claimed_at = ?, "
                "lease_expires_at = ?, version = ?, conflict_keys_json = ? "
                "WHERE task_id = ?",
                (
                    worker_id,
                    _iso(now),
                    _iso(lease_expires),
                    new_version,
                    keys_json,
                    task_id,
                ),
            )
            self._append_event(
                task_id=task_id,
                worker_id=worker_id,
                event_type=event_type,
                version=new_version,
                lease_expires_at=lease_expires,
                occurred_at=now,
            )
            return TaskClaim(
                task_id=task_id,
                worker_id=worker_id,
                claimed_at=now,
                lease_expires_at=lease_expires,
                version=new_version,
            )

    def _has_conflicting_live_claim(
        self,
        task_id: str,
        incoming: frozenset[str],
        *,
        now: datetime,
    ) -> bool:
        # A *different* live claim (another task_id whose lease has not lapsed)
        # whose stored conflict keys overlap the incoming set blocks acquire.
        rows = self._connection.execute(
            "SELECT conflict_keys_json FROM task_claims "
            "WHERE task_id != ? AND lease_expires_at > ?",
            (task_id, _iso(now)),
        ).fetchall()
        for row in rows:
            if decode_str_set(row["conflict_keys_json"]) & incoming:
                return True
        return False

    def renew_claim(
        self,
        claim: TaskClaim,
        *,
        now: datetime,
        lease_seconds: float,
    ) -> TaskClaim:
        lease_expires = now + timedelta(seconds=lease_seconds)
        new_version = claim.version + 1
        # The UPDATE and its ``renewed`` event commit together: a stale token
        # updates no row, so the raise rolls the transaction back and no event
        # is written (criterion #5).
        with self._transaction():
            cursor = self._connection.execute(
                "UPDATE task_claims SET lease_expires_at = ?, version = ? "
                "WHERE task_id = ? AND version = ? AND worker_id = ?",
                (
                    _iso(lease_expires),
                    new_version,
                    claim.task_id,
                    claim.version,
                    claim.worker_id,
                ),
            )
            if cursor.rowcount != 1:
                raise ClaimLostError(claim.task_id)
            self._append_event(
                task_id=claim.task_id,
                worker_id=claim.worker_id,
                event_type=EVENT_RENEWED,
                version=new_version,
                lease_expires_at=lease_expires,
                occurred_at=now,
            )
        return TaskClaim(
            task_id=claim.task_id,
            worker_id=claim.worker_id,
            claimed_at=claim.claimed_at,
            lease_expires_at=lease_expires,
            version=new_version,
        )

    def release_claim(
        self, claim: TaskClaim, *, now: datetime | None = None
    ) -> None:
        # The DELETE and its ``released`` event commit together. A stale/
        # already-stolen token deletes no row (rowcount 0), so no event is
        # written (criterion #7). ``now`` is the injected occurred-at; absent a
        # caller-supplied clock it falls back to the released lease's expiry.
        with self._transaction():
            cursor = self._connection.execute(
                "DELETE FROM task_claims "
                "WHERE task_id = ? AND version = ? AND worker_id = ?",
                (claim.task_id, claim.version, claim.worker_id),
            )
            if cursor.rowcount == 1:
                self._append_event(
                    task_id=claim.task_id,
                    worker_id=claim.worker_id,
                    event_type=EVENT_RELEASED,
                    version=claim.version,
                    lease_expires_at=claim.lease_expires_at,
                    occurred_at=(
                        now if now is not None else claim.lease_expires_at
                    ),
                )

    def load_claim(self, task_id: str) -> TaskClaim | None:
        row = self._connection.execute(
            "SELECT task_id, worker_id, claimed_at, lease_expires_at, "
            "version FROM task_claims WHERE task_id = ?",
            (task_id,),
        ).fetchone()
        if row is None:
            return None
        return TaskClaim(
            task_id=row["task_id"],
            worker_id=row["worker_id"],
            claimed_at=_parse_iso(row["claimed_at"]),
            lease_expires_at=_parse_iso(row["lease_expires_at"]),
            version=int(row["version"]),
        )

    def list_claims(self) -> list[TaskClaim]:
        # Every held row, reusing load_claim's column projection and
        # _parse_iso. Released claims are deleted rows, so absent; expiry is
        # not filtered (an expired-but-not-yet-stolen lease still appears).
        rows = self._connection.execute(
            "SELECT task_id, worker_id, claimed_at, lease_expires_at, "
            "version FROM task_claims"
        ).fetchall()
        return [
            TaskClaim(
                task_id=row["task_id"],
                worker_id=row["worker_id"],
                claimed_at=_parse_iso(row["claimed_at"]),
                lease_expires_at=_parse_iso(row["lease_expires_at"]),
                version=int(row["version"]),
            )
            for row in rows
        ]

    def sweep_expired_claims(self, *, now: datetime) -> list[str]:
        # Batch-delete every lapsed row (lease_expires_at <= now) in one
        # transaction, returning the freed task ids. Lexical comparison on the
        # ISO timestamp string matches load/acquire's "> now means live" test
        # (same encoding used by _has_conflicting_live_claim). Released rows
        # leave list_claims and are immediately re-acquirable; still-valid
        # claims are not touched.
        now_iso = _iso(now)
        with self._transaction():
            rows = self._connection.execute(
                "SELECT task_id, worker_id, version, lease_expires_at "
                "FROM task_claims WHERE lease_expires_at <= ? ORDER BY task_id",
                (now_iso,),
            ).fetchall()
            released = [row["task_id"] for row in rows]
            if released:
                self._connection.execute(
                    "DELETE FROM task_claims WHERE lease_expires_at <= ?",
                    (now_iso,),
                )
                # One ``expired`` event per reaped claim, carrying that claim's
                # holder/version (D-2); still-valid rows were never selected.
                for row in rows:
                    self._append_event(
                        task_id=row["task_id"],
                        worker_id=row["worker_id"],
                        event_type=EVENT_EXPIRED,
                        version=int(row["version"]),
                        lease_expires_at=_parse_iso(row["lease_expires_at"]),
                        occurred_at=now,
                    )
        return released

    def list_events(self) -> list[OrchestratorEventRecord]:
        # Global stream: every recorded event in id (insertion) order.
        rows = self._connection.execute(
            "SELECT id, task_id, worker_id, event_type, version, "
            "lease_expires_at, occurred_at FROM orchestrator_events "
            "ORDER BY id"
        ).fetchall()
        return [_row_to_orchestrator_event_record(row) for row in rows]

    def list_task_events(self, task_id: str) -> list[OrchestratorEventRecord]:
        # Per-task timeline: one task's events in id order.
        rows = self._connection.execute(
            "SELECT id, task_id, worker_id, event_type, version, "
            "lease_expires_at, occurred_at FROM orchestrator_events "
            "WHERE task_id = ? ORDER BY id",
            (task_id,),
        ).fetchall()
        return [_row_to_orchestrator_event_record(row) for row in rows]

    # -- stop-event ledger (schema v6) -------------------------------------

    def _insert_stop_event(
        self,
        *,
        kind: str,
        subject: str,
        detail: str,
        occurred_at: datetime,
        run_id: str = "",
    ) -> None:
        # Append one stop row. MUST be called inside an open ``_transaction`` so
        # a prepare-skip stop commits atomically with the claim release it
        # accompanies (D-3). Never dedupes -- recurrence is the signal.
        self._connection.execute(
            "INSERT INTO orchestrator_stop_events (kind, subject, detail, "
            "occurred_at, run_id) VALUES (?, ?, ?, ?, ?)",
            (kind, subject, detail, _iso(occurred_at), run_id),
        )

    def record_stop_event(
        self,
        *,
        kind: str,
        subject: str,
        detail: str,
        occurred_at: datetime,
    ) -> None:
        # Audit-witness only: append one row naming the stop and its cause.
        with self._transaction():
            self._insert_stop_event(
                kind=kind,
                subject=subject,
                detail=detail,
                occurred_at=occurred_at,
            )

    def record_prepare_skip(
        self,
        claim: TaskClaim,
        *,
        detail: str,
        now: datetime,
    ) -> None:
        # A sandbox prepare/preflight failure: the claim release (with its
        # ``released`` event when the token still owns a row) and the
        # ``prepare-skip`` stop row commit in one transaction (D-3). The stop
        # row is written unconditionally -- the dead-end happened regardless of
        # whether the release found a matching row to delete.
        with self._transaction():
            cursor = self._connection.execute(
                "DELETE FROM task_claims "
                "WHERE task_id = ? AND version = ? AND worker_id = ?",
                (claim.task_id, claim.version, claim.worker_id),
            )
            if cursor.rowcount == 1:
                self._append_event(
                    task_id=claim.task_id,
                    worker_id=claim.worker_id,
                    event_type=EVENT_RELEASED,
                    version=claim.version,
                    lease_expires_at=claim.lease_expires_at,
                    occurred_at=now,
                )
            self._insert_stop_event(
                kind=STOP_PREPARE_SKIP,
                subject=claim.task_id,
                detail=detail,
                occurred_at=now,
            )

    def list_stop_events(self) -> list[OrchestratorStopEventRecord]:
        # Global stream: every recorded stop in id (insertion) order.
        rows = self._connection.execute(
            "SELECT id, kind, subject, detail, occurred_at, run_id "
            "FROM orchestrator_stop_events ORDER BY id"
        ).fetchall()
        return [_row_to_orchestrator_stop_event_record(row) for row in rows]

    def list_subject_stop_events(
        self, subject: str
    ) -> list[OrchestratorStopEventRecord]:
        # Per-subject timeline: one subject's stops in id order.
        rows = self._connection.execute(
            "SELECT id, kind, subject, detail, occurred_at, run_id "
            "FROM orchestrator_stop_events WHERE subject = ? ORDER BY id",
            (subject,),
        ).fetchall()
        return [_row_to_orchestrator_stop_event_record(row) for row in rows]

    def record_human_review(
        self,
        *,
        reason: str,
        task_id: str,
        occurred_at: datetime,
        run_id: str = "",
        detail: str = "",
    ) -> None:
        # Route one unit into the single human-review queue by appending a stop
        # row whose ``kind`` is the machine-readable ``reason`` (no new silo).
        # The reason MUST be a stable token from HUMAN_REVIEW_QUEUE_REASONS,
        # never free text. Append-only, never deduped.
        _require_review_reason(reason)
        with self._transaction():
            self._insert_stop_event(
                kind=reason,
                subject=task_id,
                detail=detail,
                occurred_at=occurred_at,
                run_id=run_id,
            )

    def list_human_review_queue(self) -> list[HumanReviewQueueEntry]:
        # The single queue read: every routed unit across all kinds, in id
        # (insertion) order. Filters the shared ledger to rows whose kind is a
        # review reason (disjoint from the pre-run stop kinds), so pre-run
        # dead-ends never leak into the queue. Empty queue -> empty list.
        placeholders = ",".join("?" for _ in HUMAN_REVIEW_QUEUE_REASONS)
        rows = self._connection.execute(
            "SELECT id, kind, subject, detail, occurred_at, run_id "
            "FROM orchestrator_stop_events "
            f"WHERE kind IN ({placeholders}) ORDER BY id",
            tuple(sorted(HUMAN_REVIEW_QUEUE_REASONS)),
        ).fetchall()
        return [_row_to_human_review_entry(row) for row in rows]

    # -- WorkGraph snapshots (schema v5) -----------------------------------

    def record_graph_snapshot(
        self,
        items: Iterable[GraphSnapshotItem],
        *,
        captured_at: datetime,
    ) -> GraphSnapshotRecord:
        """Record one WorkGraph snapshot atomically (spec 00055, D-3).

        The header row and every item row commit in a single transaction, so a
        reader never sees a snapshot whose item rows are a subset of what it
        captured (criterion #3). ``last_event_id`` is stamped by the store as
        the live ``orchestrator_events`` max id read *inside* this transaction
        (0 when the ledger is empty, D-2), never caller-supplied, so it cannot
        drift from the ledger. An empty ``items`` still records a valid snapshot
        with item count 0 (criterion #11). Append-only: a fresh snapshot id per
        call, never overwriting an earlier one.
        """
        materialized = list(items)
        with self._transaction():
            row = self._connection.execute(
                "SELECT COALESCE(MAX(id), 0) AS hwm FROM orchestrator_events"
            ).fetchone()
            last_event_id = int(row["hwm"])
            cursor = self._connection.execute(
                "INSERT INTO graph_snapshots "
                "(captured_at, item_count, last_event_id) VALUES (?, ?, ?)",
                (_iso(captured_at), len(materialized), last_event_id),
            )
            snapshot_id = cursor.lastrowid
            assert snapshot_id is not None  # AUTOINCREMENT INSERT sets it
            for item in materialized:
                self._connection.execute(
                    "INSERT INTO graph_snapshot_items ("
                    "  snapshot_id, task_id, source_kind, source_ref, "
                    "  source_url, source_version, priority, "
                    "  required_capabilities_json, conflict_keys_json, "
                    "  state, ready, claim_holder, resolved_prerequisites_json"
                    ") VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    (
                        snapshot_id,
                        item.task_id,
                        item.source_kind,
                        item.source_ref,
                        item.source_url,
                        item.source_version,
                        item.priority,
                        encode_str_set(item.required_capabilities),
                        encode_str_set(item.conflict_keys),
                        item.state,
                        1 if item.ready else 0,
                        item.claim_holder,
                        encode_str_set(item.resolved_prerequisites),
                    ),
                )
        return GraphSnapshotRecord(
            id=int(snapshot_id),
            captured_at=captured_at,
            item_count=len(materialized),
            last_event_id=last_event_id,
        )

    def list_graph_snapshots(self) -> list[GraphSnapshotRecord]:
        # Snapshot stream: every recorded header in id (insertion) order.
        rows = self._connection.execute(
            "SELECT id, captured_at, item_count, last_event_id "
            "FROM graph_snapshots ORDER BY id"
        ).fetchall()
        return [_row_to_graph_snapshot_record(row) for row in rows]

    def list_graph_snapshot_items(
        self, snapshot_id: int
    ) -> list[GraphSnapshotItem]:
        # One snapshot's item rows in task_id order. Unknown id -> empty list.
        rows = self._connection.execute(
            "SELECT task_id, source_kind, source_ref, source_url, "
            "source_version, priority, required_capabilities_json, "
            "conflict_keys_json, state, ready, claim_holder, "
            "resolved_prerequisites_json FROM graph_snapshot_items "
            "WHERE snapshot_id = ? ORDER BY task_id",
            (snapshot_id,),
        ).fetchall()
        return [_row_to_graph_snapshot_item(row) for row in rows]

    def latest_graph_snapshot(self) -> GraphSnapshotRecord | None:
        # Most recently recorded snapshot header; None on an empty store.
        row = self._connection.execute(
            "SELECT id, captured_at, item_count, last_event_id "
            "FROM graph_snapshots ORDER BY id DESC LIMIT 1"
        ).fetchone()
        return _row_to_graph_snapshot_record(row) if row is not None else None

    # -- WorkGraph persistence (schema v2) ---------------------------------

    def upsert_work_item(self, item: WorkItem, *, now: datetime) -> None:
        """Insert or refresh the ``work_items`` row for an observed item.

        ``first_seen_at`` is set only on the initial insert; ``last_seen_at``
        is set to ``now`` on every observation and any prior
        ``disappeared_at`` is cleared. ``task_content_hash`` is
        ``task_digest(item.task)`` (D-1). ``priority`` /
        ``required_capabilities_json`` / ``conflict_keys_json`` are written
        from the item's scheduling metadata (spec 00049); ``metadata_json``
        is left at its column default.
        """
        with self._transaction():
            self._connection.execute(
                "INSERT INTO work_items ("
                "  task_id, source_kind, source_ref, source_url, "
                "  source_version, task_content_hash, priority, "
                "  required_capabilities_json, conflict_keys_json, "
                "  first_seen_at, last_seen_at, disappeared_at"
                ") VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL) "
                "ON CONFLICT(task_id) DO UPDATE SET "
                "  source_kind = excluded.source_kind, "
                "  source_ref = excluded.source_ref, "
                "  source_url = excluded.source_url, "
                "  source_version = excluded.source_version, "
                "  task_content_hash = excluded.task_content_hash, "
                "  priority = excluded.priority, "
                "  required_capabilities_json = "
                "    excluded.required_capabilities_json, "
                "  conflict_keys_json = excluded.conflict_keys_json, "
                "  last_seen_at = excluded.last_seen_at, "
                "  disappeared_at = NULL",
                (
                    item.task.id,
                    item.source_kind,
                    item.source_ref,
                    item.source_url,
                    item.source_version,
                    task_digest(item.task),
                    item.priority,
                    encode_str_set(item.required_capabilities),
                    encode_str_set(item.conflict_keys),
                    _iso(now),
                    _iso(now),
                ),
            )

    def replace_work_item_dependencies(
        self,
        task_id: str,
        prerequisite_task_ids: Iterable[str],
        *,
        now: datetime,
    ) -> None:
        """Replace the dependency edge set for ``task_id`` with the given
        prerequisites (the current-graph edges); duplicates collapse."""
        with self._transaction():
            self._connection.execute(
                "DELETE FROM work_item_dependencies WHERE task_id = ?",
                (task_id,),
            )
            for prerequisite in dict.fromkeys(prerequisite_task_ids):
                self._connection.execute(
                    "INSERT INTO work_item_dependencies "
                    "(task_id, prerequisite_task_id, created_at) "
                    "VALUES (?, ?, ?)",
                    (task_id, prerequisite, _iso(now)),
                )

    def mark_work_items_disappeared(
        self,
        observed_task_ids: Iterable[str],
        *,
        now: datetime,
    ) -> None:
        """Stamp ``disappeared_at`` on previously-seen items absent from the
        current observed set, without deleting any row. Items already marked
        disappeared keep their original timestamp."""
        observed = list(dict.fromkeys(observed_task_ids))
        with self._transaction():
            if observed:
                placeholders = ",".join("?" for _ in observed)
                self._connection.execute(
                    "UPDATE work_items SET disappeared_at = ? "
                    "WHERE disappeared_at IS NULL "
                    f"AND task_id NOT IN ({placeholders})",
                    (_iso(now), *observed),
                )
            else:
                self._connection.execute(
                    "UPDATE work_items SET disappeared_at = ? "
                    "WHERE disappeared_at IS NULL",
                    (_iso(now),),
                )

    def load_work_item(self, task_id: str) -> WorkItemRecord | None:
        row = self._connection.execute(
            "SELECT task_id, source_kind, source_ref, source_url, "
            "source_version, task_content_hash, priority, "
            "required_capabilities_json, conflict_keys_json, first_seen_at, "
            "last_seen_at, disappeared_at, metadata_json "
            "FROM work_items WHERE task_id = ?",
            (task_id,),
        ).fetchone()
        if row is None:
            return None
        return _row_to_work_item_record(row)

    def list_work_items(self) -> list[WorkItemRecord]:
        rows = self._connection.execute(
            "SELECT task_id, source_kind, source_ref, source_url, "
            "source_version, task_content_hash, priority, "
            "required_capabilities_json, conflict_keys_json, first_seen_at, "
            "last_seen_at, disappeared_at, metadata_json "
            "FROM work_items"
        ).fetchall()
        return [_row_to_work_item_record(row) for row in rows]

    def load_work_item_dependencies(self, task_id: str) -> list[str]:
        rows = self._connection.execute(
            "SELECT prerequisite_task_id FROM work_item_dependencies "
            "WHERE task_id = ? ORDER BY prerequisite_task_id",
            (task_id,),
        ).fetchall()
        return [row["prerequisite_task_id"] for row in rows]

    def list_work_item_dependencies(self) -> list[tuple[str, str]]:
        rows = self._connection.execute(
            "SELECT task_id, prerequisite_task_id FROM work_item_dependencies "
            "ORDER BY task_id, prerequisite_task_id"
        ).fetchall()
        return [(row["task_id"], row["prerequisite_task_id"]) for row in rows]

    # -- source-sync recording (schema v2) ---------------------------------

    def record_source_sync_start(
        self,
        source_kind: str,
        source_name: str,
        *,
        now: datetime,
    ) -> int:
        """Open a ``source_syncs`` row for a pass and return its id.

        The row starts ``status='running'`` with ``finished_at`` NULL; the
        returned id is handed to :meth:`record_source_sync_finish` to settle
        the row once the pass succeeds or fails.
        """
        with self._transaction():
            cursor = self._connection.execute(
                "INSERT INTO source_syncs ("
                "  source_kind, source_name, started_at, status, "
                "  observed_count"
                ") VALUES (?, ?, ?, 'running', 0)",
                (source_kind, source_name, _iso(now)),
            )
            row_id = cursor.lastrowid
            assert row_id is not None  # AUTOINCREMENT INSERT always sets it
            return int(row_id)

    def record_source_sync_finish(
        self,
        sync_id: int,
        *,
        status: str,
        observed_count: int = 0,
        error: str | None = None,
        now: datetime,
    ) -> None:
        """Settle the ``source_syncs`` row ``sync_id``.

        ``status='ok'`` carries ``observed_count`` (the number of items the
        pass observed); ``status='error'`` carries a non-empty ``error``.
        ``finished_at`` is stamped to ``now`` either way. Recording a finish
        does NOT touch ``work_items`` — the failed-pass-marks-nothing posture
        (D-3) lives in the caller, which simply skips the mark-disappeared
        step on the error path.
        """
        with self._transaction():
            self._connection.execute(
                "UPDATE source_syncs SET status = ?, observed_count = ?, "
                "error = ?, finished_at = ? WHERE id = ?",
                (status, observed_count, error, _iso(now), sync_id),
            )

    def load_source_sync(self, sync_id: int) -> SourceSyncRecord | None:
        row = self._connection.execute(
            "SELECT id, source_kind, source_name, started_at, finished_at, "
            "status, observed_count, error, metadata_json "
            "FROM source_syncs WHERE id = ?",
            (sync_id,),
        ).fetchone()
        if row is None:
            return None
        return _row_to_source_sync_record(row)

    def list_source_syncs(self) -> list[SourceSyncRecord]:
        rows = self._connection.execute(
            "SELECT id, source_kind, source_name, started_at, finished_at, "
            "status, observed_count, error, metadata_json "
            "FROM source_syncs ORDER BY id"
        ).fetchall()
        return [_row_to_source_sync_record(row) for row in rows]

    def close(self) -> None:
        self._connection.close()


__all__ = [
    "CURRENT_ORCH_SCHEMA_VERSION",
    "EVENT_ACQUIRED",
    "EVENT_EXPIRED",
    "EVENT_RELEASED",
    "EVENT_RENEWED",
    "EVENT_STOLEN",
    "ORCHESTRATOR_EVENT_TYPES",
    "ORCHESTRATOR_STOP_EVENT_KINDS",
    "HUMAN_REVIEW_QUEUE_REASONS",
    "REASON_ABORTED",
    "REASON_AWAITING_APPROVAL",
    "REASON_BUDGET_CEILING",
    "REASON_NO_PROGRESS",
    "REASON_PREREQUISITE_MISSING",
    "REASON_RETRIES_EXHAUSTED_AFTER_ESCALATION",
    "STOP_DANGLING_PREREQUISITE",
    "STOP_INDETERMINATE_LANDING",
    "STOP_NO_OP_CYCLE",
    "STOP_NO_PROGRESS",
    "STOP_NO_PROGRESS_RESET",
    "STOP_PREPARE_SKIP",
    "STOP_RESOLVED",
    "STOP_RETRIES_ESCALATED",
    "STOP_SOURCE_TRUNCATION",
    "STOP_ZERO_GRADER_DROP",
    "ClaimLostError",
    "ClaimStore",
    "GraphSnapshotItem",
    "GraphSnapshotRecord",
    "HumanReviewQueueEntry",
    "InMemoryClaimStore",
    "SqliteClaimStore",
    "OrchestratorEventRecord",
    "OrchestratorSchemaError",
    "OrchestratorStopEventRecord",
    "SourceSyncRecord",
    "StopEventStore",
    "TaskClaim",
    "WorkItemRecord",
]

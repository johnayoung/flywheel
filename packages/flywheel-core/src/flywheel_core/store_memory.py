"""In-memory implementation of every store Protocol from
``flywheel_core.store_protocols``.

This is the test substrate: a single ``InMemoryStore`` class backed by
plain dicts and lists. It is not durable, not thread-safe, and contains
no ``sqlite3``, file, or network IO — durable persistence lives in the
SQLite store (a sibling task).

Storage layout follows the table boundaries in
``flywheel/_schema/persistence-schema.sql``:

* ``_lifecycles`` is keyed by ``run_id`` and holds the row state with no
  attempts attached; ``attempts`` are populated on read by joining the
  attempts table.
* ``_attempts`` is keyed by ``(run_id, number)``.
* ``_events`` and ``_grader_results`` are append-only ordered lists with
  monotonic ids; ``_events`` holds domain rows only (telemetry flows to a
  ``TelemetrySink``, never the store) and the contract surface exposes no
  update/delete entry point for grader_results.
"""

from __future__ import annotations

from collections.abc import Mapping
from datetime import datetime, timezone
from typing import Any, cast

from flywheel_core.event_serde import event_from_record, event_kind, event_payload
from flywheel_core.events import (
    AttemptFinalized,
    AttemptStarted,
    DomainEvent,
    GraderEvaluated,
    LifecycleInitialized,
    apply,
)
from flywheel_core.lifecycle import Attempt, Lifecycle
from flywheel_core.loaders import deserialize_task, serialize_task, task_digest
from flywheel_core.notifier import RunNotifier
from flywheel_core.task import Task
from flywheel_core.store_protocols import (
    ControlCommandRecord,
    EventRecord,
    GraderResultRecord,
    GraderType,
    LifecycleAlreadyExistsError,
    LifecycleNotFoundError,
    OptimisticConcurrencyError,
)


def _clone_lifecycle_row(lc: Lifecycle) -> Lifecycle:
    """Defensive copy of the lifecycle row. ``attempts`` is intentionally
    left empty so the attempts table stays the single source of truth."""
    return Lifecycle(
        task_id=lc.task_id,
        run_id=lc.run_id,
        worker_id=lc.worker_id,
        status=lc.status,
        timestamps=dict(lc.timestamps),
        version=lc.version,
        retries=lc.retries,
        error=lc.error,
        agent_output=lc.agent_output,
        attempts=[],
        session_id=lc.session_id,
        artifacts_dir=lc.artifacts_dir,
        blocked_requires_json=lc.blocked_requires_json,
        awaiting_manual_ordinal=lc.awaiting_manual_ordinal,
        task_content_hash=lc.task_content_hash,
    )


def _clone_attempt(a: Attempt) -> Attempt:
    return Attempt(
        number=a.number,
        started_at=a.started_at,
        run_id=a.run_id,
        ended_at=a.ended_at,
        outcome=a.outcome,
        agent_output=a.agent_output,
        error=a.error,
        agent_context=dict(a.agent_context),
        input_tokens=a.input_tokens,
        output_tokens=a.output_tokens,
        cache_creation_input_tokens=a.cache_creation_input_tokens,
        cache_read_input_tokens=a.cache_read_input_tokens,
        iterations_completed=a.iterations_completed,
        turns=a.turns,
        total_cost_usd=a.total_cost_usd,
        last_activity_at=a.last_activity_at,
    )


def _clone_grader_result(r: GraderResultRecord) -> GraderResultRecord:
    return GraderResultRecord(
        run_id=r.run_id,
        attempt_number=r.attempt_number,
        ordinal=r.ordinal,
        grader_type=r.grader_type,
        grader_spec=dict(r.grader_spec),
        passed=r.passed,
        duration_ms=r.duration_ms,
        payload=dict(r.payload),
        ts=r.ts,
        grader_name=r.grader_name,
        id=r.id,
    )


def _clone_control_command(c: ControlCommandRecord) -> ControlCommandRecord:
    return ControlCommandRecord(
        run_id=c.run_id,
        kind=c.kind,
        payload=dict(c.payload),
        enqueued_at=c.enqueued_at,
        claimed_at=c.claimed_at,
        id=c.id,
    )


class InMemoryStore:
    """In-memory implementation of every store protocol.

    Each public method satisfies the corresponding Protocol in
    ``flywheel_core.store_protocols``. Records returned to callers are
    defensive copies so caller mutations do not corrupt stored rows; the
    same holds in reverse for record arguments.
    """

    def __init__(self, *, notifier: RunNotifier | None = None) -> None:
        # Default-on in-process reactivity: a consumer that shares this
        # store instance (the common single-host case) gets push wakeups;
        # separate instances fall back to the audit follower's poll.
        self.notifier: RunNotifier = notifier or RunNotifier()
        self._lifecycles: dict[str, Lifecycle] = {}
        self._attempts: dict[tuple[str, int], Attempt] = {}
        # Domain rows only; telemetry never reaches the store.
        self._events: list[EventRecord] = []
        self._event_seq: int = 0
        self._grader_results: list[GraderResultRecord] = []
        self._grader_result_seq: int = 0
        # Task model mirroring the persistence schema:
        #   _task_identities  -- logical task id -> first_seen_at (the FK
        #                        anchor; a run can only reference a registered
        #                        identity).
        #   _task_versions    -- (id, content_hash) -> (definition dict,
        #                        created_at, insertion-seq) for deterministic
        #                        "latest version" resolution on ties. The
        #                        definition dict holds the hashed, immutable
        #                        fields: goal/graders/tags/context.
        self._task_identities: dict[str, datetime] = {}
        self._task_versions: dict[
            tuple[str, str], tuple[dict[str, Any], datetime, int]
        ] = {}
        self._task_seq: int = 0
        # Control-command queue. Rows are kept in a single list with a
        # monotonic ``id`` (the enqueue-order key) so claim_commands can
        # surface them in ascending order without a separate index.
        self._control_commands: list[ControlCommandRecord] = []
        self._control_command_seq: int = 0

    def _next_domain_sequence(self, run_id: str) -> int:
        # MAX + 1 over the run's existing event rows, mirroring the
        # durable stores: strictly monotonic per run without a shared
        # cross-table counter.
        return (
            max(
                (
                    e.sequence
                    for e in self._events
                    if e.run_id == run_id and e.sequence is not None
                ),
                default=0,
            )
            + 1
        )

    # --- LifecycleStore ----------------------------------------------------

    def create_lifecycle(self, lifecycle: Lifecycle) -> None:
        if lifecycle.run_id in self._lifecycles:
            raise LifecycleAlreadyExistsError(lifecycle.run_id)
        # Auto-register the task identity so a run always references a
        # catalogued task (mirrors the lifecycles.task_id -> tasks(id) FK the
        # durable stores enforce). Idempotent: save_task usually created it.
        self._task_identities.setdefault(
            lifecycle.task_id, datetime.now(timezone.utc)
        )
        self._lifecycles[lifecycle.run_id] = _clone_lifecycle_row(lifecycle)

    def update_lifecycle(
        self,
        lifecycle: Lifecycle,
        *,
        expected_version: int,
    ) -> None:
        stored = self._lifecycles.get(lifecycle.run_id)
        if stored is None:
            raise LifecycleNotFoundError(lifecycle.run_id)
        if stored.version != expected_version:
            raise OptimisticConcurrencyError(
                lifecycle.run_id,
                expected_version=expected_version,
                actual_version=stored.version,
            )
        self._lifecycles[lifecycle.run_id] = _clone_lifecycle_row(lifecycle)

    def load_lifecycle(self, run_id: str) -> Lifecycle | None:
        stored = self._lifecycles.get(run_id)
        if stored is None:
            return None
        lc = _clone_lifecycle_row(stored)
        lc.attempts = self.list_attempts(run_id)
        return lc

    # --- TaskStore ---------------------------------------------------------

    def save_task(self, task: Task, *, now: datetime) -> str:
        content_hash = task_digest(task)
        data = serialize_task(task)
        # Register the task identity (the FK anchor in the durable stores).
        self._task_identities.setdefault(task.id, now)
        key = (task.id, content_hash)
        if key not in self._task_versions:
            self._task_seq += 1
            definition = {
                "goal": data["goal"],
                "graders": data["graders"],
                "tags": data["tags"],
                "context": data["context"],
            }
            self._task_versions[key] = (definition, now, self._task_seq)
        return content_hash

    def _assemble_task(
        self, task_id: str, definition: dict[str, Any]
    ) -> Task:
        return deserialize_task(
            {
                "id": task_id,
                "goal": definition["goal"],
                "graders": definition["graders"],
                "tags": definition["tags"],
                "context": definition["context"],
            }
        )

    def load_task(
        self, task_id: str, content_hash: str | None = None
    ) -> Task | None:
        if content_hash is not None:
            entry = self._task_versions.get((task_id, content_hash))
            if entry is None:
                return None
            return self._assemble_task(task_id, entry[0])
        candidates = [
            value
            for (tid, _ch), value in self._task_versions.items()
            if tid == task_id
        ]
        if not candidates:
            return None
        # Most recent by created_at, breaking ties on insertion order.
        definition, _created, _seq = max(
            candidates, key=lambda v: (v[1], v[2])
        )
        return self._assemble_task(task_id, definition)

    def load_task_for_run(self, run_id: str) -> Task | None:
        stored = self._lifecycles.get(run_id)
        if stored is None:
            return None
        return self.load_task(
            stored.task_id, stored.task_content_hash or None
        )

    # --- DomainEventStore --------------------------------------------------

    def append_domain_event(
        self,
        event: DomainEvent,
        *,
        expected_version: int,
    ) -> Lifecycle:
        if isinstance(event, LifecycleInitialized):
            folded = apply(None, event)
            # Raises LifecycleAlreadyExistsError on a duplicate seed.
            self.create_lifecycle(folded)
        else:
            current = self.load_lifecycle(event.run_id)
            if current is None:
                raise LifecycleNotFoundError(event.run_id)
            if current.version != expected_version:
                raise OptimisticConcurrencyError(
                    event.run_id,
                    expected_version=expected_version,
                    actual_version=current.version,
                )
            folded = apply(current, event)
            self.update_lifecycle(folded, expected_version=expected_version)
            self._project_domain_event(event, folded)
        sequence = self._append_domain_event_row(event)
        self.notifier.notify(event.run_id, sequence)
        return folded

    def _append_domain_event_row(self, event: DomainEvent) -> int:
        self._event_seq += 1
        sequence = self._next_domain_sequence(event.run_id)
        self._events.append(
            EventRecord(
                run_id=event.run_id,
                ts=event.ts,
                kind=event_kind(event),
                payload=event_payload(event),
                attempt_number=event.attempt_number,
                id=self._event_seq,
                sequence=sequence,
            )
        )
        return sequence

    def _project_domain_event(
        self, event: DomainEvent, folded: Lifecycle
    ) -> None:
        if isinstance(event, (AttemptStarted, AttemptFinalized)):
            attempt = next(
                a for a in folded.attempts if a.number == event.number
            )
            self.save_attempt(folded.run_id, attempt)
        elif isinstance(event, GraderEvaluated):
            assert event.attempt_number is not None
            self.append_grader_result(
                GraderResultRecord(
                    run_id=event.run_id,
                    attempt_number=event.attempt_number,
                    ordinal=event.ordinal,
                    grader_type=cast(GraderType, event.grader_type),
                    grader_spec=dict(event.grader_spec),
                    passed=event.passed,
                    duration_ms=event.duration_ms,
                    payload=dict(event.payload),
                    ts=event.ts,
                    grader_name=event.grader_name,
                )
            )

    def list_domain_events(self, run_id: str) -> list[DomainEvent]:
        rows = [e for e in self._events if e.run_id == run_id]
        rows.sort(key=lambda e: e.sequence if e.sequence is not None else 0)
        return [
            event_from_record(
                kind=e.kind,
                payload=dict(e.payload),
                run_id=e.run_id,
                ts=e.ts,
                attempt_number=e.attempt_number,
                sequence=e.sequence,
                id=e.id,
            )
            for e in rows
        ]

    # --- AttemptStore ------------------------------------------------------

    def save_attempt(
        self,
        run_id: str,
        attempt: Attempt,
        *,
        expected_version: int | None = None,
    ) -> None:
        if expected_version is not None:
            # Versioned write (the harness's iteration-boundary aggregate
            # rollup): verify the lifecycle's optimistic-concurrency key
            # before the upsert so a stale worker cannot clobber counters
            # after the run moved on.
            stored = self._lifecycles.get(run_id)
            if stored is None:
                raise LifecycleNotFoundError(run_id)
            if stored.version != expected_version:
                raise OptimisticConcurrencyError(
                    run_id,
                    expected_version=expected_version,
                    actual_version=stored.version,
                )
        self._attempts[(run_id, attempt.number)] = _clone_attempt(attempt)

    def load_attempt(self, run_id: str, number: int) -> Attempt | None:
        stored = self._attempts.get((run_id, number))
        if stored is None:
            return None
        return _clone_attempt(stored)

    def list_attempts(self, run_id: str) -> list[Attempt]:
        rows = [
            a for (rid, _n), a in self._attempts.items() if rid == run_id
        ]
        rows.sort(key=lambda a: a.number)
        return [_clone_attempt(a) for a in rows]

    # --- GraderResultStore -------------------------------------------------

    def append_grader_result(
        self, result: GraderResultRecord
    ) -> GraderResultRecord:
        self._grader_result_seq += 1
        record = GraderResultRecord(
            run_id=result.run_id,
            attempt_number=result.attempt_number,
            ordinal=result.ordinal,
            grader_type=result.grader_type,
            grader_spec=dict(result.grader_spec),
            passed=result.passed,
            duration_ms=result.duration_ms,
            payload=dict(result.payload),
            ts=result.ts,
            grader_name=result.grader_name,
            id=self._grader_result_seq,
        )
        self._grader_results.append(record)
        return _clone_grader_result(record)

    def list_grader_results(
        self,
        run_id: str,
        attempt_number: int,
    ) -> list[GraderResultRecord]:
        rows = [
            r
            for r in self._grader_results
            if r.run_id == run_id and r.attempt_number == attempt_number
        ]
        rows.sort(key=lambda r: r.ordinal)
        return [_clone_grader_result(r) for r in rows]

    # --- ControlCommandStore -----------------------------------------------

    def enqueue_command(
        self,
        run_id: str,
        kind: str,
        payload: Mapping[str, Any],
        *,
        now: datetime,
    ) -> ControlCommandRecord:
        # Snapshot the payload up front so a caller mutating the input
        # dict after enqueue cannot poison the stored row, matching the
        # defensive-copy pattern used everywhere else in this store.
        self._control_command_seq += 1
        record = ControlCommandRecord(
            run_id=run_id,
            kind=kind,
            payload=dict(payload),
            enqueued_at=now,
            claimed_at=None,
            id=self._control_command_seq,
        )
        self._control_commands.append(record)
        return _clone_control_command(record)

    def claim_commands(
        self,
        run_id: str,
        *,
        now: datetime,
    ) -> list[ControlCommandRecord]:
        # Claim-once: every still-pending row for ``run_id`` flips to
        # claimed_at = now in this single call. A second call for the same
        # run skips already-claimed rows so the consumer never double-
        # applies a command. The list itself is already in enqueue order
        # (we append on each enqueue and never reorder), so iterating it
        # in storage order is equivalent to ascending by id.
        claimed: list[ControlCommandRecord] = []
        for idx, cmd in enumerate(self._control_commands):
            if cmd.run_id != run_id or cmd.claimed_at is not None:
                continue
            updated = ControlCommandRecord(
                run_id=cmd.run_id,
                kind=cmd.kind,
                payload=dict(cmd.payload),
                enqueued_at=cmd.enqueued_at,
                claimed_at=now,
                id=cmd.id,
            )
            self._control_commands[idx] = updated
            claimed.append(updated)
        claimed.sort(key=lambda c: c.id if c.id is not None else 0)
        return [_clone_control_command(c) for c in claimed]


__all__ = ["InMemoryStore"]

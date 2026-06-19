"""Evidence-derived status rollup -- the read-only projection behind
``flywheel status --rollup``.

This is a pure projection over state the store already holds. It introduces
no authoritative state of its own (consistent with ``docs/data-taxonomy.md``):
every node's status is *computed* from lifecycle state and grader receipts,
never set by hand. That is the whole point -- a rollup whose "verified" can
only be earned by a green grader, not asserted by an operator dragging a card.

The honest distinction this surface draws, and a plain status table does not:
a task that reached ``done`` with passing graders is ``verified``; a task that
reached ``done`` with *no* graders is ``accepted`` -- the agent's own claim,
structurally unverifiable. The rollup never lets the second masquerade as the
first.
"""

from __future__ import annotations

from collections import Counter
from dataclasses import dataclass
from enum import Enum
from pathlib import Path
from typing import TYPE_CHECKING, Any

from flywheel_core.lifecycle import Status
from flywheel_core.store_sqlite import SqliteStore
from flywheel_orchestrator._workflow import TaskState, TaskStatusRow

if TYPE_CHECKING:
    from flywheel_core.store_postgres import PostgresStore


class RollupStatus(str, Enum):
    """A task's status as the rollup *derives* it from evidence.

    Ordering of the members is the report's column legend order.
    """

    VERIFIED = "verified"  # done, every recorded grader passed
    ACCEPTED = "accepted"  # done, but no graders ran -- agent claim, unverified
    IN_PROGRESS = "in_progress"  # an active lifecycle exists
    BLOCKED = "blocked"  # interrupted or parked on a manual gate
    FAILED = "failed"  # last lifecycle failed (retryable or terminal)
    BLOCKED_BY_PREREQ = "blocked_by_prereq"  # never started; a prereq isn't done
    NOT_STARTED = "not_started"  # never started; prerequisites are satisfied


# Statuses that count as "this task reached done" for prerequisite resolution.
# Both verified and accepted reached the DONE lifecycle state; the rollup only
# distinguishes them on *evidence quality*, not on completion.
_DONE_STATUSES: frozenset[RollupStatus] = frozenset(
    {RollupStatus.VERIFIED, RollupStatus.ACCEPTED}
)


@dataclass(frozen=True, kw_only=True)
class TaskRollup:
    """One task's derived status plus the evidence that earned it."""

    task_id: str
    phase: str
    status: RollupStatus
    passed_graders: int
    total_graders: int
    prerequisites: tuple[str, ...]
    unsatisfied_prerequisites: tuple[str, ...]
    detail: str


@dataclass(frozen=True, kw_only=True)
class PhaseRollup:
    """All tasks under one phase, in source order."""

    name: str
    tasks: tuple[TaskRollup, ...]

    @property
    def verified(self) -> int:
        return sum(1 for t in self.tasks if t.status is RollupStatus.VERIFIED)


@dataclass(frozen=True, kw_only=True)
class Rollup:
    """The full projection: phases in first-seen order plus status totals."""

    phases: tuple[PhaseRollup, ...]
    totals: dict[str, int]


def _phase_of(row: TaskStatusRow) -> str:
    """Group key for a row -- mirrors the plain ``status`` renderer.

    File-backed rows group under their phase directory; external items
    (no local path) group under their source ref, falling back to
    ``"external"``.
    """
    return row.task_file.parent.name or row.source_ref or "external"


def _verification_evidence(
    store: SqliteStore | PostgresStore, task_id: str
) -> tuple[int, int]:
    """Return ``(passed, total)`` grader receipts for the verifying attempt
    of the most recent DONE run, or ``(0, 0)`` when the run recorded none.

    The verifying attempt is the highest-numbered attempt that recorded any
    grader receipt: a DONE run's final attempt is the one whose graders all
    passed. Reading the *latest* receipt-bearing attempt avoids counting an
    earlier failed attempt's red graders against a task that has since passed.
    """
    done = store.list_lifecycles(statuses=[Status.DONE], task_id=task_id)
    if not done:
        return (0, 0)
    lc = done[0]
    for attempt in sorted(lc.attempts, key=lambda a: a.number, reverse=True):
        results = store.list_grader_results(lc.run_id, attempt.number)
        if results:
            passed = sum(1 for r in results if r.passed)
            return (passed, len(results))
    return (0, 0)


def _classify(
    row: TaskStatusRow, store: SqliteStore | PostgresStore
) -> tuple[RollupStatus, int, int, str]:
    """Map one status row to its base rollup status plus grader evidence.

    The prerequisite refinement (NOT_STARTED -> BLOCKED_BY_PREREQ) happens in
    a second pass in :func:`build_rollup`, once every task's status is known.
    """
    if row.state is TaskState.DONE:
        passed, total = _verification_evidence(store, row.task.id)
        if total > 0 and passed == total:
            return (RollupStatus.VERIFIED, passed, total, "")
        return (
            RollupStatus.ACCEPTED,
            passed,
            total,
            "done with no graders -- agent claim, unverified",
        )
    if row.state is TaskState.IN_PROGRESS:
        detail = row.latest_status.value if row.latest_status else "active"
        return (RollupStatus.IN_PROGRESS, 0, 0, detail)
    if row.state is TaskState.AWAITING_APPROVAL:
        return (RollupStatus.BLOCKED, 0, 0, "awaiting operator approval")
    if row.state is TaskState.INTERRUPTED:
        detail = row.latest_error or "interrupted"
        return (RollupStatus.BLOCKED, 0, 0, detail)
    if row.state is TaskState.RETRYABLE:
        return (RollupStatus.FAILED, 0, 0, row.latest_error)
    # TaskState.FRESH -- never attempted.
    return (RollupStatus.NOT_STARTED, 0, 0, "")


def build_rollup(
    rows: list[TaskStatusRow], store: SqliteStore | PostgresStore
) -> Rollup:
    """Project status rows into a phase-grouped, evidence-derived rollup."""
    # First pass: classify each task in isolation, recording its status so the
    # prerequisite pass can resolve dependencies by id.
    classified: list[tuple[TaskStatusRow, RollupStatus, int, int, str]] = []
    status_by_id: dict[str, RollupStatus] = {}
    for row in rows:
        status, passed, total, detail = _classify(row, store)
        classified.append((row, status, passed, total, detail))
        status_by_id[row.task.id] = status

    # Second pass: a not-started task whose prerequisites have not all reached
    # done is blocked *by* those prerequisites, not merely idle. A missing
    # prerequisite (absent from the workspace) counts as unsatisfied.
    phases: dict[str, list[TaskRollup]] = {}
    for row, status, passed, total, detail in classified:
        unsatisfied: tuple[str, ...] = ()
        if status is RollupStatus.NOT_STARTED and row.prerequisites:
            unsatisfied = tuple(
                prereq
                for prereq in row.prerequisites
                if status_by_id.get(prereq) not in _DONE_STATUSES
            )
            if unsatisfied:
                status = RollupStatus.BLOCKED_BY_PREREQ
                detail = "waiting on: " + ", ".join(unsatisfied)
        phase = _phase_of(row)
        phases.setdefault(phase, []).append(
            TaskRollup(
                task_id=row.task.id,
                phase=phase,
                status=status,
                passed_graders=passed,
                total_graders=total,
                prerequisites=row.prerequisites,
                unsatisfied_prerequisites=unsatisfied,
                detail=detail,
            )
        )

    totals: Counter[str] = Counter()
    for tasks in phases.values():
        for task in tasks:
            totals[task.status.value] += 1

    return Rollup(
        phases=tuple(
            PhaseRollup(name=name, tasks=tuple(tasks))
            for name, tasks in phases.items()
        ),
        totals=dict(totals),
    )


# Width of the widest status label ("blocked_by_prereq"), so the status column
# aligns the way the plain ``status`` renderer pads its own state column.
_STATUS_WIDTH = max(len(s.value) for s in RollupStatus)


def _detail_for(task: TaskRollup) -> str:
    if task.status is RollupStatus.VERIFIED:
        return f"graders {task.passed_graders}/{task.total_graders} passed"
    return task.detail


def render_rollup_text(rollup: Rollup) -> str:
    """Render the rollup as an aligned, evidence-first text report."""
    lines: list[str] = [
        "rollup -- status is derived from grader evidence, not self-reported",
    ]
    if not rollup.phases:
        lines.append("(no tasks)")
        return "\n".join(lines)

    width = max(
        (len(t.task_id) for p in rollup.phases for t in p.tasks),
        default=0,
    )
    for phase in rollup.phases:
        lines.append("")
        lines.append(
            f"  {phase.name}  ({phase.verified}/{len(phase.tasks)} verified)"
        )
        for task in phase.tasks:
            detail = _detail_for(task)
            suffix = f"  {detail}" if detail else ""
            lines.append(
                f"    {task.status.value:<{_STATUS_WIDTH}}  "
                f"{task.task_id:<{width}}{suffix}"
            )

    lines.append("")
    ordered = [
        f"{status.value} {rollup.totals[status.value]}"
        for status in RollupStatus
        if rollup.totals.get(status.value)
    ]
    lines.append("  totals: " + ("  ".join(ordered) if ordered else "(none)"))
    return "\n".join(lines)


def rollup_to_json(rollup: Rollup) -> dict[str, Any]:
    """Serialize the rollup for ``status --rollup --json`` consumers.

    The shape is the stakeholder-legible read projection: a stable
    machine-readable face over the same evidence the text report renders.
    """
    return {
        "totals": rollup.totals,
        "phases": [
            {
                "name": phase.name,
                "verified": phase.verified,
                "task_count": len(phase.tasks),
                "tasks": [
                    {
                        "task_id": task.task_id,
                        "status": task.status.value,
                        "graders": {
                            "passed": task.passed_graders,
                            "total": task.total_graders,
                        },
                        "prerequisites": list(task.prerequisites),
                        "unsatisfied_prerequisites": list(
                            task.unsatisfied_prerequisites
                        ),
                        "detail": task.detail,
                    }
                    for task in phase.tasks
                ],
            }
            for phase in rollup.phases
        ],
    }

"""Pure data shape + collector for one dashboard frame.

The Textual app and the ``--json`` snapshot mode render the exact same
:class:`DashboardSnapshot` so the two surfaces stay field-for-field
parallel: one row per active run (RUNNING / VALIDATING /
AWAITING_APPROVAL) sourced from the public
:func:`flywheel_orchestrator.collect_live_rows` seam, plus a summary
header whose task-state counts mirror ``flywheel-orchestrate status``
against the same store.
"""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass
from datetime import datetime
from typing import Any

from flywheel.store_sqlite import SqliteStore
from flywheel_orchestrator import (
    LiveRunRow,
    WorkSource,
    WorkSourceError,
    collect_live_rows,
    status_rows_for_items,
)


@dataclass(frozen=True, kw_only=True)
class RowSnapshot:
    """One in-flight run's render-ready fields.

    Mirrors :class:`flywheel_orchestrator.LiveRunRow` minus the live
    ``last_ts`` (collapsed to ``age_seconds`` against the snapshot's
    ``now``) so the dashboard and ``--json`` surfaces consume the same
    flat shape.
    """

    run_id: str
    task_id: str
    status: str
    attempt: int | None
    iteration: int | None
    age_seconds: int | None
    tokens: int
    cost_usd: float
    turns: int
    iterations_completed: int
    last_kind: str
    last_detail: str
    awaiting_instruction: str | None


@dataclass(frozen=True, kw_only=True)
class SummaryData:
    """Summary header fields aggregated across the snapshot.

    ``task_counts`` is keyed by :class:`flywheel_orchestrator.TaskState`'s
    ``value`` (``fresh``/``in_progress``/``retryable``/``interrupted``/
    ``awaiting_approval``/``done``) and matches what
    ``flywheel-orchestrate status`` would emit against the same store
    and work source. Missing keys mean zero.
    """

    active_workers: int
    task_counts: Mapping[str, int]
    tokens_total: int
    cost_usd_total: float
    runtime_seconds: int


@dataclass(frozen=True, kw_only=True)
class DashboardSnapshot:
    """One full dashboard frame: header summary plus active-run rows.

    ``rows`` is sorted by ``task_id`` (the same ordering
    :func:`collect_live_rows` guarantees) so two consecutive snapshots
    over the same store are stable.
    """

    summary: SummaryData
    rows: tuple[RowSnapshot, ...]


def build_snapshot(
    store: SqliteStore,
    *,
    work_source: WorkSource | None = None,
    now: datetime,
    started_at: datetime,
) -> DashboardSnapshot:
    """Snapshot the store for one dashboard frame.

    ``work_source`` is consulted only for the summary header's task-state
    counts (matching ``flywheel-orchestrate status``); a missing or
    raising source degrades to an empty counts map so the live-rows
    half of the dashboard still renders. ``started_at`` and ``now`` are
    threaded explicitly so callers (the polling loop, tests) own time.
    """
    live = collect_live_rows(store)
    rows = tuple(_row_snapshot(r, now=now) for r in live)
    counts: dict[str, int] = {}
    if work_source is not None:
        try:
            for sr in status_rows_for_items(work_source.list_work(), store):
                counts[sr.state.value] = counts.get(sr.state.value, 0) + 1
        except WorkSourceError:
            # Counts degrade to empty rather than crashing the dashboard;
            # the active-run half is still useful on its own.
            counts = {}
    summary = SummaryData(
        active_workers=len(live),
        task_counts=counts,
        tokens_total=sum(r.tokens_total for r in live),
        cost_usd_total=sum(r.cost_usd_total for r in live),
        runtime_seconds=max(0, int((now - started_at).total_seconds())),
    )
    return DashboardSnapshot(summary=summary, rows=rows)


def _row_snapshot(row: LiveRunRow, *, now: datetime) -> RowSnapshot:
    if row.last_ts is None:
        age_s: int | None = None
    else:
        age_s = int((now - row.last_ts).total_seconds())
        if age_s < 0:
            # Clock skew between SQLite and the host reads as 0s rather
            # than a misleading negative — mirrors ``_format_live_line``.
            age_s = 0
    return RowSnapshot(
        run_id=row.run_id,
        task_id=row.task_id,
        status=row.status.value,
        attempt=row.attempt,
        iteration=row.iteration,
        age_seconds=age_s,
        tokens=row.tokens_total,
        cost_usd=row.cost_usd_total,
        turns=row.turns_total,
        iterations_completed=row.iterations_completed,
        last_kind=row.last_kind,
        last_detail=row.last_detail,
        awaiting_instruction=row.awaiting_instruction,
    )


def snapshot_to_dict(snapshot: DashboardSnapshot) -> dict[str, Any]:
    """Render a :class:`DashboardSnapshot` as plain JSON-encodable data.

    Field names are stable: scripts piping ``flywheel-tui --json`` rely
    on the same keys the Textual surface uses internally.
    """
    return {
        "summary": {
            "active_workers": snapshot.summary.active_workers,
            "task_counts": dict(snapshot.summary.task_counts),
            "tokens_total": snapshot.summary.tokens_total,
            "cost_usd_total": snapshot.summary.cost_usd_total,
            "runtime_seconds": snapshot.summary.runtime_seconds,
        },
        "rows": [
            {
                "run_id": r.run_id,
                "task_id": r.task_id,
                "status": r.status,
                "attempt": r.attempt,
                "iteration": r.iteration,
                "age_seconds": r.age_seconds,
                "tokens": r.tokens,
                "cost_usd": r.cost_usd,
                "turns": r.turns,
                "iterations_completed": r.iterations_completed,
                "last_kind": r.last_kind,
                "last_detail": r.last_detail,
                "awaiting_instruction": r.awaiting_instruction,
            }
            for r in snapshot.rows
        ],
    }

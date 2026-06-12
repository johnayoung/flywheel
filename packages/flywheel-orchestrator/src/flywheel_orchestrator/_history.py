"""Completed-run history: the read path over terminal lifecycles.

The live surfaces (``flywheel live``, the fw console) deliberately show
only in-flight runs; once a run reaches a terminal status it leaves
those views, and ``flywheel archive`` then moves the task files out of
``active/``. Nothing is lost — the store keeps every lifecycle, attempt,
event, and grader receipt — but until this module there was no way to
*list* that history. :func:`collect_history_rows` is that listing, and
:func:`collect_run_detail` is the drill-in for one run.

Grouping: a task retried across runs has several lifecycle rows; the
history surface shows one row per task (the most recently updated
terminal run) with the older runs carried as ``prior_runs`` so a
failed-then-succeeded task reads as one line, not noise.

Phase attribution: every run seeded through the orchestrator records the
work item's ``source_ref`` on the lifecycle (``lifecycles.source``,
schema v12). For the directory work source that is the task-file path,
and :func:`phase_from_source` derives the phase directory name from it.
Runs recorded before v12 have no source; callers may supply a
``fallback_phases`` map (see :func:`build_task_phase_index`, which scans
``active/`` and ``archive/``) so pre-migration history still groups.
"""

from __future__ import annotations

import json
from collections.abc import Mapping
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path, PurePath

from flywheel_core.lifecycle import Status
from flywheel_core.store_sqlite import SqliteStore
from flywheel_core.store_protocols import GraderResultRecord
from flywheel_core.task import Task

# A run is history once it can no longer move on its own. INTERRUPTED and
# AWAITING_APPROVAL are parked-but-live (an operator owes an action) and
# stay on the live surfaces instead.
TERMINAL_STATUSES: tuple[Status, ...] = (
    Status.DONE,
    Status.FAILED,
    Status.FAILED_VALIDATION,
)


@dataclass(frozen=True, kw_only=True)
class HistoryRun:
    """One terminal run's rolled-up summary.

    Totals sum the per-attempt aggregate columns the harness maintains at
    iteration boundaries — the same relational rollup ``collect_live_rows``
    reads, so live and history surfaces agree on a run's numbers.
    ``finished_at`` is the terminal transition's timestamp (falling back
    to the row's ``updated_at``); ``started_at`` is the earliest recorded
    transition.
    """

    run_id: str
    task_id: str
    status: Status
    source: str
    started_at: datetime | None
    finished_at: datetime | None
    retries: int
    error: str
    attempts: int
    tokens_total: int
    cost_usd_total: float
    turns_total: int


@dataclass(frozen=True, kw_only=True)
class HistoryRow:
    """One task's history line: the latest terminal run plus its priors.

    ``phase`` is the derived grouping label (``None`` when the source is
    absent or not phase-shaped). ``prior_runs`` are older terminal runs
    of the same task, most recent first.
    """

    task_id: str
    phase: str | None
    latest: HistoryRun
    prior_runs: tuple[HistoryRun, ...] = ()


@dataclass(frozen=True, kw_only=True)
class RunDetail:
    """Everything ``show`` renders for one run.

    ``task`` is the exact definition the run pinned (content-addressed);
    ``None`` when the pin cannot resolve. ``grader_results`` are the
    receipts of the latest attempt. ``related_runs`` are the task's other
    runs (any status), most recent first, so the operator sees the full
    retry story from one screen.
    """

    run: HistoryRun
    phase: str | None
    task: Task | None
    agent_output: str
    attempts: tuple[AttemptSummary, ...]
    grader_results: tuple[GraderResultRecord, ...]
    related_runs: tuple[HistoryRun, ...]


@dataclass(frozen=True, kw_only=True)
class AttemptSummary:
    """One attempt's render-ready rollup for the ``show`` surface."""

    number: int
    outcome: str
    started_at: datetime | None
    ended_at: datetime | None
    iterations: int
    turns: int
    tokens: int
    cost_usd: float
    error: str


def phase_from_source(source: str | None) -> str | None:
    """Derive the phase directory name from a recorded source label.

    Only path-shaped sources from the directory layout
    (``.../active/<phase>/<task>.json`` or the post-archive
    ``.../archive/<phase>/<task>.json``) yield a phase: the file's
    parent directory name. Tracker refs (``owner/repo#123``) and absent
    sources return ``None`` — the caller renders those ungrouped.
    """
    if not source:
        return None
    path = PurePath(source)
    if path.suffix != ".json":
        return None
    parent = path.parent.name
    if parent in ("", "active", "archive", "tasks"):
        return None
    return parent


def build_task_phase_index(tasks_dir: Path) -> dict[str, str]:
    """Map task id -> phase by scanning ``active/`` and ``archive/``.

    The fallback for runs recorded before ``lifecycles.source`` existed
    (schema v12): their lifecycle rows carry no provenance, but for the
    directory layout the task files themselves still sit in (or were
    archived into) a phase directory. Archive is scanned first so an
    active re-issue of the same task id wins. Unreadable or id-less
    files are skipped — the index is best-effort by design.
    """
    index: dict[str, str] = {}
    for bucket in ("archive", "active"):
        root = tasks_dir / bucket
        if not root.is_dir():
            continue
        for phase_dir in sorted(root.iterdir()):
            if not phase_dir.is_dir() or phase_dir.name.startswith("."):
                continue
            for entry in sorted(phase_dir.iterdir()):
                if (
                    not entry.is_file()
                    or entry.suffix != ".json"
                    or entry.name.startswith("_")
                    or entry.name.startswith(".")
                ):
                    continue
                try:
                    data = json.loads(entry.read_text(encoding="utf-8"))
                except (OSError, json.JSONDecodeError):
                    continue
                task_id = (
                    data.get("id") if isinstance(data, dict) else None
                )
                if isinstance(task_id, str) and task_id:
                    index[task_id] = phase_dir.name
    return index


def _parse_ts(value: object) -> datetime | None:
    if not isinstance(value, str):
        return None
    try:
        return datetime.fromisoformat(value)
    except ValueError:
        return None


def _timestamps(timestamps_json: object) -> dict[str, datetime]:
    if not isinstance(timestamps_json, str) or not timestamps_json:
        return {}
    try:
        payload = json.loads(timestamps_json)
    except json.JSONDecodeError:
        return {}
    if not isinstance(payload, Mapping):
        return {}
    out: dict[str, datetime] = {}
    for key, value in payload.items():
        ts = _parse_ts(value)
        if ts is not None:
            out[str(key)] = ts
    return out


def _attempt_rollups(
    store: SqliteStore, run_id: str
) -> tuple[int, int, float, int]:
    """``(attempts, tokens_total, cost_usd_total, turns_total)`` for a run."""
    row = store._connection.execute(  # noqa: SLF001 — read-side, like collect_live_rows
        """
        SELECT COUNT(*) AS attempts,
               COALESCE(SUM(input_tokens + output_tokens
                   + cache_creation_input_tokens
                   + cache_read_input_tokens), 0) AS tokens,
               COALESCE(SUM(total_cost_usd), 0.0) AS cost,
               COALESCE(SUM(turns), 0) AS turns
        FROM attempts
        WHERE run_id = ?
        """,
        (run_id,),
    ).fetchone()
    return (
        int(row["attempts"]),
        int(row["tokens"]),
        float(row["cost"]),
        int(row["turns"]),
    )


def _history_run_from_row(
    store: SqliteStore, row: Mapping[str, object]
) -> HistoryRun:
    status = Status(str(row["status"]))
    stamps = _timestamps(row["timestamps_json"])
    finished = stamps.get(status.value) or _parse_ts(row["updated_at"])
    started = min(stamps.values()) if stamps else None
    attempts, tokens, cost, turns = _attempt_rollups(
        store, str(row["run_id"])
    )
    return HistoryRun(
        run_id=str(row["run_id"]),
        task_id=str(row["task_id"]),
        status=status,
        source=str(row["source"] or ""),
        started_at=started,
        finished_at=finished,
        retries=int(row["retries"] or 0),
        error=str(row["error"] or ""),
        attempts=attempts,
        tokens_total=tokens,
        cost_usd_total=cost,
        turns_total=turns,
    )


def _select_lifecycles(
    store: SqliteStore,
    *,
    statuses: tuple[Status, ...],
    task_id: str | None = None,
) -> list[Mapping[str, object]]:
    placeholders = ", ".join("?" for _ in statuses)
    params: list[object] = [s.value for s in statuses]
    task_clause = ""
    if task_id is not None:
        task_clause = "AND task_id = ?"
        params.append(task_id)
    cursor = store._connection.execute(  # noqa: SLF001 — read-side, like collect_live_rows
        f"""
        SELECT run_id, task_id, status, retries, error, source,
               timestamps_json, updated_at
        FROM lifecycles
        WHERE status IN ({placeholders}) {task_clause}
        ORDER BY updated_at DESC, run_id DESC
        """,
        params,
    )
    return cursor.fetchall()


def collect_history_rows(
    store: SqliteStore,
    *,
    statuses: tuple[Status, ...] = TERMINAL_STATUSES,
    phase: str | None = None,
    limit: int = 0,
    fallback_phases: Mapping[str, str] | None = None,
) -> list[HistoryRow]:
    """List finished work, one row per task, most recently finished first.

    ``statuses`` narrows which terminal states qualify; ``phase`` keeps
    only rows whose derived (or fallback) phase matches; ``limit`` caps
    the number of *task rows* returned (0 = no cap). ``fallback_phases``
    supplies task-id -> phase attribution for pre-v12 runs whose
    lifecycle carries no source (see :func:`build_task_phase_index`).
    """
    rows: dict[str, HistoryRow] = {}
    priors: dict[str, list[HistoryRun]] = {}
    for db_row in _select_lifecycles(store, statuses=statuses):
        run = _history_run_from_row(store, db_row)
        existing = rows.get(run.task_id)
        if existing is None:
            derived = phase_from_source(run.source)
            if derived is None and fallback_phases is not None:
                derived = fallback_phases.get(run.task_id)
            rows[run.task_id] = HistoryRow(
                task_id=run.task_id, phase=derived, latest=run
            )
            priors[run.task_id] = []
        else:
            priors[run.task_id].append(run)

    merged: list[HistoryRow] = []
    for task_id, row in rows.items():
        if priors[task_id]:
            row = HistoryRow(
                task_id=row.task_id,
                phase=row.phase,
                latest=row.latest,
                prior_runs=tuple(priors[task_id]),
            )
        if phase is not None and row.phase != phase:
            continue
        merged.append(row)
    # Most recently *finished* first. The SQL orders by updated_at (the
    # row's last write) which tracks the terminal transition in
    # production, but the terminal timestamp itself is the honest sort
    # key; rows without one sink to the bottom.
    floor = datetime.min.replace(tzinfo=timezone.utc)
    merged.sort(
        key=lambda r: r.latest.finished_at or floor, reverse=True
    )
    if limit:
        return merged[:limit]
    return merged


def resolve_run_id(store: SqliteStore, run_or_task_id: str) -> str | None:
    """Resolve a ``show`` argument to a run id.

    A literal run id wins; otherwise the argument is treated as a task id
    and the task's most recently updated lifecycle (any status) is
    chosen. ``None`` when neither resolves.
    """
    if store.load_lifecycle(run_or_task_id) is not None:
        return run_or_task_id
    row = store._connection.execute(  # noqa: SLF001 — read-side, like collect_live_rows
        """
        SELECT run_id FROM lifecycles
        WHERE task_id = ?
        ORDER BY updated_at DESC, run_id DESC
        LIMIT 1
        """,
        (run_or_task_id,),
    ).fetchone()
    return str(row["run_id"]) if row is not None else None


def collect_run_detail(
    store: SqliteStore,
    run_id: str,
    *,
    fallback_phases: Mapping[str, str] | None = None,
) -> RunDetail | None:
    """Assemble the full ``show`` payload for one run, or ``None``.

    Unlike :func:`collect_history_rows` this accepts any status — an
    operator may inspect a parked or in-flight run too; the surface
    renders whatever the row says.
    """
    lifecycle = store.load_lifecycle(run_id)
    if lifecycle is None:
        return None
    db_rows = store._connection.execute(  # noqa: SLF001 — read-side, like collect_live_rows
        """
        SELECT run_id, task_id, status, retries, error, source,
               timestamps_json, updated_at
        FROM lifecycles
        WHERE run_id = ?
        """,
        (run_id,),
    ).fetchall()
    run = _history_run_from_row(store, db_rows[0])

    derived = phase_from_source(run.source)
    if derived is None and fallback_phases is not None:
        derived = fallback_phases.get(run.task_id)

    attempts = tuple(
        AttemptSummary(
            number=a.number,
            outcome=a.outcome.value if a.outcome is not None else "(open)",
            started_at=a.started_at,
            ended_at=a.ended_at,
            iterations=a.iterations_completed,
            turns=a.turns,
            tokens=a.total_tokens,
            cost_usd=a.total_cost_usd,
            error=a.error,
        )
        for a in lifecycle.attempts
    )
    grader_results: tuple[GraderResultRecord, ...] = ()
    if lifecycle.attempts:
        grader_results = tuple(
            store.list_grader_results(
                run_id, lifecycle.attempts[-1].number
            )
        )

    related = tuple(
        _history_run_from_row(store, row)
        for row in _select_lifecycles(
            store,
            statuses=tuple(Status),
            task_id=run.task_id,
        )
        if str(row["run_id"]) != run_id
    )

    return RunDetail(
        run=run,
        phase=derived,
        task=store.load_task_for_run(run_id),
        agent_output=lifecycle.agent_output,
        attempts=attempts,
        grader_results=grader_results,
        related_runs=related,
    )


__all__ = [
    "TERMINAL_STATUSES",
    "AttemptSummary",
    "HistoryRow",
    "HistoryRun",
    "RunDetail",
    "build_task_phase_index",
    "collect_history_rows",
    "collect_run_detail",
    "phase_from_source",
    "resolve_run_id",
]

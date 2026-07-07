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
from collections.abc import Collection, Mapping
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path, PurePath
from typing import Protocol

from flywheel_core.lifecycle import Attempt, Lifecycle, Status
from flywheel_core.store_protocols import GraderResultRecord
from flywheel_core.task import Task


class _HistoryReadStore(Protocol):
    """Backend-agnostic read surface the history functions consume.

    The orchestrator history path reads cross-task lifecycles, per-run
    attempts, the latest attempt's grader receipts, and the run's pinned
    task — all through public protocol methods that every concrete store
    (SQLite and Postgres) implements, never the SQLite-only private
    connection. Typing on this structural surface (rather than
    ``SqliteStore``) is what lets ``collect_history_rows`` / ``resolve_run_id``
    / ``collect_run_detail`` run unchanged against a ``PostgresStore``.
    """

    def list_lifecycles(
        self,
        *,
        statuses: Collection[Status] | None = None,
        task_id: str | None = None,
    ) -> list[Lifecycle]: ...

    def load_lifecycle(self, run_id: str) -> Lifecycle | None: ...

    def list_attempts(self, run_id: str) -> list[Attempt]: ...

    def list_grader_results(
        self, run_id: str, attempt_number: int
    ) -> list[GraderResultRecord]: ...

    def load_task_for_run(self, run_id: str) -> Task | None: ...

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
    signal — the folded transition stamps and the first attempt's start.
    (Per-status stamps are overwritten when a retry re-enters a status,
    so on a retried run only the attempt record still carries the true
    start.)
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
    ``None`` when the pin cannot resolve. ``grader_results`` are *every*
    attempt's receipts, ordered by attempt then ordinal (each record
    carries its own ``attempt_number``); the per-attempt split lives on
    each ``AttemptSummary.grader_results`` so a retried run shows the
    verdicts of the discarded attempt too, not only the last. ``related_runs``
    are the task's other runs (any status), most recent first, so the
    operator sees the full retry story from one screen.
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
    """One attempt's render-ready rollup for the ``show`` surface.

    ``grader_results`` are that attempt's own receipts in ordinal order
    (empty when the attempt was finalized before grading — e.g. a protocol
    failure). Keying each receipt set to its own attempt is what lets the
    run detail carry every attempt's verdicts, not only the last.
    """

    number: int
    outcome: str
    started_at: datetime | None
    ended_at: datetime | None
    iterations: int
    turns: int
    tokens: int
    cost_usd: float
    error: str
    grader_results: tuple[GraderResultRecord, ...] = ()


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


def _attempt_rollups(
    store: _HistoryReadStore, run_id: str
) -> tuple[int, int, float, int, datetime | None]:
    """``(attempts, tokens_total, cost_usd_total, turns_total,
    first_started_at)`` for a run.

    Sums the per-attempt rolled-up counters through the backend-agnostic
    ``list_attempts`` seam so the rollup is identical on SQLite and
    Postgres. ``tokens_total`` is the four token columns summed across the
    run's attempts (``Attempt.total_tokens`` per attempt); ``cost`` and
    ``turns`` are the matching per-attempt sums. ``first_started_at`` is
    attempt 1's start (``list_attempts`` orders by number), ``None`` when
    the run recorded no attempts.
    """
    attempts = store.list_attempts(run_id)
    tokens = sum(a.total_tokens for a in attempts)
    cost = sum(a.total_cost_usd for a in attempts)
    turns = sum(a.turns for a in attempts)
    first_started = attempts[0].started_at if attempts else None
    return (len(attempts), tokens, float(cost), turns, first_started)


def _history_run_from_row(
    store: _HistoryReadStore, lifecycle: Lifecycle
) -> HistoryRun:
    status = lifecycle.status
    stamps = lifecycle.timestamps
    # The terminal transition's timestamp is the honest finish instant; for
    # a folded Lifecycle the per-status stamps are the only timestamp source
    # (the store-set updated_at column is not a Lifecycle field). A terminal
    # run always records its terminal status' stamp, so this resolves in
    # practice; an unstamped row degrades finished_at to None.
    finished = stamps.get(status)
    attempts, tokens, cost, turns, first_attempt_started = _attempt_rollups(
        store, lifecycle.run_id
    )
    # The folded per-status stamps are overwritten when a retry re-enters
    # a status (READY/RUNNING), so min(stamps) alone reports the *latest*
    # attempt's start on a retried run. Attempt 1's start is the honest
    # run start; take the earliest signal available.
    candidates = list(stamps.values())
    if first_attempt_started is not None:
        candidates.append(first_attempt_started)
    started = min(candidates) if candidates else None
    return HistoryRun(
        run_id=lifecycle.run_id,
        task_id=lifecycle.task_id,
        status=status,
        source=lifecycle.source or "",
        started_at=started,
        finished_at=finished,
        retries=lifecycle.retries,
        error=lifecycle.error or "",
        attempts=attempts,
        tokens_total=tokens,
        cost_usd_total=cost,
        turns_total=turns,
    )


def _select_lifecycles(
    store: _HistoryReadStore,
    *,
    statuses: tuple[Status, ...],
    task_id: str | None = None,
) -> list[Lifecycle]:
    # Cross-task lifecycle read through the public protocol surface (SI-3),
    # off the private connection. list_lifecycles already returns rows in
    # (updated_at DESC, run_id DESC) order across every backend, so the
    # latest-for-a-task pick and the history ordering are preserved exactly.
    return store.list_lifecycles(statuses=statuses, task_id=task_id)


def collect_history_rows(
    store: _HistoryReadStore,
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
    for lifecycle in _select_lifecycles(store, statuses=statuses):
        run = _history_run_from_row(store, lifecycle)
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


def resolve_run_id(store: _HistoryReadStore, run_or_task_id: str) -> str | None:
    """Resolve a ``show`` argument to a run id.

    A literal run id wins; otherwise the argument is treated as a task id
    and the task's most recently updated lifecycle (any status) is
    chosen. ``None`` when neither resolves.
    """
    if store.load_lifecycle(run_or_task_id) is not None:
        return run_or_task_id
    # Treat the argument as a task id: the most-recently-updated lifecycle
    # (any status) is the first element of the (updated_at DESC, run_id DESC)
    # ordered list_lifecycles result for that task.
    lifecycles = store.list_lifecycles(task_id=run_or_task_id)
    return lifecycles[0].run_id if lifecycles else None


def collect_run_detail(
    store: _HistoryReadStore,
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
    run = _history_run_from_row(store, lifecycle)

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
            grader_results=tuple(
                store.list_grader_results(run_id, a.number)
            ),
        )
        for a in lifecycle.attempts
    )
    # Every attempt's receipts, not just the last one's (F2/D-4). Each
    # record already carries its own attempt_number, so the flat
    # concatenation stays keyed by attempt; the per-attempt split lives on
    # each AttemptSummary above for callers that render by attempt.
    grader_results: tuple[GraderResultRecord, ...] = tuple(
        g for a in attempts for g in a.grader_results
    )

    related = tuple(
        _history_run_from_row(store, other)
        for other in _select_lifecycles(
            store,
            statuses=tuple(Status),
            task_id=run.task_id,
        )
        if other.run_id != run_id
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

"""Multi-task scheduling, phase/queue management, live dashboard, and the
orchestrator's argparse entry point — the consumer layer above the
single-task loop.

Extracted verbatim from ``flywheel_core.workflow`` in the core/consumer split.
Verbs are now exposed through the unified product shell
(``flywheel status`` / ``flywheel live`` / ``flywheel init`` / ...),
which calls :func:`main` here in-process. Module-level plumbing remains
runnable as ``python -m flywheel_orchestrator._workflow``.

Shared single-task helpers are imported from ``flywheel_core.workflow`` (the
dependency arrow points orchestrator -> core, never the reverse).
"""

from __future__ import annotations

import argparse
import asyncio
import json
import re
import shutil
import subprocess
import sys
import time
import tomllib
from collections.abc import (
    Callable,
    Collection,
    Iterable,
    Mapping,
)
from dataclasses import dataclass
from datetime import datetime, timezone
from enum import Enum
from pathlib import Path
from typing import TYPE_CHECKING, Any, Protocol


from flywheel_core.harness import (
    RecheckOutcome,
    recheck_blocked_lifecycle,
)
from flywheel_core.event_serde import event_kind, event_payload
from flywheel_core.events import (
    LANDING_STRAND_KINDS,
    DomainEvent,
    Landed,
    LandingParked,
)
from flywheel_core.lifecycle import Attempt, Lifecycle, Status
from flywheel_core.loaders import TaskLoadError, load_task_file
from flywheel_core.loop_path_marker import LoopPathSignal, detect_loop_path_signals
from flywheel_core.redaction import Redactor, default_policy
from flywheel_core.store_protocols import EventRecord, GraderResultRecord
from flywheel_core.store_sqlite import SqliteStore

if TYPE_CHECKING:
    # Optional postgres backend, typing-only so this module never hard-depends
    # on the psycopg extra. The store factory returns SqliteStore |
    # PostgresStore and both answer these reads through the store protocol.
    from flywheel_core.store_postgres import PostgresStore
    from flywheel_orchestrator._claims_postgres import PostgresClaimStore
from flywheel_core.telemetry_file import FileTelemetrySink
from flywheel_core.task import ManualGrader, Task
from flywheel_core.validation import TaskDefect, validate_task
from flywheel_core.workflow import (
    DEFAULT_MAX_RETRIES,
    DEFAULT_MAX_TURNS,
    _add_common_db,
    _has_done_lifecycle,
    _resolve_db,
    _short,
    recover_stranded_lifecycles,
)
from flywheel_orchestrator._claims import (
    RESOLUTION_ATTRIBUTION_OPERATOR,
    RESOLUTION_ATTRIBUTION_PROBE,
    RESOLUTION_ATTRIBUTIONS,
    STOP_INDETERMINATE_LANDING,
    STOP_RESOLVED,
    OrchestratorStopEventRecord,
    SqliteClaimStore,
)
from flywheel_orchestrator._history import (
    TERMINAL_STATUSES,
    HistoryRow,
    HistoryRun,
    RunDetail,
    build_task_phase_index,
    collect_history_rows,
    collect_run_detail,
    resolve_run_id,
)
from flywheel_orchestrator._policy import (
    DEFAULT_POLICY_FILENAME,
    PolicyError,
    WorkPolicy,
    build_work_source,
    load_policy,
    resolve_sandbox_root,
)
from flywheel_orchestrator._skills import (
    DEFAULT_SKILLS_ROOT,
    SKILL_NAMES,
    install_skills,
    settings_from_policy,
)
from flywheel_orchestrator._sources import (
    DirectoryWorkSource,
    WorkItem,
    WorkSource,
    WorkSourceError,
    iter_active_phase_dirs,
    load_active_tasks,
)
from flywheel_orchestrator._surface_lint import (
    build_surface,
    surface_overlap_defects,
)
from flywheel_orchestrator._store_factory import (
    PG_DSN_ENV,
    PG_DSN_FALLBACK_ENV,
    build_claim_store,
    open_sqlite_bound_store,
    resolve_postgres_dsn,
)
from flywheel_orchestrator._pg_preflight import (
    format_report,
    run_postgres_preflight,
)

DEFAULT_TASKS_DIR = Path(".flywheel/tasks")

#: The provenance trailer key ``fw show`` reads back off a landed commit to
#: resolve its producing run (spec 00078, D-3). This mirrors the authoritative
#: definition ``flywheel_worktree._trailers.TRAILER_KEY_RUN`` — the stamping
#: engine that writes the trailer lives one layer up (worktree depends on the
#: orchestrator, never the reverse), so the lookup cannot import it. The two are
#: pinned equal by ``test_show_commit_lookup`` so the shared vocabulary cannot
#: drift; do not re-derive the value here from anything else.
TRAILER_KEY_RUN = "Flywheel-Run"


class TaskState(str, Enum):
    """Task-level status derived from the latest lifecycle, if any."""

    FRESH = "fresh"  # never attempted
    IN_PROGRESS = "in_progress"  # active lifecycle exists
    RETRYABLE = "retryable"  # last lifecycle failed
    INTERRUPTED = "interrupted"  # last lifecycle paused for operator
    AWAITING_APPROVAL = "awaiting_approval"  # parked on a manual gate
    DONE = "done"  # at least one lifecycle reached DONE

@dataclass(frozen=True, kw_only=True)
class TaskStatusRow:
    """Per-task status snapshot used by ``status`` and ``next`` reporting.

    ``blocked_requires`` carries the raw ``blocked_requires_json`` column
    value from the latest lifecycle verbatim (no parsing). ``None`` means
    either the lifecycle never blocked with structured requires, or the
    lifecycle is in a state where the snapshot was cleared (e.g. resumed
    READY/RUNNING/DONE). The status surface decodes it lazily.

    ``awaiting_manual_ordinal`` mirrors the latest lifecycle column of the
    same name verbatim. It is the index in ``task.graders`` of the manual
    gate the lifecycle is currently parked on; ``None`` in every state
    except ``AWAITING_APPROVAL``. Surfaces use it (together with the
    in-row ``task``) to render the pending gate's instruction.

    ``prerequisites`` is the task's dependency edges (other task ids that
    must reach DONE first). This is an orchestration-layer concept — not part
    of the core ``Task`` definition — so the orchestrator reads it from the
    task source and carries it on the row, where ``select_next_task`` consults
    it.

    ``source_ref`` is the work source's opaque handle for the item (see
    :class:`flywheel_orchestrator._sources.WorkItem`). ``task_file`` is the
    on-disk path for file-backed sources and an empty ``Path()`` otherwise —
    path-deriving consumers must treat an empty path as "no file".

    ``priority`` and ``required_capabilities`` mirror the item's
    orchestration-layer scheduling metadata (spec 00049), carried on the row
    so :func:`select_next_task` can offer ready items highest-priority-first
    and withhold an item whose ``required_capabilities`` is not a subset of
    the worker's advertised capability set. Both default (priority ``0``,
    empty set) so a default-metadata row sorts to its original walk order and
    is selectable by any worker.
    """

    task_file: Path
    task: Task
    state: TaskState
    latest_run_id: str | None
    latest_status: Status | None
    latest_error: str
    blocked_requires: str | None = None
    awaiting_manual_ordinal: int | None = None
    prerequisites: tuple[str, ...] = ()
    source_ref: str = ""
    priority: int = 0
    required_capabilities: frozenset[str] = frozenset()

_ACTIVE_STATUSES: frozenset[Status] = frozenset(
    {Status.READY, Status.RUNNING, Status.VALIDATING}
)

def _latest_lifecycle_row(
    store: SqliteStore | PostgresStore, task_id: str
) -> tuple[str, Status, str, str | None, int | None] | None:
    """Return ``(run_id, status, error, blocked_requires_json,
    awaiting_manual_ordinal)`` of the most recent lifecycle for
    ``task_id``, or ``None`` if no lifecycle exists.

    The by-task-id lookup goes through the public ``list_lifecycles``
    surface (SI-3): the most-recently-updated lifecycle for a task is the
    first element of the ``(updated_at DESC, run_id DESC)`` ordered result
    ``list_lifecycles(task_id=...)`` returns on every backend.
    ``blocked_requires_json`` is returned verbatim so callers can decide
    whether to parse it (text status flattens, JSON status emits the
    decoded list). ``awaiting_manual_ordinal`` is the index in
    ``task.graders`` of the manual gate a parked ``AWAITING_APPROVAL``
    lifecycle is pinned to; ``None`` in every other state.
    """
    lifecycles = store.list_lifecycles(task_id=task_id)
    if not lifecycles:
        return None
    lc = lifecycles[0]
    return (
        lc.run_id,
        lc.status,
        lc.error or "",
        lc.blocked_requires_json,
        lc.awaiting_manual_ordinal,
    )

def task_state(store: SqliteStore | PostgresStore, task: Task) -> TaskStatusRow:
    """Classify ``task`` based on its lifecycle history in ``store``."""
    latest = _latest_lifecycle_row(store, task.id)
    if latest is None:
        return TaskStatusRow(
            task_file=Path(),
            task=task,
            state=TaskState.FRESH,
            latest_run_id=None,
            latest_status=None,
            latest_error="",
            blocked_requires=None,
            awaiting_manual_ordinal=None,
        )
    run_id, status, error, blocked_requires, awaiting_ordinal = latest

    if _has_done_lifecycle(store, task.id):
        state = TaskState.DONE
    elif status in _ACTIVE_STATUSES:
        state = TaskState.IN_PROGRESS
    elif status == Status.AWAITING_APPROVAL:
        # Distinct classification: a parked manual gate is not generic
        # IN_PROGRESS — an operator owes a decision, and the renderer
        # surfaces the gate's instruction so the owed action is visible.
        state = TaskState.AWAITING_APPROVAL
    elif status == Status.INTERRUPTED:
        state = TaskState.INTERRUPTED
    elif status in (
        Status.FAILED,
        Status.FAILED_VALIDATION,
        Status.INTERNAL_ERROR,
    ):
        # A persisted INTERNAL_ERROR is a dead-worker strand (run_task's own
        # loop never exits in this state — it retries or walks to FAILED), so
        # it is recoverable exactly like FAILED_VALIDATION. Without this it
        # falls to the IN_PROGRESS catch-all below: never re-selected, never
        # finalized by the RUNNING/VALIDATING stranded sweep, and its phase
        # never archives -- the task wedges forever.
        state = TaskState.RETRYABLE
    elif status == Status.PENDING:
        state = TaskState.IN_PROGRESS
    else:
        # DONE is handled above; anything else is a defensive fallback.
        state = TaskState.IN_PROGRESS
    return TaskStatusRow(
        task_file=Path(),
        task=task,
        state=state,
        latest_run_id=run_id,
        latest_status=status,
        latest_error=error,
        blocked_requires=blocked_requires,
        awaiting_manual_ordinal=awaiting_ordinal,
    )

def status_rows_for_items(
    items: Iterable[WorkItem], store: SqliteStore | PostgresStore
) -> list[TaskStatusRow]:
    """Classify each work item's task against ``store``, in item order.

    The source-agnostic core of :func:`build_status_rows`: items come from
    any :class:`~flywheel_orchestrator._sources.WorkSource`; the row carries
    the item's ``source_ref`` and (for file-backed sources) its
    ``local_path`` so downstream consumers keep their path-derived behavior.
    """
    rows: list[TaskStatusRow] = []
    for item in items:
        snapshot = task_state(store, item.task)
        rows.append(
            TaskStatusRow(
                task_file=item.local_path if item.local_path else Path(),
                task=snapshot.task,
                state=snapshot.state,
                latest_run_id=snapshot.latest_run_id,
                latest_status=snapshot.latest_status,
                latest_error=snapshot.latest_error,
                blocked_requires=snapshot.blocked_requires,
                awaiting_manual_ordinal=snapshot.awaiting_manual_ordinal,
                prerequisites=item.prerequisites,
                source_ref=item.source_ref,
                priority=item.priority,
                required_capabilities=item.required_capabilities,
            )
        )
    return rows

def build_status_rows(
    tasks_dir: Path, store: SqliteStore | PostgresStore
) -> list[TaskStatusRow]:
    """Walk active tasks and return their classified status, in walk order."""
    return status_rows_for_items(
        DirectoryWorkSource(tasks_dir).list_work(), store
    )

def select_next_task(
    rows: Iterable[TaskStatusRow],
    *,
    exclude_ids: frozenset[str] = frozenset(),
    worker_capabilities: frozenset[str] = frozenset(),
    satisfied_prerequisites: frozenset[str] = frozenset(),
) -> TaskStatusRow | None:
    """Pick the highest-priority eligible task from ``rows``.

    A task is eligible when:

    * its ``id`` is not in ``exclude_ids``, AND
    * its state is :attr:`TaskState.FRESH`, :attr:`TaskState.RETRYABLE`,
      or :attr:`TaskState.INTERRUPTED`, AND
    * every prerequisite is satisfied — either a listed task (by ``id``)
      with state :attr:`TaskState.DONE`, or an id present in
      ``satisfied_prerequisites``, AND
    * its ``required_capabilities`` is a subset of ``worker_capabilities``.

    Among the eligible candidates the offer order is descending ``priority``,
    ties broken by walk (enumeration) order via a *stable* sort (spec 00049,
    decision D-1). With every candidate at the default priority (0) this
    reduces to the original first-eligible-in-walk-order pick exactly, so an
    all-default set is byte-identical to the pre-feature behavior.

    Tasks whose prerequisites are missing from the workspace are treated
    as ineligible so a dangling reference never silently runs — unless the
    missing id is named in ``satisfied_prerequisites``.

    ``satisfied_prerequisites`` is a caller-supplied set of prerequisite ids
    the store already resolves to DONE even though no listed row provides
    them — a prerequisite whose defining task has left the work-source
    listing (e.g. its phase archived) while its DONE lifecycle remains in the
    authoritative store. The caller performs the store read (only for ids not
    present among ``rows``, so no per-listed-task read is added) and hands the
    result in as data, mirroring ``exclude_ids``'s caller-supplied precedent.
    The default empty set keeps a fully-listed graph byte-identical.

    ``exclude_ids`` removes tasks from *candidacy* without removing them
    from the prerequisite-resolution map, so a caller (e.g. the
    orchestrator) can skip an already-attempted or still-blocked task while
    that task can still satisfy a dependent's prerequisite. The default
    empty set preserves the pull-based CLI's behavior exactly.

    ``worker_capabilities`` is the worker's advertised capability set (spec
    00049, decision D-2). An item with empty ``required_capabilities`` is
    selectable by any worker, including one with an empty set; the default
    empty set preserves today's behavior for every existing zero-requirement
    item. The same filter applies in both execution modes.

    Interrupted tasks resume because ``run_task`` normalizes an entry-time
    ``INTERRUPTED`` lifecycle back to ``READY`` (see harness ``run_task``
    and docs/task-lifecycle.md): the interrupt preserves retry budget and
    parks the worktree for reuse, it does not require operator unblock.
    """
    by_id: dict[str, TaskStatusRow] = {row.task.id: row for row in rows}
    eligible_states = (
        TaskState.FRESH,
        TaskState.RETRYABLE,
        TaskState.INTERRUPTED,
    )
    candidates: list[TaskStatusRow] = []
    for row in by_id.values():
        if row.task.id in exclude_ids:
            continue
        if row.state not in eligible_states:
            continue
        if not row.required_capabilities <= worker_capabilities:
            continue
        if not all(
            (
                (dep := by_id.get(prereq_id)) is not None
                and dep.state == TaskState.DONE
            )
            or prereq_id in satisfied_prerequisites
            for prereq_id in row.prerequisites
        ):
            continue
        candidates.append(row)
    if not candidates:
        return None
    # Stable descending-priority sort: equal-priority candidates keep their
    # walk order, so an all-default (priority 0) set returns the first
    # eligible candidate exactly as before.
    candidates.sort(key=lambda row: row.priority, reverse=True)
    return candidates[0]

def satisfied_prerequisites_from_store(
    rows: Iterable[TaskStatusRow],
    store: SqliteStore | PostgresStore,
) -> frozenset[str]:
    """Prerequisite ids absent from ``rows`` yet DONE in ``store``.

    The store is the authoritative record of completion (docs/data-taxonomy);
    the source listing is an input surface, not the record. A prerequisite
    whose defining task has left the listing — e.g. its phase archived, moving
    the task JSON out of ``active/`` — is still satisfied when its lifecycle
    reached :attr:`Status.DONE`.

    Only ids that no listed row provides are consulted: a listed
    prerequisite's DONE-ness is already folded into its row state by
    :func:`task_state` (which classifies DONE through the same
    ``_has_done_lifecycle`` authority), so this extends that authority to
    unlisted ids without adding a per-listed-task store read to the pass. A
    DONE lifecycle is the sole satisfier — a FAILED / RUNNING / INTERRUPTED /
    absent id is never returned — so the result is exactly the set a caller
    may hand to :func:`select_next_task` /
    :meth:`WorkGraph.ready_set` as ``satisfied_prerequisites`` and use to drop
    a store-DONE edge from the dangling-prerequisite re-driver's issues.
    """
    materialized = list(rows)
    listed_ids = {row.task.id for row in materialized}
    candidates = {
        prereq_id
        for row in materialized
        for prereq_id in row.prerequisites
        if prereq_id not in listed_ids
    }
    return frozenset(
        prereq_id
        for prereq_id in candidates
        if _has_done_lifecycle(store, prereq_id)
    )


@dataclass(frozen=True, kw_only=True)
class PrerequisiteReachabilityHold:
    """A dependent held because a DONE prerequisite's landed work is not yet
    reachable from the base the dependent would branch from (spec 00079, #7).

    ``blocking_phase`` names the prerequisite's phase whose integration branch
    has not merged into the true base -- the phase whose PR must merge before
    the dependent becomes claimable. ``held_by`` lists the DONE-but-unreachable
    prerequisite ids driving the hold, in the dependent's prerequisite order.
    The hold is a scheduling verdict only: it never parks, fails, or consumes
    the task, which is re-offered the first pass after reachability holds.
    """

    task_id: str
    blocking_phase: str
    held_by: tuple[str, ...]


def _phase_of_row(row: TaskStatusRow) -> str:
    """The phase directory name a file-backed row belongs to, or ``""``.

    Mirrors the phase key the archive sweep derives from ``active/<phase>``:
    for a file-backed row it is the parent directory name; a pathless row
    (external source) has no phase directory, so no ``flywheel/phase/<phase>``
    branch exists to resolve against and the reachability hold does not apply.
    """
    return row.task_file.parent.name if row.task_file != Path() else ""


def _phase_branches_present(repo_root: Path) -> bool:
    """True when any ``flywheel/phase/<phase>`` integration branch exists.

    The phase strategy is the only landing strategy that creates these refs
    (spec 00079); under merge/pr none is ever created, so one cheap
    ``git for-each-ref`` short-circuits the reachability hold to a no-op for
    those repos -- keeping their scheduling byte-identical to pre-feature
    behavior.
    """
    rc, out = _git_capture(
        repo_root,
        "for-each-ref",
        "--format=%(refname)",
        "refs/heads/flywheel/phase/",
    )
    return rc == 0 and bool(out.strip())


def reachability_held_prerequisites(
    rows: Iterable[TaskStatusRow],
    *,
    repo_root: Path | None,
    true_base: str | None,
) -> dict[str, PrerequisiteReachabilityHold]:
    """Dependents to withhold because a DONE cross-phase prerequisite's landed
    work is not yet reachable from the base they would branch from.

    The subtractive complement of :func:`satisfied_prerequisites_from_store`:
    that function *extends* prerequisite satisfaction to unlisted store-DONE
    ids; this one *removes* satisfaction from a listed prerequisite that is
    DONE but whose phase has not merged. Under the phase strategy (spec 00079)
    each phase lands its tasks onto ``flywheel/phase/<phase>`` and phases are
    independent in v1 -- a dependent in phase B branches from the true base,
    which does not include phase A's work until A's PR merges. Claiming the
    dependent then would branch it from a base that cannot see the
    prerequisite's landed commit, so it is held (never parked/failed) until
    reachability holds, at which point the next pass claims it with no operator
    action.

    Returns ``{}`` -- a total no-op preserving merge/pr and pre-feature
    scheduling byte-for-byte -- when ``repo_root``/``true_base`` is absent or no
    ``flywheel/phase/*`` branch exists. A hold is recorded only for a listed
    prerequisite that is (a) in a *different* phase than the dependent
    (same-phase chains share one integration branch and are unaffected), (b)
    classified :attr:`TaskState.DONE`, and (c) whose phase integration branch
    tip is not an ancestor of ``true_base``. A prerequisite whose phase branch
    no longer resolves (merged and deleted, or never phase-landed) is reachable
    and never holds; an unlisted prerequisite (e.g. its phase archived) is
    likewise reachable off the true base and never holds.
    """
    if repo_root is None or true_base is None:
        return {}
    root: Path = repo_root
    base: str = true_base
    materialized = list(rows)
    if not materialized or not _phase_branches_present(root):
        return {}
    phase_by_id = {row.task.id: _phase_of_row(row) for row in materialized}
    state_by_id = {row.task.id: row.state for row in materialized}
    tip_cache: dict[str, str | None] = {}
    ancestor_cache: dict[str, bool] = {}

    def _phase_tip(phase: str) -> str | None:
        if phase not in tip_cache:
            tip_cache[phase] = _resolve_phase_branch_tip(
                root, f"flywheel/phase/{phase}"
            )
        return tip_cache[phase]

    def _reachable(tip: str) -> bool:
        if tip not in ancestor_cache:
            ancestor_cache[tip] = _is_ancestor(root, tip, base)
        return ancestor_cache[tip]

    holds: dict[str, PrerequisiteReachabilityHold] = {}
    for row in materialized:
        dep_phase = phase_by_id[row.task.id]
        if not dep_phase:
            continue
        held_by: list[str] = []
        blocking_phase = ""
        for prereq_id in row.prerequisites:
            prereq_phase = phase_by_id.get(prereq_id)
            if not prereq_phase or prereq_phase == dep_phase:
                continue
            if state_by_id.get(prereq_id) is not TaskState.DONE:
                continue
            tip = _phase_tip(prereq_phase)
            if tip is None:
                continue
            if _reachable(tip):
                continue
            held_by.append(prereq_id)
            if not blocking_phase:
                blocking_phase = prereq_phase
        if held_by:
            holds[row.task.id] = PrerequisiteReachabilityHold(
                task_id=row.task.id,
                blocking_phase=blocking_phase,
                held_by=tuple(held_by),
            )
    return holds


IN_LOOP_VERIFICATION_TAG = "in-loop-verification"

class _LandingState(Enum):
    """The archive sweep's per-task landing verdict (spec 00077, D-1/D-4).

    ``LANDED`` -- a :class:`~flywheel_core.events.Landed` receipt exists on the
    task's latest run (the fast path), or the ancestry probe confirmed the
    task's recorded work is an ancestor of the landing base at sweep time.
    ``NOT_LANDED`` -- the work resolved to a commit but it is NOT an ancestor of
    the landing base (a determinate strand: a ``divergent-base`` park whose
    branch head the base cannot fast-forward to). ``INDETERMINATE`` -- no
    receipt and neither the branch nor any recorded head resolves, so landing
    state cannot be determined and fails closed (D-4).
    """

    LANDED = "landed"
    NOT_LANDED = "not-landed"
    INDETERMINATE = "indeterminate"


def _run_has_landed_receipt(
    store: SqliteStore | PostgresStore, run_id: str
) -> bool:
    """True when ``run_id``'s domain-event stream carries a ``Landed`` receipt.

    The fast path of the landed predicate (D-1): a machine land appends a
    :class:`~flywheel_core.events.Landed` witness, so a receipt is authoritative
    proof the work landed even after the branch is deleted. Ancestry is the
    slower truth for hand-landed / receipt-less work.
    """
    return any(
        isinstance(event, Landed)
        for event in store.list_domain_events(run_id)
    )


def _resolve_task_head(
    repo_root: Path, phase_dir: Path, task_id: str
) -> str | None:
    """Resolve the commit the task's recorded work points at, or ``None``.

    The recorded work lives on the ``flywheel/<phase>/<task-id>`` branch the
    worktree strategy names (``flywheel_worktree.worker._branch``); the phase is
    the active phase directory name. Resolved via ``git rev-parse --verify
    --quiet refs/heads/<branch>`` against the already-threaded ``repo_root`` --
    the same shared-object-DB shell-out the loop-path diff uses -- so the
    orchestrator never imports the worktree package to run the probe. ``None``
    when the branch ref does not exist (deleted after a sweep, never created),
    which the caller treats as the branch leg of the D-4 indeterminate check.
    """
    branch = f"flywheel/{phase_dir.name}/{task_id}"
    rc, out = _git_capture(
        repo_root, "rev-parse", "--verify", "--quiet", f"refs/heads/{branch}"
    )
    if rc != 0:
        return None
    return out.strip() or None


def _is_ancestor(repo_root: Path, ancestor: str, rev: str) -> bool:
    """True when ``ancestor`` is reachable from ``rev`` in ``repo_root``.

    ``git merge-base --is-ancestor`` exits 0 for ancestor, 1 for not, and
    non-zero (128) when either revision is unresolvable -- both non-zero cases
    read as "not an ancestor" so an unresolvable landing base fails closed
    rather than blessing the work as landed. Mirrors
    ``flywheel_worktree.worker.GitWorktreeSubmitter._is_ancestor``, which
    predicts the fast-forward outcome exactly under the merge lock.
    """
    rc, _ = _git_capture(
        repo_root, "merge-base", "--is-ancestor", ancestor, rev
    )
    return rc == 0


def _probe_task_landing(
    store: SqliteStore | PostgresStore,
    repo_root: Path,
    phase_dir: Path,
    task_id: str,
    landing_base: str,
) -> _LandingState:
    """Decide a DONE task's landing state for the archive sweep (D-1/D-4).

    Receipt first (fast path): a ``Landed`` event on the task's latest run is
    authoritative even if the branch is gone. Otherwise probe ancestry: resolve
    the task's recorded work to a commit and ask whether it is an ancestor of
    ``landing_base`` at sweep time. A resolvable head that is NOT an ancestor is
    a determinate strand (``NOT_LANDED``); an unresolvable head with no receipt
    is ``INDETERMINATE`` -- landing state cannot be determined, so it fails
    closed. A probe that always answered ``LANDED`` would archive the
    divergent-base strand criterion 1 forbids; keying the truthy answer on real
    ancestry is what forecloses that.
    """
    row = _latest_lifecycle_row(store, task_id)
    if row is not None and _run_has_landed_receipt(store, row[0]):
        return _LandingState.LANDED
    head = _resolve_task_head(repo_root, phase_dir, task_id)
    if head is None:
        return _LandingState.INDETERMINATE
    if _is_ancestor(repo_root, head, landing_base):
        return _LandingState.LANDED
    return _LandingState.NOT_LANDED


def _format_landing_refusal(
    phase_dir: Path, task_id: str, state: _LandingState
) -> str:
    """Render the refusal message for a phase blocked by an unlanded task.

    Names the offending phase and the blocking task id -- the same
    ``Callable[[str], None]`` log seam the loop-path and phase-verify refusals
    use -- so the strand is visible on the sweep's log instead of vanishing when
    the phase silently stays active. The determinate (``NOT_LANDED``) and
    indeterminate causes read differently so the operator's next step is
    unambiguous.
    """
    if state is _LandingState.INDETERMINATE:
        cause = (
            "its landing state cannot be determined (no landing receipt and "
            "neither its branch nor a recorded head resolves for the ancestry "
            "probe)"
        )
    else:
        cause = (
            "its recorded work is not landed (no landing receipt and its "
            "branch head is not an ancestor of the landing base)"
        )
    return (
        f"Refusing to archive phase {phase_dir.name}: task {task_id} is not "
        f"landed -- {cause}"
    )


class _PhaseMergeState(Enum):
    """The archive sweep's phase-branch merge verdict (spec 00079, D-2/D-3).

    ``MERGED`` -- the phase integration branch tip is an ancestor of the true
    base: the phase PR merged as a merge commit, so ``git revert -m 1`` reverts
    the whole phase and the review unit is real. ``UNMERGED`` -- the branch
    exists, its tip is not an ancestor, and it still carries changes the true
    base lacks (an open phase PR). ``MERGE_METHOD_MISMATCH`` -- the branch's
    content is already applied to the true base but its tip is not an ancestor
    (a squash or rebase merge discarded the branch commit); a DISTINCT, visible
    reason so a squash-merged phase is never a silent forever-block, never the
    generic open-PR reason.
    """

    MERGED = "merged"
    UNMERGED = "unmerged"
    MERGE_METHOD_MISMATCH = "merge-method-mismatch"


def _resolve_phase_branch_tip(
    repo_root: Path, phase_branch: str
) -> str | None:
    """Resolve the phase integration branch tip commit, or ``None``.

    The phase strategy lands each task onto ``flywheel/phase/<phase>`` (spec
    00079); that ref's existence is the git-truth signal the phase is under
    phase-branch landing. Resolved via ``git rev-parse --verify --quiet`` the
    same way :func:`_resolve_task_head` resolves a task branch, so the sweep
    reads merge state offline without importing the worktree package. ``None``
    when the ref does not exist -- under the merge/pr strategies no such branch
    is ever created (criterion 9), and a phase branch deleted after a clean
    merge is likewise gone -- so the merge gate simply never arms and the
    spec-00077 per-task landed predicate (receipt or ancestry) remains the
    fail-closed backstop for genuinely lost work.
    """
    rc, out = _git_capture(
        repo_root,
        "rev-parse",
        "--verify",
        "--quiet",
        f"refs/heads/{phase_branch}",
    )
    if rc != 0:
        return None
    return out.strip() or None


def _probe_phase_merge(
    repo_root: Path, phase_tip: str, true_base: str
) -> _PhaseMergeState:
    """Decide whether a phase branch has merged into the true base (D-2/D-3).

    Merged-ness is git ancestry, never a remote PR state string (D-3): only a
    merge-commit merge keeps ``phase_tip`` reachable from ``true_base`` and
    gives ``git revert -m 1`` of the whole phase. When the tip is not an
    ancestor the sweep distinguishes a genuinely open PR from a squash/rebase
    merge by comparing the two commits' trees (``git diff --quiet``): a
    squash/rebase merge applies the phase's content to the true base while
    discarding the branch commit, so the trees match though ancestry is broken
    -- surfaced as a distinct mismatch rather than the generic open-PR block so
    it is never a silent forever-loop. Both signals are git-truth, checkable
    offline; neither trusts the remote's ``merged`` claim.
    """
    if _is_ancestor(repo_root, phase_tip, true_base):
        return _PhaseMergeState.MERGED
    rc, _ = _git_capture(repo_root, "diff", "--quiet", true_base, phase_tip)
    if rc == 0:
        return _PhaseMergeState.MERGE_METHOD_MISMATCH
    return _PhaseMergeState.UNMERGED


def _format_phase_merge_refusal(
    phase_dir: Path, phase_branch: str, state: _PhaseMergeState
) -> str:
    """Render the refusal for a phase whose integration branch has not merged.

    The unmerged (open PR) and squash/rebase mismatch causes read differently
    -- the first waits on a human (or the merge queue) to merge the phase PR,
    the second needs the phase re-merged with a merge commit or the strand
    resolved -- so the operator's next step is unambiguous and neither state is
    a silent non-archival. Uses the same ``Callable[[str], None]`` log seam the
    landing/loop-path/phase-verify refusals use.
    """
    if state is _PhaseMergeState.MERGE_METHOD_MISMATCH:
        cause = (
            f"its integration branch {phase_branch} is not an ancestor of the "
            f"true base but its content is already applied there -- a squash or "
            f"rebase merge discarded the branch commit (merge-method mismatch); "
            f"re-merge the phase PR with a merge commit or resolve the strand"
        )
    else:
        cause = (
            f"its integration branch {phase_branch} is not an ancestor of the "
            f"true base -- the phase PR is still open; archival waits for it to "
            f"merge (the worker performs no local merge of the phase branch)"
        )
    return (
        f"Refusing to archive phase {phase_dir.name}: the phase PR is not "
        f"merged -- {cause}"
    )


def _record_indeterminate_landing(
    claims: SqliteClaimStore | PostgresClaimStore,
    task_id: str,
    phase_dir: Path,
    clock: Callable[[], datetime],
) -> None:
    """Append one indeterminate-landing marker for ``task_id``, idempotently.

    Fails closed loudly (D-4): a DONE task whose landing state cannot be
    determined surfaces in ``flywheel status`` via a stop row with the stable
    :data:`~flywheel_orchestrator._claims.STOP_INDETERMINATE_LANDING` kind keyed
    to the task id. The append is skipped when the subject's latest stop row is
    already an unresolved indeterminate-landing marker, so repeated sweeps over
    a blocked phase surface the strand once rather than flooding the ledger; a
    later :data:`~flywheel_orchestrator._claims.STOP_RESOLVED` (the phase
    archived once the task landed) supersedes it, so a fresh recurrence
    re-surfaces.
    """
    events = claims.list_subject_stop_events(task_id)
    if events and events[-1].kind == STOP_INDETERMINATE_LANDING:
        return
    claims.record_stop_event(
        kind=STOP_INDETERMINATE_LANDING,
        subject=task_id,
        detail=(
            f"phase {phase_dir.name!r} cannot archive: task {task_id!r} has "
            f"no landing receipt and neither its branch nor a recorded head "
            f"resolves for the ancestry probe"
        ),
        occurred_at=clock(),
    )


def _has_operator_resolution(
    claims: SqliteClaimStore | PostgresClaimStore,
    task_id: str,
) -> bool:
    """True when ``task_id``'s latest stop event is an operator resolution.

    The ``resolve`` verb (spec 00077, D-3) appends one
    :data:`~flywheel_orchestrator._claims.STOP_RESOLVED` marker carrying
    :data:`~flywheel_orchestrator._claims.RESOLUTION_ATTRIBUTION_OPERATOR`
    when an operator deliberately abandons a strand. The archive sweep treats
    such a task as no longer blocking -- its verified work need never land.

    The verdict reads the attribution off the record's own column, never
    parsed from the free-text ``detail`` prose, so a machine (probe)
    resolution can never masquerade as an operator decision. Only the LATEST
    row is consulted, so a fresh stop appended AFTER the marker (a recurrence)
    makes the task blocking again.
    """
    events = claims.list_subject_stop_events(task_id)
    if not events:
        return False
    latest = events[-1]
    return (
        latest.kind == STOP_RESOLVED
        and latest.attribution == RESOLUTION_ATTRIBUTION_OPERATOR
    )


def archive_completed_phases(
    tasks_dir: Path,
    store: SqliteStore | PostgresStore,
    *,
    repo_root: Path | None = None,
    log: Callable[[str], None] | None = None,
    phase_verify: str | None = None,
    landing_base: str | None = None,
    true_base: str | None = None,
    claims: SqliteClaimStore | PostgresClaimStore | None = None,
    now: Callable[[], datetime] | None = None,
) -> list[Path]:
    """Move ``active/<phase>`` dirs to ``archive/`` when every task is done.

    Returns the list of moved phase directories (post-move paths). Idempotent:
    safe to call repeatedly. Phases with any non-done task are left in place.

    ``landing_base`` arms the landed predicate (spec 00077, D-1/D-4): a DONE
    task counts landed only when a :class:`~flywheel_core.events.Landed` receipt
    exists on its latest run (fast path) OR the ancestry probe confirms its
    recorded work -- the ``flywheel/<phase>/<task-id>`` branch head -- is an
    ancestor of ``landing_base`` (a git ref, e.g. the resolved submit base or
    ``HEAD``) at sweep time. A phase with any task that is not landed stays in
    ``active/`` and the blocking task id is reported via ``log`` (the same seam
    the loop-path/phase-verify refusals use); a task whose landing state cannot
    be determined -- no receipt and no resolvable branch/recorded head -- fails
    closed as a surfaced indeterminate-landing strand (one
    :data:`~flywheel_orchestrator._claims.STOP_INDETERMINATE_LANDING` row keyed
    to the task id via ``claims``) rather than counting as landed. The predicate
    requires both ``repo_root`` (to run the git probe) and ``landing_base``;
    ``None`` for either preserves the legacy DONE-only archival contract, so
    callers that pass neither (and the synthetic archive/loop-path tests) keep
    their previous behavior.

    ``true_base`` arms the phase-branch merge predicate (spec 00079, criteria
    4/5/6/8, D-2/D-3): under the phase strategy each task lands on the phase
    integration branch ``flywheel/phase/<phase>``, and the completed phase is a
    review unit that archives only once that branch tip is an ancestor of the
    true base -- i.e. the phase PR merged as a merge commit (git ancestry, never
    a remote ``merged`` claim). The gate arms per-phase on the existence of that
    branch (never created under the merge/pr strategies, so those repos are
    byte-identical -- criterion 9) with the resolved ``true_base`` ref every
    archival caller threads (criterion 8). An unmerged branch keeps the phase in
    ``active/`` with the open PR surfaced via ``log`` and never advances the
    true base itself (the worker performs no local merge); a squash/rebase merge
    (content applied, ancestry broken) surfaces a distinct merge-method-mismatch
    reason rather than a silent forever-block. It runs AFTER the spec-00077
    per-task landed predicate, so a DONE-but-parked strand still blocks first and
    independently. ``None`` (or a phase with no integration branch) leaves the
    gate disarmed.

    When ``repo_root`` is supplied, the phase's cumulative diff vs its
    recorded ``.loop-base`` is inspected for the watched loop-path signals
    (see :mod:`flywheel_core.loop_path_marker`). A phase whose diff hits any
    signal is gated: it archives only when it contains a DONE task tagged
    ``in-loop-verification`` OR a valid ``loop-path-exempt.md`` opt-out
    artifact lives alongside the task files. A gated phase stays in
    ``active/`` and the refusal reason is reported via ``log`` (the same
    ``Callable[[str], None]`` seam :func:`.flywheel.worker.archive_phases`
    uses). An empty marker (no watched signal, no recorded base, or
    ``repo_root`` omitted) archives exactly as before -- the gate is a
    pure addition for the loop-path case.

    ``phase_verify`` is the optional phase-exit integration gate (spec
    00035): when set (and ``repo_root`` is supplied), the command runs
    (shell) against the merged phase base in ``repo_root`` -- the landed
    integration result -- once a phase is eligible to archive. A non-zero
    exit leaves the phase active and reports the failure via ``log``; exit 0
    archives exactly as the no-gate path does. ``None`` (the default)
    preserves today's archival behavior for operators who configured no
    gate.

    ``claims`` threads the orchestrator stop-event ledger through the sweep.
    Archival is the verified resolution act for a *non-landing* stop -- a
    queue-routed strand serviced by hand, a dangling prerequisite that later
    resolved -- so each such archived task gets one appended
    :data:`STOP_RESOLVED` marker (no ``attribution``; archival is never an
    attributor), clearing it from the ``status`` stranded view without
    deleting ledger history. A *landing strand* (spec 00077, criteria 3/4,
    D-2 -- the kinds in
    :data:`~flywheel_core.events.LANDING_STRAND_KINDS` plus
    :data:`STOP_INDETERMINATE_LANDING`) is not resolvable by the act of
    archiving: it survives every sweep unresolved until git-truth or an
    operator clears it. This sweep stamps a resolution on a landing strand
    only when its own landed predicate confirmed the task reachable from
    ``landing_base`` (``_LandingState.LANDED``); that marker is attributed to
    the probe (:data:`~flywheel_orchestrator._claims.RESOLUTION_ATTRIBUTION_PROBE`),
    queryable from the stop-event record's ``attribution`` column and never
    parsed from the detail prose. An unarmed sweep proves nothing and leaves
    every landing strand surfaced. ``None`` (the default) skips the markers,
    preserving the legacy contract. ``now`` is the marker's injected clock;
    ``None`` falls back to wall-clock UTC, the same convention as the
    work-source stop sink.
    """
    moved: list[Path] = []
    archive_root = tasks_dir / "archive"
    archive_root.mkdir(parents=True, exist_ok=True)
    clock = now if now is not None else (
        lambda: datetime.now(timezone.utc)
    )

    for phase_dir in iter_active_phase_dirs(tasks_dir):
        task_files = [
            entry
            for entry in sorted(phase_dir.iterdir())
            if entry.is_file()
            and entry.suffix == ".json"
            and not entry.name.startswith("_")
            and not entry.name.startswith(".")
        ]
        if not task_files:
            continue
        loaded_tasks: list[Task] = [load_task_file(p) for p in task_files]
        if not all(
            _has_done_lifecycle(store, task.id) for task in loaded_tasks
        ):
            continue

        # Landed predicate (spec 00077, D-1/D-4): every DONE task's verified
        # work must be landed -- a receipt on its latest run, or its recorded
        # work an ancestor of ``landing_base`` -- before the phase may archive.
        # A determinate strand (resolvable head that is not an ancestor) blocks
        # and names the task; an indeterminate one (no receipt, no resolvable
        # head) blocks AND fails closed as a surfaced indeterminate-landing
        # strand. Armed only when both ``repo_root`` and ``landing_base`` are
        # threaded, so legacy callers keep the DONE-only contract.
        # Per-task landing verdict from this sweep's probe, keyed by task id.
        # Empty when the predicate is unarmed (legacy callers), so the tail
        # resolution block can tell a git-truth-confirmed landing (LANDED)
        # apart from an unproven one and never machine-resolves a strand it
        # did not confirm landed.
        landing_states: dict[str, _LandingState] = {}
        if repo_root is not None and landing_base is not None:
            blocking = False
            for task in loaded_tasks:
                # An operator-attributed resolution (spec 00077, D-3)
                # deliberately abandons a strand: the operator ran the
                # ``resolve`` verb, which appended an operator-attributed
                # STOP_RESOLVED marker keyed to this task. Such a task no
                # longer blocks archival even though its work never landed --
                # skip the landing probe entirely and do not surface it as a
                # strand. A DIFFERENT unlanded, unresolved task in the same
                # phase still blocks, since each task is judged on its own
                # latest marker.
                if claims is not None and _has_operator_resolution(
                    claims, task.id
                ):
                    continue
                state = _probe_task_landing(
                    store, repo_root, phase_dir, task.id, landing_base
                )
                landing_states[task.id] = state
                if state is _LandingState.LANDED:
                    continue
                blocking = True
                if log is not None:
                    log(_format_landing_refusal(phase_dir, task.id, state))
                if (
                    state is _LandingState.INDETERMINATE
                    and claims is not None
                ):
                    _record_indeterminate_landing(
                        claims, task.id, phase_dir, clock
                    )
            if blocking:
                continue

        # Phase-branch merge predicate (spec 00079, criteria 4/5/6/8, D-2/D-3).
        # Under the phase strategy each task lands on the phase integration
        # branch ``flywheel/phase/<phase>``; the completed phase archives only
        # once that branch tip is an ancestor of the true base -- the phase PR
        # merged as a merge commit, which alone preserves ``git revert -m 1`` of
        # the whole phase (D-3, merged-ness is git ancestry, never a remote PR
        # state string). Armed per-phase on the existence of that branch (never
        # created under merge/pr -- criterion 9) plus a threaded ``true_base``
        # (criterion 8: every archival surface passes it). An unmerged branch
        # (open PR) keeps the phase active with the PR surfaced and never
        # advances the true base itself; a squash/rebase merge (content applied,
        # ancestry broken) surfaces a DISTINCT merge-method-mismatch reason so it
        # is never a silent forever-block. A branch deleted after a clean merge
        # simply disarms the gate -- the spec-00077 per-task predicate above,
        # which ran first, is the fail-closed backstop for genuinely lost work.
        if repo_root is not None and true_base is not None:
            phase_branch = f"flywheel/phase/{phase_dir.name}"
            phase_tip = _resolve_phase_branch_tip(repo_root, phase_branch)
            if phase_tip is not None:
                merge_state = _probe_phase_merge(
                    repo_root, phase_tip, true_base
                )
                if merge_state is not _PhaseMergeState.MERGED:
                    if log is not None:
                        log(
                            _format_phase_merge_refusal(
                                phase_dir, phase_branch, merge_state
                            )
                        )
                    continue

        # Loop-path archive gate (FR-2): a non-empty marker requires either
        # a DONE in-loop-verification task or a recorded opt-out artifact.
        # The marker is empty whenever no ``repo_root`` was threaded
        # through or no ``.loop-base`` was recorded -- legacy callers and
        # synthetic test phases keep their previous archive semantics.
        if repo_root is not None:
            signals = detect_loop_path_signals(
                phase_diff_vs_base(repo_root, phase_dir)
            )
            if signals and not _loop_path_gate_satisfied(
                phase_dir, loaded_tasks, store
            ):
                if log is not None:
                    log(_format_gate_refusal(phase_dir, signals))
                continue

        # Phase-exit integration gate (spec 00035): run the operator's
        # phase-verify command against the merged base in repo_root -- the
        # landed integration result -- and gate archival on its exit code. A
        # non-zero exit leaves the phase active and surfaces the failure;
        # exit 0 archives as the no-gate path does. Unset (or no repo_root)
        # preserves today's archival.
        if phase_verify is not None and repo_root is not None:
            result = subprocess.run(
                phase_verify, shell=True, cwd=repo_root
            )
            if result.returncode != 0:
                if log is not None:
                    log(
                        _format_phase_verify_refusal(
                            phase_dir, phase_verify, result.returncode
                        )
                    )
                continue

        dest = archive_root / phase_dir.name
        # If a same-named archive exists, leave the active dir alone rather
        # than clobber prior history — operator can resolve manually.
        if dest.exists():
            continue
        shutil.move(str(phase_dir), str(dest))
        if repo_root is not None:
            _materialize_loop_base(repo_root, dest)
        if claims is not None:
            for task in loaded_tasks:
                events = claims.list_subject_stop_events(task.id)
                if not events or events[-1].kind == STOP_RESOLVED:
                    continue
                latest_kind = events[-1].kind
                if (
                    latest_kind in LANDING_STRAND_KINDS
                    or latest_kind == STOP_INDETERMINATE_LANDING
                ):
                    # Landing strands (spec 00077, criteria 3/4, D-2) are not
                    # resolvable by the act of archiving -- only git-truth or
                    # an operator clears them. This sweep may stamp a
                    # resolution only when its own probe confirmed the task
                    # landed (``_LandingState.LANDED``); the marker is then
                    # attributed to the probe, never to archival. An unarmed
                    # sweep (empty ``landing_states``) proves nothing and
                    # leaves the strand surfaced.
                    if (
                        landing_states.get(task.id)
                        is _LandingState.LANDED
                    ):
                        claims.record_stop_event(
                            kind=STOP_RESOLVED,
                            subject=task.id,
                            detail=(
                                f"phase {phase_dir.name!r} archived after the "
                                f"landing probe confirmed this task's work "
                                f"reachable from landing base "
                                f"{landing_base!r}; supersedes the surfaced "
                                f"landing strand ({latest_kind!r})"
                            ),
                            occurred_at=clock(),
                            attribution=RESOLUTION_ATTRIBUTION_PROBE,
                        )
                    continue
                # Non-landing stops (a hand-serviced queue strand, a dangling
                # prerequisite that later resolved) keep the archival
                # supersession marker: archival IS their verified resolution.
                # No attribution -- archival is never an attributor.
                claims.record_stop_event(
                    kind=STOP_RESOLVED,
                    subject=task.id,
                    detail=(
                        f"phase {phase_dir.name!r} archived with every task "
                        f"done and its exit gates passed; supersedes the "
                        f"surfaced stop ({latest_kind!r})"
                    ),
                    occurred_at=clock(),
                )
        moved.append(dest)
    return moved

def _loop_path_gate_satisfied(
    phase_dir: Path, tasks: Iterable[Task], store: SqliteStore | PostgresStore
) -> bool:
    """Return ``True`` when ``phase_dir`` clears the loop-path archive gate.

    The gate is satisfied by either a valid opt-out artifact or a DONE
    task carrying the ``in-loop-verification`` tag. A malformed opt-out
    artifact (``LoopPathOptOutError``) is treated as no opt-out -- the
    refusal still fires and the operator must fix the artifact; the
    audit re-check does the symbol-level second look on the claim. The
    signature only takes a closed iterable of ``Task`` so the caller
    can avoid re-reading task JSON.
    """
    try:
        if load_loop_path_optout(phase_dir) is not None:
            return True
    except LoopPathOptOutError:
        # Defer to the operator: a malformed opt-out is not a downgrade.
        pass
    for task in tasks:
        if IN_LOOP_VERIFICATION_TAG in task.tags and _has_done_lifecycle(
            store, task.id
        ):
            return True
    return False

def _format_gate_refusal(
    phase_dir: Path, signals: frozenset[LoopPathSignal]
) -> str:
    """Render the human-readable refusal message for a gated phase.

    Signal names are sorted so the message is stable across runs (the
    underlying ``frozenset`` is order-independent). The message names
    the offending phase, the tripped signals, and the two ways to
    clear the gate so the operator's next step is unambiguous.
    """
    names = ", ".join(sorted(signal.value for signal in signals))
    return (
        f"Refusing to archive phase {phase_dir.name}: loop-path signal(s) "
        f"[{names}] detected and neither a DONE "
        f"{IN_LOOP_VERIFICATION_TAG} task nor a "
        f"{LOOP_PATH_OPTOUT_FILENAME} opt-out is present"
    )

def _format_phase_verify_refusal(
    phase_dir: Path, command: str, returncode: int
) -> str:
    """Render the refusal message for a phase whose phase-verify gate failed.

    Names the offending phase, the command that ran against the merged base,
    and its exit code so the operator's next step is unambiguous.
    """
    return (
        f"Refusing to archive phase {phase_dir.name}: phase-verify command "
        f"{command!r} failed against the merged base (exit {returncode})"
    )

LOOP_BASE_FILENAME = ".loop-base"

# Active phases record their base SHA here rather than as a committed file:
# the worker must never create commits on the operator's branch. Archiving
# materializes the ref into the phase dir as the legacy dotfile (see
# :func:`archive_completed_phases`), so audits keep one contract — the
# ``.loop-base`` file travels with every archived phase.
LOOP_BASE_REF_PREFIX = "refs/flywheel/loop-base/"

def loop_base_ref(phase_dir: Path) -> str:
    """The git ref recording ``phase_dir``'s base SHA while it is active.

    Keyed by directory name; phase names are sequential (``NN-<name>``),
    so collisions across a repo's lifetime do not occur in practice.
    """
    return f"{LOOP_BASE_REF_PREFIX}{phase_dir.name}"

def _git_capture(repo_root: Path, *args: str) -> tuple[int, str]:
    """Run ``git -C <repo_root> <args>`` and return ``(returncode, stdout)``.

    Helper used by the loop-base capture and diff helpers. ``stderr`` is
    discarded -- failure here is data (the diff degrades to empty) rather
    than an operator-actionable error.
    """
    proc = subprocess.run(
        ["git", "-C", str(repo_root), *args],
        text=True,
        capture_output=True,
        check=False,
    )
    return proc.returncode, proc.stdout

def read_phase_base(
    phase_dir: Path, repo_root: Path | None = None
) -> str | None:
    """Return the SHA recorded as ``phase_dir``'s base, or ``None``.

    The legacy committed ``phase_dir/.loop-base`` dotfile wins when present
    (pre-ref phases, and every archived phase — archiving materializes the
    ref into the file). Otherwise, when ``repo_root`` is supplied, the
    ``refs/flywheel/loop-base/<phase>`` ref is consulted. Missing, empty,
    or unreadable sources all map to ``None`` so the diff helper can
    degrade safely without raising.
    """
    base_file = phase_dir / LOOP_BASE_FILENAME
    if base_file.is_file():
        try:
            sha = base_file.read_text(encoding="utf-8").strip()
        except OSError:
            sha = ""
        if sha:
            return sha
    if repo_root is None:
        return None
    rc, out = _git_capture(
        repo_root, "rev-parse", "--verify", "--quiet", loop_base_ref(phase_dir)
    )
    if rc != 0:
        return None
    return out.strip() or None

def write_phase_base_if_missing(repo_root: Path, phase_dir: Path) -> bool:
    """Record the current ``HEAD`` SHA as ``phase_dir``'s base if absent.

    The base lands as the ``refs/flywheel/loop-base/<phase>`` ref — pure
    ref plumbing, no working-tree write and nothing for anyone to commit.
    Idempotent: returns ``True`` when a fresh base was recorded, ``False``
    when one already existed (ref, or a legacy committed ``.loop-base``
    file) so the first-seen SHA is preserved — a re-run must not move the
    recorded base forward.

    Returns ``False`` (no write) when ``phase_dir`` does not exist or when
    ``git rev-parse HEAD`` fails -- both signal "no usable base to record."
    """
    if not phase_dir.is_dir():
        return False
    if read_phase_base(phase_dir, repo_root) is not None:
        return False
    rc, out = _git_capture(repo_root, "rev-parse", "HEAD")
    if rc != 0:
        return False
    sha = out.strip()
    if not sha:
        return False
    rc, _ = _git_capture(
        repo_root, "update-ref", loop_base_ref(phase_dir), sha
    )
    return rc == 0

def _materialize_loop_base(repo_root: Path, dest: Path) -> None:
    """Carry an archived phase's loop-base ref into ``dest`` as the legacy
    ``.loop-base`` dotfile, then drop the ref.

    Keeps the audit contract — the file travels with every archived phase
    (``/audit-phase`` re-derives signals from it) — while the ref namespace
    holds only live phases. Versioning the archived file is the operator's
    call, exactly like the archive move itself. A phase that already
    carries a committed dotfile is left as-is.
    """
    if (dest / LOOP_BASE_FILENAME).is_file():
        return
    ref = loop_base_ref(dest)
    rc, out = _git_capture(repo_root, "rev-parse", "--verify", "--quiet", ref)
    if rc != 0:
        return
    sha = out.strip()
    if sha:
        (dest / LOOP_BASE_FILENAME).write_text(sha + "\n", encoding="utf-8")
    _git_capture(repo_root, "update-ref", "-d", ref)

def phase_diff_vs_base(repo_root: Path, phase_dir: Path) -> str:
    """Return ``git diff <recorded-base> HEAD`` for ``repo_root`` as text.

    Returns ``""`` when no base has been recorded for the phase (degrades
    safely rather than raising -- callers can treat an empty diff as "no
    signal"), or when the underlying ``git diff`` exits non-zero (e.g. the
    recorded SHA has been garbage-collected). The returned text is the raw
    unified-diff payload from git, suitable for feeding the loop-path
    marker's symbol-level scans.
    """
    base = read_phase_base(phase_dir, repo_root)
    if base is None:
        return ""
    rc, out = _git_capture(repo_root, "diff", base, "HEAD")
    if rc != 0:
        return ""
    return out

LOOP_PATH_OPTOUT_FILENAME = "loop-path-exempt.md"

_LOOP_PATH_OPTOUT_REQUIRED_KEYS: tuple[str, ...] = (
    "phase",
    "author",
    "reason",
)

class LoopPathOptOutError(ValueError):
    """Raised when an opt-out artifact exists but its front-matter is invalid.

    The message identifies the offending file path so callers see actionable
    errors. Absent files are NOT an error -- :func:`load_loop_path_optout`
    returns ``None`` in that case (the phase has simply not opted out).
    """

@dataclass(frozen=True, kw_only=True)
class LoopPathOptOut:
    """One parsed ``loop-path-exempt.md`` artifact.

    The three required fields map to the spec's FR-5 attestation:
    ``phase`` names which phase opted out, ``author`` records who made
    the claim, and ``reason`` is the human justification for "no new
    loop path." All three are required and non-empty -- a silently-empty
    opt-out is rejected at load time so the audit re-check has something
    falsifiable to evaluate.
    """

    phase: str
    author: str
    reason: str

def load_loop_path_optout(phase_dir: Path) -> LoopPathOptOut | None:
    """Locate and parse the opt-out artifact for ``phase_dir``.

    Returns ``None`` when no ``loop-path-exempt.md`` exists inside
    ``phase_dir`` (the common case -- a phase has not opted out).
    Returns a parsed :class:`LoopPathOptOut` when the artifact exists
    with valid, complete front-matter. Raises
    :class:`LoopPathOptOutError` when the artifact exists but its
    front-matter is missing, malformed, or missing a required key --
    a silently-empty opt-out must not pass.

    Probing a non-existent ``phase_dir`` also returns ``None`` so callers
    may safely test arbitrary candidate paths; the loader only errors on
    a real artifact whose front-matter does not validate.
    """
    artifact = phase_dir / LOOP_PATH_OPTOUT_FILENAME
    if not artifact.is_file():
        return None
    text = artifact.read_text(encoding="utf-8")
    fields = _parse_optout_frontmatter(text, source=str(artifact))
    missing = [
        key
        for key in _LOOP_PATH_OPTOUT_REQUIRED_KEYS
        if not fields.get(key)
    ]
    if missing:
        raise LoopPathOptOutError(
            f"{artifact}: opt-out front-matter is missing required "
            f"key(s): {', '.join(missing)}"
        )
    return LoopPathOptOut(
        phase=fields["phase"],
        author=fields["author"],
        reason=fields["reason"],
    )

def _parse_optout_frontmatter(text: str, *, source: str) -> dict[str, str]:
    """Parse a leading ``---`` ... ``---`` block into a ``{key: value}`` dict.

    Each line inside the block is ``key: value``; blank lines and lines
    whose first non-whitespace character is ``#`` are skipped. Anything
    after the closing ``---`` is treated as free-form prose and ignored.
    Unknown keys are tolerated so the format can grow forward-compat
    fields; required-key enforcement happens in the caller.

    Raises :class:`LoopPathOptOutError` when the file does not start with
    a ``---`` line, when the front-matter block is never closed by a
    matching ``---`` line, or when a non-blank/non-comment line inside
    the block is not in ``key: value`` shape (including an empty key).
    """
    lines = text.splitlines()
    if not lines or lines[0].strip() != "---":
        raise LoopPathOptOutError(
            f"{source}: opt-out must start with a '---' front-matter "
            f"delimiter"
        )
    fields: dict[str, str] = {}
    closed = False
    for raw in lines[1:]:
        if raw.strip() == "---":
            closed = True
            break
        stripped = raw.strip()
        if not stripped or stripped.startswith("#"):
            continue
        if ":" not in raw:
            raise LoopPathOptOutError(
                f"{source}: malformed front-matter line "
                f"(expected 'key: value'): {raw!r}"
            )
        key, _, value = raw.partition(":")
        key = key.strip()
        if not key:
            raise LoopPathOptOutError(
                f"{source}: malformed front-matter line "
                f"(empty key): {raw!r}"
            )
        fields[key] = value.strip()
    if not closed:
        raise LoopPathOptOutError(
            f"{source}: opt-out front-matter is not closed by a "
            f"matching '---' line"
        )
    return fields

def load_effective_policy(
    policy_path: Path | str | None = None,
) -> WorkPolicy | None:
    """Load the policy the orchestrator verbs honor for an invocation.

    Mirrors the orchestrate CLI's precedence so a downstream package can
    pin to the same resolution without reaching for private helpers:

    * an explicit ``policy_path`` is loaded (and a missing/invalid file is
      an error -- propagated as :class:`PolicyError`);
    * otherwise ``flywheel.toml`` in the working directory is auto-detected;
    * otherwise there is no policy and every default falls back to the
      built-in ``.flywheel/`` layout.

    Passing ``None`` (the default) selects auto-detection; an empty string
    is treated as "not specified" to match the argparse default surface.
    """
    if policy_path:
        return load_policy(Path(policy_path))
    candidate = Path(DEFAULT_POLICY_FILENAME)
    if candidate.is_file():
        return load_policy(candidate)
    return None

def _load_effective_policy(args: argparse.Namespace) -> WorkPolicy | None:
    """argparse-flavored wrapper around :func:`load_effective_policy`."""
    return load_effective_policy(getattr(args, "policy", None))

def _resolve_work_source(
    args: argparse.Namespace, policy: WorkPolicy | None
) -> WorkSource:
    """Resolve the work source for a CLI invocation.

    Precedence: an explicit ``--tasks-dir`` always selects the directory
    source (the historical behavior); otherwise the policy decides;
    otherwise the default directory layout applies.
    """
    if args.tasks_dir:
        return DirectoryWorkSource(Path(args.tasks_dir))
    if policy is not None:
        return build_work_source(policy)
    return DirectoryWorkSource(DEFAULT_TASKS_DIR)

def resolve_db_path(
    db: Path | str | None = None,
    *,
    policy: WorkPolicy | None = None,
) -> Path:
    """Resolve the SQLite store path with orchestrate's precedence.

    Explicit ``db`` argument (the CLI's ``--db`` flag) wins, else the
    policy's ``[paths] db`` setting, else the built-in
    ``.flywheel/flywheel.sqlite`` default. A downstream consumer (e.g. the
    TUI) can pin to the same resolution without reaching for argparse or
    private helpers.

    Passing ``None`` (the default) means "not specified at the CLI"; an
    empty string is treated identically to match the argparse default.
    """
    if db:
        return Path(db)
    if policy is not None and policy.db_path is not None:
        return policy.db_path
    return _resolve_db(None)

def _resolve_db_path(
    args: argparse.Namespace, policy: WorkPolicy | None
) -> Path:
    """argparse-flavored wrapper around :func:`resolve_db_path`."""
    return resolve_db_path(args.db, policy=policy)

def _cmd_next(args: argparse.Namespace) -> int:
    policy = _load_effective_policy(args)
    source = _resolve_work_source(args, policy)
    db_path = _resolve_db_path(args, policy)
    db_path.parent.mkdir(parents=True, exist_ok=True)
    store = open_sqlite_bound_store(policy, db_path=db_path)
    worker_capabilities = (
        policy.execution_capabilities if policy is not None else frozenset()
    )
    try:
        rows = status_rows_for_items(source.list_work(), store)
        # A prerequisite absent from the listing but DONE in the store (e.g.
        # its phase archived) is satisfied off the authoritative record, so a
        # dependent whose only unlisted prerequisite already completed is not
        # withheld. Only unlisted ids are consulted -- no per-listed-task read.
        satisfied = satisfied_prerequisites_from_store(rows, store)
        # Phase-prerequisite reachability hold (spec 00079, #7): the pull
        # surface must not hand out a dependent whose DONE prerequisite landed
        # on an unmerged sibling phase -- the worker would refuse to claim it.
        # No-op for non-phase (merge/pr, tracker) repos, so selection is
        # otherwise unchanged.
        repo_root, true_base = _reachability_context(args, policy)
        held = reachability_held_prerequisites(
            rows, repo_root=repo_root, true_base=true_base
        )
        pick = select_next_task(
            rows,
            exclude_ids=frozenset(held),
            worker_capabilities=worker_capabilities,
            satisfied_prerequisites=satisfied,
        )
    finally:
        store.close()
    if pick is None:
        return 1
    # File-backed picks print the path (scripts consume it); external items
    # print the source's opaque ref.
    print(pick.task_file if pick.task_file != Path() else pick.source_ref)
    return 0

def _cmd_orchestrate(args: argparse.Namespace) -> int:
    # TEMPORARY bridge (core/consumer split, Phase 3): the orchestrate driver
    # now lives in the flywheel-orchestrator package. This lazy import is the
    # one remaining core -> consumer reference; it goes away in Phase 3b when
    # the multi-task CLI moves to the orchestrator package.
    from flywheel_orchestrator import (
        DEFAULT_SESSION_PAUSE_CEILING_SECONDS,
        DEFAULT_SWEEP_SECONDS,
        orchestrate,
    )

    policy = _load_effective_policy(args)
    source = _resolve_work_source(args, policy)
    db_path = _resolve_db_path(args, policy)
    sandbox_root = resolve_sandbox_root(
        args.sandbox_root
        or (policy.sandbox_root if policy is not None else None),
        repo_root=Path.cwd(),
    )
    report = asyncio.run(
        orchestrate(
            source=source,
            policy=policy,
            db_path=db_path,
            sandbox_root=sandbox_root,
            model=args.model,
            max_turns=args.max_turns,
            max_retries=args.max_retries,
            worker_id=args.worker_id,
            lease_seconds=args.lease_seconds,
            reconcile_seconds=args.reconcile_seconds or None,
            sweep_seconds=DEFAULT_SWEEP_SECONDS,
            session_pause_ceiling_seconds=(
                policy.worker_session_pause_ceiling_seconds
                if policy is not None
                else DEFAULT_SESSION_PAUSE_CEILING_SECONDS
            ),
        )
    )
    for run_id in report.recovered:
        print(f"[orchestrate] recovered: {run_id}", file=sys.stderr)
    for record in report.runs:
        print(
            f"[orchestrate] {record.mode:<6} {record.task_id} "
            f"-> {record.status.value}  ({record.run_id})",
            file=sys.stderr,
        )
    # Non-zero exit if any task ended in a non-done terminal/paused state,
    # so a CI driver can tell the batch did not fully complete.
    incomplete = [r for r in report.runs if r.status != Status.DONE]
    return 0 if not incomplete else 1

_LIVE_STALE_AFTER_SECONDS: int = 90

_LIVE_DETAIL_MAX_WIDTH: int = 120


class _LiveReadStore(Protocol):
    """Backend-agnostic read surface ``collect_live_rows`` consumes.

    The live snapshot reads in-flight lifecycles, each run's attempts, and
    the run's pinned task definition — all through public protocol methods
    every concrete store (SQLite and Postgres) implements, never the
    SQLite-only private connection. Typing on this structural surface
    (rather than ``SqliteStore``) is what lets ``collect_live_rows`` run
    unchanged against a ``PostgresStore``.
    """

    def list_lifecycles(
        self,
        *,
        statuses: Collection[Status] | None = None,
        task_id: str | None = None,
    ) -> list[Lifecycle]: ...

    def list_attempts(self, run_id: str) -> list[Attempt]: ...

    def load_task_for_run(self, run_id: str) -> Task | None: ...


@dataclass(frozen=True, kw_only=True)
class LiveRunRow:
    """Per-in-flight-run snapshot used by ``live`` reporting.

    ``attempt`` / ``iteration`` form the lifecycle-position breadcrumb
    (``attempt=N iter=K``). ``last_kind`` + ``last_detail`` describe the
    run's latest recorded position. All fields are computed from
    relational rows only (``lifecycles`` + ``attempts`` aggregate
    columns, per spec 00025 FR-6 — no telemetry-event scan):
    ``tokens_total``/``cost_usd_total``/``turns_total`` sum the
    per-attempt rolled-up counters the harness writes at iteration
    boundaries, and ``iterations_completed`` sums the per-attempt
    iteration counts. Zero iterations completed means "no totals yet"
    and the totals fields are all zero by definition. ``last_ts`` is the
    latest attempt's ``last_activity_at`` rollup timestamp (falling back
    to its ``started_at`` before the first completed iteration), so
    idle/staleness updates at iteration-boundary cadence.

    ``awaiting_instruction`` carries the pending manual gate's
    instruction text when the run is parked on ``AWAITING_APPROVAL``;
    ``None`` for every other status. Resolved at snapshot time from the
    task definition pinned to the run via ``load_task_for_run``.
    """

    run_id: str
    task_id: str
    status: Status
    attempt: int | None
    iteration: int | None
    last_kind: str
    last_detail: str
    last_ts: datetime | None
    tokens_total: int
    cost_usd_total: float
    turns_total: int
    iterations_completed: int
    awaiting_instruction: str | None = None
    # Earliest lifecycle transition timestamp -- when the run started.
    # ``last_ts`` answers "how long since the run did anything" (idle /
    # staleness); ``started_at`` answers "how long has this run been
    # going" (the age an operator expects to grow monotonically).
    started_at: datetime | None = None
    # The persisted ``lifecycles.worker_id`` of the worker driving the
    # run (SI-11). ``None`` when the column is unset -- never a sentinel
    # string -- so an operator surface can distinguish "no worker
    # recorded" from a real worker id.
    worker_id: str | None = None

def collect_live_rows(store: _LiveReadStore) -> list[LiveRunRow]:
    """Snapshot every in-flight run from relational rows only.

    Reads ``lifecycles`` for status in ``{running, validating,
    awaiting_approval}`` and joins each row to its ``attempts`` rows,
    whose rolled-up aggregate columns the harness maintains at iteration
    boundaries (spec 00025 FR-6). No telemetry events or SDK messages
    are scanned: totals sum the per-attempt counters, the breadcrumb's
    ``attempt``/``iteration`` come from the latest attempt row, and
    ``last_ts`` is that row's ``last_activity_at`` rollup timestamp
    (falling back to ``started_at`` before the first completed
    iteration). Output is sorted by ``task_id`` for stable multi-run
    rendering per the 00011 spec.

    ``awaiting_approval`` rows have no live worker but are still owed
    operator attention; including them lets ``flywheel live`` surface
    the pending manual-gate instruction. The instruction is resolved
    from the task definition pinned to the run.
    """
    # Cross-task lifecycle read AND the per-run attempts join both go through
    # the public protocol surface (list_lifecycles SI-3 + list_attempts), off
    # the SQLite-only private connection, so the snapshot computes identically
    # on SQLite and Postgres. The rolled-up token/cost/turn aggregation stays
    # local to this function (no telemetry scan — spec 00025 FR-6).
    active_statuses = (
        Status.RUNNING,
        Status.VALIDATING,
        Status.AWAITING_APPROVAL,
    )
    lifecycles = store.list_lifecycles(statuses=active_statuses)
    # list_lifecycles orders by (updated_at DESC, run_id DESC); the live
    # surface renders sorted by task_id (ties by run_id) per spec 00011.
    lifecycles.sort(key=lambda lc: (lc.task_id, lc.run_id))
    rows: list[LiveRunRow] = []
    for lc in lifecycles:
        run_id = lc.run_id
        attempts = store.list_attempts(run_id)  # ascending by number
        tokens = 0
        cost = 0.0
        turns = 0
        iters_completed = 0
        for a in attempts:
            tokens += a.total_tokens
            cost += a.total_cost_usd
            turns += a.turns
            iters_completed += a.iterations_completed
        iteration: int | None = None
        attempt: int | None = None
        last_kind = "(none)"
        last_detail = "(no activity yet)"
        last_ts: datetime | None = None
        if attempts:
            latest = attempts[-1]
            attempt = latest.number
            latest_iters = latest.iterations_completed
            if latest_iters > 0:
                iteration = latest_iters
                last_kind = "ITERATION"
                last_detail = f"iteration {latest_iters} completed"
                last_ts = latest.last_activity_at or latest.started_at
            else:
                last_kind = "ATTEMPT"
                last_detail = f"attempt {attempt} started"
                last_ts = latest.started_at
        # Earliest recorded transition timestamp off the folded Lifecycle's
        # per-status stamps (retries overwrite ready/running, so the minimum
        # is the stable "when did this run start" answer).
        started_at = min(lc.timestamps.values()) if lc.timestamps else None
        # Empty-string worker_id (the unset default) -> None (never a
        # sentinel), so an unset worker is distinguishable from a real id
        # (SI-11).
        worker_id = lc.worker_id or None
        status = lc.status
        awaiting_instruction = _resolve_awaiting_instruction(
            store,
            run_id=run_id,
            status=status,
            awaiting_ordinal=lc.awaiting_manual_ordinal,
        )
        rows.append(
            LiveRunRow(
                run_id=run_id,
                task_id=lc.task_id,
                status=status,
                attempt=attempt,
                iteration=iteration,
                last_kind=last_kind,
                last_detail=last_detail,
                last_ts=last_ts,
                tokens_total=tokens,
                cost_usd_total=cost,
                turns_total=turns,
                iterations_completed=iters_completed,
                awaiting_instruction=awaiting_instruction,
                started_at=started_at,
                worker_id=worker_id,
            )
        )
    return rows

def _resolve_awaiting_instruction(
    store: _LiveReadStore,
    *,
    run_id: str,
    status: Status,
    awaiting_ordinal: Any,
) -> str | None:
    """Look up the pending manual gate's instruction for a parked run.

    Returns ``None`` unless the lifecycle is at ``AWAITING_APPROVAL``
    with a recorded ordinal that addresses a :class:`ManualGrader` in
    the task definition pinned to the run. A pointer that does not
    resolve (forward-compat, data-skew, archived task) is absorbed
    silently rather than crashing the live view; the surface still
    renders the status line, just without the instruction.
    """
    if status != Status.AWAITING_APPROVAL:
        return None
    if not isinstance(awaiting_ordinal, int):
        return None
    task = store.load_task_for_run(run_id)
    if task is None:
        return None
    if awaiting_ordinal < 0 or awaiting_ordinal >= len(task.graders):
        return None
    grader = task.graders[awaiting_ordinal]
    if not isinstance(grader, ManualGrader):
        return None
    return grader.instruction

def _format_breadcrumb(row: LiveRunRow) -> str:
    """Lifecycle-position breadcrumb: ``attempt=N iter=K`` (``?`` when
    unknown). The macro position (status) is rendered separately."""
    attempt_str = (
        f"attempt={row.attempt}" if row.attempt is not None else "attempt=?"
    )
    iter_str = (
        f"iter={row.iteration}" if row.iteration is not None else "iter=?"
    )
    return f"{attempt_str} {iter_str}"

def _format_totals(row: LiveRunRow) -> str:
    """Compact ``tokens=… cost=$… turns=…`` rollup of the run's
    per-attempt aggregate counters. Renders zero/`--` when the
    run has not completed an iteration yet (so the breadcrumb and
    action line still render — 00011 edge case)."""
    if row.iterations_completed == 0:
        return "tokens=0 cost=-- turns=0"
    return (
        f"tokens={row.tokens_total} "
        f"cost=${row.cost_usd_total:.4f} "
        f"turns={row.turns_total}"
    )

def _format_live_line(row: LiveRunRow, now: datetime) -> str:
    # ``age`` is how long the run has existed (earliest lifecycle
    # transition); ``idle`` is how long since its last recorded
    # activity. Staleness is an idle property -- a long-running but
    # chatty run is healthy, a quiet one is not.
    if row.started_at is None:
        age_str = "—"
    else:
        age_str = f"{_clamped_seconds(row.started_at, now)}s"
    if row.last_ts is None:
        idle_str = "—"
        stale = ""
    else:
        idle_s = _clamped_seconds(row.last_ts, now)
        idle_str = f"{idle_s}s"
        stale = "  STALE" if idle_s > _LIVE_STALE_AFTER_SECONDS else ""
    detail = _short(row.last_detail, _LIVE_DETAIL_MAX_WIDTH)
    head = (
        f"{row.task_id}  {row.status.value}  "
        f"{_format_breadcrumb(row)}  "
        f"{_format_totals(row)}  "
        f"age={age_str}  idle={idle_str}  {row.last_kind}  {detail}{stale}"
    )
    if row.awaiting_instruction is not None:
        # The owed decision is rendered as an indented follow-up line —
        # mirrors ``flywheel status``'s ``awaiting_on:`` / ``blocked_on:``
        # pattern so operators have one consistent shape for "this run
        # needs you" surfacing across both views.
        return f"{head}\n    awaiting_on: {row.awaiting_instruction}"
    return head

def _clamped_seconds(then: datetime, now: datetime) -> int:
    """Whole seconds between two instants, clamped at zero.

    Negative spans (clock skew between SQLite and host) read as 0
    rather than a misleading negative.
    """
    return max(0, int((now - then).total_seconds()))

def _cmd_live(args: argparse.Namespace) -> int:
    policy = _load_effective_policy(args)
    db_path = _resolve_db_path(args, policy)
    db_path.parent.mkdir(parents=True, exist_ok=True)
    interval = int(args.watch) if args.watch else 0

    def snapshot() -> None:
        store = open_sqlite_bound_store(policy, db_path=db_path)
        try:
            rows = collect_live_rows(store)
        finally:
            store.close()
        now = datetime.now(timezone.utc)
        if not rows:
            print("(no in-flight runs)")
            return
        for row in rows:
            print(_format_live_line(row, now))

    if interval <= 0:
        snapshot()
        return 0

    try:
        while True:
            # ANSI clear+home; falls back to scroll on dumb terminals.
            sys.stdout.write("\x1b[2J\x1b[H")
            sys.stdout.write(
                f"flywheel live  (refresh {interval}s, Ctrl-C to exit)\n\n"
            )
            snapshot()
            sys.stdout.flush()
            time.sleep(interval)
    except KeyboardInterrupt:
        return 0

def _parse_blocked_requires(payload: str | None) -> list[dict[str, Any]] | None:
    """Decode ``blocked_requires_json`` into a list of predicate dicts.

    Returns ``None`` when the payload is ``None`` or fails to decode into a
    list of objects. The status surface treats parse failure the same as
    "no snapshot" because the harness writes this column itself — any
    divergence is data corruption, not untrusted input.
    """
    if payload is None:
        return None
    try:
        data = json.loads(payload)
    except json.JSONDecodeError:
        return None
    if not isinstance(data, list):
        return None
    return [entry for entry in data if isinstance(entry, dict)]

def _predicate_identifier(predicate: Mapping[str, Any]) -> str:
    """Map a persisted predicate dict to its primary identifier string.

    Mirrors the keys the harness emits in :func:`_serialize_requires`:
    ``command_grader`` -> ``name``; ``file_exists`` -> ``path``;
    ``env_var_set`` -> ``name``. Unknown predicate types fall back to
    ``"?"`` so the operator surface never crashes on a forward-compat
    payload.
    """
    ptype = predicate.get("type")
    if ptype == "command_grader" or ptype == "env_var_set":
        value = predicate.get("name")
    elif ptype == "file_exists":
        value = predicate.get("path")
    else:
        value = None
    return str(value) if isinstance(value, str) and value else "?"

def _format_blocked_on(predicates: list[dict[str, Any]]) -> str:
    """Render the persisted requires list as ``type=identifier, ...``."""
    parts: list[str] = []
    for pred in predicates:
        ptype = pred.get("type")
        type_str = str(ptype) if isinstance(ptype, str) and ptype else "?"
        parts.append(f"{type_str}={_predicate_identifier(pred)}")
    return ", ".join(parts)

def _format_unsatisfied(
    per_predicate: tuple[Mapping[str, Any], ...],
) -> str:
    """Render the unmet predicates from a ``RecheckOutcome.per_predicate``.

    The recheck primitive emits per-predicate dicts with normalized
    ``type``/``identifier`` keys (see
    :func:`flywheel_core.harness._evaluate_blocked_predicate`), so we can read
    them directly without re-deriving identifiers per predicate shape.
    """
    parts: list[str] = []
    for pred in per_predicate:
        if pred.get("satisfied"):
            continue
        ptype = pred.get("type")
        type_str = str(ptype) if isinstance(ptype, str) and ptype else "?"
        identifier = pred.get("identifier")
        if isinstance(identifier, str) and identifier:
            parts.append(f"{type_str}={identifier}")
        else:
            parts.append(type_str)
    return ", ".join(parts)

def _awaiting_instruction_for_row(row: TaskStatusRow) -> str | None:
    """Resolve the pending manual gate's instruction text for ``row``.

    Returns the instruction string when the row's latest lifecycle is
    parked at ``AWAITING_APPROVAL`` with a recorded ordinal pointing at a
    :class:`ManualGrader` in the task definition; ``None`` otherwise.

    The lookup is the task itself (not the persisted column) because the
    instruction is part of the immutable task definition the run pinned
    to; the column is only the pointer into ``task.graders``. A pointer
    that does not address a manual grader is a forward-compat / data-skew
    case the surface absorbs silently rather than crashing.
    """
    if row.latest_status != Status.AWAITING_APPROVAL:
        return None
    ordinal = row.awaiting_manual_ordinal
    if ordinal is None:
        return None
    graders = row.task.graders
    if ordinal < 0 or ordinal >= len(graders):
        return None
    grader = graders[ordinal]
    if not isinstance(grader, ManualGrader):
        return None
    return grader.instruction

def _cmd_status_rollup(args: argparse.Namespace) -> int:
    # Evidence-derived projection: classify every task from grader receipts
    # and lifecycle state, then render the phase-grouped rollup. Imported
    # locally so _rollup can import this module's TaskStatusRow/TaskState at
    # its top level without a circular import.
    from flywheel_orchestrator._rollup import (
        build_rollup,
        render_rollup_text,
        rollup_to_json,
    )

    policy = _load_effective_policy(args)
    source = _resolve_work_source(args, policy)
    db_path = _resolve_db_path(args, policy)
    db_path.parent.mkdir(parents=True, exist_ok=True)
    # Thread repo_root + true_base so the rollup surfaces the phase-prerequisite
    # reachability hold (spec 00079, #7): a not-started dependent whose DONE
    # prerequisite landed on an unmerged sibling phase reads blocked_by_prereq
    # naming that phase, not idle not_started. A no-op for non-phase repos.
    repo_root, true_base = _reachability_context(args, policy)
    store = open_sqlite_bound_store(policy, db_path=db_path)
    try:
        rollup = build_rollup(
            status_rows_for_items(source.list_work(), store),
            store,
            repo_root=repo_root,
            true_base=true_base,
        )
    finally:
        store.close()
    if args.json:
        print(json.dumps(rollup_to_json(rollup), indent=2))
        return 0
    print(render_rollup_text(rollup))
    return 0


def _landing_park_for_run(
    store: SqliteStore | PostgresStore, run_id: str
) -> LandingParked | None:
    """The most recent :class:`LandingParked` event on ``run_id``, or ``None``.

    A parked DONE run is *stranded*: its work finished and graded green, but the
    strategy could not land it (uncommitted tree, divergent base, or a failed
    ``[submit] verify`` standing build invariant), so it sits on an unmerged
    branch/worktree. This is the marker the ``stranded:`` status annotation
    keys on so a strand is visible instead of accumulating silently."""
    parks = [
        e
        for e in store.list_domain_events(run_id)
        if isinstance(e, LandingParked)
    ]
    return parks[-1] if parks else None


def _stop_events_by_subject(
    policy: WorkPolicy | None,
    db_path: Path,
) -> dict[str, OrchestratorStopEventRecord]:
    """The most recent pre-run stop event per subject, keyed by subject.

    A pre-run dead-end -- a dangling prerequisite, a no-op refill cycle, a
    prepare/preflight skip, a source-listing truncation, or a zero-grader item
    drop -- never mints a run, so it lands in the orchestrator claim store's
    append-only ``orchestrator_stop_events`` ledger (its own tables on the same
    sqlite file the core store uses) rather than on any run's domain-event
    stream. This is the second record surface the ``stranded:`` status view
    unions in alongside the per-run :class:`LandingParked` parks: a unit
    stopped before a run existed is just as invisible as a DONE-but-unlanded
    strand unless status enumerates it.

    Recurrence is the ledger's signal (it never dedupes), so a subject stopped
    on several passes surfaces once here with its latest reason -- ``list_stop_events``
    returns id (insertion) order, so the last row for a subject wins. A
    :data:`STOP_RESOLVED` marker (appended when the subject's phase archived
    after a non-landing stop, when the landing probe confirmed a strand's work
    landed, or when an operator serviced the strand by hand) clears the
    subject -- regardless of its ``attribution`` (probe or operator both
    clear): only a stop appended AFTER the marker -- a fresh recurrence --
    surfaces again. An empty ledger yields an empty map, keeping the
    omit-when-absent convention intact for a healthy store. The claim store is
    built through the policy-driven :func:`build_claim_store` factory -- so a
    ``[store]`` backend of postgres reads the shared ledger, never a stale
    local sqlite file -- opened read-only-in-intent and closed before
    returning.
    """
    claims = build_claim_store(policy, db_path=db_path)
    try:
        latest: dict[str, OrchestratorStopEventRecord] = {}
        for event in claims.list_stop_events():
            # A resolution clears its subject on kind alone; the attribution
            # (probe vs operator) is an audit fact, not a gate -- both a
            # git-truth-confirmed landing and an operator abandonment clear.
            if event.kind == STOP_RESOLVED:
                latest.pop(event.subject, None)
                continue
            latest[event.subject] = event
        return latest
    finally:
        claims.close()


def _landing_strands_by_subject(
    store: SqliteStore | PostgresStore,
) -> dict[str, LandingParked]:
    """The latest landing strand per task id, read across every DONE run.

    A *landing strand* is a DONE run whose most recent
    :class:`~flywheel_core.events.LandingParked` names a park kind in
    :data:`~flywheel_core.events.LANDING_STRAND_KINDS` -- verified work the
    strategy could not land (uncommitted tree, divergent base, a failed
    ``[submit] verify`` invariant, or a protected-path refusal). Unlike
    :func:`_landing_park_for_run`, which the active-listing surface consults
    only for a task still on disk, this reads the strand from the store keyed by
    task id, so it outlives the task file: a strand whose phase archived or
    whose task file an operator moved stays surfaced (spec 00077, criterion 6),
    making visibility independent of the accident of phase composition. The
    store's ``list_lifecycles`` returns ``(updated_at DESC, run_id DESC)``, so
    the first DONE lifecycle seen for a task is its latest run; a later clean
    land (no park, or a non-strand park) on that latest run means the task is
    not a strand. Non-strand park kinds (``held-out-gate`` / ``push-failed`` /
    ``submit-error`` / ``merge-conflict``) keep the active-listing-only
    behavior and are deliberately not surfaced here.
    """
    strands: dict[str, LandingParked] = {}
    seen: set[str] = set()
    for lifecycle in store.list_lifecycles(statuses={Status.DONE}):
        if lifecycle.task_id in seen:
            continue
        seen.add(lifecycle.task_id)
        park = _landing_park_for_run(store, lifecycle.run_id)
        if park is not None and park.park_kind in LANDING_STRAND_KINDS:
            strands[lifecycle.task_id] = park
    return strands


def _attributed_resolutions_by_subject(
    policy: WorkPolicy | None,
    db_path: Path,
) -> dict[str, datetime]:
    """The latest attributed-resolution ``occurred_at`` per subject.

    A landing strand clears ONLY on a resolution marker whose ``attribution``
    names WHO cleared it -- the archive sweep's landability probe
    (:data:`RESOLUTION_ATTRIBUTION_PROBE`) or a deliberate operator
    (:data:`RESOLUTION_ATTRIBUTION_OPERATOR`) via the ``resolve`` verb (spec
    00077, criteria 3/4; D-2/D-3). The plain archival-supersession marker (a
    :data:`STOP_RESOLVED` row with empty ``attribution``, written when a phase
    archives over a *non-landing* stop) is deliberately excluded, so the act of
    archiving -- like moving or deleting the task file -- never clears a landing
    strand; only git-truth or an operator does. ``list_stop_events`` returns id
    (insertion) order, so the last attributed marker for a subject wins; a park
    appended after it -- a fresh recurrence -- surfaces again because its
    ``ts`` post-dates the marker. Read through :func:`build_claim_store` so a
    postgres ``[store]`` backend consults the shared ledger rather than a stale
    local sqlite file (the d63cf3e read-path contract).
    """
    claims = build_claim_store(policy, db_path=db_path)
    try:
        latest: dict[str, datetime] = {}
        for event in claims.list_stop_events():
            if (
                event.kind == STOP_RESOLVED
                and event.attribution in RESOLUTION_ATTRIBUTIONS
            ):
                latest[event.subject] = event.occurred_at
        return latest
    finally:
        claims.close()


def _cmd_status(args: argparse.Namespace) -> int:
    if getattr(args, "rollup", False):
        return _cmd_status_rollup(args)
    policy = _load_effective_policy(args)
    source = _resolve_work_source(args, policy)
    db_path = _resolve_db_path(args, policy)
    db_path.parent.mkdir(parents=True, exist_ok=True)
    store = open_sqlite_bound_store(policy, db_path=db_path)
    try:
        rows = status_rows_for_items(source.list_work(), store)
        # Map each in-flight run to its persisted worker_id from the same
        # relational live snapshot the ``live`` view uses, so ``status
        # --json`` can surface who is driving an active run. Unset
        # worker_id stays None (never a sentinel) per SI-11.
        live_worker_ids = {
            live.run_id: live.worker_id for live in collect_live_rows(store)
        }
        # A parked DONE run is stranded -- finished but never landed. Collect
        # the park reason per run while the store is open (the rows render
        # after it closes), bounded to DONE rows since a park only attaches at
        # the DONE landing site.
        parked_landings: dict[str, LandingParked] = {}
        for row in rows:
            if (
                row.latest_run_id is not None
                and row.latest_status == Status.DONE
            ):
                park = _landing_park_for_run(store, row.latest_run_id)
                if park is not None:
                    parked_landings[row.latest_run_id] = park
        # The store-backed landing-strand surface (spec 00077, criterion 6):
        # every DONE run whose latest park is a landing strand, keyed by task
        # id, read from the store so it outlives the active listing. Rows still
        # on disk render their strand via ``parked_landings`` above; the tail
        # unions in the ones whose task file is gone.
        landing_strands = _landing_strands_by_subject(store)
    finally:
        store.close()
    # The second stranded record surface: pre-run stops (dangling prerequisite,
    # no-op cycle, prepare skip, source truncation, zero-grader drop) that
    # dead-ended a scheduling pass before any run existed. Read from the
    # orchestrator claim store's stop-event ledger and unioned into the same
    # stranded surface as the per-run parks above -- keyed by subject (a task id
    # for the per-task kinds, a source name for the source-level kinds).
    stops_by_subject = _stop_events_by_subject(policy, db_path)
    stopped_task_ids = {row.task.id for row in rows}
    # A landing strand whose task file left the active listing (its phase
    # archived, or an operator moved/deleted it) has no row above, so surface it
    # keyed by subject here -- unless an attributed resolution marker cleared it.
    # A strand for a task still in the listing is rendered on its row above; a
    # park whose ``ts`` post-dates the latest resolution is a fresh recurrence
    # and surfaces again (spec 00077, criterion 6).
    resolved_at = _attributed_resolutions_by_subject(policy, db_path)
    rowless_strands = {
        subject: park
        for subject, park in landing_strands.items()
        if subject not in stopped_task_ids
        and (subject not in resolved_at or park.ts > resolved_at[subject])
    }
    if args.json:
        out: list[dict[str, Any]] = []
        for row in rows:
            entry: dict[str, Any] = {
                "task_id": row.task.id,
                "task_file": str(row.task_file),
                "source_ref": row.source_ref,
                "state": row.state.value,
                "latest_run_id": row.latest_run_id,
                "latest_status": (
                    row.latest_status.value if row.latest_status else None
                ),
                "latest_error": row.latest_error,
                "prerequisites": list(row.prerequisites),
            }
            # Surface the in-flight worker_id only for a run that is
            # actually live and carries a persisted worker id; omit the key
            # otherwise (mirrors the blocked_requires / awaiting_on
            # omit-when-absent convention).
            worker_id = (
                live_worker_ids.get(row.latest_run_id)
                if row.latest_run_id is not None
                else None
            )
            if worker_id is not None:
                entry["worker_id"] = worker_id
            parsed = _parse_blocked_requires(row.blocked_requires)
            if parsed is not None:
                # Spec: omit the key entirely when null; emit the parsed
                # list (list of dicts) when present.
                entry["blocked_requires"] = parsed
            instruction = _awaiting_instruction_for_row(row)
            if instruction is not None:
                # Mirrors the blocked_requires convention: emit the
                # awaiting-gate context only when the lifecycle is
                # actually parked on a manual gate, omit otherwise.
                entry["awaiting_on"] = {
                    "ordinal": row.awaiting_manual_ordinal,
                    "instruction": instruction,
                }
            park = (
                parked_landings.get(row.latest_run_id)
                if row.latest_run_id is not None
                else None
            )
            if park is not None:
                # A landed-but-parked (stranded) DONE run: surface the park
                # reason on the same omit-when-absent convention as awaiting_on.
                entry["stranded"] = {
                    "park_kind": park.park_kind,
                    "detail": park.detail,
                }
            stop = stops_by_subject.get(row.task.id)
            if stop is not None:
                # A pre-run stop keyed to this task id (a dangling prerequisite
                # or a prepare skip): surface its kind and detail on the same
                # omit-when-absent convention as stranded.
                entry["stopped"] = {"kind": stop.kind, "detail": stop.detail}
            out.append(entry)
        # Source-level stops (no-op cycle, source truncation, zero-grader drop)
        # are keyed to a source name, not a task in the work list, so they have
        # no row above. Enumerate them as their own entries so no stopped unit
        # is dropped from the surface.
        for subject in sorted(stops_by_subject):
            if subject in stopped_task_ids:
                continue
            stop = stops_by_subject[subject]
            out.append(
                {
                    "subject": subject,
                    "stopped": {"kind": stop.kind, "detail": stop.detail},
                }
            )
        # Store-backed landing strands whose task file left the active listing
        # have no row above; enumerate each as its own entry so an unlanded
        # strand stays visible after its phase archived (spec 00077, criterion
        # 6), keyed by subject like the source-level stops.
        for subject in sorted(rowless_strands):
            park = rowless_strands[subject]
            out.append(
                {
                    "subject": subject,
                    "stranded": {
                        "park_kind": park.park_kind,
                        "detail": park.detail,
                    },
                }
            )
        print(json.dumps(out, indent=2))
        return 0
    if not rows and not stops_by_subject and not rowless_strands:
        print("(no active tasks)")
        return 0
    width = max((len(row.task.id) for row in rows), default=0)
    for row in rows:
        # File-backed rows render their phase directory; external items
        # (empty task_file) render under their source ref instead.
        phase = row.task_file.parent.name or row.source_ref or "external"
        suffix = ""
        if row.latest_error:
            suffix = f"  -- {row.latest_error}"
        print(
            f"  {phase}/{row.task.id:<{width}}  "
            f"{row.state.value:<17}{suffix}"
        )
        if (
            row.latest_status == Status.INTERRUPTED
            and row.blocked_requires is not None
        ):
            parsed = _parse_blocked_requires(row.blocked_requires)
            if parsed:
                print(f"    blocked_on: {_format_blocked_on(parsed)}")
        instruction = _awaiting_instruction_for_row(row)
        if instruction is not None:
            # The owed decision is rendered as a follow-up line, mirroring
            # ``blocked_on:`` for INTERRUPTED rows. Operators reading
            # ``flywheel status`` see the gate's instruction without
            # cross-referencing the task file or the audit stream.
            print(f"    awaiting_on: {instruction}")
        park = (
            parked_landings.get(row.latest_run_id)
            if row.latest_run_id is not None
            else None
        )
        if park is not None:
            # A DONE run whose work never landed: surface the strand and its
            # cause as a follow-up line so it is visible at a glance instead of
            # accumulating as a silent unmerged branch.
            detail = f" -- {park.detail}" if park.detail else ""
            print(f"    stranded: {park.park_kind}{detail}")
        stop = stops_by_subject.get(row.task.id)
        if stop is not None:
            # A pre-run stop keyed to this task id: surface its kind and cause
            # as a follow-up line, mirroring the stranded: convention.
            detail = f" -- {stop.detail}" if stop.detail else ""
            print(f"    stopped: {stop.kind}{detail}")
    # Source-level stops have no task row above; enumerate each as its own line
    # so every stopped unit is visible with its reason.
    for subject in sorted(stops_by_subject):
        if subject in stopped_task_ids:
            continue
        stop = stops_by_subject[subject]
        detail = f" -- {stop.detail}" if stop.detail else ""
        print(f"  {subject}  stopped: {stop.kind}{detail}")
    # Landing strands whose task file left the active listing have no row above;
    # enumerate each as its own line so an unlanded strand stays visible after
    # its phase archived, mirroring the source-level stopped: convention.
    for subject in sorted(rowless_strands):
        park = rowless_strands[subject]
        detail = f" -- {park.detail}" if park.detail else ""
        print(f"  {subject}  stranded: {park.park_kind}{detail}")
    return 0

def _list_blocked_lifecycles(store: SqliteStore | PostgresStore) -> list[tuple[str, str]]:
    """Return ``(run_id, task_id)`` for every recheckable blocked lifecycle.

    Filter mirrors spec FR-7: only ``INTERRUPTED`` rows with a non-NULL
    ``blocked_requires_json`` qualify — SIGINT-paused lifecycles (which
    leave the column NULL) are intentionally excluded so they keep using
    the existing run_task entry-time normalization to resume.
    """
    # Cross-task lifecycle read through the public protocol surface (SI-3),
    # off the private connection. list_lifecycles orders (updated_at DESC,
    # run_id DESC); reverse to keep the original oldest-updated-first scan
    # order, then keep only the recheck-eligible rows (non-NULL persisted
    # requires snapshot).
    interrupted = store.list_lifecycles(statuses=(Status.INTERRUPTED,))
    return [
        (lc.run_id, lc.task_id)
        for lc in reversed(interrupted)
        if lc.blocked_requires_json is not None
    ]

def _format_recheck_line(
    run_id: str, outcome: RecheckOutcome, *, dry_run: bool
) -> str:
    """Render one CLI line for a single recheck outcome.

    Three cases per spec FR-6:

    * ``applied=True`` (live mode, transitioned) -> ``<run_id>: unblocked``;
    * ``reason="dry_run"`` with every predicate satisfied -> ``would unblock``;
    * anything else (``unsatisfied``, ``dry_run`` with misses, ``parse_error``)
      -> ``still blocked (...)`` with a short summary of unmet predicates.

    The summary is intentionally derived from ``per_predicate`` rather
    than the persisted requires snapshot so it names only the misses, not
    every predicate.
    """
    if outcome.applied:
        return f"{run_id}: unblocked"
    if dry_run and outcome.reason == "dry_run" and all(
        bool(p.get("satisfied")) for p in outcome.per_predicate
    ):
        return f"{run_id}: would unblock"
    summary = _format_unsatisfied(outcome.per_predicate)
    if not summary:
        summary = outcome.reason
    return f"{run_id}: still blocked ({summary})"

def _cmd_recheck_blocked(args: argparse.Namespace) -> int:
    policy = _load_effective_policy(args)
    source = _resolve_work_source(args, policy)
    db_path = _resolve_db_path(args, policy)
    db_path.parent.mkdir(parents=True, exist_ok=True)
    store = open_sqlite_bound_store(policy, db_path=db_path)
    sink = FileTelemetrySink(db_path.parent / "logs")
    try:
        # Build a task_id -> Task map from the work source. An archived
        # (or no-longer-listed) task whose lifecycle is blocked in the
        # store is skipped with a stderr warning so a single missing item
        # does not crash the batch.
        task_by_id: dict[str, Task] = {
            item.task.id: item.task for item in source.list_work()
        }

        if args.run_id:
            lifecycle = store.load_lifecycle(args.run_id)
            if lifecycle is None:
                print(f"{args.run_id}: not found")
                return 0
            if (
                lifecycle.status != Status.INTERRUPTED
                or lifecycle.blocked_requires_json is None
            ):
                print(f"{args.run_id}: not blocked")
                return 0
            task = task_by_id.get(lifecycle.task_id)
            if task is None:
                print(
                    f"warning: {args.run_id}: task "
                    f"{lifecycle.task_id!r} not found in active tasks; "
                    f"skipping",
                    file=sys.stderr,
                )
                return 0
            outcome = recheck_blocked_lifecycle(
                store, args.run_id, task, dry_run=args.dry_run, sink=sink
            )
            print(
                _format_recheck_line(
                    args.run_id, outcome, dry_run=args.dry_run
                )
            )
            return 0

        targets = _list_blocked_lifecycles(store)
        if not targets:
            print("(no blocked lifecycles)")
            return 0
        for run_id, task_id in targets:
            task = task_by_id.get(task_id)
            if task is None:
                print(
                    f"warning: {run_id}: task {task_id!r} not found in "
                    f"active tasks; skipping",
                    file=sys.stderr,
                )
                continue
            outcome = recheck_blocked_lifecycle(
                store, run_id, task, dry_run=args.dry_run, sink=sink
            )
            print(_format_recheck_line(run_id, outcome, dry_run=args.dry_run))
        return 0
    finally:
        store.close()
        sink.close()

def _cmd_recover(args: argparse.Namespace) -> int:
    policy = _load_effective_policy(args)
    db_path = _resolve_db_path(args, policy)
    db_path.parent.mkdir(parents=True, exist_ok=True)
    store = open_sqlite_bound_store(policy, db_path=db_path)
    try:
        with FileTelemetrySink(db_path.parent / "logs") as sink:
            finalized = recover_stranded_lifecycles(
                store, task_id=args.task_id, sink=sink
            )
    finally:
        store.close()
    if not finalized:
        print("(no stranded lifecycles)")
        return 0
    for run_id in finalized:
        print(run_id)
    return 0

def _cmd_resolve(args: argparse.Namespace) -> int:
    """Deliberately abandon a strand (spec 00077, criterion 5 / D-3).

    Records an operator-attributed
    :data:`~flywheel_orchestrator._claims.STOP_RESOLVED` marker keyed to the
    task id (the stop-event subject), carrying the required ``--reason`` text
    verbatim, through the policy-selected claim store. That marker both clears
    the strand from the ``status`` stranded view and, on the next archive
    sweep, unblocks the otherwise-landed phase -- the only non-probe path to
    resolution. Never writes direct SQL and never a task-file tombstone.

    Resolving a task id with no unresolved stop event is a deterministic,
    human-readable refusal (exit 1), never a traceback: there is nothing to
    abandon when the subject has no stop row, or its latest row is already a
    resolution.
    """
    reason = args.reason
    if not reason.strip():
        print("error: --reason must not be empty", file=sys.stderr)
        return 2
    policy = _load_effective_policy(args)
    db_path = _resolve_db_path(args, policy)
    db_path.parent.mkdir(parents=True, exist_ok=True)
    claims = build_claim_store(policy, db_path=db_path)
    try:
        events = claims.list_subject_stop_events(args.task_id)
        latest = events[-1] if events else None
        if latest is None or latest.kind == STOP_RESOLVED:
            state = (
                "already resolved" if latest is not None else "no stop event"
            )
            print(
                f"error: task {args.task_id!r} has no unresolved stop event "
                f"to abandon ({state})",
                file=sys.stderr,
            )
            return 1
        # Record the operator resolution verbatim (spec 00077, criterion 5):
        # the reason round-trips into the marker the audit trail shows, and the
        # attribution token distinguishes this deliberate abandonment from a
        # git-truth (probe) resolution. Keyed to the task id, through the
        # policy-selected claim store -- never direct SQL, never a tombstone.
        claims.record_stop_event(
            kind=STOP_RESOLVED,
            subject=args.task_id,
            detail=f"operator abandoned the strand: {reason}",
            occurred_at=datetime.now(timezone.utc),
            attribution=RESOLUTION_ATTRIBUTION_OPERATOR,
        )
    finally:
        claims.close()
    print(
        f"resolved {args.task_id}: abandoned strand "
        f"(superseded {latest.kind!r})"
    )
    return 0

def _cmd_archive(args: argparse.Namespace) -> int:
    policy = _load_effective_policy(args)
    if args.tasks_dir:
        tasks_dir = Path(args.tasks_dir)
    elif policy is not None and policy.source_kind == "directory":
        assert policy.tasks_dir is not None  # load_policy guarantees it
        tasks_dir = policy.tasks_dir
    elif policy is not None:
        raise PolicyError(
            "archive applies to directory work sources only; the active "
            "policy selects a tracker source (pass --tasks-dir to archive "
            "a directory layout explicitly)"
        )
    else:
        tasks_dir = DEFAULT_TASKS_DIR
    db_path = _resolve_db_path(args, policy)
    db_path.parent.mkdir(parents=True, exist_ok=True)
    # Thread repo_root + landing_base + true_base so the CLI verb applies the
    # same gates the worker's sweep does: the landed predicate (a phase archives
    # only when every DONE task's work is landed), the phase-branch merge
    # predicate (under the phase strategy a phase archives only when its
    # integration branch merged into the true base -- criterion 8 forecloses the
    # gateless CLI bypass), the loop-path gate, and any configured phase-verify.
    # The landing/true base is the configured submit base, else the operator's
    # checked-out branch (HEAD); the phase-merge gate arms only when a
    # ``flywheel/phase/<phase>`` branch exists, so merge/pr repos are unchanged.
    # Refusals print to stderr so the moved-dest stdout stays machine-parseable.
    repo_root = repo_root_for_tasks_dir(tasks_dir)
    landing_base = (
        policy.submit_base if policy is not None else None
    ) or "HEAD"
    phase_verify = policy.phase_verify if policy is not None else None
    store = open_sqlite_bound_store(policy, db_path=db_path)
    try:
        claims = build_claim_store(policy, db_path=db_path)
        try:
            moved = archive_completed_phases(
                tasks_dir,
                store,
                repo_root=repo_root,
                log=lambda msg: print(msg, file=sys.stderr),
                phase_verify=phase_verify,
                landing_base=landing_base,
                true_base=landing_base,
                claims=claims,
            )
        finally:
            claims.close()
    finally:
        store.close()
    for dest in moved:
        print(str(dest))
    return 0


def repo_root_for_tasks_dir(tasks_dir: Path) -> Path:
    """Resolve the repo root a grader's ``run`` path tokens are relative to.

    Tasks live under ``<repo_root>/.flywheel/tasks``; the static path checks
    in :func:`flywheel_core.validate_task` are repo-relative, so the repo
    root is the ancestor that contains the ``.flywheel`` directory. A custom
    ``--tasks-dir`` outside a ``.flywheel`` tree falls back to the current
    working directory.
    """
    resolved = tasks_dir.resolve()
    for parent in resolved.parents:
        if parent.name == ".flywheel":
            return parent.parent
    return Path.cwd()


def _reachability_context(
    args: argparse.Namespace, policy: WorkPolicy | None
) -> tuple[Path, str]:
    """Resolve ``(repo_root, true_base)`` for the phase-prerequisite
    reachability hold from a read verb's args and effective policy.

    The base is the configured submit base, else the checked-out branch
    (``HEAD``); the repo root is the ``.flywheel`` ancestor of the resolved
    tasks dir (falling back to cwd). Both feed
    :func:`reachability_held_prerequisites`, a no-op for non-phase repos, so a
    tracker source or a merge/pr repo is unaffected by the resolution.
    """
    tasks_dir_arg = getattr(args, "tasks_dir", None)
    if tasks_dir_arg:
        tasks_dir = Path(tasks_dir_arg)
    elif policy is not None and policy.tasks_dir is not None:
        tasks_dir = policy.tasks_dir
    else:
        tasks_dir = DEFAULT_TASKS_DIR
    repo_root = repo_root_for_tasks_dir(tasks_dir)
    true_base = (policy.submit_base if policy is not None else None) or "HEAD"
    return repo_root, true_base


def _cmd_validate(args: argparse.Namespace) -> int:
    """Statically validate every active task (spec 00034 + surface-overlap lint).

    Two static, pre-dispatch checks over the active listing:

    * per-task: :func:`flywheel_core.validate_task` flags an un-runnable command
      grader;
    * pairwise: :func:`surface_overlap_defects` flags two tasks whose derived
      file surfaces overlap with no shared ``conflict_keys`` entry, no
      ``overlap_ok`` allow marker, and no prerequisite chain between them (the
      concurrent-write / rebase-collision trap).

    Exits non-zero, naming each offending task and its defects, when anything is
    flagged; exit 0 when all are valid. Neither check runs a grader, reads the
    store, or tests whether a derived path exists on disk.
    """
    policy = _load_effective_policy(args)
    if args.tasks_dir:
        tasks_dir = Path(args.tasks_dir)
    elif policy is not None and policy.source_kind == "directory":
        assert policy.tasks_dir is not None  # load_policy guarantees it
        tasks_dir = policy.tasks_dir
    elif policy is not None:
        raise PolicyError(
            "validate applies to directory work sources only; the active "
            "policy selects a tracker source (pass --tasks-dir to validate "
            "a directory layout explicitly)"
        )
    else:
        tasks_dir = DEFAULT_TASKS_DIR
    repo_root = repo_root_for_tasks_dir(tasks_dir)
    loaded = load_active_tasks(tasks_dir)
    invalid: dict[str, list[TaskDefect]] = {}
    for path, task in loaded:
        defects = validate_task(task, repo_root=repo_root)
        if defects:
            invalid.setdefault(task.id, []).extend(defects)
    surfaces = [build_surface(path, task) for path, task in loaded]
    for defect in surface_overlap_defects(surfaces):
        invalid.setdefault(defect.task_id, []).append(defect)
    valid_count = len(loaded) - len(invalid)
    if not invalid:
        print(f"All {valid_count} active task(s) valid.")
        return 0
    for task_id in sorted(invalid):
        print(f"{task_id}: invalid task definition")
        for defect in invalid[task_id]:
            print(f"  - {defect.detail}")
    print(
        f"{len(invalid)} invalid task(s); {valid_count} valid.",
        file=sys.stderr,
    )
    return 1


def _resolve_fallback_phases(
    args: argparse.Namespace, policy: WorkPolicy | None
) -> Mapping[str, str] | None:
    """Task-id -> phase fallback for runs recorded before schema v12.

    Pre-v12 lifecycles carry no ``source``; for directory work sources
    the task files themselves (active or archived) still name the phase,
    so the on-disk scan attributes those runs. Tracker sources have no
    directory layout to scan — return ``None`` and those runs render
    ungrouped.
    """
    if args.tasks_dir:
        return build_task_phase_index(Path(args.tasks_dir))
    if policy is None:
        return build_task_phase_index(DEFAULT_TASKS_DIR)
    if policy.source_kind == "directory" and policy.tasks_dir is not None:
        return build_task_phase_index(policy.tasks_dir)
    return None


def _format_history_ts(ts: datetime | None) -> str:
    return ts.strftime("%Y-%m-%d %H:%M") if ts is not None else "—"


def _history_run_to_dict(run: HistoryRun) -> dict[str, Any]:
    return {
        "run_id": run.run_id,
        "task_id": run.task_id,
        "status": run.status.value,
        "source": run.source or None,
        "started_at": (
            run.started_at.isoformat() if run.started_at else None
        ),
        "finished_at": (
            run.finished_at.isoformat() if run.finished_at else None
        ),
        "retries": run.retries,
        "error": run.error,
        "attempts": run.attempts,
        "tokens_total": run.tokens_total,
        "cost_usd_total": run.cost_usd_total,
        "turns_total": run.turns_total,
    }


def _format_history_line(row: HistoryRow, *, width: int) -> str:
    run = row.latest
    label = f"{row.phase}/{row.task_id}" if row.phase else row.task_id
    runs_total = 1 + len(row.prior_runs)
    suffix = f"  -- {run.error}" if run.error else ""
    return (
        f"  {label:<{width}}  {run.status.value:<17} "
        f"{_format_history_ts(run.finished_at)}  "
        f"runs={runs_total}  tokens={run.tokens_total}  "
        f"cost=${run.cost_usd_total:.4f}{suffix}"
    )


def _cmd_history(args: argparse.Namespace) -> int:
    policy = _load_effective_policy(args)
    db_path = _resolve_db_path(args, policy)
    db_path.parent.mkdir(parents=True, exist_ok=True)
    statuses = (
        tuple(Status(s) for s in args.status)
        if args.status
        else TERMINAL_STATUSES
    )
    store = open_sqlite_bound_store(policy, db_path=db_path)
    try:
        rows = collect_history_rows(
            store,
            statuses=statuses,
            phase=args.phase,
            limit=args.limit,
            fallback_phases=_resolve_fallback_phases(args, policy),
        )
    finally:
        store.close()
    if args.json:
        out = [
            {
                "task_id": row.task_id,
                "phase": row.phase,
                "latest": _history_run_to_dict(row.latest),
                "prior_runs": [
                    _history_run_to_dict(r) for r in row.prior_runs
                ],
            }
            for row in rows
        ]
        print(json.dumps(out, indent=2))
        return 0
    if not rows:
        print("(no finished runs)")
        return 0
    width = max(
        len(f"{row.phase}/{row.task_id}" if row.phase else row.task_id)
        for row in rows
    )
    for row in rows:
        print(_format_history_line(row, width=width))
    return 0


def _print_run_detail(
    detail: RunDetail, *, redactor: Redactor | None = None
) -> None:
    run = detail.run
    print(f"task     : {run.task_id}")
    print(f"phase    : {detail.phase or '—'}")
    print(f"run      : {run.run_id}")
    print(f"status   : {run.status.value}")
    if run.source:
        print(f"source   : {run.source}")
    print(f"started  : {_format_history_ts(run.started_at)}")
    print(f"finished : {_format_history_ts(run.finished_at)}")
    print(f"retries  : {run.retries}")
    print(
        f"totals   : tokens={run.tokens_total} "
        f"cost=${run.cost_usd_total:.4f} turns={run.turns_total}"
    )
    if run.error:
        print(f"error    : {run.error}")
    if detail.task is not None:
        print(f"goal     : {detail.task.goal}")
    if detail.attempts:
        print("attempts :")
        for a in detail.attempts:
            line = (
                f"  {a.number}  {a.outcome:<18} iter={a.iterations} "
                f"turns={a.turns} tokens={a.tokens} "
                f"cost=${a.cost_usd:.4f}"
            )
            if a.error:
                line += f"  -- {_short(a.error, 80)}"
            print(line)
            for g in a.grader_results:
                verdict = "pass" if g.passed else "FAIL"
                name = g.grader_name or g.grader_type
                print(
                    f"      {verdict}  {g.grader_type:<10} {name}  "
                    f"({g.duration_ms} ms)"
                )
    if detail.decisions:
        print("decisions:")
        for event in detail.decisions:
            for line in _format_decision_lines(event, redactor):
                print(line)
    if detail.agent_output:
        print("agent output:")
        print(detail.agent_output)
    if detail.related_runs:
        print("related runs:")
        for r in detail.related_runs:
            print(
                f"  {r.run_id}  {r.status.value:<17} "
                f"{_format_history_ts(r.finished_at)}"
            )


def _grader_result_to_dict(g: GraderResultRecord) -> dict[str, Any]:
    return {
        "attempt_number": g.attempt_number,
        "ordinal": g.ordinal,
        "grader_type": g.grader_type,
        "grader_name": g.grader_name,
        "passed": g.passed,
        "duration_ms": g.duration_ms,
    }


def _decision_to_dict(
    event: DomainEvent, redactor: Redactor | None
) -> dict[str, Any]:
    """Render one landing-stage decision event as a JSON-ready dict.

    The event-specific payload (``event_payload``) carries the diagnosable
    fields the spec names -- the gate ``outcome``/``receipts`` with each
    grader's ``output_excerpt`` (00073 #1), a park's ``park_kind``/``detail``
    (#2), a landing's ``strategy``/``landed_ref`` (#3), a redrive's ``result``
    (#5) -- flattened onto the record beside a ``kind`` discriminator and the
    ledger coordinates.

    Redaction runs through the exact audit path: the payload is wrapped in an
    :class:`EventRecord` and passed through ``redactor`` so every string leaf
    (including a nested ``receipts[].output_excerpt``) is scrubbed by the same
    rules the audit stream uses. ``redactor=None`` -- the ``--raw`` opt-out --
    renders the stored value verbatim (00073 #10).
    """
    payload: dict[str, Any] = event_payload(event)
    if redactor is not None:
        redacted = redactor.redact(
            EventRecord(
                run_id=event.run_id,
                ts=event.ts,
                kind=event_kind(event),
                payload=payload,
                attempt_number=event.attempt_number,
                sequence=event.sequence,
            )
        )
        payload = dict(redacted.payload)
    return {
        "kind": event_kind(event),
        "ts": event.ts.isoformat(),
        "attempt_number": event.attempt_number,
        "sequence": event.sequence,
        **payload,
    }


def _format_decision_lines(
    event: DomainEvent, redactor: Redactor | None
) -> list[str]:
    """Format one decision event as human-readable text lines.

    Reuses :func:`_decision_to_dict` so the text view redacts identically to
    the JSON view. The first line is a ``<kind>`` summary; a gate verdict adds
    one indented line per executed grader carrying its (redacted) output
    excerpt.
    """
    data = _decision_to_dict(event, redactor)
    kind = data["kind"]
    lines: list[str] = []
    if kind == "held_out_gate_evaluated":
        summary = f"  gate      {data.get('outcome', '')}"
        reason = data.get("reason") or ""
        if reason:
            summary += f"  {_short(reason, 80)}"
        lines.append(summary)
        for receipt in data.get("receipts", []):
            verdict = "pass" if receipt.get("passed") else "FAIL"
            name = receipt.get("grader_name") or "(unnamed)"
            excerpt = _short(str(receipt.get("output_excerpt", "")), 120)
            lines.append(f"      {verdict}  {name}  {excerpt}")
    elif kind == "landing_parked":
        detail_text = data.get("detail") or ""
        line = f"  parked    {data.get('park_kind', '')}"
        if detail_text:
            line += f"  {_short(detail_text, 80)}"
        lines.append(line)
    elif kind == "landed":
        lines.append(
            f"  landed    {data.get('strategy', '')}  "
            f"{data.get('landed_ref', '')}"
        )
    elif kind == "landing_redriven":
        park_kind = data.get("park_kind") or ""
        line = f"  redriven  {data.get('result', '')}"
        if park_kind:
            line += f"  (was {park_kind})"
        lines.append(line)
    else:  # pragma: no cover - defensive over the closed decision set
        lines.append(f"  {kind}")
    return lines


def _run_detail_to_dict(
    detail: RunDetail, *, redactor: Redactor | None = None
) -> dict[str, Any]:
    return {
        "run": _history_run_to_dict(detail.run),
        "phase": detail.phase,
        "goal": detail.task.goal if detail.task is not None else None,
        "agent_output": detail.agent_output,
        "attempts": [
            {
                "number": a.number,
                "outcome": a.outcome,
                "started_at": (
                    a.started_at.isoformat() if a.started_at else None
                ),
                "ended_at": (
                    a.ended_at.isoformat() if a.ended_at else None
                ),
                "iterations": a.iterations,
                "turns": a.turns,
                "tokens": a.tokens,
                "cost_usd": a.cost_usd,
                "error": a.error,
                # Each attempt's verdicts keyed to that attempt (00073 #6):
                # a retried run surfaces the discarded attempt's receipts too.
                "grader_results": [
                    _grader_result_to_dict(g) for g in a.grader_results
                ],
            }
            for a in detail.attempts
        ],
        # Flat view of every attempt's receipts (each carries attempt_number);
        # attempts[].grader_results above is the per-attempt keyed split.
        "grader_results": [
            _grader_result_to_dict(g) for g in detail.grader_results
        ],
        "related_runs": [
            _history_run_to_dict(r) for r in detail.related_runs
        ],
        # Landing-stage decisions from the ledger (00073): gate verdicts,
        # parks, landings, redrives. Read from the store, so they survive
        # telemetry-file loss (#8); secret-shaped values are redacted unless
        # the caller opted into --raw (#10).
        "decisions": [
            _decision_to_dict(d, redactor) for d in detail.decisions
        ],
    }


@dataclass(frozen=True)
class _CommitProvenance:
    """How a ``show`` argument resolved against git as a commit object.

    ``is_commit`` is ``True`` iff the argument names a commit in the repo.
    ``run_id`` carries the commit's ``Flywheel-Run`` trailer value when one is
    present (spec 00078, criterion 5), else ``None`` — an un-attributed commit
    (criterion 6). Resolution is git-truth (D-3): the run id is read straight
    off the commit's trailer, never guessed from a nearby run.
    """

    is_commit: bool
    run_id: str | None = None


def _show_repo_root(
    args: argparse.Namespace, policy: WorkPolicy | None
) -> Path:
    """The repo the commit lookup resolves a SHA against.

    Mirrors :func:`_resolve_fallback_phases`' source of a tasks dir: an explicit
    ``--tasks-dir`` wins, then the policy's directory source, else the current
    working directory. Git itself walks up from this path to the work tree, so
    any directory inside the repo resolves the same objects.
    """
    if args.tasks_dir:
        return repo_root_for_tasks_dir(Path(args.tasks_dir))
    if (
        policy is not None
        and policy.source_kind == "directory"
        and policy.tasks_dir is not None
    ):
        return repo_root_for_tasks_dir(policy.tasks_dir)
    return Path.cwd()


def _resolve_commit_provenance(
    repo_root: Path, ref: str
) -> _CommitProvenance:
    """Resolve ``ref`` to its producing run via its ``Flywheel-Run`` trailer.

    Two git reads: ``rev-parse`` peels ``ref`` to a commit object (a non-commit
    — an unknown id, a tree/blob — yields ``is_commit=False`` so the caller
    keeps the existing not-found behavior), then ``git show`` extracts the
    :data:`TRAILER_KEY_RUN` trailer value using git's own trailer parser rather
    than re-deriving trailer syntax here. A commit with no such trailer is
    reported as ``run_id=None`` (un-attributed), never mapped to a nearby run.
    """
    code, _ = _git_capture(
        repo_root, "rev-parse", "--verify", "--quiet", f"{ref}^{{commit}}"
    )
    if code != 0:
        return _CommitProvenance(is_commit=False)
    code, out = _git_capture(
        repo_root,
        "show",
        "--no-patch",
        f"--format=%(trailers:key={TRAILER_KEY_RUN},valueonly)",
        ref,
    )
    if code != 0:
        return _CommitProvenance(is_commit=False)
    values = [line for line in out.strip().splitlines() if line.strip()]
    run_id = values[0].strip() if values else None
    return _CommitProvenance(is_commit=True, run_id=run_id)


def _cmd_show(args: argparse.Namespace) -> int:
    policy = _load_effective_policy(args)
    db_path = _resolve_db_path(args, policy)
    db_path.parent.mkdir(parents=True, exist_ok=True)
    store = open_sqlite_bound_store(policy, db_path=db_path)
    commit: _CommitProvenance | None = None
    try:
        run_id = resolve_run_id(store, args.run_or_task_id)
        if run_id is None:
            # Not a known run or task id: the argument may be a landed commit.
            # SHA handling engages only here — run/task resolution above is
            # byte-identical to before and never touches git (spec 00078 #5/#6).
            commit = _resolve_commit_provenance(
                _show_repo_root(args, policy), args.run_or_task_id
            )
            if not commit.is_commit:
                print(f"{args.run_or_task_id}: no run or task with that id")
                return 1
            if commit.run_id is None:
                print(
                    f"{args.run_or_task_id}: un-attributed commit "
                    f"(no {TRAILER_KEY_RUN} trailer)"
                )
                return 1
            run_id = commit.run_id
        detail = collect_run_detail(
            store,
            run_id,
            fallback_phases=_resolve_fallback_phases(args, policy),
        )
    finally:
        store.close()
    if detail is None:
        if commit is not None:
            # The commit's trailer named a run absent from the store: a
            # dangling provenance pointer, distinct from an un-attributed
            # commit — name the missing run id, never fall back to a guess.
            print(
                f"{args.run_or_task_id}: {TRAILER_KEY_RUN} names run "
                f"{run_id}, which is not in the store"
            )
            return 1
        print(f"{args.run_or_task_id}: no run or task with that id")
        return 1
    # Redaction is on by default (00073 #10): decision-record output excerpts
    # pass through the same best-effort pattern set the audit surface uses.
    # --raw opts back into verbatim output for authorized forensics.
    redactor = None if args.raw else default_policy()
    if args.json:
        print(
            json.dumps(
                _run_detail_to_dict(detail, redactor=redactor), indent=2
            )
        )
        return 0
    _print_run_detail(detail, redactor=redactor)
    return 0


INIT_ROOT = Path(".flywheel")

_INIT_GITIGNORE = """\
# Flywheel runtime state: never committed.
flywheel.sqlite
flywheel.sqlite-shm
flywheel.sqlite-wal
sandboxes/
worktrees/
# Run telemetry JSONL (logs/runs/<run_id>.jsonl) and supervisor logs:
# verbatim, sensitive-by-default payloads. Never committed.
logs/
.merge.lock
# Blind held-out oracles authored by /fw-verify: throwaway scratch run at
# verify time, never landed (the durable artifact is the recorded proof).
verification/
"""

_DOCKERIGNORE_ENTRY = ".flywheel/"

# A copy-pasteable, single-line pointer init PRINTS (never writes) so an
# operator can drop it into their own CLAUDE.md. Naming both the
# ``flywheel-ops`` skill and the ``fw docs`` verb closes the discovery gap
# ("sessions forget the skills exist") without tooling ever editing the
# operator-owned instruction file (spec 00072 D-3).
_INIT_CLAUDEMD_POINTER = (
    "This repository runs on flywheel: load the `flywheel-ops` skill to "
    "operate the loop, and run `fw docs <topic>` for authoritative flywheel "
    "documentation."
)


def _ensure_dockerignore_covers_flywheel(root: Path) -> str | None:
    """Keep ``.flywheel/`` (worktrees, logs, db) out of docker build contexts.

    Docker does not read ``.gitignore``, so a repo that builds an image with
    ``COPY . .`` would otherwise ship every nested worktree's setup artifacts
    (.venv, node_modules) into the context. Acts only when a Dockerfile or
    Containerfile sits at the context root; appends the entry once and never
    rewrites an existing one, so re-running init stays idempotent.

    Returns the line appended, or ``None`` when nothing was written.
    """
    has_dockerfile = any(
        next(root.glob(pattern), None) is not None
        for pattern in ("Dockerfile*", "Containerfile*")
    )
    if not has_dockerfile:
        return None
    dockerignore = root / ".dockerignore"
    prior = ""
    if dockerignore.is_file():
        prior = dockerignore.read_text(encoding="utf-8")
        existing = {line.strip() for line in prior.splitlines()}
        if existing & {".flywheel", ".flywheel/", "/.flywheel", "/.flywheel/"}:
            return None
    prefix = "" if not prior or prior.endswith("\n") else "\n"
    with dockerignore.open("a", encoding="utf-8") as handle:
        handle.write(
            prefix
            + "# Flywheel runtime state (worktrees, logs, db): keep out of "
            "image build contexts.\n"
            f"{_DOCKERIGNORE_ENTRY}\n"
        )
    return _DOCKERIGNORE_ENTRY

_INIT_POLICY_HEADER = """\
# Flywheel work policy: where work comes from, where runtime state lives, and
# how finished work lands. Committed with the repo; CLI flags always override.
#
# This file is self-documenting. The ACTIVE keys below are what `flywheel init`
# chose for this repo; every other knob is shown COMMENTED with its default so
# you can see the full feature surface and turn things on by uncommenting.
# Only [source] and [store] are required. Full reference: docs/configuration.md.
"""

_INIT_POLICY_TAIL_HEAD = """\
[paths]
db = ".flywheel/flywheel.sqlite"
# Worktree/sandbox root. A relative path anchors at the repo root; "@cache"
# (XDG cache dir keyed by repo identity) and "@sibling"
# (<repo-parent>/<repo>.worktrees) opt into out-of-tree layouts.
sandbox_root = ".flywheel/worktrees"

# --- Default graders -------------------------------------------------------
# Verification commands for work items that declare none. Tracker sources
# (github) lean on these; directory task files always carry their own. One
# array-of-tables entry per grader, run in order; pass is exit code 0.
# [[defaults.graders]]
# type = "command"
# run = "uv run pytest"

# --- Agent -----------------------------------------------------------------
# The model id the worker passes to the agent SDK verbatim (no allowlist).
# CLI --model overrides. Unset uses the SDK default.
# [agent]
# model = "claude-opus-4-8"

# --- Sandbox ---------------------------------------------------------------
# Where the agent runs and how the sandbox is provisioned. `setup` runs (shell)
# inside every newly created sandbox before the agent enters (dependency
# install, codegen) so tasks never pay discovery cost for a bare worktree.
# `backend` selects the execution surface: "worktree" (default) or "container"
# (Docker; needs the flywheel-container extra + a [sandbox.container] block).
# `preset` is a code-owned baseline that subtable keys override.
# [sandbox]
# setup = "uv sync"
# backend = "worktree"               # worktree | container
# preset = "fast"                    # fast | balanced | hardened
# permission_mode = "default"        # default | acceptEdits | bypassPermissions
#
# Optional [sandbox.*] subtables (full reference: docs/sandbox.md):
#   [sandbox.env]          explicit env handed to the agent AND graders
#   [sandbox.capabilities] the agent's tool/skill/MCP surface inside the sandbox
#   [sandbox.network]      network policy (e.g. policy = "offline")
#   [sandbox.exec]         exec-time knobs
#   [sandbox.limits]       cpu / memory / pids / wall-time ceilings
#   [sandbox.retention]    on_done / on_failure = "destroy" | "park"
#   [sandbox.container]    image / model / auth for the container backend
"""

_INIT_POLICY_TAIL_FOOT = """\
# --- Phase-exit gate -------------------------------------------------------
# `verify` runs (shell) against the merged phase base once every task in a
# phase has landed; a non-zero exit leaves the phase active instead of
# archiving it. Unset = today's archival (no gate). Distinct from
# [submit] verify, which gates each individual land.
# [phase]
# verify = "uv run pytest"

# --- Held-out landing gate -------------------------------------------------
# `root` is a directory of operator-authored held-out grader registrations the
# execute-time gate runs against the committed sandbox before landing — the
# agent never sees them, so it cannot game them. Unset = no gate.
# See docs/held-out-gate.md.
# [held_out]
# root = ".flywheel/verification"

# --- Autopilot intake ------------------------------------------------------
# The intake daemon (`flywheel autopilot`) keeps the queue filled. `target_depth`
# is the queue target, `interval_seconds` the refill cadence, `landing` how
# authored work lands. The optional [autopilot.weights] sub-table tunes the
# scoring axes (unset weights keep the engine defaults). See docs/autopilot.md.
# [autopilot]
# target_depth = 5
# interval_seconds = 300
# landing = "merge"
# [autopilot.weights]
# urgency = 1.0
# importance = 1.0

# --- Worker ----------------------------------------------------------------
# `concurrency` is how many tasks one worker process drives in parallel
# (default 1). >1 relies on each task's conflict_keys to keep concurrent work
# off shared files; --concurrency overrides per run.
# [worker]
# concurrency = 1

# --- Execution -------------------------------------------------------------
# `mode` is "local" (default) or "distributed" (the latter requires
# store.backend = "postgres"). `capabilities` is THIS worker's advertised set;
# the scheduler offers it only tasks whose required_capabilities is a subset.
# (Distinct from [sandbox.capabilities], the agent's in-sandbox tool surface.)
# [execution]
# mode = "local"
# capabilities = ["gpu", "cuda"]
"""


def _render_submit_block(submit_base: str | None) -> str:
    """Render the ``[submit]`` section.

    The detected current branch is recorded as a COMMENTED ``base`` suggestion,
    never an active key. An active ``base`` equal to the checked-out branch is
    refused by the landing guard (:func:`resolve_landing_base`) — so pinning the
    just-detected branch (the universal single-branch case right after init)
    would make the worker exit on startup. Left unset, the worker falls back to
    the checked-out branch and FF-merges in-tree, the working default. The
    operator uncomments and edits ``base`` only to land onto a SEPARATE
    integration branch they do not have checked out. ``json.dumps`` quoting is
    valid for a TOML basic string, so an unusual branch name cannot break the
    file.
    """
    suggestion = json.dumps(submit_base) if submit_base else '"main"'
    detected_note = (
        f" (detected current branch: {submit_base})" if submit_base else ""
    )
    return (
        "# --- Landing --------------------------------------------------------"
        "------\n"
        "# How finished work reaches your branch. `strategy` is \"merge\" (FF-merge"
        "\n# in-tree, default) or \"pr\" (push + open a pull request; uses `remote`"
        "\n# and optional `pr_base`).\n"
        "#\n"
        "# `base` pins the branch finished work lands on and the worker resolves\n"
        "# its phase base from. Leave it UNSET to FF-merge onto your checked-out\n"
        f"# branch{detected_note} — the working default. Set base ONLY to a\n"
        "# separate integration branch you do NOT have checked out; setting it to\n"
        "# the checked-out branch is refused and the worker exits on startup.\n"
        "#\n"
        "# `verify` is the standing build gate (spec 00064): a repo-wide command\n"
        "# re-run under the merge lock against the exact tree about to land, on\n"
        "# every land path, independent of each task's own graders. A non-zero\n"
        "# exit refuses the land and parks the work — this is what stops a\n"
        "# semantic merge skew (two independently-valid changes whose union does\n"
        "# not build) from reaching your branch. Unset = no gate. It serializes\n"
        "# landings, so a slow command bottlenecks throughput.\n"
        "#\n"
        "# `protected_paths` are globs a finished branch may not touch; a match\n"
        "# refuses the land. Use it to stop authored work from rewriting the\n"
        "# verification surface (policy, CI, .flywheel state).\n"
        "[submit]\n"
        f"# base = {suggestion}\n"
        '# verify = "uv run pytest"\n'
        '# protected_paths = ["flywheel.toml", ".flywheel/**"]\n'
        '# strategy = "merge"          # merge | pr\n'
        '# remote = "origin"           # pr strategy push target\n'
        '# pr_base = "main"            # pr strategy base branch\n'
    )

_INIT_STORE_BACKENDS: tuple[str, ...] = ("sqlite", "postgres")

_INIT_SOURCE_KINDS: tuple[str, ...] = ("directory", "github")

_INIT_DONE_ACTIONS: tuple[str, ...] = ("comment", "close")

# owner: GitHub usernames/orgs are alphanumeric with interior hyphens;
# name: repo names add dots and underscores. Tight enough that a value
# can be embedded in a TOML basic string without escaping.
_GITHUB_REPO_RE = re.compile(
    r"^[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?/[A-Za-z0-9._-]+$"
)

_GITHUB_ORIGIN_RE = re.compile(
    r"github\.com[:/](?P<owner>[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?)/"
    r"(?P<name>[A-Za-z0-9._-]+?)(?:\.git)?/?$"
)

# Mirrors PostgresStore's identifier-safe schema validation so a bad name
# fails at the prompt instead of on the worker's first store open.
_PG_SCHEMA_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")


class _InitUsageError(Exception):
    """Inconsistent or invalid ``init`` flag values (exit 2, no policy
    file written)."""


@dataclass(frozen=True, kw_only=True)
class _InitAnswers:
    """The resolved init choices, however they were answered (prompt,
    flag, or default)."""

    store_backend: str
    store_schema: str | None = None
    source_kind: str
    github_repo: str | None = None
    github_label: str | None = None
    github_done_action: str = "comment"
    # The repo's current branch, detected at init and surfaced as a COMMENTED
    # [submit] base suggestion (never an active key — an active base equal to
    # the checked-out branch is refused by the landing guard). None when no
    # branch was detected; either way base stays unset so the worker FF-merges
    # in-tree onto the checked-out branch.
    submit_base: str | None = None
    # Whether to install the Claude Code skills (fw-spec / fw-plan /
    # fw-retro / fw-improve) into .claude/skills/. Interactive default
    # is yes; non-interactive runs install only with an explicit
    # ``--skills`` flag so ``--defaults`` never writes into .claude/.
    install_skills: bool = False


def _render_source_block(
    answers: _InitAnswers, *, tasks_dir: str = ".flywheel/tasks"
) -> str:
    """Render the ``[source]`` section the prompts own.

    For the directory kind, ``tasks_dir`` is the one key the prompts do
    not answer; the reconfigure path threads the existing value through
    so a hand-tuned location survives. ``json.dumps`` escaping is valid
    for TOML basic strings, so an arbitrary preserved path cannot break
    the rendered file.
    """
    if answers.source_kind == "directory":
        return (
            f'[source]\n'
            f'kind = "directory"\n'
            f'tasks_dir = {json.dumps(tasks_dir)}\n'
        )
    return (
        f'[source]\n'
        f'kind = "github"\n'
        f'repo = "{answers.github_repo}"\n'
        f'label = "{answers.github_label}"\n'
        f'done_action = "{answers.github_done_action}"\n'
    )


def _render_store_block(answers: _InitAnswers) -> str:
    """Render the ``[store]`` section the prompts own."""
    store_block = f'[store]\nbackend = "{answers.store_backend}"\n'
    if answers.store_schema is not None:
        store_block += f'schema = "{answers.store_schema}"\n'
    return store_block


def _render_init_policy(answers: _InitAnswers) -> str:
    """Render ``flywheel.toml`` from the answers.

    No credential is ever an input here: the postgres DSN lives only in
    the environment (spec FR-4), so it cannot appear in the rendered file.
    """
    return "".join(
        (
            _INIT_POLICY_HEADER,
            "\n",
            _render_source_block(answers),
            "\n",
            _render_store_block(answers),
            "\n",
            _INIT_POLICY_TAIL_HEAD,
            "\n",
            _render_submit_block(answers.submit_base),
            "\n",
            _INIT_POLICY_TAIL_FOOT,
        )
    )


def _prompt_line(prompt: str) -> str:
    """One plain stdin/stdout prompt round-trip.

    Raises :class:`EOFError` when stdin is exhausted so a closed pipe
    mid-flow aborts init instead of silently accepting defaults for the
    remaining questions.
    """
    sys.stdout.write(prompt)
    sys.stdout.flush()
    line = sys.stdin.readline()
    if line == "":
        raise EOFError("stdin closed during init prompts")
    return line.strip()


def _prompt_choice(
    label: str, choices: tuple[str, ...], default: str
) -> str:
    rendered = "/".join(
        f"[{choice}]" if choice == default else choice for choice in choices
    )
    while True:
        raw = _prompt_line(f"{label} ({rendered}): ")
        if not raw:
            return default
        if raw in choices:
            return raw
        print(
            f"  invalid value {raw!r}; choose one of: {', '.join(choices)}"
        )


def _prompt_yes_no(label: str, *, default: bool) -> bool:
    """One yes/no prompt round-trip; enter accepts ``default``."""
    rendered = "[y]/n" if default else "y/[n]"
    while True:
        raw = _prompt_line(f"{label} ({rendered}): ").lower()
        if not raw:
            return default
        if raw in ("y", "yes"):
            return True
        if raw in ("n", "no"):
            return False
        print(f"  invalid value {raw!r}; answer y or n")


def _validate_pg_schema(value: str) -> str | None:
    if not _PG_SCHEMA_RE.match(value):
        return (
            f"invalid schema name {value!r}: must match "
            f"[A-Za-z_][A-Za-z0-9_]*"
        )
    return None


def _validate_github_repo(value: str) -> str | None:
    if not _GITHUB_REPO_RE.match(value):
        return f"invalid repo {value!r}: expected owner/name"
    return None


def _validate_github_label(value: str) -> str | None:
    if not value or any(ch in value for ch in ('"', "\\", "\n")):
        return (
            f"invalid label {value!r}: must be non-empty and contain no "
            f"quotes or backslashes"
        )
    return None


def _prompt_pg_schema() -> str | None:
    while True:
        raw = _prompt_line("postgres schema [none]: ")
        if not raw:
            return None
        error = _validate_pg_schema(raw)
        if error is None:
            return raw
        print(f"  {error}")


def _prompt_github_repo(default: str | None) -> str:
    suffix = f" [{default}]" if default else ""
    while True:
        raw = _prompt_line(f"github repo (owner/name){suffix}: ")
        value = raw or (default or "")
        if not value:
            print("  a repo is required (owner/name)")
            continue
        error = _validate_github_repo(value)
        if error is None:
            return value
        print(f"  {error}")


def _prompt_github_label() -> str:
    while True:
        raw = _prompt_line("issue label [flywheel]: ")
        value = raw or "flywheel"
        error = _validate_github_label(value)
        if error is None:
            return value
        print(f"  {error}")


def _github_repo_from_origin() -> str | None:
    """Parse ``owner/name`` from the ``origin`` remote, or ``None``.

    Absent remote, non-GitHub-shaped URL, or any subprocess failure all
    degrade to "no default" -- the repo prompt then has no pre-fill and
    the non-interactive github path requires an explicit ``--repo``.
    """
    try:
        proc = subprocess.run(
            ["git", "remote", "get-url", "origin"],
            capture_output=True,
            text=True,
            timeout=5,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    if proc.returncode != 0:
        return None
    match = _GITHUB_ORIGIN_RE.search(proc.stdout.strip())
    if match is None:
        return None
    return f"{match.group('owner')}/{match.group('name')}"


def _report_agent_auth() -> None:
    """Print whether agent credentials are in place (all backends).

    The worker drives the Claude agent SDK, which authenticates from
    ``ANTHROPIC_API_KEY`` or a ``claude login`` session. This is runtime
    config the operator may legitimately set later (e.g. init in CI), so a
    missing credential warns rather than blocks -- but a developer-facing
    init should surface it as a check, not bury it in next-steps prose.
    """
    import os

    if os.environ.get("ANTHROPIC_API_KEY"):
        print("agent auth: ANTHROPIC_API_KEY is set")
        return
    creds = Path.home() / ".claude" / ".credentials.json"
    if creds.is_file():
        print("agent auth: detected a `claude login` credential file")
        return
    print(
        "warning: no agent credentials detected; set ANTHROPIC_API_KEY (or "
        "run `claude login`) before the first worker run"
    )


def _postgres_preflight(args: argparse.Namespace, schema: str | None) -> None:
    """Guided, failsafe Postgres bring-up at init time.

    With no DSN resolvable from the environment, this stays advisory and
    non-blocking (the DSN is runtime config the operator may set later):
    it points at the env vars and returns, init writes the policy and
    exits 0. Once a DSN *is* present, the ordered checks in
    :mod:`._pg_preflight` run -- pooler mode, privileges, schema version,
    and optional provisioning. The full report prints regardless; if any
    check blocks and ``--allow-unverified`` was not passed, this raises
    :class:`_InitUsageError`, which both init callers turn into a
    non-zero exit that writes no ``flywheel.toml`` -- so a misconfigured
    target never yields a config that looks ready.
    """
    dsn = resolve_postgres_dsn()
    if dsn is None:
        try:
            import flywheel_core.store_postgres  # noqa: F401
        except ImportError:
            print(
                "warning: the postgres extra is not installed; "
                "install with: uv add 'flywheel[postgres]'"
            )
        print(
            f"postgres: no DSN found; set {PG_DSN_ENV} (or "
            f"{PG_DSN_FALLBACK_ENV}) before running flywheel worker"
        )
        return

    provision = bool(getattr(args, "provision", False))
    allow_unverified = bool(getattr(args, "allow_unverified", False))
    outcome = run_postgres_preflight(
        dsn,
        schema or "public",
        provision=provision,
        allow_unverified=allow_unverified,
    )
    print(format_report(outcome.checks))
    if outcome.blocked and not allow_unverified:
        raise _InitUsageError(
            "postgres preflight found blocking issues (see the report "
            "above); fix them, or re-run with --allow-unverified to "
            "scaffold the policy anyway"
        )


def _resolve_install_skills(
    args: argparse.Namespace, *, interactive: bool
) -> bool:
    """Resolve the install-skills choice from flag, prompt, or default.

    ``--skills`` / ``--no-skills`` pre-answer the prompt. Interactive
    runs default to yes; non-interactive runs default to no so a bare
    ``--defaults`` (or non-TTY stdin) never writes into ``.claude/``
    without an explicit flag.
    """
    if args.skills is not None:
        return bool(args.skills)
    if interactive:
        return _prompt_yes_no(
            f"install Claude Code skills ({', '.join(SKILL_NAMES)})?",
            default=True,
        )
    return False


def _install_skills_from_policy_file(policy_path: Path) -> None:
    """Render and install the managed skills for the policy on disk.

    The policy is re-loaded from ``policy_path`` (rather than rebuilt
    from answers) so the render binds to exactly what the file says —
    including keys init does not own, like a hand-tuned ``tasks_dir``.
    A missing file renders the built-in defaults.
    """
    policy = load_policy(policy_path) if policy_path.is_file() else None
    report = install_skills(
        DEFAULT_SKILLS_ROOT, settings_from_policy(policy)
    )
    for path in report.created:
        print(f"created: {path}")
    for path in report.updated:
        print(f"updated: {path}")
    for path in report.skipped:
        print(f"exists:  {path} (user-owned, left untouched)")


def _collect_init_answers(
    args: argparse.Namespace, *, interactive: bool
) -> _InitAnswers:
    """Resolve every init choice from flags, prompts, or defaults.

    Each flag suppresses exactly its own prompt (spec FR-2); with
    ``interactive`` false every unanswered choice takes its default.
    Flag values fail with the same messages the prompts re-prompt with.
    """
    submit_base = _current_branch()
    if args.store is not None:
        backend = args.store
    elif interactive:
        backend = _prompt_choice(
            "store backend", _INIT_STORE_BACKENDS, "sqlite"
        )
    else:
        backend = "sqlite"

    schema: str | None = None
    if backend == "postgres":
        if args.pg_schema is not None:
            flag_schema: str = args.pg_schema.strip()
            error = _validate_pg_schema(flag_schema)
            if error is not None:
                raise _InitUsageError(error)
            schema = flag_schema
        elif interactive:
            schema = _prompt_pg_schema()
        _postgres_preflight(args, schema)
    elif args.pg_schema is not None:
        raise _InitUsageError(
            "--pg-schema applies only to the postgres store backend; "
            "the selected backend is 'sqlite'"
        )

    if args.source is not None:
        kind = args.source
    elif interactive:
        kind = _prompt_choice(
            "work source", _INIT_SOURCE_KINDS, "directory"
        )
    else:
        kind = "directory"

    if kind == "directory":
        if args.repo is not None or args.label is not None:
            raise _InitUsageError(
                "--repo/--label apply only to the github work source; "
                "the selected source is 'directory'"
            )
        return _InitAnswers(
            store_backend=backend,
            store_schema=schema,
            source_kind="directory",
            submit_base=submit_base,
            install_skills=_resolve_install_skills(
                args, interactive=interactive
            ),
        )

    origin_default = _github_repo_from_origin()
    if args.repo is not None:
        repo = args.repo.strip()
        error = _validate_github_repo(repo)
        if error is not None:
            raise _InitUsageError(error)
    elif interactive:
        repo = _prompt_github_repo(origin_default)
    elif origin_default is not None:
        repo = origin_default
    else:
        raise _InitUsageError(
            "the github work source needs --repo OWNER/NAME (the origin "
            "remote is absent or not GitHub-shaped)"
        )

    if args.label is not None:
        label = args.label.strip()
        error = _validate_github_label(label)
        if error is not None:
            raise _InitUsageError(error)
    elif interactive:
        label = _prompt_github_label()
    else:
        label = "flywheel"

    if interactive:
        done_action = _prompt_choice(
            "done action", _INIT_DONE_ACTIONS, "comment"
        )
    else:
        done_action = "comment"

    return _InitAnswers(
        store_backend=backend,
        store_schema=schema,
        source_kind="github",
        github_repo=repo,
        github_label=label,
        github_done_action=done_action,
        submit_base=submit_base,
        install_skills=_resolve_install_skills(
            args, interactive=interactive
        ),
    )


def _print_init_next_steps(
    store_backend: str | None, *, skills_installed: bool = False
) -> None:
    print("Next steps:")
    print(
        f"  1. Drop one JSON file per task into "
        f"{INIT_ROOT}/tasks/active/<phase>/ "
        f"('goal' and 'graders' are the only required fields)."
    )
    if skills_installed:
        print(
            "     Or, in Claude Code: /fw-spec to spec a feature, "
            "/fw-plan to queue tasks."
        )
    print(
        "  2. Authenticate the agent: set ANTHROPIC_API_KEY (or run "
        "`claude login`) before the first worker run."
    )
    print("  3. Run: flywheel worker")
    print("  4. Watch: flywheel status / live")
    if store_backend == "postgres":
        print()
        print(
            f"The postgres DSN is read from {PG_DSN_ENV} (fallback: "
            f"{PG_DSN_FALLBACK_ENV}) at runtime; it is never stored in "
            f"flywheel.toml."
        )
        print(
            "  Use a session connection (not a transaction-mode pooler): "
            "for Supabase the Session pooler (port 5432) or the direct "
            "connection, never port 6543."
        )
        print(
            "  Re-run 'flywheel init --provision' with the DSN set to "
            "create the schema and tables now instead of on the first "
            "worker run."
        )
    print()
    print(
        "To help future Claude Code sessions discover flywheel, paste this "
        "line into your CLAUDE.md (init never edits it for you):"
    )
    print(f"  {_INIT_CLAUDEMD_POINTER}")


# Matches a top-level TOML table (``[name]``) or array-of-tables
# (``[[name]]``) header line, tolerating a trailing comment. The captured
# name is the full dotted path, so ``[source.extra]`` and
# ``[[defaults.graders]]`` do NOT collide with the bare ``source`` /
# ``store`` sections the reconfigure rewrite owns.
_TOML_HEADER_RE = re.compile(
    r"^\s*\[\[?\s*(?P<name>[^\]]*?)\s*\]\]?\s*(?:#.*)?$"
)


def _split_toml_sections(text: str) -> list[tuple[str | None, str]]:
    """Split raw TOML text into ``(header name, chunk)`` pairs.

    The first chunk (name ``None``) is the root-table preamble (header
    comment, any bare keys). Every other chunk starts at its section
    header line and runs up to the next header, comments and blank lines
    included. Concatenating the chunk texts reproduces ``text``
    byte-for-byte -- the rewrite below leans on that to leave sections
    it does not own untouched.
    """
    chunks: list[tuple[str | None, list[str]]] = [(None, [])]
    for line in text.splitlines(keepends=True):
        match = _TOML_HEADER_RE.match(line)
        if match is not None:
            chunks.append((match.group("name"), [line]))
        else:
            chunks[-1][1].append(line)
    return [(name, "".join(lines)) for name, lines in chunks]


def _trailing_blank_lines(chunk: str) -> str:
    """Return the run of whitespace-only lines closing ``chunk``.

    Preserved across a section replacement so the rewritten file keeps
    the original blank-line spacing between sections.
    """
    lines = chunk.splitlines(keepends=True)
    tail: list[str] = []
    for line in reversed(lines):
        if line.strip():
            break
        tail.append(line)
    return "".join(reversed(tail))


def _rewrite_policy_text(
    original: str, data: Mapping[str, Any], answers: _InitAnswers
) -> str:
    """Rewrite only the ``[source]`` and ``[store]`` sections of a policy.

    Every other section (``[paths]``, ``[agent]``,
    ``[[defaults.graders]]``, anything unknown) passes through verbatim,
    comments and formatting included (spec FR-9). A section the prompts
    own but the file lacks -- e.g. ``[store]`` in a pre-``[store]``-era
    config -- is appended at the end. The directory source's
    ``tasks_dir`` is the one unanswered key inside an owned section, so
    the existing value is carried over instead of reset.
    """
    tasks_dir = ".flywheel/tasks"
    source_table = data.get("source")
    if isinstance(source_table, Mapping) and isinstance(
        source_table.get("tasks_dir"), str
    ):
        tasks_dir = source_table["tasks_dir"]
    pending: dict[str, str] = {
        "source": _render_source_block(answers, tasks_dir=tasks_dir),
        "store": _render_store_block(answers),
    }
    out: list[str] = []
    for name, chunk in _split_toml_sections(original):
        block = pending.pop(name, None) if name is not None else None
        if block is None:
            out.append(chunk)
        else:
            out.append(block + _trailing_blank_lines(chunk))
    text = "".join(out)
    for name in ("source", "store"):
        block = pending.get(name)
        if block is None:
            continue
        if text and not text.endswith("\n"):
            text += "\n"
        text += "\n" + block
    return text


def _describe_current_policy(data: Mapping[str, Any]) -> str:
    """One-line summary of the settings the reconfigure prompts own."""
    store = data.get("store")
    backend = "sqlite"
    if isinstance(store, Mapping) and isinstance(store.get("backend"), str):
        backend = store["backend"]
    source = data.get("source")
    kind = "(unset)"
    if isinstance(source, Mapping) and isinstance(source.get("kind"), str):
        kind = source["kind"]
    summary = f"store backend = {backend}, work source = {kind}"
    if isinstance(source, Mapping) and isinstance(source.get("repo"), str):
        summary += f" ({source['repo']})"
    return summary


def _reconfigure_policy(
    args: argparse.Namespace, policy_path: Path
) -> int:
    """Offer to reconfigure an existing ``flywheel.toml`` (spec FR-9).

    Reachable only on the interactive path -- non-TTY / ``--defaults``
    runs keep the historical never-touch guarantee. Declining (the
    default answer) leaves the file byte-identical. Accepting rewrites
    only the keys the prompts answered; see :func:`_rewrite_policy_text`.
    A malformed file is reported and left alone rather than overwritten.
    """
    original = policy_path.read_text(encoding="utf-8")
    try:
        data = tomllib.loads(original)
    except tomllib.TOMLDecodeError as exc:
        print(
            f"error: {policy_path} is not valid TOML ({exc}); "
            f"left untouched",
            file=sys.stderr,
        )
        return 2
    print(f"{policy_path}: {_describe_current_policy(data)}")
    try:
        if not _prompt_yes_no("reconfigure?", default=False):
            print(f"exists:  {policy_path} (left untouched)")
            print()
            _print_init_next_steps(None)
            return 0
        answers = _collect_init_answers(args, interactive=True)
    except _InitUsageError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    except KeyboardInterrupt:
        print(file=sys.stderr)
        print(
            "init: interrupted; flywheel.toml was left untouched",
            file=sys.stderr,
        )
        return 130
    except EOFError:
        print(
            "init: stdin closed before the prompts finished; "
            "flywheel.toml was left untouched",
            file=sys.stderr,
        )
        return 1
    policy_path.write_text(
        _rewrite_policy_text(original, data, answers), encoding="utf-8"
    )
    print(f"updated: {policy_path}")
    if answers.install_skills:
        # Regenerate against the just-rewritten policy so a changed
        # tasks_dir / work source propagates into the managed skills.
        _install_skills_from_policy_file(policy_path)
    _report_agent_auth()
    print()
    _print_init_next_steps(
        answers.store_backend, skills_installed=answers.install_skills
    )
    return 0


def _init_git_preflight_error() -> str | None:
    """Return an actionable refusal message, or ``None`` when init may run.

    init scaffolds state a worker later acts on, so it must not produce a
    state the worker's own preconditions reject. The gate mirrors the
    worktree worker exactly: ``_repo_root`` refuses a non-git working
    directory (``git rev-parse --show-toplevel``) and ``resolve_landing_base``
    refuses a detached HEAD with no configured base (``git rev-parse
    --abbrev-ref HEAD`` yielding ``""``/``"HEAD"``). A refusal is a hard gate -- the caller writes no
    ``flywheel.toml`` -- never a warn-and-continue (spec D-1). The
    detached refusal is unconditional here: ``init --defaults`` writes no
    ``[submit] base`` key, so the state init produces is exactly the one
    the worker rejects (spec SI-8).
    """
    toplevel = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"],
        text=True,
        capture_output=True,
        check=False,
    )
    if toplevel.returncode != 0:
        return (
            "init: the working directory is not a git repository. flywheel "
            "drives work through git worktrees, so run `git init` (and make "
            "at least one commit on a branch) before initializing flywheel."
        )
    branch = subprocess.run(
        ["git", "rev-parse", "--abbrev-ref", "HEAD"],
        text=True,
        capture_output=True,
        check=False,
    )
    name = branch.stdout.strip()
    if branch.returncode != 0 or not name or name == "HEAD":
        return (
            "init: HEAD is detached. flywheel resolves a worker's phase base "
            "from the checked-out branch, so check out a branch "
            "(`git switch -c <branch>`) before initializing flywheel."
        )
    return None


def _current_branch() -> str | None:
    """Return the repo's current branch name, or ``None`` if undetectable.

    init's git preflight already guarantees an attached branch by the time
    answers are collected, so this normally returns that branch -- recorded
    as ``[submit] base`` so the landing target is explicit in the policy.
    """
    branch = subprocess.run(
        ["git", "rev-parse", "--abbrev-ref", "HEAD"],
        text=True,
        capture_output=True,
        check=False,
    )
    name = branch.stdout.strip()
    if branch.returncode != 0 or not name or name == "HEAD":
        return None
    return name


def _cmd_init_agent(policy_path: Path) -> int:
    """Drive the agent-driven init path and report the proposal (FR ``--agent``).

    Imported lazily so the agent seam (and its lazy SDK boundary) is only
    pulled in when ``--agent`` is actually used -- the default init path never
    touches it. The model resolution mirrors the autopilot daemon's default
    (the production SDK-backed invoker rooted at the cwd). The proposal is
    printed BEFORE the policy is written so the operator sees what was chosen;
    an unparseable response or an invalid render raises and writes nothing.
    """
    from flywheel_orchestrator._init_agent import (
        InitAgentError,
        run_agent_init,
    )

    repo_root = Path.cwd()
    submit_base = _current_branch()
    try:
        result = run_agent_init(
            repo_root=repo_root,
            submit_base=submit_base,
            policy_path=policy_path,
        )
    except InitAgentError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1

    proposal = result.proposal
    print("Agent proposal:")
    if proposal.notes:
        print(f"  notes: {proposal.notes}")
    if proposal.default_graders:
        print("  default graders:")
        for run in proposal.default_graders:
            print(f"    - {run}")
    else:
        print("  default graders: (none)")
    print(f"  sandbox setup: {proposal.sandbox_setup or '(none)'}")
    if proposal.target_depth is not None:
        print(f"  autopilot target_depth: {proposal.target_depth}")
    if proposal.interval_seconds is not None:
        print(f"  autopilot interval_seconds: {proposal.interval_seconds}")
    print(f"created: {result.policy_path}")
    for line in result.gitignore_added:
        print(f"gitignore: +{line}")
    dockerignore_added = _ensure_dockerignore_covers_flywheel(repo_root)
    if dockerignore_added is not None:
        print(f"dockerignore: +{dockerignore_added}")
    _report_agent_auth()
    print()
    _print_init_next_steps(result.policy.store_backend)
    return 0


def _cmd_init(args: argparse.Namespace) -> int:
    """Scaffold ``.flywheel/`` and a ``flywheel.toml`` in the working dir.

    Idempotent: existing files are left untouched and reported, so
    re-running ``init`` never clobbers a tuned policy or an in-use task
    queue. On a TTY (without ``--defaults``) the store backend and work
    source are prompted for; flags pre-answer individual prompts; a
    non-TTY stdin behaves exactly like ``--defaults``. Answers are
    collected fully before the policy file is written, so aborting
    mid-prompts (Ctrl-C / EOF) never leaves a partial ``flywheel.toml``.

    With an existing ``flywheel.toml``, an interactive run offers to
    reconfigure it (see :func:`_reconfigure_policy`); a non-interactive
    run never touches it.

    Before any file is written, a git preflight gate refuses (non-zero
    exit, nothing scaffolded) when the working directory is not a git
    repository or HEAD is detached, so init never leaves a state the
    worker's own preconditions later reject (spec D-1 / SI-8).
    """
    preflight_error = _init_git_preflight_error()
    if preflight_error is not None:
        print(preflight_error, file=sys.stderr)
        return 2

    created: list[str] = []
    existing: list[str] = []

    def ensure_file(path: Path, content: str) -> None:
        if path.exists():
            existing.append(str(path))
            return
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")
        created.append(str(path))

    ensure_file(INIT_ROOT / "tasks" / "active" / ".gitkeep", "")
    ensure_file(INIT_ROOT / "tasks" / "archive" / ".gitkeep", "")
    ensure_file(INIT_ROOT / ".gitignore", _INIT_GITIGNORE)

    for path in created:
        print(f"created: {path}")
    for path in existing:
        print(f"exists:  {path} (left untouched)")
    dockerignore_added = _ensure_dockerignore_covers_flywheel(Path.cwd())
    if dockerignore_added is not None:
        print(f"updated: .dockerignore (+{dockerignore_added})")

    policy_path = Path(DEFAULT_POLICY_FILENAME)
    interactive = sys.stdin.isatty() and not args.defaults
    if policy_path.exists():
        if not interactive:
            # Non-interactive re-runs preserve the historical never-touch
            # guarantee (spec FR-3 / FR-9) for the policy file. An
            # explicit --skills still (re)generates the managed skills
            # against the policy as it stands, so a settings change can
            # be propagated scriptably: edit flywheel.toml, then run
            # ``flywheel init --skills --defaults``.
            print(f"exists:  {policy_path} (left untouched)")
            if args.skills:
                _install_skills_from_policy_file(policy_path)
            # --provision against an already-configured repo re-runs the
            # postgres preflight (and bootstrap) over the committed policy:
            # set the DSN, then `flywheel init --provision` to verify and
            # create the schema without rewriting flywheel.toml.
            if getattr(args, "provision", False):
                existing_policy = load_policy(policy_path)
                if existing_policy.store_backend == "postgres":
                    try:
                        _postgres_preflight(args, existing_policy.store_schema)
                    except _InitUsageError as exc:
                        print(f"error: {exc}", file=sys.stderr)
                        return 2
                else:
                    print(
                        "note: --provision applies only to the postgres "
                        "store backend; nothing to provision"
                    )
            print()
            _print_init_next_steps(
                None, skills_installed=bool(args.skills)
            )
            return 0
        return _reconfigure_policy(args, policy_path)

    if getattr(args, "agent", False):
        return _cmd_init_agent(policy_path)

    try:
        answers = _collect_init_answers(args, interactive=interactive)
    except _InitUsageError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    except KeyboardInterrupt:
        print(file=sys.stderr)
        print(
            "init: interrupted; flywheel.toml was not written",
            file=sys.stderr,
        )
        return 130
    except EOFError:
        print(
            "init: stdin closed before the prompts finished; "
            "flywheel.toml was not written",
            file=sys.stderr,
        )
        return 1

    policy_path.write_text(_render_init_policy(answers), encoding="utf-8")
    print(f"created: {policy_path}")
    if answers.install_skills:
        _install_skills_from_policy_file(policy_path)
    _report_agent_auth()
    print()
    _print_init_next_steps(
        answers.store_backend, skills_installed=answers.install_skills
    )
    return 0

def _add_common_tasks_dir(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--tasks-dir",
        default=None,
        help=f"Tasks root directory (default: {DEFAULT_TASKS_DIR}).",
    )


def _add_common_policy(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--policy",
        default=None,
        help=(
            f"Work-policy file selecting the work source "
            f"(default: {DEFAULT_POLICY_FILENAME} if present). "
            f"An explicit --tasks-dir overrides the policy."
        ),
    )


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="flywheel",
        description=(
            "Schedule and drive many flywheel tasks laid out under "
            ".flywheel/tasks/active/<phase>/."
        ),
    )
    sub = parser.add_subparsers(dest="command", required=True)

    p_init = sub.add_parser(
        "init",
        help=(
            "Scaffold .flywheel/ (tasks dir, gitignored runtime state) and "
            "a flywheel.toml work policy in the current directory. "
            "Prompts for store backend and work source on a TTY; flags "
            "pre-answer prompts; a non-TTY stdin takes every default. "
            "With an existing flywheel.toml, an interactive run offers "
            "to reconfigure it (rewriting only the answered keys); a "
            "non-interactive run never touches it."
        ),
    )
    p_init.add_argument(
        "--store",
        choices=_INIT_STORE_BACKENDS,
        default=None,
        help=(
            "Store backend recorded as [store] backend "
            "(pre-answers the prompt; default: sqlite)."
        ),
    )
    p_init.add_argument(
        "--pg-schema",
        default=None,
        metavar="NAME",
        help=(
            "Postgres schema name recorded as [store] schema "
            "(postgres backend only)."
        ),
    )
    p_init.add_argument(
        "--provision",
        action="store_true",
        help=(
            "After the postgres preflight passes, create the schema and "
            "run the store's bootstrap DDL now (postgres backend with a "
            "resolvable DSN only), so tables exist before the first "
            "worker run instead of being created lazily by it."
        ),
    )
    p_init.add_argument(
        "--allow-unverified",
        action="store_true",
        help=(
            "Scaffold the policy even when the postgres preflight reports "
            "a blocking issue (unreachable DSN, transaction-mode pooler, "
            "missing privileges, incompatible schema version). The report "
            "still prints; this only downgrades the blocks to warnings."
        ),
    )
    p_init.add_argument(
        "--source",
        choices=_INIT_SOURCE_KINDS,
        default=None,
        help=(
            "Work source kind recorded as [source] kind "
            "(pre-answers the prompt; default: directory)."
        ),
    )
    p_init.add_argument(
        "--repo",
        default=None,
        metavar="OWNER/NAME",
        help=(
            "GitHub repo for the github work source "
            "(default: parsed from the origin remote)."
        ),
    )
    p_init.add_argument(
        "--label",
        default=None,
        help=(
            "Issue label for the github work source (default: flywheel)."
        ),
    )
    p_init.add_argument(
        "--skills",
        action=argparse.BooleanOptionalAction,
        default=None,
        help=(
            "Install (or regenerate) the Claude Code skills "
            f"({', '.join(SKILL_NAMES)}) under .claude/skills/ "
            "(pre-answers the prompt; interactive default: yes, "
            "non-interactive default: no). With an existing "
            "flywheel.toml, a non-interactive 'flywheel init --skills' "
            "regenerates the skills against the current policy without "
            "touching the policy file."
        ),
    )
    p_init.add_argument(
        "--defaults",
        action="store_true",
        help=(
            "Accept every default without prompting (a non-TTY stdin "
            "implies this)."
        ),
    )
    p_init.add_argument(
        "--agent",
        action="store_true",
        help=(
            "Drive an agent to inspect the repo and PROPOSE the flywheel.toml "
            "policy (detected toolchain -> default graders, sandbox setup, "
            "autopilot cadence, .gitignore additions) instead of prompting. "
            "The proposal is printed before the policy is written and is "
            "re-validated; an unparseable response writes nothing."
        ),
    )
    p_init.set_defaults(func=_cmd_init)

    p_next = sub.add_parser(
        "next",
        help=(
            "Print path to the next eligible task and exit 0; exit 1 if "
            "nothing is eligible."
        ),
    )
    _add_common_tasks_dir(p_next)
    _add_common_policy(p_next)
    _add_common_db(p_next)
    p_next.set_defaults(func=_cmd_next)

    p_orchestrate = sub.add_parser(
        "orchestrate",
        help=(
            "Drive every eligible task to quiescence: honor prerequisites, "
            "reactively unblock and resume blocked lifecycles, one in-process "
            "worker. Exit 0 only if every task it ran reached done."
        ),
    )
    _add_common_tasks_dir(p_orchestrate)
    _add_common_policy(p_orchestrate)
    _add_common_db(p_orchestrate)
    p_orchestrate.add_argument(
        "--sandbox-root",
        default=None,
        help=(
            "Root under which each task runs in <sandbox-root>/<task-id> "
            "(default: .flywheel/worktrees). Relative paths anchor at the "
            "repo root; @cache and @sibling select out-of-tree layouts."
        ),
    )
    p_orchestrate.add_argument(
        "--model",
        default=None,
        help="Override the Claude model passed to the SDK.",
    )
    p_orchestrate.add_argument(
        "--max-turns",
        type=int,
        default=DEFAULT_MAX_TURNS,
        help=f"Max agent turns per iteration (default: {DEFAULT_MAX_TURNS}).",
    )
    p_orchestrate.add_argument(
        "--max-retries",
        type=int,
        default=DEFAULT_MAX_RETRIES,
        help=(
            f"Harness retry budget after failed_validation "
            f"(default: {DEFAULT_MAX_RETRIES})."
        ),
    )
    p_orchestrate.add_argument(
        "--worker-id",
        default=None,
        help=(
            "Stable id for this worker (default: a random per-process id). "
            "Used for the per-task claim lease that keeps concurrent workers "
            "from double-running a task."
        ),
    )
    p_orchestrate.add_argument(
        "--lease-seconds",
        type=float,
        default=300.0,
        help=(
            "Task-claim lease window in seconds (default: 300). A live lease "
            "is renewed by a heartbeat while the task runs; a crashed "
            "worker's lease lapses after this window so the task is "
            "reclaimable. Expiry uses each host's wall clock, so set this "
            "well above (max cross-host clock skew + max heartbeat gap) or a "
            "fast-clocked worker may steal a live peer's lease."
        ),
    )
    p_orchestrate.add_argument(
        "--reconcile-seconds",
        type=float,
        default=15.0,
        help=(
            "Steering bridge: re-list the work source every N seconds and "
            "enqueue an interrupt for any in-flight run whose item is no "
            "longer listed (closed issue, pulled label, deleted task file). "
            "0 disables (default: 15)."
        ),
    )
    p_orchestrate.set_defaults(func=_cmd_orchestrate)

    p_status = sub.add_parser(
        "status",
        help="Print the state of every active task.",
    )
    _add_common_tasks_dir(p_status)
    _add_common_policy(p_status)
    _add_common_db(p_status)
    p_status.add_argument(
        "--json",
        action="store_true",
        help="Emit machine-readable JSON instead of a text table.",
    )
    p_status.add_argument(
        "--rollup",
        action="store_true",
        help=(
            "Render a phase-grouped, evidence-derived rollup: each task's "
            "status is computed from grader receipts (verified vs accepted "
            "vs blocked/failed/not-started), never self-reported."
        ),
    )
    p_status.set_defaults(func=_cmd_status)

    p_live = sub.add_parser(
        "live",
        help=(
            "Print one line per in-flight run showing its latest agent "
            "message or harness event (with age) so a watcher can tell at "
            "a glance whether progress is still being made."
        ),
    )
    _add_common_policy(p_live)
    _add_common_db(p_live)
    p_live.add_argument(
        "--watch",
        type=int,
        default=0,
        metavar="SECONDS",
        help=(
            "Refresh continuously every SECONDS; clears the screen on each "
            "tick. Default 0 prints one snapshot and exits."
        ),
    )
    p_live.set_defaults(func=_cmd_live)

    p_history = sub.add_parser(
        "history",
        help=(
            "List finished runs (done / failed / failed_validation), one "
            "line per task, most recently finished first. Retried tasks "
            "fold into one line (runs=N); 'show' drills into a run."
        ),
    )
    _add_common_tasks_dir(p_history)
    _add_common_policy(p_history)
    _add_common_db(p_history)
    p_history.add_argument(
        "--status",
        action="append",
        choices=[s.value for s in TERMINAL_STATUSES],
        default=None,
        help=(
            "Only list runs whose terminal status matches (repeatable; "
            "default: all terminal statuses)."
        ),
    )
    p_history.add_argument(
        "--phase",
        default=None,
        help="Only list tasks attributed to this phase directory name.",
    )
    p_history.add_argument(
        "--limit",
        type=int,
        default=0,
        metavar="N",
        help="Cap the number of task rows printed (default: 0 = all).",
    )
    p_history.add_argument(
        "--json",
        action="store_true",
        help="Emit machine-readable JSON instead of a text table.",
    )
    p_history.set_defaults(func=_cmd_history)

    p_show = sub.add_parser(
        "show",
        help=(
            "Show one run in full: lifecycle, attempts, grader receipts, "
            "final agent output, and the task's other runs. Accepts a "
            "run_id or a task id (resolves to its latest run)."
        ),
    )
    p_show.add_argument(
        "run_or_task_id",
        help="Run id, or a task id (its most recent run is shown).",
    )
    _add_common_tasks_dir(p_show)
    _add_common_policy(p_show)
    _add_common_db(p_show)
    p_show.add_argument(
        "--json",
        action="store_true",
        help="Emit machine-readable JSON instead of the text report.",
    )
    p_show.add_argument(
        "--raw",
        action="store_true",
        help=(
            "Disable redaction of decision-record output excerpts and emit "
            "them verbatim as stored. On by default, decision output passes "
            "through a best-effort secret-redaction policy; use --raw only "
            "for authorized forensics on trusted output sinks."
        ),
    )
    p_show.set_defaults(func=_cmd_show)

    p_archive = sub.add_parser(
        "archive",
        help=(
            "Move active phase directories whose tasks are all done into "
            "archive/."
        ),
    )
    _add_common_tasks_dir(p_archive)
    _add_common_policy(p_archive)
    _add_common_db(p_archive)
    p_archive.set_defaults(func=_cmd_archive)

    p_validate = sub.add_parser(
        "validate",
        help=(
            "Statically validate every active task's command graders "
            "(shell-parse + repo-relative path existence) without running "
            "them; exit non-zero naming each invalid task."
        ),
    )
    _add_common_tasks_dir(p_validate)
    _add_common_policy(p_validate)
    p_validate.set_defaults(func=_cmd_validate)

    p_recheck_blocked = sub.add_parser(
        "recheck-blocked",
        help=(
            "Re-evaluate blocked lifecycles' persisted requires and, when "
            "all predicates are satisfied, transition interrupted -> ready. "
            "Default scans every blocked lifecycle; --run-id targets one; "
            "--dry-run reports without transitioning."
        ),
    )
    _add_common_tasks_dir(p_recheck_blocked)
    _add_common_policy(p_recheck_blocked)
    _add_common_db(p_recheck_blocked)
    p_recheck_blocked.add_argument(
        "--run-id",
        default=None,
        help=(
            "Target a single lifecycle by run_id; skips the default scan. "
            "Prints a clear message and exits 0 when the run is missing "
            "or not blocked."
        ),
    )
    p_recheck_blocked.add_argument(
        "--dry-run",
        action="store_true",
        help=(
            "Evaluate predicates and report without applying the "
            "interrupted -> ready transition. Emits harness.recheck_attempted "
            "for the audit trail; does not emit harness.unblocked."
        ),
    )
    p_recheck_blocked.set_defaults(func=_cmd_recheck_blocked)

    p_recover = sub.add_parser(
        "recover",
        help=(
            "Finalize lifecycles stuck in running/validating "
            "(prints run_ids transitioned to interrupted)."
        ),
    )
    _add_common_policy(p_recover)
    _add_common_db(p_recover)
    p_recover.add_argument(
        "--task-id",
        default=None,
        help=(
            "Only recover stranded lifecycles for this task id. Omit to "
            "recover every stranded lifecycle in the store."
        ),
    )
    p_recover.set_defaults(func=_cmd_recover)

    p_resolve = sub.add_parser(
        "resolve",
        help=(
            "Deliberately abandon a stranded task: record an "
            "operator-attributed resolution with a reason so the next archive "
            "sweep archives the otherwise-landed phase. The only non-probe "
            "path to clearing a strand."
        ),
    )
    p_resolve.add_argument(
        "task_id",
        help=(
            "Task id of the strand to abandon (the stop-event subject). "
            "One subject per invocation."
        ),
    )
    p_resolve.add_argument(
        "--reason",
        required=True,
        help=(
            "Why the strand is being abandoned. Recorded verbatim on the "
            "resolution marker for the audit trail."
        ),
    )
    _add_common_policy(p_resolve)
    _add_common_db(p_resolve)
    p_resolve.set_defaults(func=_cmd_resolve)

    return parser


def main(argv: Iterable[str] | None = None) -> int:
    parser = _build_parser()
    args = parser.parse_args(list(argv) if argv is not None else None)
    try:
        return int(args.func(args))
    except (TaskLoadError, PolicyError, WorkSourceError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    except FileNotFoundError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())

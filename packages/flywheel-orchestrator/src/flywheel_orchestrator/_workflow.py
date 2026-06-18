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
from urllib.parse import urlsplit


from flywheel_core.harness import (
    RecheckOutcome,
    recheck_blocked_lifecycle,
)
from flywheel_core.lifecycle import Attempt, Lifecycle, Status
from flywheel_core.loaders import TaskLoadError, load_task_file
from flywheel_core.loop_path_marker import LoopPathSignal, detect_loop_path_signals
from flywheel_core.store_sqlite import SqliteStore

if TYPE_CHECKING:
    # Optional postgres backend, typing-only so this module never hard-depends
    # on the psycopg extra. The store factory returns SqliteStore |
    # PostgresStore and both answer these reads through the store protocol.
    from flywheel_core.store_postgres import PostgresStore
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
from flywheel_orchestrator._store_factory import (
    PG_DSN_ENV,
    PG_DSN_FALLBACK_ENV,
    open_sqlite_bound_store,
    resolve_postgres_dsn,
)

DEFAULT_TASKS_DIR = Path(".flywheel/tasks")


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
) -> TaskStatusRow | None:
    """Pick the first eligible task from ``rows``.

    A task is eligible when:

    * its ``id`` is not in ``exclude_ids``, AND
    * its state is :attr:`TaskState.FRESH`, :attr:`TaskState.RETRYABLE`,
      or :attr:`TaskState.INTERRUPTED`, AND
    * every prerequisite task (by ``id``) has state :attr:`TaskState.DONE`.

    Tasks whose prerequisites are missing from the workspace are treated
    as ineligible so a dangling reference never silently runs.

    ``exclude_ids`` removes tasks from *candidacy* without removing them
    from the prerequisite-resolution map, so a caller (e.g. the
    orchestrator) can skip an already-attempted or still-blocked task while
    that task can still satisfy a dependent's prerequisite. The default
    empty set preserves the pull-based CLI's behavior exactly.

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
    for row in by_id.values():
        if row.task.id in exclude_ids:
            continue
        if row.state not in eligible_states:
            continue
        if not all(
            (dep := by_id.get(prereq_id)) is not None
            and dep.state == TaskState.DONE
            for prereq_id in row.prerequisites
        ):
            continue
        return row
    return None

IN_LOOP_VERIFICATION_TAG = "in-loop-verification"

def archive_completed_phases(
    tasks_dir: Path,
    store: SqliteStore | PostgresStore,
    *,
    repo_root: Path | None = None,
    log: Callable[[str], None] | None = None,
    phase_verify: str | None = None,
) -> list[Path]:
    """Move ``active/<phase>`` dirs to ``archive/`` when every task is done.

    Returns the list of moved phase directories (post-move paths). Idempotent:
    safe to call repeatedly. Phases with any non-done task are left in place.

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
    """
    moved: list[Path] = []
    archive_root = tasks_dir / "archive"
    archive_root.mkdir(parents=True, exist_ok=True)

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
    try:
        rows = status_rows_for_items(source.list_work(), store)
        pick = select_next_task(rows)
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
    from flywheel_orchestrator import orchestrate

    policy = _load_effective_policy(args)
    source = _resolve_work_source(args, policy)
    db_path = _resolve_db_path(args, policy)
    if args.sandbox_root:
        sandbox_root = Path(args.sandbox_root)
    elif policy is not None and policy.sandbox_root is not None:
        sandbox_root = policy.sandbox_root
    else:
        sandbox_root = Path(".flywheel/worktrees")
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

def _cmd_status(args: argparse.Namespace) -> int:
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
    finally:
        store.close()
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
            out.append(entry)
        print(json.dumps(out, indent=2))
        return 0
    if not rows:
        print("(no active tasks)")
        return 0
    width = max(len(row.task.id) for row in rows)
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
    store = open_sqlite_bound_store(policy, db_path=db_path)
    try:
        moved = archive_completed_phases(tasks_dir, store)
    finally:
        store.close()
    for dest in moved:
        print(str(dest))
    return 0


def _repo_root_for_tasks_dir(tasks_dir: Path) -> Path:
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


def _cmd_validate(args: argparse.Namespace) -> int:
    """Statically validate every active task's graders (spec 00034).

    Runs :func:`flywheel_core.validate_task` over each active task and exits
    non-zero, naming each invalid task and its defects, when any task is
    statically broken; exit 0 when all are valid. Same defect shape the
    schedule-time dispatch consult surfaces.
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
    repo_root = _repo_root_for_tasks_dir(tasks_dir)
    invalid: dict[str, list[TaskDefect]] = {}
    valid_count = 0
    for _path, task in load_active_tasks(tasks_dir):
        defects = validate_task(task, repo_root=repo_root)
        if defects:
            invalid.setdefault(task.id, []).extend(defects)
        else:
            valid_count += 1
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


def _print_run_detail(detail: RunDetail) -> None:
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
    if detail.grader_results:
        last = detail.attempts[-1].number if detail.attempts else "?"
        print(f"graders  : (attempt {last})")
        for g in detail.grader_results:
            verdict = "pass" if g.passed else "FAIL"
            name = g.grader_name or g.grader_type
            print(
                f"  {verdict}  {g.grader_type:<10} {name}  "
                f"({g.duration_ms} ms)"
            )
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


def _run_detail_to_dict(detail: RunDetail) -> dict[str, Any]:
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
            }
            for a in detail.attempts
        ],
        "grader_results": [
            {
                "attempt_number": g.attempt_number,
                "ordinal": g.ordinal,
                "grader_type": g.grader_type,
                "grader_name": g.grader_name,
                "passed": g.passed,
                "duration_ms": g.duration_ms,
            }
            for g in detail.grader_results
        ],
        "related_runs": [
            _history_run_to_dict(r) for r in detail.related_runs
        ],
    }


def _cmd_show(args: argparse.Namespace) -> int:
    policy = _load_effective_policy(args)
    db_path = _resolve_db_path(args, policy)
    db_path.parent.mkdir(parents=True, exist_ok=True)
    store = open_sqlite_bound_store(policy, db_path=db_path)
    try:
        run_id = resolve_run_id(store, args.run_or_task_id)
        if run_id is None:
            print(f"{args.run_or_task_id}: no run or task with that id")
            return 1
        detail = collect_run_detail(
            store,
            run_id,
            fallback_phases=_resolve_fallback_phases(args, policy),
        )
    finally:
        store.close()
    if detail is None:
        print(f"{args.run_or_task_id}: no run or task with that id")
        return 1
    if args.json:
        print(json.dumps(_run_detail_to_dict(detail), indent=2))
        return 0
    _print_run_detail(detail)
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

_INIT_POLICY_HEADER = """\
# Flywheel work policy: where work comes from and where runtime state lives.
# Committed with the repo; CLI flags always override.
"""

_INIT_POLICY_TAIL = """\
[paths]
db = ".flywheel/flywheel.sqlite"
sandbox_root = ".flywheel/sandboxes"

# Default graders for work items that declare none (tracker sources only;
# directory task files always declare their own).
# [[defaults.graders]]
# type = "command"
# run = "uv run pytest"

# Agent runtime settings. Pin the model id the worker passes to the SDK
# verbatim (no allowlist enforced). CLI flags still override.
# [agent]
# model = "claude-sonnet-4-5"

# Landing policy. base pins the branch finished work lands on and the
# worker resolves its phase base from; unset falls back to the
# checked-out branch (back-compat).
# [submit]
# base = "main"

# Sandbox provisioning. setup runs (shell) inside every newly created
# sandbox before the agent enters, so tasks never pay discovery cost for
# a bare worktree. Unset means new sandboxes are used bare.
# [sandbox]
# setup = "uv sync"

# Phase-exit gate. verify runs (shell) against the merged phase base once
# every task in a phase has landed; a non-zero exit leaves the phase active
# instead of archiving it. Unset means today's archival (no gate).
# [phase]
# verify = "uv run pytest"
"""

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
            _INIT_POLICY_TAIL,
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


def _sanitize_connection_error(message: str, dsn: str) -> str:
    """Strip the DSN (and, defensively, its password) from an error.

    Host and database name surviving in the message is acceptable per
    spec; the verbatim DSN or password never is.
    """
    sanitized = message.replace(dsn, "<dsn>")
    try:
        password = urlsplit(dsn).password
    except ValueError:
        password = None
    if password:
        sanitized = sanitized.replace(password, "<password>")
    return " ".join(sanitized.split())


def _check_postgres_connection(dsn: str) -> str | None:
    """Test-connect to ``dsn``; return a sanitized error or ``None``.

    A short timeout keeps a wrong DSN from hanging init. Only called
    when the postgres extra imported successfully, so the local import
    cannot fail here.
    """
    import psycopg

    try:
        with psycopg.connect(dsn, connect_timeout=5):
            pass
    except Exception as exc:
        return _sanitize_connection_error(str(exc), dsn)
    return None


def _report_postgres_environment() -> None:
    """Print the postgres readiness report (spec FR-4/FR-5).

    Never raises and never blocks init: a missing extra, an unset DSN
    env var, and a failed connection each print and return -- the
    config is written regardless and init exits 0.
    """
    extra_ok = True
    try:
        import flywheel_core.store_postgres  # noqa: F401
    except ImportError:
        extra_ok = False
        print(
            "warning: the postgres extra is not installed; "
            "install with: uv add 'flywheel[postgres]'"
        )
    dsn = resolve_postgres_dsn()
    if dsn is None:
        print(
            f"postgres: no DSN found; set {PG_DSN_ENV} (or "
            f"{PG_DSN_FALLBACK_ENV}) before running flywheel worker"
        )
        return
    if not extra_ok:
        return
    error = _check_postgres_connection(dsn)
    if error is None:
        print("postgres: connection OK")
    else:
        print(f"warning: postgres connection failed: {error}")


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
        _report_postgres_environment()
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
            print()
            _print_init_next_steps(
                None, skills_installed=bool(args.skills)
            )
            return 0
        return _reconfigure_policy(args, policy_path)

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
            "(default: .flywheel/worktrees)."
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

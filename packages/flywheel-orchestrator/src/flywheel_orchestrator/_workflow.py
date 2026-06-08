"""Multi-task scheduling, phase/queue management, live dashboard, and the
``flywheel-orchestrate`` CLI — the consumer layer above the single-task loop.

Extracted verbatim from ``flywheel.workflow`` in the core/consumer split.
Shared single-task helpers are imported from ``flywheel.workflow`` (the
dependency arrow points orchestrator -> core, never the reverse).
"""

from __future__ import annotations

import argparse
import asyncio
import json
import shutil
import subprocess
import sys
import time
from collections.abc import (
    Callable,
    Iterable,
    Iterator,
    Mapping,
)
from dataclasses import dataclass
from datetime import datetime, timezone
from enum import Enum
from pathlib import Path
from typing import Any


from flywheel.harness import (
    RecheckOutcome,
    recheck_blocked_lifecycle,
)
from flywheel.lifecycle import Status
from flywheel.loaders import TaskLoadError, load_task_file
from flywheel.loop_path_marker import LoopPathSignal, detect_loop_path_signals
from flywheel.store_sqlite import SqliteStore
from flywheel.task import ManualGrader, Task
from flywheel.workflow import (
    DEFAULT_MAX_RETRIES,
    DEFAULT_MAX_TURNS,
    _add_common_db,
    _has_done_lifecycle,
    _resolve_db,
    _short,
    recover_stranded_lifecycles,
)

DEFAULT_TASKS_DIR = Path(".workflow/tasks")

DEFAULT_LOG_DIR = Path("logs/worker")

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
    """

    task_file: Path
    task: Task
    state: TaskState
    latest_run_id: str | None
    latest_status: Status | None
    latest_error: str
    blocked_requires: str | None = None
    awaiting_manual_ordinal: int | None = None

def iter_active_phase_dirs(tasks_dir: Path) -> Iterator[Path]:
    """Yield ``active/<phase>`` subdirectories in deterministic order.

    Filename prefix (``NN-...``) sorts the phases. Hidden directories and
    files are skipped.
    """
    active = tasks_dir / "active"
    if not active.is_dir():
        return
    for entry in sorted(active.iterdir()):
        if entry.is_dir() and not entry.name.startswith("."):
            yield entry

def iter_active_task_files(tasks_dir: Path) -> Iterator[Path]:
    """Yield every ``active/<phase>/<task>.json`` in deterministic order."""
    for phase in iter_active_phase_dirs(tasks_dir):
        for entry in sorted(phase.iterdir()):
            if (
                entry.is_file()
                and entry.suffix == ".json"
                and not entry.name.startswith("_")
                and not entry.name.startswith(".")
            ):
                yield entry

def load_active_tasks(tasks_dir: Path) -> list[tuple[Path, Task]]:
    """Load every active task; raise ``TaskLoadError`` on the first bad file.

    The list is in deterministic walk order so ``next`` ties break by
    filename.
    """
    out: list[tuple[Path, Task]] = []
    for path in iter_active_task_files(tasks_dir):
        out.append((path, load_task_file(path)))
    return out

_ACTIVE_STATUSES: frozenset[Status] = frozenset(
    {Status.READY, Status.RUNNING, Status.VALIDATING}
)

def _latest_lifecycle_row(
    store: SqliteStore, task_id: str
) -> tuple[str, Status, str, str | None, int | None] | None:
    """Return ``(run_id, status, error, blocked_requires_json,
    awaiting_manual_ordinal)`` of the most recent lifecycle for
    ``task_id``, or ``None`` if no lifecycle exists.

    Uses the SQLite connection directly because no Protocol method exposes
    a by-task-id lookup — and adding one would leak workflow concerns into
    the store contract. ``blocked_requires_json`` is returned verbatim so
    callers can decide whether to parse it (text status flattens, JSON
    status emits the decoded list). ``awaiting_manual_ordinal`` is the
    index in ``task.graders`` of the manual gate a parked
    ``AWAITING_APPROVAL`` lifecycle is pinned to; ``NULL`` in every
    other state.
    """
    cursor = store._connection.execute(  # noqa: SLF001 — intentional
        """
        SELECT run_id, status, error, blocked_requires_json,
               awaiting_manual_ordinal
        FROM lifecycles
        WHERE task_id = ?
        ORDER BY updated_at DESC, run_id DESC
        LIMIT 1
        """,
        (task_id,),
    )
    row = cursor.fetchone()
    if row is None:
        return None
    return (
        row["run_id"],
        Status(row["status"]),
        row["error"] or "",
        row["blocked_requires_json"],
        row["awaiting_manual_ordinal"],
    )

def task_state(store: SqliteStore, task: Task) -> TaskStatusRow:
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
    elif status in (Status.FAILED, Status.FAILED_VALIDATION):
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

def build_status_rows(
    tasks_dir: Path, store: SqliteStore
) -> list[TaskStatusRow]:
    """Walk active tasks and return their classified status, in walk order."""
    rows: list[TaskStatusRow] = []
    for path, task in load_active_tasks(tasks_dir):
        snapshot = task_state(store, task)
        rows.append(
            TaskStatusRow(
                task_file=path,
                task=snapshot.task,
                state=snapshot.state,
                latest_run_id=snapshot.latest_run_id,
                latest_status=snapshot.latest_status,
                latest_error=snapshot.latest_error,
                blocked_requires=snapshot.blocked_requires,
                awaiting_manual_ordinal=snapshot.awaiting_manual_ordinal,
            )
        )
    return rows

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
            for prereq_id in row.task.prerequisites
        ):
            continue
        return row
    return None

IN_LOOP_VERIFICATION_TAG = "in-loop-verification"

def archive_completed_phases(
    tasks_dir: Path,
    store: SqliteStore,
    *,
    repo_root: Path | None = None,
    log: Callable[[str], None] | None = None,
) -> list[Path]:
    """Move ``active/<phase>`` dirs to ``archive/`` when every task is done.

    Returns the list of moved phase directories (post-move paths). Idempotent:
    safe to call repeatedly. Phases with any non-done task are left in place.

    When ``repo_root`` is supplied, the phase's cumulative diff vs its
    recorded ``.loop-base`` is inspected for the watched loop-path signals
    (see :mod:`flywheel.loop_path_marker`). A phase whose diff hits any
    signal is gated: it archives only when it contains a DONE task tagged
    ``in-loop-verification`` OR a valid ``loop-path-exempt.md`` opt-out
    artifact lives alongside the task files. A gated phase stays in
    ``active/`` and the refusal reason is reported via ``log`` (the same
    ``Callable[[str], None]`` seam :func:`.workflow.worker.archive_phases`
    uses). An empty marker (no watched signal, no recorded base, or
    ``repo_root`` omitted) archives exactly as before -- the gate is a
    pure addition for the loop-path case.
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

        dest = archive_root / phase_dir.name
        # If a same-named archive exists, leave the active dir alone rather
        # than clobber prior history — operator can resolve manually.
        if dest.exists():
            continue
        shutil.move(str(phase_dir), str(dest))
        moved.append(dest)
    return moved

def _loop_path_gate_satisfied(
    phase_dir: Path, tasks: Iterable[Task], store: SqliteStore
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

LOOP_BASE_FILENAME = ".loop-base"

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

def read_phase_base(phase_dir: Path) -> str | None:
    """Return the SHA recorded in ``phase_dir/.loop-base`` or ``None``.

    Missing file, empty file, or unreadable file all map to ``None`` so the
    diff helper can degrade safely without raising.
    """
    base_file = phase_dir / LOOP_BASE_FILENAME
    if not base_file.is_file():
        return None
    try:
        sha = base_file.read_text(encoding="utf-8").strip()
    except OSError:
        return None
    return sha or None

def write_phase_base_if_missing(repo_root: Path, phase_dir: Path) -> bool:
    """Write the current ``HEAD`` SHA into ``phase_dir/.loop-base`` if absent.

    Idempotent: returns ``True`` when a fresh ``.loop-base`` was written,
    ``False`` when one already existed (so the first-seen SHA is preserved
    -- a re-run must not move the recorded base forward). The caller owns
    staging/committing the new file under the worker's merge lock; this
    helper only touches the working tree so the logic is testable without
    the worker.

    Returns ``False`` (no write) when ``phase_dir`` does not exist or when
    ``git rev-parse HEAD`` fails -- both signal "no usable base to record."
    """
    if not phase_dir.is_dir():
        return False
    base_file = phase_dir / LOOP_BASE_FILENAME
    if base_file.exists():
        return False
    rc, out = _git_capture(repo_root, "rev-parse", "HEAD")
    if rc != 0:
        return False
    sha = out.strip()
    if not sha:
        return False
    base_file.write_text(sha + "\n", encoding="utf-8")
    return True

def phase_diff_vs_base(repo_root: Path, phase_dir: Path) -> str:
    """Return ``git diff <recorded-base> HEAD`` for ``repo_root`` as text.

    Returns ``""`` when no ``.loop-base`` has been recorded for the phase
    (degrades safely rather than raising -- callers can treat an empty
    diff as "no signal"), or when the underlying ``git diff`` exits
    non-zero (e.g. the recorded SHA has been garbage-collected). The
    returned text is the raw unified-diff payload from git, suitable for
    feeding the loop-path marker's symbol-level scans.
    """
    base = read_phase_base(phase_dir)
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

def _resolve_tasks_dir(arg: str | None) -> Path:
    return Path(arg) if arg else DEFAULT_TASKS_DIR

def _cmd_next(args: argparse.Namespace) -> int:
    tasks_dir = _resolve_tasks_dir(args.tasks_dir)
    db_path = _resolve_db(args.db)
    db_path.parent.mkdir(parents=True, exist_ok=True)
    store = SqliteStore(db_path)
    try:
        rows = build_status_rows(tasks_dir, store)
        pick = select_next_task(rows)
    finally:
        store.close()
    if pick is None:
        return 1
    print(pick.task_file)
    return 0

def _cmd_orchestrate(args: argparse.Namespace) -> int:
    # TEMPORARY bridge (core/consumer split, Phase 3): the orchestrate driver
    # now lives in the flywheel-orchestrator package. This lazy import is the
    # one remaining core -> consumer reference; it goes away in Phase 3b when
    # the multi-task CLI moves to the orchestrator package.
    from flywheel_orchestrator import orchestrate

    tasks_dir = _resolve_tasks_dir(args.tasks_dir)
    db_path = _resolve_db(args.db)
    sandbox_root = (
        Path(args.sandbox_root)
        if args.sandbox_root
        else Path(".workflow/worktrees")
    )
    report = asyncio.run(
        orchestrate(
            tasks_dir=tasks_dir,
            db_path=db_path,
            sandbox_root=sandbox_root,
            model=args.model,
            max_turns=args.max_turns,
            max_retries=args.max_retries,
            worker_id=args.worker_id,
            lease_seconds=args.lease_seconds,
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

@dataclass(frozen=True, kw_only=True)
class LiveRunRow:
    """Per-in-flight-run snapshot used by ``live`` reporting.

    ``attempt`` / ``iteration`` form the lifecycle-position breadcrumb
    (``attempt=N iter=K``). ``last_kind`` + ``last_detail`` are the
    current agent action. ``tokens_total``/``cost_usd_total``/
    ``turns_total`` are running totals summed from this run's
    ``harness.iteration_completed`` events (per the 00011 spec —
    summed at query time, no harness counter). ``iterations_completed``
    is the count of those events; zero means "no totals yet" and the
    totals fields are all zero by definition.

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

_SDK_KIND_LABELS: dict[str, str] = {
    "AssistantMessage": "ASSISTANT",
    "UserMessage": "USER",
    "SystemMessage": "SYSTEM",
    "ResultMessage": "RESULT",
}

def _summarize_assistant(payload: Mapping[str, Any]) -> str:
    content = payload.get("content") or []
    if not content:
        return "(empty)"
    first = content[0]
    if not isinstance(first, Mapping):
        return _short(first)
    ctype = first.get("type")
    if ctype == "tool_use":
        name = first.get("name", "?")
        input_obj = first.get("input") or {}
        if isinstance(input_obj, Mapping) and input_obj:
            kv = ", ".join(
                f"{k}={_short(v, 30)}"
                for k, v in list(input_obj.items())[:2]
            )
            return f"{name}({kv})"
        return f"{name}()"
    text = first.get("text") if isinstance(first, Mapping) else None
    if isinstance(text, str):
        return _short(text)
    return _short(first)

def _summarize_user(payload: Mapping[str, Any]) -> str:
    content = payload.get("content") or []
    if not content:
        return "(empty)"
    first = content[0]
    if isinstance(first, Mapping):
        if "tool_use_id" in first:
            body = first.get("content")
            size = len(body) if isinstance(body, str) else 0
            return f"tool_result({size}B)"
        text = first.get("text")
        if isinstance(text, str):
            return _short(text)
    return _short(first)

def _summarize_result(payload: Mapping[str, Any]) -> str:
    return (
        f"subtype={payload.get('subtype', '?')} "
        f"turns={payload.get('num_turns', '?')} "
        f"dur={payload.get('duration_ms', '?')}ms"
    )

def _summarize_system(payload: Mapping[str, Any]) -> str:
    subtype = payload.get("subtype")
    return str(subtype) if subtype is not None else "(system)"

def _summarize_sdk_message(message_type: str, payload_json: str) -> str:
    try:
        payload = json.loads(payload_json)
    except json.JSONDecodeError:
        return "(unparseable payload)"
    if not isinstance(payload, Mapping):
        return _short(payload)
    if message_type == "AssistantMessage":
        return _summarize_assistant(payload)
    if message_type == "UserMessage":
        return _summarize_user(payload)
    if message_type == "ResultMessage":
        return _summarize_result(payload)
    if message_type == "SystemMessage":
        return _summarize_system(payload)
    return message_type

def _parse_db_ts(value: str | None) -> datetime | None:
    if value is None:
        return None
    try:
        return datetime.fromisoformat(value)
    except ValueError:
        return None

_EVENT_KINDS_WITH_ITERATION: frozenset[str] = frozenset(
    {"harness.iteration_completed"}
)

def _iteration_from_event_payload(
    kind: str, payload_json: str | None
) -> int | None:
    """Best-effort extract of the iteration number from an event payload.

    Only ``harness.iteration_completed`` (the only iteration-bearing
    telemetry event today) is parsed. Any decode / shape failure
    silently returns ``None`` — the breadcrumb falls back to ``iter=?``
    rather than crashing the view.
    """
    if kind not in _EVENT_KINDS_WITH_ITERATION:
        return None
    if not payload_json:
        return None
    try:
        payload = json.loads(payload_json)
    except json.JSONDecodeError:
        return None
    if not isinstance(payload, Mapping):
        return None
    value = payload.get("iteration")
    return value if isinstance(value, int) else None

def _sum_run_totals(
    conn: Any, run_id: str
) -> tuple[int, float, int, int]:
    """Aggregate this run's iteration-completed telemetry.

    Returns ``(tokens, cost_usd, turns, iterations_completed)``. Missing
    fields on an older payload are treated as zero / null and skipped —
    the remaining fields still aggregate. ``cost_usd``/``turns`` are
    SDK-reported as session-cumulative per iteration; the 00011 spec
    explicitly summed them at query time, accepting the known
    overcount when the SDK reuses a session across iterations.
    """
    rows = conn.execute(
        """
        SELECT payload_json
        FROM events
        WHERE run_id = ? AND kind = 'harness.iteration_completed'
        """,
        (run_id,),
    ).fetchall()
    tokens = 0
    cost = 0.0
    turns = 0
    count = 0
    for row in rows:
        count += 1
        try:
            payload = json.loads(row["payload_json"])
        except json.JSONDecodeError:
            continue
        if not isinstance(payload, Mapping):
            continue
        usage = payload.get("usage")
        if isinstance(usage, Mapping):
            tt = usage.get("total_tokens")
            if isinstance(tt, int):
                tokens += tt
        cost_val = payload.get("total_cost_usd")
        if isinstance(cost_val, (int, float)):
            cost += float(cost_val)
        turns_val = payload.get("num_turns")
        if isinstance(turns_val, int):
            turns += turns_val
    return tokens, cost, turns, count

def collect_live_rows(store: SqliteStore) -> list[LiveRunRow]:
    """Snapshot every in-flight run with its latest activity.

    Reads ``lifecycles`` for status in ``{running, validating,
    awaiting_approval}`` and joins each row to the freshest
    ``sdk_messages``/``events`` entry by per-run sequence. The two tables
    share a monotonic counter (see ``run_sequence``), so picking the
    higher sequence is correct even when the timestamps are within
    microseconds of each other. The same join also pins the latest
    ``attempt_number`` so the breadcrumb stays accurate across retries.
    Totals are summed from the run's ``harness.iteration_completed``
    events at query time (no harness counter). Output is sorted by
    ``task_id`` for stable multi-run rendering per the 00011 spec.

    ``awaiting_approval`` rows have no live worker but are still owed
    operator attention; including them lets ``flywheel live`` surface
    the pending manual-gate instruction. The instruction is resolved
    from the task definition pinned to the run.
    """
    conn = store._connection  # noqa: SLF001
    lifecycles = conn.execute(
        """
        SELECT run_id, task_id, status, awaiting_manual_ordinal
        FROM lifecycles
        WHERE status IN ('running', 'validating', 'awaiting_approval')
        ORDER BY task_id, run_id
        """
    ).fetchall()
    rows: list[LiveRunRow] = []
    for lc in lifecycles:
        run_id = lc["run_id"]
        sdk = conn.execute(
            """
            SELECT iteration_number, attempt_number, sequence, message_type,
                   payload_json, ts
            FROM sdk_messages
            WHERE run_id = ?
            ORDER BY sequence DESC
            LIMIT 1
            """,
            (run_id,),
        ).fetchone()
        evt = conn.execute(
            """
            SELECT attempt_number, sequence, kind, payload_json, ts
            FROM events
            WHERE run_id = ?
            ORDER BY sequence DESC
            LIMIT 1
            """,
            (run_id,),
        ).fetchone()
        sdk_seq = sdk["sequence"] if sdk is not None else -1
        evt_seq = evt["sequence"] if evt is not None else -1
        iteration: int | None = None
        attempt: int | None = None
        last_kind = "(none)"
        last_detail = "(no activity yet)"
        last_ts: datetime | None = None
        if sdk_seq >= 0 and sdk_seq >= evt_seq and sdk is not None:
            iteration = sdk["iteration_number"]
            attempt_raw = sdk["attempt_number"]
            attempt = (
                int(attempt_raw) if isinstance(attempt_raw, int) else None
            )
            # sqlite3.Row indexing is typed Any; pin to str so last_kind stays
            # str (dict.get over an Any key/default otherwise widens to
            # str | None).
            message_type = str(sdk["message_type"])
            last_kind = _SDK_KIND_LABELS.get(
                message_type, message_type.upper()
            )
            last_detail = _summarize_sdk_message(
                message_type, sdk["payload_json"]
            )
            last_ts = _parse_db_ts(sdk["ts"])
        elif evt is not None and evt_seq >= 0:
            attempt_raw = evt["attempt_number"]
            attempt = (
                int(attempt_raw) if isinstance(attempt_raw, int) else None
            )
            last_kind = "EVENT"
            last_detail = str(evt["kind"])
            last_ts = _parse_db_ts(evt["ts"])
            # Iteration-bearing harness events carry the iteration in the
            # payload — fold it into the breadcrumb so the position stays
            # accurate when the freshest activity is the iteration-end
            # event, not an sdk_message.
            iteration = _iteration_from_event_payload(
                str(evt["kind"]), evt["payload_json"]
            )
        tokens, cost, turns, iters_completed = _sum_run_totals(conn, run_id)
        status = Status(lc["status"])
        awaiting_instruction = _resolve_awaiting_instruction(
            store,
            run_id=run_id,
            status=status,
            awaiting_ordinal=lc["awaiting_manual_ordinal"],
        )
        rows.append(
            LiveRunRow(
                run_id=run_id,
                task_id=lc["task_id"],
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
            )
        )
    return rows

def _resolve_awaiting_instruction(
    store: SqliteStore,
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
    ``harness.iteration_completed`` events. Renders zero/`--` when the
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
    if row.last_ts is None:
        age_str = "—"
        stale = ""
    else:
        age_s = int((now - row.last_ts).total_seconds())
        # Negative ages (clock skew between SQLite and host) read as "0s"
        # rather than a misleading negative.
        if age_s < 0:
            age_s = 0
        age_str = f"{age_s}s"
        stale = "  STALE" if age_s > _LIVE_STALE_AFTER_SECONDS else ""
    detail = _short(row.last_detail, _LIVE_DETAIL_MAX_WIDTH)
    head = (
        f"{row.task_id}  {row.status.value}  "
        f"{_format_breadcrumb(row)}  "
        f"{_format_totals(row)}  "
        f"age={age_str}  {row.last_kind}  {detail}{stale}"
    )
    if row.awaiting_instruction is not None:
        # The owed decision is rendered as an indented follow-up line —
        # mirrors ``flywheel status``'s ``awaiting_on:`` / ``blocked_on:``
        # pattern so operators have one consistent shape for "this run
        # needs you" surfacing across both views.
        return f"{head}\n    awaiting_on: {row.awaiting_instruction}"
    return head

def _cmd_live(args: argparse.Namespace) -> int:
    db_path = _resolve_db(args.db)
    db_path.parent.mkdir(parents=True, exist_ok=True)
    interval = int(args.watch) if args.watch else 0

    def snapshot() -> None:
        store = SqliteStore(db_path)
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
    :func:`flywheel.harness._evaluate_blocked_predicate`), so we can read
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
    tasks_dir = _resolve_tasks_dir(args.tasks_dir)
    db_path = _resolve_db(args.db)
    db_path.parent.mkdir(parents=True, exist_ok=True)
    store = SqliteStore(db_path)
    try:
        rows = build_status_rows(tasks_dir, store)
    finally:
        store.close()
    if args.json:
        out: list[dict[str, Any]] = []
        for row in rows:
            entry: dict[str, Any] = {
                "task_id": row.task.id,
                "task_file": str(row.task_file),
                "state": row.state.value,
                "latest_run_id": row.latest_run_id,
                "latest_status": (
                    row.latest_status.value if row.latest_status else None
                ),
                "latest_error": row.latest_error,
                "prerequisites": list(row.task.prerequisites),
            }
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
        phase = row.task_file.parent.name
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

def _list_blocked_lifecycles(store: SqliteStore) -> list[tuple[str, str]]:
    """Return ``(run_id, task_id)`` for every recheckable blocked lifecycle.

    Filter mirrors spec FR-7: only ``INTERRUPTED`` rows with a non-NULL
    ``blocked_requires_json`` qualify — SIGINT-paused lifecycles (which
    leave the column NULL) are intentionally excluded so they keep using
    the existing run_task entry-time normalization to resume.
    """
    cursor = store._connection.execute(  # noqa: SLF001 — intentional
        """
        SELECT run_id, task_id
        FROM lifecycles
        WHERE status = ? AND blocked_requires_json IS NOT NULL
        ORDER BY updated_at
        """,
        (Status.INTERRUPTED.value,),
    )
    return [(row["run_id"], row["task_id"]) for row in cursor.fetchall()]

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
    tasks_dir = _resolve_tasks_dir(args.tasks_dir)
    db_path = _resolve_db(args.db)
    db_path.parent.mkdir(parents=True, exist_ok=True)
    store = SqliteStore(db_path)
    try:
        # Build a task_id -> Task map by walking the active tasks dir.
        # An archived (or not-yet-active) task whose lifecycle is blocked
        # in the store is skipped with a stderr warning so a single
        # missing file does not crash the batch.
        task_by_id: dict[str, Task] = {}
        for _path, task in load_active_tasks(tasks_dir):
            task_by_id[task.id] = task

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
                store, args.run_id, task, dry_run=args.dry_run
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
                store, run_id, task, dry_run=args.dry_run
            )
            print(_format_recheck_line(run_id, outcome, dry_run=args.dry_run))
        return 0
    finally:
        store.close()

def _cmd_recover(args: argparse.Namespace) -> int:
    db_path = _resolve_db(args.db)
    db_path.parent.mkdir(parents=True, exist_ok=True)
    store = SqliteStore(db_path)
    try:
        finalized = recover_stranded_lifecycles(store, task_id=args.task_id)
    finally:
        store.close()
    if not finalized:
        print("(no stranded lifecycles)")
        return 0
    for run_id in finalized:
        print(run_id)
    return 0

def _cmd_archive(args: argparse.Namespace) -> int:
    tasks_dir = _resolve_tasks_dir(args.tasks_dir)
    db_path = _resolve_db(args.db)
    db_path.parent.mkdir(parents=True, exist_ok=True)
    store = SqliteStore(db_path)
    try:
        moved = archive_completed_phases(tasks_dir, store)
    finally:
        store.close()
    for dest in moved:
        print(str(dest))
    return 0


def _add_common_tasks_dir(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--tasks-dir",
        default=None,
        help=f"Tasks root directory (default: {DEFAULT_TASKS_DIR}).",
    )


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="flywheel-orchestrate",
        description=(
            "Schedule and drive many flywheel tasks laid out under "
            ".workflow/tasks/active/<phase>/."
        ),
    )
    sub = parser.add_subparsers(dest="command", required=True)

    p_next = sub.add_parser(
        "next",
        help=(
            "Print path to the next eligible task and exit 0; exit 1 if "
            "nothing is eligible."
        ),
    )
    _add_common_tasks_dir(p_next)
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
    _add_common_db(p_orchestrate)
    p_orchestrate.add_argument(
        "--sandbox-root",
        default=None,
        help=(
            "Root under which each task runs in <sandbox-root>/<task-id> "
            "(default: .workflow/worktrees)."
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
    p_orchestrate.set_defaults(func=_cmd_orchestrate)

    p_status = sub.add_parser(
        "status",
        help="Print the state of every active task.",
    )
    _add_common_tasks_dir(p_status)
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

    p_archive = sub.add_parser(
        "archive",
        help=(
            "Move active phase directories whose tasks are all done into "
            "archive/."
        ),
    )
    _add_common_tasks_dir(p_archive)
    _add_common_db(p_archive)
    p_archive.set_defaults(func=_cmd_archive)

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
    except TaskLoadError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    except FileNotFoundError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())

"""Workflow CLI — drive a directory of flywheel tasks against a real agent.

This module is the dogfooding bridge between the ``.workflow/`` task layout
on disk and :func:`flywheel.harness.run_task`. It owns no execution logic
of its own — task selection and persistence-status queries live here, but
running a task delegates to the harness with the production Claude Code
invoker.

Layout assumed on disk::

    .workflow/
        flywheel.sqlite                       # store; created on first run
        tasks/
            active/
                01-phase-name/
                    <task-id>.json            # one flywheel Task per file
                    <task-id>.json
                02-other/
                    ...
            archive/
                01-finished-phase/
                    ...

Task files conform to ``docs/task-schema.md`` and are loaded via
:func:`flywheel.loaders.load_task_file`. Their on-disk JSON is immutable;
all execution state lives in SQLite. Phase ordering is purely the directory
name's numeric prefix; cross-task dependencies use the ``prerequisites``
field on each task.

Subcommands::

    python -m flywheel.workflow next [--tasks-dir DIR] [--db PATH]
    python -m flywheel.workflow run  GOAL_OR_FILE [--check CMD] [--rubric A]
                                     [--db PATH] [--sandbox DIR]
                                     [--model MODEL] [--max-retries N]
                                     [--max-turns N] [--json | --quiet]

``run`` accepts either an inline goal string or a task-file path; an inline
goal with no ``--check``/``--rubric`` is an unverified run (DONE reflects the
agent's own claim). Installed as the ``flywheel`` console script, so
``flywheel run "<goal>"`` is the zero-config front door. Events stream to
stdout as they fire (readable by default, NDJSON with ``--json``).
    python -m flywheel.workflow status [--tasks-dir DIR] [--db PATH]
    python -m flywheel.workflow live   [--db PATH] [--watch SECONDS]
    python -m flywheel.workflow is-done TASK_FILE [--db PATH]
    python -m flywheel.workflow archive [--tasks-dir DIR] [--db PATH]
    python -m flywheel.workflow recover [--db PATH]
    python -m flywheel.workflow recheck-blocked [--tasks-dir DIR] [--db PATH]
                                                [--run-id ID] [--dry-run]
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import shutil
import signal
import subprocess
import sys
import time
from collections.abc import Callable, Iterable, Iterator, Mapping, Sequence
from dataclasses import dataclass
from datetime import datetime, timezone
from enum import Enum
from pathlib import Path
from typing import Any, TextIO

from claude_agent_sdk import (
    AssistantMessage,
    ClaudeAgentOptions,
    Message,
    ResultMessage,
    TextBlock,
    ToolResultBlock,
    ToolUseBlock,
    UserMessage,
)

from flywheel.events import DomainEvent
from flywheel.harness import (
    HarnessConfig,
    HarnessOutcome,
    HarnessStore,
    InvocationRequest,
    InvokeFunc,
    RecheckOutcome,
    finalize_stranded_lifecycle,
    recheck_blocked_lifecycle,
    run_task,
)
from flywheel.invoker import (
    IterationResult,
    _serialize_sdk_message,
    invoke_iteration,
)
from flywheel.invoker_client import invoke_iteration_with_client
from flywheel.lifecycle import Attempt, Lifecycle, Status
from flywheel.loaders import TaskLoadError, load_task_file
from flywheel.loop_path_marker import LoopPathSignal, detect_loop_path_signals
from flywheel.store_protocols import (
    ControlCommandStore,
    EventRecord,
    GraderResultRecord,
    SdkMessageRecord,
)
from flywheel.store_sqlite import SqliteStore
from flywheel.task import (
    CommandGrader,
    Grader,
    ManualGrader,
    RubricGrader,
    Task,
    ValidationError,
)


DEFAULT_TASKS_DIR = Path(".workflow/tasks")
DEFAULT_DB_PATH = Path(".workflow/flywheel.sqlite")
DEFAULT_LOG_DIR = Path("logs/worker")
DEFAULT_MAX_TURNS = 500
DEFAULT_MAX_RETRIES = 1


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


# --- Filesystem walking -----------------------------------------------------


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


# --- Status queries ---------------------------------------------------------


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


def _has_done_lifecycle(store: SqliteStore, task_id: str) -> bool:
    cursor = store._connection.execute(  # noqa: SLF001
        "SELECT 1 FROM lifecycles WHERE task_id = ? AND status = ? LIMIT 1",
        (task_id, Status.DONE.value),
    )
    return cursor.fetchone() is not None


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


# --- Next-task selection ----------------------------------------------------


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


# --- Stranded-lifecycle recovery -------------------------------------------


_STRANDED_STATUSES: frozenset[Status] = frozenset(
    {Status.RUNNING, Status.VALIDATING}
)


def _stranded_run_ids(store: SqliteStore, task_id: str | None = None) -> list[str]:
    """Return run_ids whose lifecycle is mid-attempt with no live worker.

    A run is considered stranded when its status sits in ``running`` or
    ``validating`` after the worker that owned it exited — there is no
    process-level liveness check available here, so the caller uses this
    only at boundaries where it knows no harness is currently running
    that lifecycle (worker start, post-interrupt cleanup, ``recover``).
    """
    placeholders = ", ".join("?" for _ in _STRANDED_STATUSES)
    params: list[str] = [s.value for s in _STRANDED_STATUSES]
    sql = (
        f"SELECT run_id FROM lifecycles WHERE status IN ({placeholders})"
    )
    if task_id is not None:
        sql += " AND task_id = ?"
        params.append(task_id)
    sql += " ORDER BY updated_at"
    cursor = store._connection.execute(sql, params)  # noqa: SLF001
    return [row["run_id"] for row in cursor.fetchall()]


def recover_stranded_lifecycles(
    store: SqliteStore, *, task_id: str | None = None
) -> list[str]:
    """Finalize every stranded lifecycle, optionally filtered by ``task_id``.

    Delegates to :func:`flywheel.harness.finalize_stranded_lifecycle` for
    each match; returns the run_ids that were actually finalized (i.e.
    were in ``running``/``validating`` at the time of the call).
    """
    finalized: list[str] = []
    for run_id in _stranded_run_ids(store, task_id):
        if finalize_stranded_lifecycle(store, run_id):
            finalized.append(run_id)
    return finalized


# --- Run subcommand ---------------------------------------------------------

EVENTS_PLAIN = "plain"
EVENTS_JSON = "json"
EVENTS_NONE = "none"


def _event_payload_summary(payload: Mapping[str, Any], *, limit: int = 100) -> str:
    """Render a telemetry event's payload as a compact ``k=v`` tail."""
    if not payload:
        return ""
    parts: list[str] = []
    for key, value in payload.items():
        text = str(value).replace("\n", " ").replace("\r", " ")
        if len(text) > 40:
            text = text[:39] + "…"
        parts.append(f"{key}={text}")
    joined = " ".join(parts)
    if len(joined) > limit:
        joined = joined[: limit - 1] + "…"
    return joined


def _format_event_line(event: EventRecord) -> str:
    """One readable line per persisted event for the default ``run`` stream."""
    attempt = (
        f" attempt={event.attempt_number}"
        if event.attempt_number is not None
        else ""
    )
    ts = event.ts.strftime("%H:%M:%S")
    summary = _event_payload_summary(dict(event.payload))
    tail = f"  {summary}" if summary else ""
    return f"[{ts}] {event.kind}{attempt}{tail}"


def _event_json_line(event: EventRecord) -> str:
    """One NDJSON line per persisted event for ``--json`` consumers."""
    return json.dumps(
        {
            "id": event.id,
            "run_id": event.run_id,
            "ts": event.ts.isoformat(),
            "kind": event.kind,
            "attempt_number": event.attempt_number,
            "category": event.category,
            "payload": dict(event.payload),
        },
        sort_keys=True,
    )


def _format_tool_use(name: str, tool_input: Mapping[str, Any]) -> str:
    """Render a tool call as ``name(k=v, ...)`` for the live stream."""
    if tool_input:
        kv = ", ".join(
            f"{key}={_short(value, 40)}"
            for key, value in list(tool_input.items())[:3]
        )
        return f"{name}({kv})"
    return f"{name}()"


def _summarize_assistant_blocks(content: Sequence[Any]) -> str:
    """Join an assistant turn's text and tool calls into one readable line."""
    parts: list[str] = []
    for block in content:
        if isinstance(block, TextBlock):
            text = block.text.strip()
            if text:
                parts.append(_short(text, 120))
        elif isinstance(block, ToolUseBlock):
            parts.append(_format_tool_use(block.name, block.input))
    return "  ".join(p for p in parts if p) or "(no content)"


def _summarize_user_content(content: object) -> str:
    """Summarize a user turn — tool results (with size) or echoed text."""
    if isinstance(content, str):
        return _short(content, 120)
    if not isinstance(content, Sequence):
        return _short(content)
    parts: list[str] = []
    for block in content:
        if isinstance(block, ToolResultBlock):
            body = block.content
            if isinstance(body, str):
                size = len(body)
            elif body is None:
                size = 0
            else:
                size = len(json.dumps(body, default=str))
            err = " ERR" if block.is_error else ""
            parts.append(f"tool_result({size}B{err})")
        elif isinstance(block, TextBlock):
            text = block.text.strip()
            if text:
                parts.append(_short(text, 120))
    return "  ".join(p for p in parts if p) or "(empty)"


def _summarize_live_message(msg: Message) -> tuple[str, str]:
    """Map an SDK message to a ``(LABEL, detail)`` pair for the live stream.

    Reads the typed message objects directly (not the persisted JSON) so
    tool calls render as ``Write(file_path=...)`` rather than a raw block
    dict — the live stream has the real objects in hand.
    """
    if isinstance(msg, AssistantMessage):
        return ("ASSISTANT", _summarize_assistant_blocks(msg.content))
    if isinstance(msg, UserMessage):
        return ("USER", _summarize_user_content(msg.content))
    if isinstance(msg, ResultMessage):
        cost = msg.total_cost_usd
        cost_str = f" cost=${cost:.4f}" if isinstance(cost, float) else ""
        return (
            "RESULT",
            f"subtype={msg.subtype} turns={msg.num_turns}{cost_str}",
        )
    # SystemMessage and any forward-compat type: label by class name. Avoid
    # importing SystemMessage (not all SDK versions export it).
    name = type(msg).__name__
    if name == "SystemMessage":
        subtype = getattr(msg, "subtype", None)
        return ("SYSTEM", str(subtype) if subtype is not None else "")
    return (name.upper(), "")


def _format_live_message(msg: Message) -> str:
    """One readable line for a live SDK message, aligned with event lines."""
    label, detail = _summarize_live_message(msg)
    ts = datetime.now(timezone.utc).strftime("%H:%M:%S")
    tail = f"  {detail}" if detail else ""
    return f"[{ts}] {label}{tail}"


def _message_json_line(msg: Message) -> str:
    """One NDJSON line for a live SDK message (the persisted serialization)."""
    return json.dumps(_serialize_sdk_message(msg), sort_keys=True)


def _make_message_observer(
    events: str, *, out: TextIO
) -> Callable[[Message], None] | None:
    """Build the per-SDK-message stdout printer that interleaves with events.

    Returns ``None`` for :data:`EVENTS_NONE`; a readable formatter for
    :data:`EVENTS_PLAIN`; an NDJSON formatter for :data:`EVENTS_JSON`.
    Prints to ``out`` so messages share the event stream and, because both
    run on one event loop, land in true arrival order.
    """
    if events == EVENTS_NONE:
        return None
    if events == EVENTS_JSON:

        def emit_json(msg: Message) -> None:
            print(_message_json_line(msg), file=out, flush=True)

        return emit_json

    def emit_plain(msg: Message) -> None:
        print(_format_live_message(msg), file=out, flush=True)

    return emit_plain


class _EventStreamingStore:
    """Wrap a :class:`HarnessStore` and emit each persisted telemetry event.

    Every store call is forwarded verbatim to ``wrapped``; ``append_event``
    additionally hands the persisted record to ``emit`` so the run is
    observable live on stdout while the agent works. No authoritative state
    is owned here — the wrapped backend stays the single source of truth.
    Mirrors the seam in ``flywheel.examples.hello`` but with a pluggable
    formatter so ``run`` can offer both readable and JSON output.
    """

    def __init__(
        self, wrapped: HarnessStore, *, emit: Callable[[EventRecord], None]
    ) -> None:
        self._wrapped = wrapped
        self._emit = emit

    @property
    def notifier(self) -> Any:
        """Expose the wrapped store's notifier so an in-process audit
        follower sharing this wrapper still gets push wakeups."""
        return getattr(self._wrapped, "notifier", None)

    def create_lifecycle(self, lifecycle: Lifecycle) -> None:
        self._wrapped.create_lifecycle(lifecycle)

    def update_lifecycle(
        self, lifecycle: Lifecycle, *, expected_version: int
    ) -> None:
        self._wrapped.update_lifecycle(
            lifecycle, expected_version=expected_version
        )

    def load_lifecycle(self, run_id: str) -> Lifecycle | None:
        return self._wrapped.load_lifecycle(run_id)

    def save_task(self, task: Task, *, now: datetime) -> str:
        return self._wrapped.save_task(task, now=now)

    def append_domain_event(
        self, event: DomainEvent, *, expected_version: int
    ) -> Lifecycle:
        return self._wrapped.append_domain_event(
            event, expected_version=expected_version
        )

    def list_domain_events(self, run_id: str) -> list[DomainEvent]:
        return self._wrapped.list_domain_events(run_id)

    def save_attempt(self, run_id: str, attempt: Attempt) -> None:
        self._wrapped.save_attempt(run_id, attempt)

    def list_attempts(self, run_id: str) -> list[Attempt]:
        return self._wrapped.list_attempts(run_id)

    def append_event(self, event: EventRecord) -> EventRecord:
        persisted = self._wrapped.append_event(event)
        self._emit(persisted)
        return persisted

    def append_sdk_message(
        self, message: SdkMessageRecord
    ) -> SdkMessageRecord:
        return self._wrapped.append_sdk_message(message)

    def save_sdk_messages(
        self,
        run_id: str,
        attempt_number: int,
        iteration_number: int,
        messages: Sequence[Mapping[str, Any]],
    ) -> list[SdkMessageRecord]:
        return self._wrapped.save_sdk_messages(
            run_id, attempt_number, iteration_number, messages
        )

    def append_grader_result(
        self, result: GraderResultRecord
    ) -> GraderResultRecord:
        return self._wrapped.append_grader_result(result)

    def list_grader_results(
        self, run_id: str, attempt_number: int
    ) -> list[GraderResultRecord]:
        return self._wrapped.list_grader_results(run_id, attempt_number)


def build_inline_task(
    goal: str,
    *,
    checks: Sequence[str] = (),
    rubric_assertions: Sequence[str] = (),
    task_id: str | None = None,
) -> Task:
    """Build an in-memory :class:`Task` from a goal string and inline graders.

    ``checks`` become :class:`CommandGrader` entries (the strict path);
    ``rubric_assertions`` collapse into a single :class:`RubricGrader`. With
    neither, the task is graderless — an unverified run that records DONE on
    the agent's own claim (see docs/task-schema.md). No file is read or
    written: this is the direct-construction API the CLI uses for an inline
    goal.
    """
    graders: list[Grader] = [
        CommandGrader(run=cmd, name=f"check-{index + 1}")
        for index, cmd in enumerate(checks)
    ]
    if rubric_assertions:
        graders.append(RubricGrader(assertions=list(rubric_assertions)))
    task = (
        Task(goal=goal, graders=graders)
        if task_id is None
        else Task(id=task_id, goal=goal, graders=graders)
    )
    task.validate()
    return task


def _compose_message_observers(
    *observers: Callable[[Message], None] | None,
) -> Callable[[Message], None] | None:
    """Combine multiple ``on_message`` callbacks into a single observer.

    Each callback fires independently and is wrapped in its own
    ``try/except`` so a raising renderer cannot break the persistence
    observer (and vice versa). ``None`` entries are filtered. Returns
    ``None`` when no observer remains, ``observers[0]`` when only one is
    set (no wrapping overhead), and a composed callable otherwise.
    """
    callbacks = tuple(o for o in observers if o is not None)
    if not callbacks:
        return None
    if len(callbacks) == 1:
        return callbacks[0]

    def _combined(msg: Message) -> None:
        for cb in callbacks:
            try:
                cb(msg)
            except Exception:  # noqa: BLE001 - one observer must not
                # break the others; observation is best-effort.
                pass

    return _combined


def _make_claude_code_invoke(
    sandbox: Path,
    *,
    model: str | None,
    max_turns: int,
    on_message: Callable[[Message], None] | None = None,
    control_store: ControlCommandStore | None = None,
    run_id: str | None = None,
    audit_store: HarnessStore | None = None,
) -> InvokeFunc:
    """Production invoker: real Claude Code spawned in ``sandbox``.

    Drives one iteration through :class:`claude_agent_sdk.ClaudeSDKClient`
    so the in-process watcher coroutine, running concurrently with the
    agent's message stream, can claim operator-issued control commands
    from ``control_store`` (interrupt / set_model / say) and apply them
    live against the open session. Each applied command lands as a
    ``harness.control_command_applied`` event on ``audit_store``; a
    failed dispatch emits ``harness.control_command_failed`` and the
    iteration continues, mirroring the per-message persistence contract.

    ``control_store``, ``run_id``, and ``audit_store`` are required for
    the bidirectional path — when ``control_store`` is ``None`` the
    invoker falls back to the one-shot :func:`invoke_iteration` (used by
    legacy callers that have no run identity yet, e.g. the on_message
    forwarding test).

    ``on_message`` is the static stdout renderer composed with the
    per-request persistence observer the harness threads through
    :attr:`InvocationRequest.on_message`. Both fire for every SDK message
    the instant it arrives, each isolated by its own ``try/except`` so a
    raising renderer cannot break per-message persistence and vice versa.
    """
    options = ClaudeAgentOptions(
        cwd=str(sandbox),
        add_dirs=[str(sandbox)],
        permission_mode="bypassPermissions",
        skills="all",
        max_turns=max_turns,
        model=model,
    )

    if control_store is None or run_id is None:
        async def _invoke_legacy(request: InvocationRequest) -> IterationResult:
            composed = _compose_message_observers(
                on_message, request.on_message
            )
            return await invoke_iteration(
                prompt=request.prompt,
                options=options,
                on_message=composed,
            )

        return _invoke_legacy

    pinned_run_id = run_id
    pinned_audit_store = audit_store

    async def _invoke(request: InvocationRequest) -> IterationResult:
        composed = _compose_message_observers(on_message, request.on_message)
        emit: Callable[[str, Mapping[str, Any]], None] | None = None
        if pinned_audit_store is not None:
            # Bind a non-Optional reference so the inner closure does not
            # need to re-prove the None-check on every call.
            audit_sink: HarnessStore = pinned_audit_store
            attempt_number = request.attempt_number

            def _audit_emit(kind: str, payload: Mapping[str, Any]) -> None:
                # Route control-plane events through the same store the
                # harness writes to so the wrapped streaming wrapper
                # (if any) sees them and a live operator sees them on
                # stdout exactly like harness.* events.
                audit_sink.append_event(
                    EventRecord(
                        run_id=pinned_run_id,
                        ts=datetime.now(timezone.utc),
                        kind=kind,
                        payload=dict(payload),
                        attempt_number=attempt_number,
                    )
                )

            emit = _audit_emit
        return await invoke_iteration_with_client(
            prompt=request.prompt,
            options=options,
            control_store=control_store,
            run_id=pinned_run_id,
            audit_emit=emit,
            on_message=composed,
            context_observer=request.context_observer,
        )

    return _invoke


def _make_event_emitter(
    events: str, *, out: TextIO
) -> Callable[[EventRecord], None] | None:
    """Build the per-event stdout printer for ``run_task_object``.

    Returns ``None`` for :data:`EVENTS_NONE` (no wrapping); a readable
    line-formatter for :data:`EVENTS_PLAIN`; an NDJSON formatter for
    :data:`EVENTS_JSON`. Events go to ``out`` (stdout) so they stay
    separable from the ``[workflow]`` diagnostics on stderr.
    """
    if events == EVENTS_NONE:
        return None
    if events == EVENTS_JSON:

        def emit_json(event: EventRecord) -> None:
            print(_event_json_line(event), file=out, flush=True)

        return emit_json

    def emit_plain(event: EventRecord) -> None:
        print(_format_event_line(event), file=out, flush=True)

    return emit_plain


def _install_cancel_on_signal(
    loop: asyncio.AbstractEventLoop, target: asyncio.Task[Any]
) -> list[int]:
    """Route SIGTERM/SIGINT through ``target.cancel()`` for the run's duration.

    Production shutdown — ``docker stop``, ``kubectl delete pod``,
    ``systemctl stop`` — sends SIGTERM, whose default disposition terminates
    the interpreter *without raising*, so the in-flight :func:`run_task`
    never reaches its finalizer and its lifecycle is stranded in ``running``
    (see ``.workflow/audits/02-harness-resilience.md``). Cancelling the
    running task instead funnels operator shutdown into the same
    :class:`asyncio.CancelledError` path the caller already drains via
    :func:`finalize_stranded_lifecycle`, so a graceful terminate leaves a
    clean, resumable ``interrupted`` lifecycle. SIGINT is routed the same way
    for uniformity (previously it surfaced as ``KeyboardInterrupt``).

    Returns the signal numbers actually installed so the caller removes
    exactly those. ``add_signal_handler`` is unavailable off the main thread
    and on some platforms (e.g. Windows); when it raises we degrade to the
    prior behavior rather than fail the run. SIGKILL, OOM, and host reboot
    remain uncatchable — the startup recovery sweep is their backstop.
    """
    installed: list[int] = []
    for sig in (signal.SIGTERM, signal.SIGINT):
        try:
            loop.add_signal_handler(sig, target.cancel)
        except (NotImplementedError, RuntimeError, ValueError):
            continue
        installed.append(sig)
    return installed


def _remove_cancel_on_signal(
    loop: asyncio.AbstractEventLoop, signals: list[int]
) -> None:
    """Restore default disposition for the signals :func:`_install_cancel_on_signal`
    took over, so they do not leak past this run (e.g. between orchestrator tasks)."""
    for sig in signals:
        try:
            loop.remove_signal_handler(sig)
        except (NotImplementedError, ValueError):
            pass


async def run_task_object(
    task: Task,
    *,
    db_path: Path,
    sandbox: Path,
    model: str | None = None,
    max_turns: int = DEFAULT_MAX_TURNS,
    max_retries: int = DEFAULT_MAX_RETRIES,
    invoke: InvokeFunc | None = None,
    stream: TextIO | None = None,
    run_id: str | None = None,
    events: str = EVENTS_NONE,
    source: str | None = None,
) -> HarnessOutcome:
    """Persist a lifecycle for ``task`` and drive it via ``run_task``.

    The Task-first entry point: callers pass a ``Task`` they built however
    they like (loaded from a file, or constructed inline from a goal), so
    this owns no input-source assumptions. :func:`run_task_file` is the
    thin file-loading wrapper over this.

    ``invoke`` defaults to a real Claude Code invoker. Tests inject a fake
    callable instead — same seam, different transport.

    ``events`` selects the live stdout stream: :data:`EVENTS_NONE` (silent),
    :data:`EVENTS_PLAIN` (readable lines), or :data:`EVENTS_JSON` (NDJSON).
    The stream interleaves harness telemetry events with the agent's own
    turns (assistant text, tool calls, tool results) as they arrive — both
    render on the one event loop, so the on-screen order is the true order.
    When a caller injects its own ``invoke``, only events stream (live agent
    turns come from the default invoker's observer). Diagnostics
    (``[workflow]`` lines) always go to ``stream`` (stderr by default), kept
    separate from the event stream on stdout.

    ``run_id`` selects fresh vs resume: ``None`` (default) starts a new
    lifecycle; passing an existing ``run_id`` makes ``run_task`` resume that
    lifecycle (its seed append hits ``LifecycleAlreadyExistsError`` and the
    harness reconciles from the persisted row).
    """
    out = stream if stream is not None else sys.stderr
    lifecycle = (
        Lifecycle(task_id=task.id)
        if run_id is None
        else Lifecycle(task_id=task.id, run_id=run_id)
    )

    db_path.parent.mkdir(parents=True, exist_ok=True)
    sandbox.mkdir(parents=True, exist_ok=True)

    backend = SqliteStore(db_path)
    emitter = _make_event_emitter(events, out=sys.stdout)
    store: HarnessStore = (
        backend
        if emitter is None
        else _EventStreamingStore(backend, emit=emitter)
    )

    # The default invoker surfaces the agent's turns live via on_message
    # and runs the control-command watcher against the open ClaudeSDKClient
    # session; an injected invoke (tests, alternative agents) owns its own
    # transport and the watcher is its responsibility. ``backend`` is the
    # ControlCommandStore the watcher claims from; ``store`` is the wrapped
    # audit sink so control-plane events flow through the same live-stream
    # path as harness.* events.
    if invoke is not None:
        invoker = invoke
    else:
        invoker = _make_claude_code_invoke(
            sandbox,
            model=model,
            max_turns=max_turns,
            on_message=_make_message_observer(events, out=sys.stdout),
            control_store=backend,
            run_id=lifecycle.run_id,
            audit_store=store,
        )
    try:
        # Recover any prior lifecycle for this task that was killed
        # mid-attempt before we create a new one. Keeps the audit trail
        # honest (no lifecycles stuck in `running` forever) and frees
        # the retry budget — INTERRUPTED is not a retry-source state.
        for stranded_run_id in recover_stranded_lifecycles(
            backend, task_id=task.id
        ):
            print(
                f"[workflow] recovered: stranded run {stranded_run_id} "
                f"-> interrupted",
                file=out,
                flush=True,
            )

        print(
            f"[workflow] task    : {task.id}",
            file=out,
            flush=True,
        )
        if source is not None:
            print(
                f"[workflow] source  : {source}",
                file=out,
                flush=True,
            )
        if not task.graders:
            print(
                "[workflow] graders : none (unverified run — DONE reflects "
                "the agent's own claim)",
                file=out,
                flush=True,
            )
        print(
            f"[workflow] run_id  : {lifecycle.run_id}",
            file=out,
            flush=True,
        )
        # Take over SIGTERM/SIGINT for the duration of the run so an
        # operator-driven shutdown cancels the in-flight task and lands in
        # the finalize path below, instead of terminating the interpreter
        # with the lifecycle stranded in `running`. Cancelling the current
        # task (rather than a child task) keeps run_task running inline, so
        # an externally-cancelled orchestrator still propagates into it.
        loop = asyncio.get_running_loop()
        current_task = asyncio.current_task()
        installed_signals = (
            _install_cancel_on_signal(loop, current_task)
            if current_task is not None
            else []
        )
        try:
            outcome = await run_task(
                task,
                lifecycle,
                store,
                config=HarnessConfig(
                    max_retries=max_retries,
                    agent_context={
                        "model_id": model or "claude-code-default",
                        "agent_sdk": "claude_agent_sdk",
                        "sandbox": str(sandbox),
                    },
                    worktree=sandbox,
                ),
                invoke=invoker,
            )
        except (asyncio.CancelledError, KeyboardInterrupt):
            # Operator killed the worker mid-attempt (SIGTERM/SIGINT routed
            # here by the handler above, or an external task cancellation).
            # Finalize the open attempt as INTERNAL_ERROR and transition the
            # lifecycle to INTERRUPTED so the next worker start sees a clean
            # slate rather than a lifecycle wedged in `running`.
            finalize_stranded_lifecycle(backend, lifecycle.run_id)
            print(
                f"[workflow] status  : interrupted (worker received signal)",
                file=out,
                flush=True,
            )
            raise
        finally:
            _remove_cancel_on_signal(loop, installed_signals)
        print(
            f"[workflow] status  : {outcome.lifecycle.status.value}",
            file=out,
            flush=True,
        )
        if outcome.lifecycle.error:
            print(
                f"[workflow] error   : {outcome.lifecycle.error}",
                file=out,
                flush=True,
            )
    finally:
        backend.close()
    return outcome


async def run_task_file(
    task_file: Path,
    *,
    db_path: Path,
    sandbox: Path,
    model: str | None = None,
    max_turns: int = DEFAULT_MAX_TURNS,
    max_retries: int = DEFAULT_MAX_RETRIES,
    invoke: InvokeFunc | None = None,
    stream: TextIO | None = None,
    run_id: str | None = None,
    events: str = EVENTS_NONE,
) -> HarnessOutcome:
    """Load ``task_file`` and drive it via :func:`run_task_object`.

    Thin convenience over the Task-first :func:`run_task_object`: all
    execution behavior (lifecycle persistence, stranded recovery, event
    streaming, signal handling) lives there. See that function for the
    ``events`` and ``run_id`` semantics.
    """
    task = load_task_file(task_file)
    return await run_task_object(
        task,
        db_path=db_path,
        sandbox=sandbox,
        model=model,
        max_turns=max_turns,
        max_retries=max_retries,
        invoke=invoke,
        stream=stream,
        run_id=run_id,
        events=events,
        source=str(task_file),
    )


# --- Archive subcommand -----------------------------------------------------


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


# --- Phase base-SHA capture + diff-vs-base ----------------------------------
#
# To compute a phase's cumulative diff at archive-evaluation time the worker
# must record the base SHA at *phase entry* -- by archive time the phase's
# task branches have already been FF-merged into the base, so a "diff vs
# current base" would always be empty. The recorded base lives in a committed
# ``.loop-base`` dotfile inside ``active/<phase>/``:
#
#   * The worker captures it once per phase, the first cycle that processes
#     that phase, right after ``commit_task_files`` commits any new task JSON
#     and before any task branch merges into the base. Re-runs must not move
#     the recorded base forward (the first-seen SHA is the true base).
#   * The file is a dotfile so the existing dot-prefix filters in
#     :func:`iter_active_task_files` and :func:`archive_completed_phases`
#     skip it -- it is never treated as a task.
#   * :func:`phase_diff_vs_base` returns ``git diff <recorded-base> HEAD``
#     as unified-diff text; a phase with no recorded base degrades safely
#     to an empty diff rather than raising, so callers (the loop-path
#     marker, future archive gate) can treat "no base" as "no signal."

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


# --- Loop-path opt-out artifact ---------------------------------------------
#
# A phase that has been auto-flagged as loop-path-bearing (see
# ``flywheel.loop_path_marker``) but whose author can attest the diff added
# no new path downgrades the marker by committing
# ``active/<phase>/loop-path-exempt.md`` with a structured front-matter
# block. The artifact records *who*, *which phase*, and *why no new path*
# so the claim is falsifiable -- ``/audit-phase`` re-derives the diff
# signals and emits a finding when an opt-out covers a diff that did add a
# watched symbol (FR-5, FR-6b of
# ``.workflow/specs/00017-FEATURE-in-loop-verification-gate.md``).
#
# Format is intentionally minimal: a leading ``---`` ... ``---`` block of
# ``key: value`` lines, parsed with stdlib only. Required keys are
# ``phase``, ``author``, ``reason``; a silently-empty opt-out must not
# pass as valid. The artifact lives inside the phase dir so it travels
# into ``archive/`` when the phase is archived -- ``/audit-phase`` can
# re-check the recorded claim against the same diff that motivated it.

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


# --- CLI plumbing -----------------------------------------------------------


def _resolve_tasks_dir(arg: str | None) -> Path:
    return Path(arg) if arg else DEFAULT_TASKS_DIR


def _resolve_db(arg: str | None) -> Path:
    return Path(arg) if arg else DEFAULT_DB_PATH


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


def _resolve_events_mode(args: argparse.Namespace) -> str:
    """Map ``run`` output flags to an events mode (default: readable)."""
    if getattr(args, "quiet", False):
        return EVENTS_NONE
    if getattr(args, "json", False):
        return EVENTS_JSON
    return EVENTS_PLAIN


def _cmd_run(args: argparse.Namespace) -> int:
    db_path = _resolve_db(args.db)
    sandbox = Path(args.sandbox) if args.sandbox else Path.cwd()
    events = _resolve_events_mode(args)
    target = args.target
    inline_graders = bool(args.check) or bool(args.rubric)

    # File vs inline goal: an existing file is loaded as a task (which
    # carries its own graders); anything else is treated as an inline goal.
    if Path(target).is_file():
        if inline_graders:
            print(
                "error: --check/--rubric apply to an inline goal; a task "
                "file declares its own graders",
                file=sys.stderr,
            )
            return 2
        outcome = asyncio.run(
            run_task_file(
                Path(target),
                db_path=db_path,
                sandbox=sandbox,
                model=args.model,
                max_turns=args.max_turns,
                max_retries=args.max_retries,
                events=events,
            )
        )
        return 0 if outcome.lifecycle.status == Status.DONE else 1

    try:
        task = build_inline_task(
            target,
            checks=tuple(args.check or ()),
            rubric_assertions=tuple(args.rubric or ()),
        )
    except ValidationError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    outcome = asyncio.run(
        run_task_object(
            task,
            db_path=db_path,
            sandbox=sandbox,
            model=args.model,
            max_turns=args.max_turns,
            max_retries=args.max_retries,
            events=events,
            source="(inline goal)",
        )
    )
    return 0 if outcome.lifecycle.status == Status.DONE else 1


def _cmd_orchestrate(args: argparse.Namespace) -> int:
    # Lazy import: flywheel.orchestrator imports this module, so importing it
    # at module load would be circular.
    from flywheel.orchestrator import orchestrate

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


# --- Live progress snapshot -------------------------------------------------
#
# `live` answers "what is the agent doing right now, and is it still moving?"
# by joining the in-flight lifecycles to the most recent sdk_message and event
# rows. The output is intentionally one line per run -- it is meant to be
# tail-friendly inside the worker's heartbeat as well as readable on its
# own. Stale-after threshold is heuristic: a healthy turn can take 30-60s on a
# slow read or API call, so we wait past that before flagging.

_LIVE_STALE_AFTER_SECONDS: int = 90

# Hard cap on the rendered "action" detail so a runaway tool-call payload can
# never wrap the live/heartbeat line unboundedly. The per-field summarizers
# (`_summarize_*`) already truncate individual values; this is a belt-and-
# braces ceiling on the assembled string.
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


def _short(value: object, limit: int = 60) -> str:
    text = str(value).replace("\n", " ").replace("\r", " ")
    return text if len(text) <= limit else text[: max(limit - 1, 1)] + "…"


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


def _cmd_is_done(args: argparse.Namespace) -> int:
    task_file = Path(args.task_file)
    task = load_task_file(task_file)
    db_path = _resolve_db(args.db)
    store = SqliteStore(db_path)
    try:
        done = _has_done_lifecycle(store, task.id)
    finally:
        store.close()
    return 0 if done else 1


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


# --- Steering / control commands -----------------------------------------
#
# Producers for the cross-process control plane. Each subcommand enqueues
# exactly one row into ``control_commands`` keyed by ``run_id``; the
# in-process watcher inside the worker's :func:`invoke_iteration_with_client`
# claims and applies it on its next tick. The CLI never talks to the live
# session directly — the store is the channel — so steering works against a
# detached worker daemon. A command is persisted unconditionally; if the
# named run is not currently in-flight the row sits pending and is recorded
# as stale per claim semantics.


def _enqueue_control_command(
    db_path: Path,
    run_id: str,
    kind: str,
    payload: Mapping[str, Any],
) -> int:
    """Persist one control command into ``db_path`` and report the id.

    Returns ``0`` after printing the enqueue receipt (``<id> kind=...``)
    plus, when the lifecycle is not currently in-flight, a stderr note
    explaining the row stays pending per claim semantics. An unknown
    ``run_id`` is a producer-side error: the SQLite backend enforces the
    foreign key on ``lifecycles(run_id)``, so we surface that as exit
    code ``2`` with a clear message rather than crash on the
    :class:`sqlite3.IntegrityError`.

    ``approve`` / ``reject`` are out-of-band verbs that target a
    correctly-parked ``AWAITING_APPROVAL`` lifecycle (the
    ``resolve_manual_approval`` sweep consumes them), so the in-flight
    check accepts ``AWAITING_APPROVAL`` for those verbs and the standard
    ``RUNNING`` / ``VALIDATING`` set for everything else — that way an
    operator who runs ``flywheel approve`` against a parked run does not
    see the stale-pending warning.
    """
    db_path.parent.mkdir(parents=True, exist_ok=True)
    store = SqliteStore(db_path)
    try:
        lifecycle = store.load_lifecycle(run_id)
        if lifecycle is None:
            print(
                f"error: run {run_id!r} is unknown to this store; "
                f"no command enqueued",
                file=sys.stderr,
            )
            return 2
        if kind in ("approve", "reject"):
            in_flight_statuses: tuple[Status, ...] = (Status.AWAITING_APPROVAL,)
        else:
            in_flight_statuses = (Status.RUNNING, Status.VALIDATING)
        in_flight = lifecycle.status in in_flight_statuses
        record = store.enqueue_command(
            run_id, kind, payload, now=datetime.now(timezone.utc)
        )
    finally:
        store.close()
    print(f"enqueued #{record.id} kind={kind} run_id={run_id}")
    if not in_flight:
        status_value = lifecycle.status.value
        print(
            f"  note: run {run_id} is not in-flight (status={status_value}); "
            f"the command stays pending and is recorded as stale per claim "
            f"semantics",
            file=sys.stderr,
        )
    return 0


def _cmd_interrupt(args: argparse.Namespace) -> int:
    """``flywheel interrupt RUN_ID`` — enqueue an interrupt command.

    The watcher's apply drives the lifecycle to INTERRUPTED via the same
    in-band finalization SIGINT/SIGTERM use (the harness's
    ``_run_attempt`` boundary routes through ``_handle_interrupt``);
    additionally a ``harness.control_command_applied`` event records the
    store-triggered origin in the audit stream.
    """
    return _enqueue_control_command(
        _resolve_db(args.db), args.run_id, "interrupt", {}
    )


def _cmd_steer(args: argparse.Namespace) -> int:
    """``flywheel steer RUN_ID MESSAGE`` — inject an operator message.

    The watcher dispatches via :meth:`ClaudeSDKClient.query`, appending
    one user turn to the live conversation. The task definition is not
    mutated — only the running session sees the message.
    """
    text = args.message
    if not text:
        print("error: steer message must be non-empty", file=sys.stderr)
        return 2
    return _enqueue_control_command(
        _resolve_db(args.db), args.run_id, "say", {"text": text}
    )


def _cmd_set_model(args: argparse.Namespace) -> int:
    """``flywheel set-model RUN_ID MODEL`` — switch the live session's model.

    Dispatches via :meth:`ClaudeSDKClient.set_model`. An invalid model id
    surfaces as a ``harness.control_command_failed`` event when the SDK
    rejects it; the run continues on the prior model.
    """
    return _enqueue_control_command(
        _resolve_db(args.db),
        args.run_id,
        "set_model",
        {"model": args.model},
    )


def _cmd_approve(args: argparse.Namespace) -> int:
    """``flywheel approve RUN_ID`` — approve the awaiting manual gate.

    Enqueues a ``kind=approve`` row against ``RUN_ID``. The out-of-band
    ``resolve_manual_approval`` sweep claims it on the next reactive tick,
    writes a ``passed=True`` manual ``GraderResultRecord`` for the
    parked gate, and either re-parks on the next gate or transitions
    ``AWAITING_APPROVAL -> DONE``. The producer accepts
    ``AWAITING_APPROVAL`` as the valid in-flight status for this verb so
    the operator does not see the stale-pending warning when approving a
    correctly-parked run.
    """
    return _enqueue_control_command(
        _resolve_db(args.db), args.run_id, "approve", {}
    )


def _cmd_reject(args: argparse.Namespace) -> int:
    """``flywheel reject RUN_ID [--feedback TEXT]`` — reject the awaiting gate.

    Enqueues a ``kind=reject`` row carrying an optional
    ``{"feedback": TEXT}`` payload. The out-of-band resolver writes a
    ``passed=False`` manual ``GraderResultRecord`` whose summary is the
    feedback text (or a ``"(no feedback provided)"`` placeholder when
    absent), transitions ``AWAITING_APPROVAL -> FAILED_VALIDATION``, and
    surfaces the feedback in the next attempt's reviewer-feedback section.
    The in-flight check accepts ``AWAITING_APPROVAL`` for this verb.
    """
    payload: dict[str, Any] = {}
    if args.feedback is not None:
        payload["feedback"] = args.feedback
    return _enqueue_control_command(
        _resolve_db(args.db), args.run_id, "reject", payload
    )


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


def _add_common_db(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--db",
        default=None,
        help=f"SQLite database path (default: {DEFAULT_DB_PATH}).",
    )


def _add_common_tasks_dir(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--tasks-dir",
        default=None,
        help=f"Tasks root directory (default: {DEFAULT_TASKS_DIR}).",
    )


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="python -m flywheel.workflow",
        description=(
            "Pick, run, and audit flywheel tasks laid out under "
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

    p_run = sub.add_parser(
        "run",
        help=(
            "Run a goal or task file via flywheel.run_task, streaming events "
            "to stdout; exit 0 only on DONE."
        ),
        description=(
            "TARGET is either an inline goal string (e.g. "
            "'add retries to the http client') or a path to a task JSON "
            "file. An inline goal with no --check/--rubric runs unverified: "
            "it records DONE on the agent's own claim. Events stream to "
            "stdout as they fire."
        ),
    )
    p_run.add_argument(
        "target",
        metavar="GOAL_OR_FILE",
        help="Inline goal string, or path to a flywheel task JSON file.",
    )
    _add_common_db(p_run)
    p_run.add_argument(
        "--check",
        action="append",
        default=None,
        metavar="CMD",
        help=(
            "Add a command grader (pass=exit 0) for an inline goal. "
            "Repeatable. Ignored for a task file."
        ),
    )
    p_run.add_argument(
        "--rubric",
        action="append",
        default=None,
        metavar="ASSERTION",
        help=(
            "Add a natural-language rubric assertion for an inline goal "
            "(LLM-judged). Repeatable. Ignored for a task file."
        ),
    )
    p_run.add_argument(
        "--sandbox",
        default=None,
        help="Directory the agent operates in (default: current dir).",
    )
    p_run.add_argument(
        "--model",
        default=None,
        help="Override the Claude model passed to the SDK.",
    )
    p_run.add_argument(
        "--max-turns",
        type=int,
        default=DEFAULT_MAX_TURNS,
        help=f"Max agent turns per iteration (default: {DEFAULT_MAX_TURNS}).",
    )
    p_run.add_argument(
        "--max-retries",
        type=int,
        default=DEFAULT_MAX_RETRIES,
        help=(
            f"Harness retry budget after failed_validation "
            f"(default: {DEFAULT_MAX_RETRIES})."
        ),
    )
    p_run_events = p_run.add_mutually_exclusive_group()
    p_run_events.add_argument(
        "--json",
        action="store_true",
        help="Stream events as NDJSON instead of readable lines.",
    )
    p_run_events.add_argument(
        "--quiet",
        action="store_true",
        help="Suppress the event stream; print only the final status.",
    )
    p_run.set_defaults(func=_cmd_run)

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

    p_is_done = sub.add_parser(
        "is-done",
        help="Exit 0 if the named task has at least one done lifecycle.",
    )
    p_is_done.add_argument("task_file")
    _add_common_db(p_is_done)
    p_is_done.set_defaults(func=_cmd_is_done)

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

    p_interrupt = sub.add_parser(
        "interrupt",
        help=(
            "Enqueue an interrupt control command against RUN_ID. The "
            "in-process watcher claims and applies it on its next tick, "
            "driving the lifecycle to INTERRUPTED via the same finalization "
            "SIGINT/SIGTERM use."
        ),
    )
    p_interrupt.add_argument(
        "run_id",
        metavar="RUN_ID",
        help="Lifecycle run_id of the in-flight run to interrupt.",
    )
    _add_common_db(p_interrupt)
    p_interrupt.set_defaults(func=_cmd_interrupt)

    p_steer = sub.add_parser(
        "steer",
        help=(
            "Inject an operator MESSAGE into the live conversation for "
            "RUN_ID. The watcher dispatches via ClaudeSDKClient.query so "
            "the running session sees a new user turn. The task definition "
            "is not mutated."
        ),
    )
    p_steer.add_argument(
        "run_id",
        metavar="RUN_ID",
        help="Lifecycle run_id of the in-flight run to steer.",
    )
    p_steer.add_argument(
        "message",
        metavar="MESSAGE",
        help="Operator message text to inject as a user turn.",
    )
    _add_common_db(p_steer)
    p_steer.set_defaults(func=_cmd_steer)

    p_set_model = sub.add_parser(
        "set-model",
        help=(
            "Switch the live session for RUN_ID to MODEL. Dispatched via "
            "ClaudeSDKClient.set_model. An invalid model id lands as a "
            "harness.control_command_failed event; the run continues on "
            "the prior model."
        ),
    )
    p_set_model.add_argument(
        "run_id",
        metavar="RUN_ID",
        help="Lifecycle run_id of the in-flight run to retarget.",
    )
    p_set_model.add_argument(
        "model",
        metavar="MODEL",
        help="Model identifier to switch the live session to.",
    )
    _add_common_db(p_set_model)
    p_set_model.set_defaults(func=_cmd_set_model)

    p_approve = sub.add_parser(
        "approve",
        help=(
            "Enqueue an approve control command against RUN_ID. The "
            "out-of-band manual-approval resolver claims it on the next "
            "reactive tick, writes a passing manual grader receipt for the "
            "parked gate, and advances the lifecycle (next gate or DONE)."
        ),
    )
    p_approve.add_argument(
        "run_id",
        metavar="RUN_ID",
        help="Lifecycle run_id of the AWAITING_APPROVAL run to approve.",
    )
    _add_common_db(p_approve)
    p_approve.set_defaults(func=_cmd_approve)

    p_reject = sub.add_parser(
        "reject",
        help=(
            "Enqueue a reject control command against RUN_ID, optionally "
            "carrying operator feedback. The out-of-band resolver writes a "
            "failing manual grader receipt and routes the lifecycle through "
            "FAILED_VALIDATION; --feedback flows into the next attempt's "
            "reviewer-feedback section."
        ),
    )
    p_reject.add_argument(
        "run_id",
        metavar="RUN_ID",
        help="Lifecycle run_id of the AWAITING_APPROVAL run to reject.",
    )
    p_reject.add_argument(
        "--feedback",
        default=None,
        metavar="TEXT",
        help=(
            "Optional operator critique to attach to the rejection. Renders "
            "in the next attempt's # Reviewer feedback section so the agent "
            "can address it. Absent feedback is recorded as "
            '"(no feedback provided)".'
        ),
    )
    _add_common_db(p_reject)
    p_reject.set_defaults(func=_cmd_reject)

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


__all__ = [
    "DEFAULT_DB_PATH",
    "DEFAULT_LOG_DIR",
    "DEFAULT_MAX_RETRIES",
    "DEFAULT_MAX_TURNS",
    "DEFAULT_TASKS_DIR",
    "LOOP_BASE_FILENAME",
    "LOOP_PATH_OPTOUT_FILENAME",
    "LiveRunRow",
    "LoopPathOptOut",
    "LoopPathOptOutError",
    "TaskState",
    "TaskStatusRow",
    "archive_completed_phases",
    "build_inline_task",
    "build_status_rows",
    "collect_live_rows",
    "iter_active_phase_dirs",
    "iter_active_task_files",
    "load_active_tasks",
    "load_loop_path_optout",
    "main",
    "phase_diff_vs_base",
    "read_phase_base",
    "recover_stranded_lifecycles",
    "run_task_file",
    "run_task_object",
    "select_next_task",
    "task_state",
    "write_phase_base_if_missing",
]


if __name__ == "__main__":
    raise SystemExit(main())

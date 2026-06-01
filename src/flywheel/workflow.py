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
from flywheel.lifecycle import Attempt, Lifecycle, Status
from flywheel.loaders import TaskLoadError, load_task_file
from flywheel.store_protocols import (
    EventRecord,
    GraderResultRecord,
    SdkMessageRecord,
)
from flywheel.store_sqlite import SqliteStore
from flywheel.task import (
    CommandGrader,
    Grader,
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
    DONE = "done"  # at least one lifecycle reached DONE


@dataclass(frozen=True, kw_only=True)
class TaskStatusRow:
    """Per-task status snapshot used by ``status`` and ``next`` reporting.

    ``blocked_requires`` carries the raw ``blocked_requires_json`` column
    value from the latest lifecycle verbatim (no parsing). ``None`` means
    either the lifecycle never blocked with structured requires, or the
    lifecycle is in a state where the snapshot was cleared (e.g. resumed
    READY/RUNNING/DONE). The status surface decodes it lazily.
    """

    task_file: Path
    task: Task
    state: TaskState
    latest_run_id: str | None
    latest_status: Status | None
    latest_error: str
    blocked_requires: str | None = None


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
) -> tuple[str, Status, str, str | None] | None:
    """Return ``(run_id, status, error, blocked_requires_json)`` of the
    most recent lifecycle for ``task_id``, or ``None`` if no lifecycle
    exists.

    Uses the SQLite connection directly because no Protocol method exposes
    a by-task-id lookup — and adding one would leak workflow concerns into
    the store contract. ``blocked_requires_json`` is returned verbatim so
    callers can decide whether to parse it (text status flattens, JSON
    status emits the decoded list).
    """
    cursor = store._connection.execute(  # noqa: SLF001 — intentional
        """
        SELECT run_id, status, error, blocked_requires_json
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
        )
    run_id, status, error, blocked_requires = latest

    if _has_done_lifecycle(store, task.id):
        state = TaskState.DONE
    elif status in _ACTIVE_STATUSES:
        state = TaskState.IN_PROGRESS
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


def _make_claude_code_invoke(
    sandbox: Path,
    *,
    model: str | None,
    max_turns: int,
    on_message: Callable[[Message], None] | None = None,
) -> InvokeFunc:
    """Production invoker: real Claude Code spawned in ``sandbox``.

    Mirrors :func:`flywheel.examples.hello.example.make_claude_code_invoke`
    but with the broader tool surface a real engineering task needs.

    ``on_message`` is forwarded to :func:`invoke_iteration` so the agent's
    turns surface live as they arrive — the workflow CLI passes a renderer
    here to stream them to stdout interleaved with harness events.
    """
    options = ClaudeAgentOptions(
        cwd=str(sandbox),
        add_dirs=[str(sandbox)],
        permission_mode="bypassPermissions",
        skills="all",
        max_turns=max_turns,
        model=model,
    )

    async def _invoke(request: InvocationRequest) -> IterationResult:
        return await invoke_iteration(
            prompt=request.prompt, options=options, on_message=on_message
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

    # The default invoker surfaces the agent's turns live via on_message;
    # an injected invoke (tests, alternative agents) owns its own transport.
    if invoke is not None:
        invoker = invoke
    else:
        invoker = _make_claude_code_invoke(
            sandbox,
            model=model,
            max_turns=max_turns,
            on_message=_make_message_observer(events, out=sys.stdout),
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


def archive_completed_phases(tasks_dir: Path, store: SqliteStore) -> list[Path]:
    """Move ``active/<phase>`` dirs to ``archive/`` when every task is done.

    Returns the list of moved phase directories (post-move paths). Idempotent:
    safe to call repeatedly. Phases with any non-done task are left in place.
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
        if not all(
            _has_done_lifecycle(store, load_task_file(p).id) for p in task_files
        ):
            continue
        dest = archive_root / phase_dir.name
        # If a same-named archive exists, leave the active dir alone rather
        # than clobber prior history — operator can resolve manually.
        if dest.exists():
            continue
        shutil.move(str(phase_dir), str(dest))
        moved.append(dest)
    return moved


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


@dataclass(frozen=True, kw_only=True)
class LiveRunRow:
    """Per-in-flight-run snapshot used by ``live`` reporting."""

    run_id: str
    task_id: str
    status: Status
    iteration: int | None
    last_kind: str
    last_detail: str
    last_ts: datetime | None


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


def collect_live_rows(store: SqliteStore) -> list[LiveRunRow]:
    """Snapshot every in-flight run with its latest activity, newest last.

    Reads ``lifecycles`` for status in ``{running, validating}`` and joins
    each row to the freshest ``sdk_messages``/``events`` entry by
    per-run sequence. The two tables share a monotonic counter (see
    ``run_sequence``), so picking the higher sequence is correct even
    when the timestamps are within microseconds of each other.
    """
    conn = store._connection  # noqa: SLF001
    lifecycles = conn.execute(
        """
        SELECT run_id, task_id, status
        FROM lifecycles
        WHERE status IN ('running', 'validating')
        ORDER BY updated_at
        """
    ).fetchall()
    rows: list[LiveRunRow] = []
    for lc in lifecycles:
        run_id = lc["run_id"]
        sdk = conn.execute(
            """
            SELECT iteration_number, sequence, message_type, payload_json, ts
            FROM sdk_messages
            WHERE run_id = ?
            ORDER BY sequence DESC
            LIMIT 1
            """,
            (run_id,),
        ).fetchone()
        evt = conn.execute(
            """
            SELECT attempt_number, sequence, kind, ts
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
        last_kind = "(none)"
        last_detail = "(no activity yet)"
        last_ts: datetime | None = None
        if sdk_seq >= 0 and sdk_seq >= evt_seq and sdk is not None:
            iteration = sdk["iteration_number"]
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
            last_kind = "EVENT"
            last_detail = str(evt["kind"])
            last_ts = _parse_db_ts(evt["ts"])
        rows.append(
            LiveRunRow(
                run_id=run_id,
                task_id=lc["task_id"],
                status=Status(lc["status"]),
                iteration=iteration,
                last_kind=last_kind,
                last_detail=last_detail,
                last_ts=last_ts,
            )
        )
    return rows


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
    iter_str = (
        f"iter={row.iteration}" if row.iteration is not None else "iter=?"
    )
    return (
        f"{row.task_id}  {row.status.value}  {iter_str}  "
        f"age={age_str}  {row.last_kind}  {row.last_detail}{stale}"
    )


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
            f"{row.state.value:<12}{suffix}"
        )
        if (
            row.latest_status == Status.INTERRUPTED
            and row.blocked_requires is not None
        ):
            parsed = _parse_blocked_requires(row.blocked_requires)
            if parsed:
                print(f"    blocked_on: {_format_blocked_on(parsed)}")
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
    "LiveRunRow",
    "TaskState",
    "TaskStatusRow",
    "archive_completed_phases",
    "build_inline_task",
    "build_status_rows",
    "collect_live_rows",
    "iter_active_phase_dirs",
    "iter_active_task_files",
    "load_active_tasks",
    "main",
    "recover_stranded_lifecycles",
    "run_task_file",
    "run_task_object",
    "select_next_task",
    "task_state",
]


if __name__ == "__main__":
    raise SystemExit(main())

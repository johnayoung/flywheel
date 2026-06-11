"""Single-task CLI — run and steer one flywheel task against a real agent.

This is the core single-task entry point: it owns no execution logic of its
own — running a task delegates to :func:`flywheel_core.harness.run_task` with the
production Claude Code invoker, and steering enqueues control commands. It
covers exactly one task at a time. Multi-task scheduling (selection over a
prerequisite DAG, phases, the live dashboard, stranded recovery) lives in
the ``flywheel-orchestrator`` package, which is built on top of this loop.

Core no longer ships a ``flywheel`` console script (that name belongs to the
product shell, which forwards operator verbs back here in-process). Invoke
the core verbs as ``python -m flywheel_core.workflow`` -- the surface below is
unchanged.

Subcommands::

    python -m flywheel_core.workflow run GOAL_OR_FILE [--check CMD] [--rubric A]
        [--db PATH] [--sandbox DIR] [--model MODEL]
        [--max-retries N] [--max-turns N] [--json | --quiet]
    python -m flywheel_core.workflow is-done TASK_FILE [--db PATH]
    python -m flywheel_core.workflow interrupt RUN_ID [--db PATH]
    python -m flywheel_core.workflow steer RUN_ID MESSAGE [--db PATH]
    python -m flywheel_core.workflow set-model RUN_ID MODEL [--db PATH]
    python -m flywheel_core.workflow approve RUN_ID [--db PATH]
    python -m flywheel_core.workflow reject RUN_ID [--feedback TEXT] [--db PATH]

``run`` accepts either an inline goal string or a task-file path; an inline
goal with no ``--check``/``--rubric`` is an unverified run (DONE reflects the
agent's own claim). Events stream to stdout as they fire (readable by
default, NDJSON with ``--json``).
"""

from __future__ import annotations

import argparse
import asyncio
import json
import signal
import subprocess
import sys
from collections.abc import (
    Callable,
    Iterable,
    Mapping,
    Sequence,
)
from datetime import datetime, timezone
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

from flywheel_core.harness import (
    HarnessConfig,
    HarnessOutcome,
    InvocationRequest,
    InvokeFunc,
    finalize_stranded_lifecycle,
    run_task,
)
from flywheel_core.invoker import (
    IterationResult,
    _serialize_sdk_message,
    invoke_iteration,
)
from flywheel_core.invoker_client import invoke_iteration_with_client
from flywheel_core.lifecycle import Lifecycle, Status
from flywheel_core.loaders import TaskLoadError, load_task_file
from flywheel_core.store_protocols import (
    ControlCommandStore,
    TelemetryRecord,
    TelemetrySink,
)
from flywheel_core.store_sqlite import SqliteStore
from flywheel_core.telemetry_file import FileTelemetrySink
from flywheel_core.task import (
    CommandGrader,
    Grader,
    RubricGrader,
    Task,
    ValidationError,
)


DEFAULT_DB_PATH = Path(".flywheel/flywheel.sqlite")
DEFAULT_MAX_TURNS = 500
DEFAULT_MAX_RETRIES = 1


# --- Status queries ---------------------------------------------------------


def _has_done_lifecycle(store: SqliteStore, task_id: str) -> bool:
    cursor = store._connection.execute(  # noqa: SLF001
        "SELECT 1 FROM lifecycles WHERE task_id = ? AND status = ? LIMIT 1",
        (task_id, Status.DONE.value),
    )
    return cursor.fetchone() is not None


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
    store: SqliteStore,
    *,
    task_id: str | None = None,
    sink: TelemetrySink | None = None,
) -> list[str]:
    """Finalize every stranded lifecycle, optionally filtered by ``task_id``.

    Delegates to :func:`flywheel_core.harness.finalize_stranded_lifecycle` for
    each match; returns the run_ids that were actually finalized (i.e.
    were in ``running``/``validating`` at the time of the call).
    """
    finalized: list[str] = []
    for run_id in _stranded_run_ids(store, task_id):
        if finalize_stranded_lifecycle(store, run_id, sink=sink):
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


def _format_event_line(record: TelemetryRecord) -> str:
    """One readable line per telemetry record for the default ``run`` stream."""
    attempt = (
        f" attempt={record.attempt_number}"
        if record.attempt_number is not None
        else ""
    )
    ts = record.ts.strftime("%H:%M:%S")
    summary = _event_payload_summary(dict(record.payload))
    tail = f"  {summary}" if summary else ""
    return f"[{ts}] {record.kind}{attempt}{tail}"


def _event_json_line(record: TelemetryRecord) -> str:
    """One NDJSON line per telemetry record for ``--json`` consumers."""
    return json.dumps(
        {
            "run_id": record.run_id,
            "ts": record.ts.isoformat(),
            "kind": record.kind,
            "attempt_number": record.attempt_number,
            "iteration_number": record.iteration_number,
            "payload": dict(record.payload),
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


class _StreamingTelemetrySink:
    """Wrap a :class:`TelemetrySink` and print harness telemetry live.

    Every record is appended to ``wrapped`` first (the durable per-run
    JSONL file), then ``harness.*`` records are additionally handed to
    ``emit`` so the run is observable live on stdout while the agent
    works. SDK message records are skipped here — the live message
    observer (:func:`_make_message_observer`) already renders the
    agent's turns with the typed objects in hand — and ``domain.*``
    mirror lines are skipped to keep the stream's shape unchanged from
    the store-backed era (lifecycle changes still surface via their
    ``harness.*`` companions).
    """

    def __init__(
        self,
        wrapped: TelemetrySink,
        *,
        emit: Callable[[TelemetryRecord], None],
    ) -> None:
        self._wrapped = wrapped
        self._emit = emit

    def append_telemetry(self, record: TelemetryRecord) -> None:
        self._wrapped.append_telemetry(record)
        if record.kind.startswith("harness."):
            self._emit(record)


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
    telemetry_sink: TelemetrySink | None = None,
) -> InvokeFunc:
    """Production invoker: real Claude Code spawned in ``sandbox``.

    Drives one iteration through :class:`claude_agent_sdk.ClaudeSDKClient`
    so the in-process watcher coroutine, running concurrently with the
    agent's message stream, can claim operator-issued control commands
    from ``control_store`` (interrupt / set_model / say) and apply them
    live against the open session. Each applied command lands as a
    ``harness.control_command_applied`` record on ``telemetry_sink``; a
    failed dispatch records ``harness.control_command_failed`` and the
    iteration continues, mirroring the per-message persistence contract.

    ``control_store``, ``run_id``, and ``telemetry_sink`` are required
    for the bidirectional path — when ``control_store`` is ``None`` the
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
    pinned_sink = telemetry_sink

    async def _invoke(request: InvocationRequest) -> IterationResult:
        composed = _compose_message_observers(on_message, request.on_message)
        emit: Callable[[str, Mapping[str, Any]], None] | None = None
        if pinned_sink is not None:
            # Bind a non-Optional reference so the inner closure does not
            # need to re-prove the None-check on every call.
            control_sink: TelemetrySink = pinned_sink
            attempt_number = request.attempt_number

            def _audit_emit(kind: str, payload: Mapping[str, Any]) -> None:
                # Route control-plane events through the same sink the
                # harness streams to so the run file carries them and a
                # live operator sees them on stdout exactly like
                # harness.* records (the watcher wraps this in
                # _emit_safe, so a raising sink never breaks the run).
                control_sink.append_telemetry(
                    TelemetryRecord(
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
            recovery_interrupt_event=request.recovery_interrupt_event,
            on_applied=request.on_command_applied,
        )

    return _invoke


def _resolve_base_commit_sha(sandbox: Path) -> str | None:
    """Resolve the workspace's base commit SHA (spec 00025 FR-11).

    Called at run start against the freshly prepared workspace, so
    ``HEAD`` here is the commit the worktree was created from (the
    consumer's ``prepare_sandbox`` hands the harness a clean checkout
    immediately before this runs). A sandbox that is not a git checkout
    — direct flywheel-core invocations without a worktree — yields
    ``None`` and the pin is omitted cleanly rather than fabricated.
    """
    try:
        result = subprocess.run(
            ["git", "-C", str(sandbox), "rev-parse", "HEAD"],
            capture_output=True,
            text=True,
            timeout=10,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if result.returncode != 0:
        return None
    sha = result.stdout.strip()
    return sha if len(sha) == 40 else None


def _make_event_emitter(
    events: str, *, out: TextIO
) -> Callable[[TelemetryRecord], None] | None:
    """Build the per-record stdout printer for ``run_task_object``.

    Returns ``None`` for :data:`EVENTS_NONE` (no wrapping); a readable
    line-formatter for :data:`EVENTS_PLAIN`; an NDJSON formatter for
    :data:`EVENTS_JSON`. Records go to ``out`` (stdout) so they stay
    separable from the ``[workflow]`` diagnostics on stderr.
    """
    if events == EVENTS_NONE:
        return None
    if events == EVENTS_JSON:

        def emit_json(record: TelemetryRecord) -> None:
            print(_event_json_line(record), file=out, flush=True)

        return emit_json

    def emit_plain(record: TelemetryRecord) -> None:
        print(_format_event_line(record), file=out, flush=True)

    return emit_plain


def _install_cancel_on_signal(
    loop: asyncio.AbstractEventLoop, target: asyncio.Task[Any]
) -> list[int]:
    """Route SIGTERM/SIGINT through ``target.cancel()`` for the run's duration.

    Production shutdown — ``docker stop``, ``kubectl delete pod``,
    ``systemctl stop`` — sends SIGTERM, whose default disposition terminates
    the interpreter *without raising*, so the in-flight :func:`run_task`
    never reaches its finalizer and its lifecycle is stranded in ``running``
    (see ``.flywheel/audits/02-harness-resilience.md``). Cancelling the
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
    sink: TelemetrySink | None = None,
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

    ``sink`` overrides the run's telemetry destination (tests inject a
    fake); ``None`` builds a :class:`FileTelemetrySink` rooted next to
    the database (``<db dir>/logs`` — the spec 00025 default of
    ``.flywheel/logs`` for the default db path), writing one JSONL file
    per run under ``logs/runs/``.
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
    owned_sink: FileTelemetrySink | None = None
    if sink is None:
        owned_sink = FileTelemetrySink(db_path.parent / "logs")
        sink = owned_sink
    emitter = _make_event_emitter(events, out=sys.stdout)
    run_sink: TelemetrySink = (
        sink
        if emitter is None
        else _StreamingTelemetrySink(sink, emit=emitter)
    )

    # The default invoker surfaces the agent's turns live via on_message
    # and runs the control-command watcher against the open ClaudeSDKClient
    # session; an injected invoke (tests, alternative agents) owns its own
    # transport and the watcher is its responsibility. ``backend`` is the
    # ControlCommandStore the watcher claims from; ``run_sink`` is the
    # telemetry destination so control-plane records flow through the same
    # live-stream path as harness.* records.
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
            telemetry_sink=run_sink,
        )
    try:
        # Recover any prior lifecycle for this task that was killed
        # mid-attempt before we create a new one. Keeps the audit trail
        # honest (no lifecycles stuck in `running` forever) and frees
        # the retry budget — INTERRUPTED is not a retry-source state.
        for stranded_run_id in recover_stranded_lifecycles(
            backend, task_id=task.id, sink=run_sink
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
        # World-state pin (spec 00025 FR-11): every attempt's
        # agent_context records the effective model id the SDK is
        # invoked with (post --model / policy / default resolution;
        # "claude-code-default" is the documented stand-in when the SDK
        # falls through to the Claude Code default) and, when the
        # sandbox is a git checkout, the resolved base commit SHA the
        # workspace was created from. agent_context rides the
        # AttemptStarted payload, so both values survive replay.
        agent_context = {
            "model_id": model or "claude-code-default",
            "agent_sdk": "claude_agent_sdk",
            "sandbox": str(sandbox),
        }
        base_sha = _resolve_base_commit_sha(sandbox)
        if base_sha is not None:
            agent_context["base_commit_sha"] = base_sha
        try:
            outcome = await run_task(
                task,
                lifecycle,
                backend,
                config=HarnessConfig(
                    max_retries=max_retries,
                    agent_context=agent_context,
                    worktree=sandbox,
                ),
                invoke=invoker,
                sink=run_sink,
            )
        except (asyncio.CancelledError, KeyboardInterrupt):
            # Operator killed the worker mid-attempt (SIGTERM/SIGINT routed
            # here by the handler above, or an external task cancellation).
            # Finalize the open attempt as INTERNAL_ERROR and transition the
            # lifecycle to INTERRUPTED so the next worker start sees a clean
            # slate rather than a lifecycle wedged in `running`.
            finalize_stranded_lifecycle(
                backend, lifecycle.run_id, sink=run_sink
            )
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
        if owned_sink is not None:
            owned_sink.close()
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


#
# A phase that has been auto-flagged as loop-path-bearing (see
# ``flywheel_core.loop_path_marker``) but whose author can attest the diff added
# no new path downgrades the marker by committing
# ``active/<phase>/loop-path-exempt.md`` with a structured front-matter
# block. The artifact records *who*, *which phase*, and *why no new path*
# so the claim is falsifiable -- ``/audit-phase`` re-derives the diff
# signals and emits a finding when an opt-out covers a diff that did add a
# watched symbol (FR-5, FR-6b of
# ``.flywheel/specs/00017-FEATURE-in-loop-verification-gate.md``).
#
# Format is intentionally minimal: a leading ``---`` ... ``---`` block of
# ``key: value`` lines, parsed with stdlib only. Required keys are
# ``phase``, ``author``, ``reason``; a silently-empty opt-out must not
# pass as valid. The artifact lives inside the phase dir so it travels
# into ``archive/`` when the phase is archived -- ``/audit-phase`` can
# re-check the recorded claim against the same diff that motivated it.


# --- CLI plumbing -----------------------------------------------------------


def _resolve_db(arg: str | None) -> Path:
    return Path(arg) if arg else DEFAULT_DB_PATH


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


# --- Shared helpers + is-done -----------------------------------------------


# Hard cap on the rendered "action" detail so a runaway tool-call payload can
# never wrap the live/heartbeat line unboundedly. The per-field summarizers
# (`_summarize_*`) already truncate individual values; this is a belt-and-
# braces ceiling on the assembled string.


def _short(value: object, limit: int = 60) -> str:
    text = str(value).replace("\n", " ").replace("\r", " ")
    return text if len(text) <= limit else text[: max(limit - 1, 1)] + "…"


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


def _add_common_db(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--db",
        default=None,
        help=f"SQLite database path (default: {DEFAULT_DB_PATH}).",
    )


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="python -m flywheel_core.workflow",
        description=(
            "Run and steer a single flywheel task. Multi-task scheduling "
            "lives in the flywheel-orchestrator package, exposed via the "
            "flywheel/fw product shell."
        ),
    )
    sub = parser.add_subparsers(dest="command", required=True)

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

    p_is_done = sub.add_parser(
        "is-done",
        help="Exit 0 if the named task has at least one done lifecycle.",
    )
    p_is_done.add_argument("task_file")
    _add_common_db(p_is_done)
    p_is_done.set_defaults(func=_cmd_is_done)

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
    "DEFAULT_MAX_RETRIES",
    "DEFAULT_MAX_TURNS",
    "build_inline_task",
    "main",
    "recover_stranded_lifecycles",
    "run_task_file",
    "run_task_object",
]


if __name__ == "__main__":
    raise SystemExit(main())

"""Hello: drive one trivial task to ``done`` with a real Claude Code agent.

This module exercises the harness's two plugin seams in their production
configuration:

* ``invoke`` — the default :func:`flywheel_core.invoker.invoke_iteration`, which
  drives the Claude Code CLI through :func:`claude_agent_sdk.query`. The same
  callable seam that unit tests use to inject fakes is used here to inject a
  :class:`claude_agent_sdk.ClaudeAgentOptions` tuned for this run (sandbox
  ``cwd``, restricted toolset, bypass-permissions, ``max_turns`` ceiling).
* ``store`` — :class:`flywheel_core.store_sqlite.SqliteStore`, a real persistence
  backend. After the run we re-query every table and print what got written
  so the audit trail the harness emitted is visible end-to-end.

The agent's task is concrete and verifiable: create ``hello.txt`` in the
sandbox directory containing a known exact string, then signal
``intent=verify``. The :class:`CommandGrader` re-reads the file with
``grep -Fxq`` and the run reaches ``done`` only if that grader returns
exit ``0``.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import shlex
import sys
from collections.abc import Iterable
from pathlib import Path
from typing import Any, TextIO


from claude_agent_sdk import ClaudeAgentOptions

from flywheel_core import (
    CommandGrader,
    HarnessConfig,
    HarnessOutcome,
    InvocationRequest,
    InvokeFunc,
    IterationResult,
    Lifecycle,
    SqliteStore,
    Status,
    Task,
    TranscriptGrader,
    run_task,
)
from flywheel_core.invoker import invoke_iteration
from flywheel_core.store_protocols import TelemetryRecord
from flywheel_core.telemetry_file import FileTelemetrySink


TARGET_FILENAME = "hello.txt"
TARGET_CONTENT = "hello from flywheel"

DEFAULT_MAX_TURNS = 6
DEFAULT_MAX_WALL_SECONDS = 120.0

# Default artifact location: <hello pkg>/runs/. Reused across invocations
# so the audit trail accumulates in one place. The whole directory is
# gitignored via .gitignore next to this module.
_PACKAGE_DIR: Path = Path(__file__).resolve().parent
_DEFAULT_RUNS_DIR: Path = _PACKAGE_DIR / "runs"
DEFAULT_DB_PATH: Path = _DEFAULT_RUNS_DIR / "hello.sqlite"
DEFAULT_SANDBOX: Path = _DEFAULT_RUNS_DIR / "sandbox"


def build_task(sandbox: Path) -> Task:
    """Build the hello task pinned to ``sandbox``.

    ``CommandGrader.run`` is a real shell command — no mock, no stub:
    ``grep -Fxq`` exits ``0`` only when the file contains exactly
    ``TARGET_CONTENT`` on a single line.
    """
    target = sandbox / TARGET_FILENAME
    quoted_target = shlex.quote(str(target))
    quoted_content = shlex.quote(TARGET_CONTENT)
    grader_cmd = (
        f"test -f {quoted_target} "
        f"&& grep -Fxq -- {quoted_content} {quoted_target}"
    )
    goal = (
        f"Create a file at exactly this absolute path:\n"
        f"  {target}\n\n"
        f"The file must contain exactly one line with this exact content "
        f"(no surrounding whitespace, no trailing characters beyond a "
        f"single newline):\n"
        f"  {TARGET_CONTENT}\n\n"
        f"Use the Write tool. Do not run any other commands. Once the "
        f"file is written, end your turn with the iteration envelope "
        f"using `intent=verify` so the harness can run the command "
        f"grader."
    )
    return Task(
        id="hello-example",
        goal=goal,
        graders=[
            CommandGrader(run=grader_cmd, name="hello-file-exact-match"),
            TranscriptGrader(
                max_turns=DEFAULT_MAX_TURNS,
                max_wall_seconds=DEFAULT_MAX_WALL_SECONDS,
                name="hello-budget",
            ),
        ],
    )


def make_claude_code_invoke(
    sandbox: Path,
    *,
    model: str | None = None,
    max_turns: int = DEFAULT_MAX_TURNS,
) -> InvokeFunc:
    """Return an :class:`InvokeFunc` that drives Claude Code in ``sandbox``.

    The returned callable is the production agent invoker — it spawns a
    real Claude Code subprocess via :func:`claude_agent_sdk.query` (the
    transport :func:`flywheel_core.invoker.invoke_iteration` wraps) and passes
    a :class:`ClaudeAgentOptions` whose ``cwd``, ``allowed_tools``,
    ``permission_mode``, and ``max_turns`` constrain the run to this
    sandbox.

    The harness treats this callable identically to any other
    ``InvokeFunc``; swapping in a different agent is one line at the
    call site.
    """
    options = ClaudeAgentOptions(
        cwd=str(sandbox),
        add_dirs=[str(sandbox)],
        allowed_tools=["Write", "Read"],
        permission_mode="bypassPermissions",
        max_turns=max_turns,
        model=model,
    )

    async def _invoke(request: InvocationRequest) -> IterationResult:
        return await invoke_iteration(prompt=request.prompt, options=options)

    return _invoke


class _PrintingSink:
    """Append every telemetry record to the wrapped file sink and emit it
    to stdout as one JSON line.

    Surfacing the run's telemetry stream live keeps the run observable
    while the agent is talking. The final ``dump_store_state`` snapshot
    then shows the relational rows in SQLite plus the same stream as it
    lives in the per-run JSONL file after the run finishes (spec 00025:
    telemetry is a file concern, not a store concern).
    """

    def __init__(self, wrapped: FileTelemetrySink) -> None:
        self._wrapped = wrapped

    def append_telemetry(self, record: TelemetryRecord) -> None:
        self._wrapped.append_telemetry(record)
        line = json.dumps(
            {
                "run_id": record.run_id,
                "ts": record.ts.isoformat(),
                "kind": record.kind,
                "attempt_number": record.attempt_number,
                "payload": dict(record.payload),
            },
            sort_keys=True,
        )
        print(f"event {line}", flush=True)


def _print_section(title: str) -> None:
    print()
    print(f"=== {title} ===")


def _compact_json(value: Any) -> str:
    return json.dumps(value, sort_keys=True, default=str)


def _truncate(text: str, limit: int = 240) -> str:
    if len(text) <= limit:
        return text
    return text[: limit - 1].rstrip() + "..."


def dump_store_state(
    store: SqliteStore,
    run_id: str,
    *,
    out: TextIO | None = None,
    logs_root: Path | None = None,
) -> None:
    """Print every row associated with ``run_id`` to ``out``.

    Covers the relational tables the harness writes during a run
    (``lifecycles``, ``attempts``, ``grader_results``) plus the run's
    telemetry stream, which lives in ``<logs_root>/runs/<run_id>.jsonl``
    rather than the database (spec 00025). Each section prints the
    fields most useful for understanding what happened, in row / line
    order.
    """
    stream = out if out is not None else sys.stdout

    def emit(line: str = "") -> None:
        stream.write(line + "\n")

    lifecycle = store.load_lifecycle(run_id)
    if lifecycle is None:
        emit(f"(no lifecycle row found for run_id={run_id!r})")
        return

    emit()
    emit(f"=== lifecycle  run_id={run_id} ===")
    emit(f"  task_id      : {lifecycle.task_id}")
    emit(f"  status       : {lifecycle.status.value}")
    emit(f"  version      : {lifecycle.version}")
    emit(f"  retries      : {lifecycle.retries}")
    emit(f"  worker_id    : {lifecycle.worker_id!r}")
    emit(f"  session_id   : {lifecycle.session_id!r}")
    emit(f"  error        : {lifecycle.error!r}")
    emit(f"  agent_output : {_truncate(lifecycle.agent_output)!r}")
    emit("  timestamps   :")
    for status, ts in lifecycle.timestamps.items():
        emit(f"    {status.value:>20s} -> {ts.isoformat()}")

    attempts = store.list_attempts(run_id)
    emit()
    emit(f"=== attempts ({len(attempts)}) ===")
    for attempt in attempts:
        outcome = attempt.outcome.value if attempt.outcome else None
        ended = attempt.ended_at.isoformat() if attempt.ended_at else None
        emit(f"  attempt #{attempt.number}")
        emit(f"    started_at    : {attempt.started_at.isoformat()}")
        emit(f"    ended_at      : {ended}")
        emit(f"    outcome       : {outcome}")
        emit(f"    error         : {attempt.error!r}")
        emit(f"    agent_output  : {_truncate(attempt.agent_output)!r}")
        emit(f"    agent_context : {_compact_json(dict(attempt.agent_context))}")

        grader_rows = store.list_grader_results(run_id, attempt.number)
        emit(f"    grader_results ({len(grader_rows)}):")
        for row in grader_rows:
            verdict = "PASS" if row.passed else "FAIL"
            name = row.grader_name or "(unnamed)"
            emit(
                f"      [{row.ordinal}] {row.grader_type:<10} "
                f"{name:<28} {verdict} ({row.duration_ms} ms)"
            )
            payload_snippet = _truncate(
                _compact_json(dict(row.payload)), limit=360
            )
            emit(f"          payload: {payload_snippet}")

    lines: list[dict[str, Any]] = []
    if logs_root is not None:
        run_file = Path(logs_root) / "runs" / f"{run_id}.jsonl"
        if run_file.exists():
            for raw in run_file.read_text(encoding="utf-8").splitlines():
                if raw.strip():
                    lines.append(json.loads(raw))
    emit()
    emit(f"=== events ({len(lines)}) ===")
    for index, line in enumerate(lines):
        attempt_n = (
            line["attempt_number"]
            if line.get("attempt_number") is not None
            else "-"
        )
        payload_snippet = _truncate(
            _compact_json(line.get("payload", {})), limit=200
        )
        emit(
            f"  [{index:>4}] {line.get('ts', '?')}  "
            f"attempt={attempt_n}  {line.get('kind', '?')}"
        )
        emit(f"        {payload_snippet}")


async def run_hello_example(
    *,
    db_path: str | os.PathLike[str],
    sandbox: str | os.PathLike[str],
    model: str | None = None,
    max_turns: int = DEFAULT_MAX_TURNS,
    invoke: InvokeFunc | None = None,
) -> HarnessOutcome:
    """Run the hello example end-to-end and return the harness outcome.

    ``invoke`` defaults to a real Claude Code invoker built by
    :func:`make_claude_code_invoke`. Tests inject a fake here — the same
    seam the production agent uses, just pointed at a different
    callable.
    """
    sandbox_path = Path(sandbox)
    sandbox_path.mkdir(parents=True, exist_ok=True)

    task = build_task(sandbox_path)
    lifecycle = Lifecycle(task_id=task.id)

    invoker = invoke or make_claude_code_invoke(
        sandbox_path, model=model, max_turns=max_turns
    )

    resolved_db = Path(os.fspath(db_path))
    logs_root = resolved_db.parent / "logs"
    backend = SqliteStore(resolved_db)
    file_sink = FileTelemetrySink(logs_root)
    try:
        sink = _PrintingSink(file_sink)
        outcome = await run_task(
            task,
            lifecycle,
            backend,
            config=HarnessConfig(
                agent_context={
                    "model_id": model or "claude-code-default",
                    "agent_sdk": "claude_agent_sdk",
                    "sandbox": str(sandbox_path),
                    "prompt_template_hash": "hello-v2",
                },
            ),
            invoke=invoker,
            sink=sink,
        )
        _print_section(f"run finished status={outcome.lifecycle.status.value}")
        dump_store_state(
            backend, outcome.lifecycle.run_id, logs_root=logs_root
        )
    finally:
        backend.close()
        file_sink.close()
    return outcome


def _resolve_db_path(arg: str | None) -> Path:
    if arg:
        return Path(arg)
    DEFAULT_DB_PATH.parent.mkdir(parents=True, exist_ok=True)
    return DEFAULT_DB_PATH


def _resolve_sandbox(arg: str | None) -> Path:
    if arg:
        return Path(arg)
    DEFAULT_SANDBOX.mkdir(parents=True, exist_ok=True)
    return DEFAULT_SANDBOX


def main(argv: Iterable[str] | None = None) -> int:
    """Console entry point. Returns ``0`` when lifecycle reaches ``done``."""
    parser = argparse.ArgumentParser(
        prog="python -m flywheel_core.examples.hello",
        description=(
            "Drive one trivial task to `done` with a real Claude Code "
            "agent and a real SQLite store, then dump everything that "
            "got written."
        ),
    )
    parser.add_argument(
        "--db",
        default=None,
        help=(
            f"SQLite database path. Default: {DEFAULT_DB_PATH} "
            f"(reused across runs; each run gets its own run_id)."
        ),
    )
    parser.add_argument(
        "--sandbox",
        default=None,
        help=(
            f"Directory the agent operates in. Default: {DEFAULT_SANDBOX} "
            f"(reused across runs)."
        ),
    )
    parser.add_argument(
        "--model",
        default=None,
        help="Override the Claude model (e.g. claude-haiku-4-5).",
    )
    parser.add_argument(
        "--max-turns",
        type=int,
        default=DEFAULT_MAX_TURNS,
        help="Max agent turns before the SDK terminates the iteration.",
    )
    args = parser.parse_args(list(argv) if argv is not None else None)

    db_path = _resolve_db_path(args.db)
    sandbox = _resolve_sandbox(args.sandbox)
    print(f"db      : {db_path}")
    print(f"sandbox : {sandbox}")
    print(f"model   : {args.model or '(SDK default)'}")
    print(f"max_turns: {args.max_turns}")

    outcome = asyncio.run(
        run_hello_example(
            db_path=db_path,
            sandbox=sandbox,
            model=args.model,
            max_turns=args.max_turns,
        )
    )
    return 0 if outcome.lifecycle.status == Status.DONE else 1


__all__ = [
    "DEFAULT_DB_PATH",
    "DEFAULT_MAX_TURNS",
    "DEFAULT_MAX_WALL_SECONDS",
    "DEFAULT_SANDBOX",
    "TARGET_CONTENT",
    "TARGET_FILENAME",
    "build_task",
    "dump_store_state",
    "main",
    "make_claude_code_invoke",
    "run_hello_example",
]

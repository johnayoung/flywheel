"""End-to-end smoke example: drive one trivial task to ``done``.

Wires the documented components — :class:`flywheel.task.Task`,
:class:`flywheel.lifecycle.Lifecycle`, :class:`flywheel.store_sqlite.SqliteStore`,
:func:`flywheel.harness.run_task` — into a single runnable script that
streams every harness event to stdout in its structured form (one JSON
object per line).

Run as::

    uv run python -m flywheel.examples.hello [DB_PATH]

``DB_PATH`` is optional. Omit it and a fresh ``.sqlite`` file is created
under the system temp dir for this run. Provide a path to reuse the same
database across runs — each invocation generates a new ``run_id``, so
prior runs' audit data (lifecycles, attempts, events, grader_results)
remains intact and queryable.

Why an in-process invoker
-------------------------

A smoke example must reach ``done`` without manual intervention or API
credentials. The harness exposes ``invoke`` as a first-class plugin
seam (``InvokeFunc``) precisely so non-SDK invokers can drive the loop
deterministically — every test in :mod:`tests.test_harness` does the
same. This example supplies :func:`_offline_invoke`, a real
:class:`flywheel.invoker.IterationResult`-producing callable: it
returns a complete envelope, structurally-realistic SDK messages
(``AssistantMessage`` + ``ResultMessage``), and signals derived from
those messages. The harness treats its output identically to a
SDK-backed run.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import sys
import tempfile
from pathlib import Path

from claude_agent_sdk import AssistantMessage, ResultMessage, TextBlock

from flywheel import (
    Attempt,
    CommandGrader,
    EventRecord,
    GraderResultRecord,
    HarnessConfig,
    HarnessOutcome,
    HarnessStore,
    InvocationRequest,
    InvocationSignals,
    IterationResult,
    Lifecycle,
    SqliteStore,
    Task,
    ValidEnvelope,
    run_task,
)
from flywheel.envelope import CLOSING_FENCE, OPENING_FENCE, Intent


_OFFLINE_REASON = "hello example completed"

_OFFLINE_TRANSCRIPT = (
    "The flywheel hello example completed successfully.\n"
    f"{OPENING_FENCE}\n"
    f'{{"intent": "verify", "reason": "{_OFFLINE_REASON}"}}\n'
    f"{CLOSING_FENCE}\n"
)


class _EventStreamingStore:
    """Forward every store call to ``wrapped`` and emit each persisted
    event to stdout as one JSON line.

    The constraint on this example is that *only* harness-emitted events
    reach stdout — no ad-hoc ``print`` from this script bypasses the
    event stream. Centralising stdout writes here keeps that contract
    enforceable: the only place we ``print`` is in :meth:`append_event`,
    and it only fires when the harness chooses to emit.
    """

    def __init__(self, wrapped: HarnessStore) -> None:
        self._wrapped = wrapped

    def create_lifecycle(self, lifecycle: Lifecycle) -> None:
        self._wrapped.create_lifecycle(lifecycle)

    def update_lifecycle(
        self,
        lifecycle: Lifecycle,
        *,
        expected_version: int,
    ) -> None:
        self._wrapped.update_lifecycle(
            lifecycle, expected_version=expected_version
        )

    def load_lifecycle(self, run_id: str) -> Lifecycle | None:
        return self._wrapped.load_lifecycle(run_id)

    def save_attempt(self, run_id: str, attempt: Attempt) -> None:
        self._wrapped.save_attempt(run_id, attempt)

    def list_attempts(self, run_id: str) -> list[Attempt]:
        return self._wrapped.list_attempts(run_id)

    def append_event(self, event: EventRecord) -> EventRecord:
        persisted = self._wrapped.append_event(event)
        line = json.dumps(
            {
                "id": persisted.id,
                "run_id": persisted.run_id,
                "ts": persisted.ts.isoformat(),
                "kind": persisted.kind,
                "attempt_number": persisted.attempt_number,
                "payload": dict(persisted.payload),
            },
            sort_keys=True,
        )
        print(line, flush=True)
        return persisted

    def append_grader_result(
        self, result: GraderResultRecord
    ) -> GraderResultRecord:
        return self._wrapped.append_grader_result(result)

    def list_grader_results(
        self,
        run_id: str,
        attempt_number: int,
    ) -> list[GraderResultRecord]:
        return self._wrapped.list_grader_results(run_id, attempt_number)


async def _offline_invoke(_request: InvocationRequest) -> IterationResult:
    """Deterministic in-process ``InvokeFunc`` for the smoke example.

    Returns a complete :class:`IterationResult` carrying:

    * a transcript with a valid ``<!-- LOOP_STATUS -->`` envelope,
    * an :class:`AssistantMessage` + :class:`ResultMessage` so SDK
      signals (``stop_reason``, ``num_turns``, ``total_cost_usd``) are
      structurally realistic for downstream consumers,
    * an explicit :class:`ValidEnvelope` with ``intent=verify`` so the
      harness routes the iteration through the grader pipeline.
    """
    usage = {
        "input_tokens": 12,
        "output_tokens": 4,
        "cache_read_input_tokens": 0,
        "cache_creation_input_tokens": 0,
    }
    assistant = AssistantMessage(
        content=[TextBlock(text=_OFFLINE_TRANSCRIPT)],
        model="flywheel-hello-example",
        stop_reason="end_turn",
        session_id="hello-example",
        usage=usage,
    )
    result = ResultMessage(
        subtype="success",
        duration_ms=1,
        duration_api_ms=1,
        is_error=False,
        num_turns=1,
        session_id="hello-example",
        stop_reason="end_turn",
        total_cost_usd=0.0,
        usage=usage,
    )
    signals = InvocationSignals(
        stop_reason="end_turn",
        num_turns=1,
        total_cost_usd=0.0,
        result_is_error=False,
        result_subtype="success",
        api_error_status=None,
        session_id="hello-example",
    )
    return IterationResult(
        transcript=_OFFLINE_TRANSCRIPT,
        messages=(assistant, result),
        envelope=ValidEnvelope(intent=Intent.VERIFY, reason=_OFFLINE_REASON),
        signals=signals,
    )


def build_task() -> Task:
    """The trivial task driven by this example.

    A single ``command`` grader executes a real shell invocation that
    exits ``0`` in the repo's environment — :data:`sys.executable` with
    a no-op script. No fictional commands.
    """
    return Task(
        id="hello-example",
        goal="Smoke-test the flywheel harness end-to-end against SQLite.",
        graders=[
            CommandGrader(
                run=f"{sys.executable} -c 'pass'",
                name="trivial-exit-zero",
            )
        ],
    )


async def run_hello_example(db_path: str | os.PathLike[str]) -> HarnessOutcome:
    """Run the smoke example end-to-end and return the harness outcome.

    Opens a :class:`SqliteStore` at ``db_path``, wraps it in an event
    streamer, and drives :func:`run_task` to a terminal status. Every
    run uses a fresh ``run_id`` (the :class:`Lifecycle` default), so
    re-running against the same ``db_path`` preserves prior runs' audit
    rows instead of overwriting them.
    """
    task = build_task()
    lifecycle = Lifecycle(task_id=task.id)
    backend = SqliteStore(Path(os.fspath(db_path)))
    try:
        store = _EventStreamingStore(backend)
        outcome = await run_task(
            task,
            lifecycle,
            store,
            config=HarnessConfig(
                agent_context={
                    "model_id": "flywheel-hello-example",
                    "model_version": "offline",
                    "agent_sdk_version": "n/a",
                    "prompt_template_hash": "hello",
                },
            ),
            invoke=_offline_invoke,
        )
    finally:
        backend.close()
    return outcome


def _resolve_db_path(arg: str | None) -> Path:
    if arg:
        return Path(arg)
    handle, name = tempfile.mkstemp(prefix="flywheel-hello-", suffix=".sqlite")
    os.close(handle)
    return Path(name)


def main(argv: list[str] | None = None) -> int:
    """Console entry point. Returns ``0`` on lifecycle ``done``, else ``1``."""
    parser = argparse.ArgumentParser(
        prog="python -m flywheel.examples.hello",
        description=(
            "Run the flywheel hello example against a SQLite store, "
            "streaming harness events to stdout."
        ),
    )
    parser.add_argument(
        "db_path",
        nargs="?",
        default=None,
        help=(
            "SQLite database file. Omit for a fresh temp file per run; "
            "reuse the same path across runs to accumulate run history."
        ),
    )
    args = parser.parse_args(argv)
    db_path = _resolve_db_path(args.db_path)
    outcome = asyncio.run(run_hello_example(db_path))
    return 0 if outcome.lifecycle.status.value == "done" else 1


if __name__ == "__main__":
    raise SystemExit(main())


__all__ = [
    "build_task",
    "main",
    "run_hello_example",
]

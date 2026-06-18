"""Contract tests for :mod:`flywheel_core.harness`.

Each test injects a stub ``invoke`` callable returning canned
:class:`IterationResult` instances rather than spawning a real agent.
The harness owns lifecycle transitions, attempt recording, and grader
dispatch; the tests assert that contract end-to-end across every
state-detection-map branch the MVP handles.
"""

from __future__ import annotations

import asyncio
import json
import sys
from collections.abc import Awaitable, Callable, Coroutine, Mapping
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import pytest
from claude_agent_sdk import (
    AssistantMessage,
    Message,
    RateLimitEvent,
    RateLimitInfo,
    ResultMessage,
    TextBlock,
)

from flywheel_core import (
    Attempt,
    CommandGrader,
    HarnessConfig,
    HarnessOutcome,
    InMemoryStore,
    InvocationFailure,
    InvocationRequest,
    InvocationSignals,
    IterationResult,
    Lifecycle,
    ManualGrader,
    Outcome,
    RubricGrader,
    Status,
    Task,
    TranscriptGrader,
    replay,
    run_task,
)
from flywheel_core.grader_rubric import (
    CLOSING_FENCE as RUBRIC_CLOSING_FENCE,
)
from flywheel_core.grader_rubric import (
    OPENING_FENCE as RUBRIC_OPENING_FENCE,
)
from flywheel_core.grader_rubric import (
    RubricJudgeError,
)
from flywheel_core.harness import (
    _build_observation,
    _build_usage_breakdown,
    _handle_interrupt,
    _RunTelemetry,
    finalize_stranded_lifecycle,
)
from flywheel_core.invoker import ToolInteraction, ToolResultObservation
from flywheel_core.store_protocols import TelemetryRecord
from flywheel_core.loop_guard import LoopGuardConfig
from flywheel_core.loaders import task_digest
from flywheel_core.envelope import (
    CLOSING_FENCE,
    CommandGraderRequirement,
    DuplicateEnvelope,
    EnvVarSetRequirement,
    Intent,
    MalformedEnvelope,
    MissingEnvelope,
    OPENING_FENCE,
    TruncatedEnvelope,
    ValidEnvelope,
)


# --- Helpers --------------------------------------------------------------


def _make_signals(**overrides: Any) -> InvocationSignals:
    defaults: dict[str, Any] = {
        "stop_reason": "end_turn",
        "num_turns": 1,
        "total_cost_usd": 0.01,
        "result_is_error": False,
        "result_subtype": "success",
        "api_error_status": None,
        "session_id": "sess-1",
    }
    defaults.update(overrides)
    return InvocationSignals(**defaults)


def _assistant(
    *,
    usage: dict[str, Any] | None = None,
    text: str = "ok",
    stop_reason: str | None = "end_turn",
) -> AssistantMessage:
    return AssistantMessage(
        content=[TextBlock(text=text)],
        model="claude-test",
        stop_reason=stop_reason,
        session_id="sess-1",
        usage=usage,
    )


def _result_msg(
    *,
    num_turns: int = 1,
    usage: dict[str, Any] | None = None,
) -> ResultMessage:
    return ResultMessage(
        subtype="success",
        duration_ms=10,
        duration_api_ms=8,
        is_error=False,
        num_turns=num_turns,
        session_id="sess-1",
        stop_reason="end_turn",
        total_cost_usd=0.01,
        usage=usage,
    )


def _iteration(
    *,
    envelope: ValidEnvelope | MissingEnvelope | TruncatedEnvelope
    | DuplicateEnvelope | MalformedEnvelope,
    messages: tuple[Message, ...] = (),
    transcript: str = "",
    signals: InvocationSignals | None = None,
    failure: InvocationFailure | None = None,
) -> IterationResult:
    return IterationResult(
        transcript=transcript,
        messages=messages,
        envelope=envelope,
        signals=signals or _make_signals(),
        failure=failure,
    )


def _wrap(payload: str) -> str:
    return f"{OPENING_FENCE}\n{payload}\n{CLOSING_FENCE}"


def _strip_attempt_aggregates(*lifecycles: Lifecycle) -> None:
    """Zero the boundary-rolled aggregate counters on every attempt.

    The aggregates (tokens, iterations, turns, cost, last activity) are
    OLTP rollups written outside the domain-event ledger, so replay
    cannot reproduce them; oracle tests comparing ``loaded == folded``
    normalize them away first."""
    for lc in lifecycles:
        for a in lc.attempts:
            a.input_tokens = 0
            a.output_tokens = 0
            a.cache_creation_input_tokens = 0
            a.cache_read_input_tokens = 0
            a.iterations_completed = 0
            a.turns = 0
            a.total_cost_usd = 0.0
            a.last_activity_at = None


def _run(coro: Coroutine[Any, Any, HarnessOutcome]) -> HarnessOutcome:
    return asyncio.run(coro)


class _ListSink:
    """In-memory TelemetrySink capturing records in emission order.

    Telemetry no longer lands in the store (spec 00025): the harness
    streams SDK messages, harness.* telemetry, and domain.* ledger
    mirrors to the run's sink. The view helpers slice the captured
    stream the way the old store verbs did so assertions stay readable.
    """

    def __init__(self) -> None:
        self.records: list[TelemetryRecord] = []

    def append_telemetry(self, record: TelemetryRecord) -> None:
        self.records.append(record)

    def events(self, run_id: str) -> list[TelemetryRecord]:
        """Harness telemetry records (the old list_events view)."""
        return [
            r
            for r in self.records
            if r.run_id == run_id and r.kind.startswith("harness.")
        ]

    def messages(self, run_id: str) -> list[TelemetryRecord]:
        """SDK message records (the old list_sdk_messages view)."""
        return [
            r
            for r in self.records
            if r.run_id == run_id
            and not r.kind.startswith(("harness.", "domain."))
        ]

    def domain_mirrors(self, run_id: str) -> list[TelemetryRecord]:
        """domain.* ledger mirror lines, in emission order."""
        return [
            r
            for r in self.records
            if r.run_id == run_id and r.kind.startswith("domain.")
        ]


def _scripted_invoker(
    results: list[IterationResult],
) -> Callable[[InvocationRequest], Awaitable[IterationResult]]:
    """Build an invoker that returns ``results`` in order, recording calls.

    Each call pops the next IterationResult. Tests can inspect the
    accumulated ``InvocationRequest`` list via the function's ``.calls``
    attribute. Honors the per-message observer contract: before
    returning, the scripted invoker calls ``request.on_message`` once
    per :class:`Message` in ``IterationResult.messages``, matching what
    :func:`invoke_iteration` does for a real SDK transport so the
    harness's persistence observer fires identically in tests.
    """
    calls: list[InvocationRequest] = []

    async def _invoker(request: InvocationRequest) -> IterationResult:
        calls.append(request)
        result = results.pop(0)
        if request.on_message is not None:
            for msg in result.messages:
                try:
                    request.on_message(msg)
                except Exception:  # noqa: BLE001 - the production
                    # invoker swallows observer exceptions; mirror it
                    # here so test seams stay faithful to that contract.
                    pass
        return result

    _invoker.calls = calls  # type: ignore[attr-defined]
    return _invoker


# --- Successful run -------------------------------------------------------


class TestSuccessfulRun:
    def test_verify_then_all_graders_pass_reaches_done(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'import sys; sys.exit(0)'",
                    name="ok",
                ),
                TranscriptGrader(max_turns=10, name="caps"),
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-success")
        transcript = _wrap('{"intent": "verify", "reason": "all done"}')
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(
                        intent=Intent.VERIFY, reason="all done"
                    ),
                    messages=(_assistant(), _result_msg(num_turns=1)),
                    transcript=transcript,
                    signals=_make_signals(num_turns=1),
                )
            ]
        )

        outcome = _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke))

        assert outcome.lifecycle.status == Status.DONE
        assert len(outcome.attempts) == 1
        attempt = outcome.attempts[0]
        assert attempt.outcome == Outcome.SUCCEEDED
        assert attempt.ended_at is not None

        # grader_results: one command row, one transcript row, both passed.
        grader_rows = store.list_grader_results(lifecycle.run_id, 1)
        assert [r.grader_type for r in grader_rows] == ["command", "transcript"]
        assert all(r.passed for r in grader_rows)

    def test_agent_context_persisted_on_attempt(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'import sys; sys.exit(0)'",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-ctx")
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                )
            ]
        )
        ctx = {
            "model_id": "claude-opus-4-7",
            "model_version": "2026-01-01",
            "agent_sdk_version": "0.1.0",
            "prompt_template_hash": "abc123",
        }
        config = HarnessConfig(agent_context=ctx)

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        attempt = outcome.attempts[0]
        assert attempt.agent_context == ctx
        # Mutating the returned agent_context must not affect the store.
        attempt.agent_context["model_id"] = "tampered"
        reloaded = store.list_attempts(lifecycle.run_id)[0]
        assert reloaded.agent_context["model_id"] == "claude-opus-4-7"

    def test_command_grader_runs_in_worktree(self, tmp_path: Path) -> None:
        # A relative-path check passes only if the grader's CWD is the
        # configured sandbox. Regression guard for the harness threading
        # config.worktree into run_command_graders (and not grading its
        # own ambient CWD).
        sandbox = tmp_path / "sandbox"
        sandbox.mkdir()
        (sandbox / "sentinel.flag").write_text("ok", encoding="utf-8")
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c "
                    f"\"import os,sys; sys.exit(0 if "
                    f"os.path.exists('sentinel.flag') else 1)\"",
                    name="relative-check",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-wt-cwd")
        config = HarnessConfig(worktree=sandbox)
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                )
            ]
        )

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        # The sentinel only exists inside the sandbox, so reaching DONE
        # proves the grader ran there rather than in the harness CWD.
        assert outcome.lifecycle.status == Status.DONE

    def test_per_attempt_artifact_directory_created(self, tmp_path: Path) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'print(\"hi\")'",
                    name="echo",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-art")
        config = HarnessConfig(artifacts_root=tmp_path)
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                )
            ]
        )

        _run(run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke))

        attempt_dir = tmp_path / "attempt-001"
        assert attempt_dir.is_dir()
        # The command grader writes stdout/stderr files under it.
        produced = sorted(p.name for p in attempt_dir.iterdir())
        assert any(name.startswith("grader-000") for name in produced)


# --- Retry policy ---------------------------------------------------------


class TestRetryPolicy:
    def test_validation_failure_retries_when_budget_remains(
        self, tmp_path: Path
    ) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        marker = tmp_path / "attempt_counter"
        # Grader: exit 0 only on the second invocation.
        grader_run = (
            f"{sys.executable} -c \"import os, pathlib; "
            f"p=pathlib.Path(r'{marker}'); "
            f"n=int(p.read_text()) if p.exists() else 0; "
            f"p.write_text(str(n+1)); "
            f"raise SystemExit(0 if n>=1 else 1)\""
        )
        task = Task(goal="g", graders=[CommandGrader(run=grader_run)])
        lifecycle = Lifecycle(task_id="t1", run_id="run-retry")
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                ),
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                ),
            ]
        )
        config = HarnessConfig(max_retries=1)

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        assert outcome.lifecycle.status == Status.DONE
        assert outcome.lifecycle.retries == 1
        assert len(outcome.attempts) == 2
        assert outcome.attempts[0].outcome == Outcome.VALIDATION_FAILED
        assert outcome.attempts[1].outcome == Outcome.SUCCEEDED

    def test_validation_failure_terminates_when_retries_exhausted(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(1)'",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-exhaust")
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                )
            ]
        )
        config = HarnessConfig(max_retries=0)

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        assert outcome.lifecycle.status == Status.FAILED
        assert outcome.lifecycle.retries == 0
        assert len(outcome.attempts) == 1
        assert outcome.attempts[0].outcome == Outcome.VALIDATION_FAILED

    def test_retry_eligibility_delegates_to_lifecycle(self) -> None:
        """``Lifecycle.is_retry_eligible`` is the single source of truth.

        With no retry budget (``max_retries=0``), the first validation
        failure is terminal: ``retries`` (0) is not ``< max_retries`` (0),
        so the harness routes to ``FAILED`` without reimplementing the
        rule. (Under event sourcing ``retries`` accumulates only via real
        retry edges in the log, so exhaustion is expressed through the
        budget rather than a constructor-injected count.)
        """
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(1)'",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-elig")
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                )
            ]
        )
        config = HarnessConfig(max_retries=0)  # retries (0) >= max (0) — exhausted.

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )
        assert outcome.lifecycle.status == Status.FAILED


# --- Envelope protocol failures -------------------------------------------


class TestProtocolFailures:
    @pytest.mark.parametrize(
        "envelope,expected_substr",
        [
            (MissingEnvelope(), "missing"),
            (TruncatedEnvelope(detail="opening only"), "truncated"),
            (DuplicateEnvelope(count=2), "duplicate"),
            (MalformedEnvelope(reason="bad json"), "malformed"),
        ],
    )
    def test_protocol_failure_routes_to_failed_validation(
        self,
        envelope: Any,
        expected_substr: str,
    ) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                )
            ],
        )
        lifecycle = Lifecycle(
            task_id="t1", run_id=f"run-proto-{expected_substr}"
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=envelope,
                    messages=(_assistant(), _result_msg()),
                )
            ]
        )

        outcome = _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke))

        assert outcome.lifecycle.status == Status.FAILED
        # Without retries, failed_validation terminates in failed; check
        # the attempt's outcome and the recorded error.
        attempt = outcome.attempts[0]
        assert attempt.outcome == Outcome.AGENT_ERROR
        assert expected_substr in attempt.error
        # No command grader rows recorded — validation did not progress.
        rows = store.list_grader_results(lifecycle.run_id, 1)
        assert rows == []

    def test_protocol_failure_does_not_silently_coerce_to_continue(self) -> None:
        """Missing envelopes must not be retried-as-continue silently."""
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-no-coerce")
        invoke = _scripted_invoker(
            [
                _iteration(envelope=MissingEnvelope()),
            ]
        )
        # max_iterations_per_attempt=5 — if the harness silently coerced
        # missing to continue it would loop through all 5; instead it
        # should classify after the first iteration.
        config = HarnessConfig(max_iterations_per_attempt=5)

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        assert outcome.lifecycle.status == Status.FAILED
        assert len(invoke.calls) == 1  # type: ignore[attr-defined]


# --- Blocked / abort ------------------------------------------------------


class TestBlockedAndAbort:
    def test_blocked_transitions_to_interrupted_and_preserves_retries(
        self,
    ) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                    name="full-suite",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-blocked")
        requires = (
            CommandGraderRequirement(name="full-suite"),
            EnvVarSetRequirement(name="MISSING_KEY"),
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(
                        intent=Intent.BLOCKED,
                        reason="need API key",
                        requires=requires,
                    ),
                    messages=(_assistant(),),
                )
            ]
        )
        config = HarnessConfig(max_retries=5)

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        assert outcome.lifecycle.status == Status.INTERRUPTED
        # Blocked must not consume retries.
        assert outcome.lifecycle.retries == 0
        attempt = outcome.attempts[0]
        assert attempt.outcome == Outcome.CANCELLED
        assert "need API key" in attempt.error
        # No grader rows — validation did not run.
        assert store.list_grader_results(lifecycle.run_id, 1) == []
        # Structured snapshot persisted on the lifecycle row.
        expected_requires = [
            {"type": "command_grader", "name": "full-suite"},
            {"type": "env_var_set", "name": "MISSING_KEY"},
        ]
        assert outcome.lifecycle.blocked_requires_json == json.dumps(
            expected_requires
        )
        # harness.blocked event carries the structured list alongside reason.
        blocked_events = [
            e for e in sink.events(lifecycle.run_id)
            if e.kind == "harness.blocked"
        ]
        assert len(blocked_events) == 1
        payload = dict(blocked_events[0].payload)
        assert payload["reason"] == "need API key"
        assert payload["requires"] == expected_requires

    def test_blocked_with_unknown_command_grader_routes_through_protocol_failure(
        self,
    ) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                    name="other",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-bad-blocked")
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(
                        intent=Intent.BLOCKED,
                        reason="need help",
                        requires=(
                            CommandGraderRequirement(name="not-on-task"),
                        ),
                    ),
                    messages=(_assistant(),),
                )
            ]
        )
        config = HarnessConfig(max_retries=0)

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        # Routed through protocol-failure path: FAILED_VALIDATION terminal
        # (no retry budget here), AGENT_ERROR outcome, harness.blocked
        # never emitted, blocked_requires_json never populated.
        assert outcome.lifecycle.status == Status.FAILED
        attempt = outcome.attempts[0]
        assert attempt.outcome == Outcome.AGENT_ERROR
        assert "invalid blocked requires" in attempt.error
        assert outcome.lifecycle.blocked_requires_json is None
        kinds = [e.kind for e in sink.events(lifecycle.run_id)]
        assert "harness.blocked" not in kinds
        assert "harness.protocol_failure" in kinds

    def test_abort_transitions_to_failed(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-abort")
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(
                        intent=Intent.ABORT,
                        reason="cannot proceed",
                    ),
                    messages=(_assistant(),),
                )
            ]
        )
        config = HarnessConfig(max_retries=5)

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        assert outcome.lifecycle.status == Status.FAILED
        assert "cannot proceed" in outcome.lifecycle.error
        # Abort must not consume retries — it goes straight to FAILED,
        # bypassing failed_validation.
        assert outcome.lifecycle.retries == 0
        assert outcome.attempts[0].outcome == Outcome.AGENT_ERROR


# --- Crash ----------------------------------------------------------------


class TestCrash:
    def test_invoker_failure_transitions_to_failed(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-crash")
        failure = InvocationFailure(
            error_type="ProcessError",
            message="cli exited 137",
            exit_code=137,
            stderr=None,
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=MissingEnvelope(),
                    failure=failure,
                )
            ]
        )

        outcome = _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke))

        assert outcome.lifecycle.status == Status.FAILED
        attempt = outcome.attempts[0]
        assert attempt.outcome == Outcome.INTERNAL_ERROR
        assert "ProcessError" in attempt.error
        # Crash recorded distinctly via an event.
        events = sink.events(lifecycle.run_id)
        crash_events = [e for e in events if e.kind == "harness.crash"]
        assert len(crash_events) == 1
        assert crash_events[0].payload["classification"] == "deferred"


# --- Budget breach --------------------------------------------------------


class TestBudgetBreach:
    def test_max_turns_breach_routes_to_failed_validation(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                ),
                TranscriptGrader(max_turns=2, name="turns-cap"),
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-budget")
        # Emit 5 AssistantMessages -> turns=5 > 2.
        messages: tuple[Message, ...] = (
            _assistant(),
            _assistant(),
            _assistant(),
            _assistant(),
            _assistant(),
            _result_msg(num_turns=5),
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    # Missing envelope is fine — breach overrides.
                    envelope=MissingEnvelope(),
                    messages=messages,
                )
            ]
        )

        outcome = _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke))

        assert outcome.lifecycle.status == Status.FAILED
        attempt = outcome.attempts[0]
        assert attempt.outcome == Outcome.VALIDATION_FAILED
        assert "max_turns" in attempt.error

        # Transcript grader row records the breach.
        rows = store.list_grader_results(lifecycle.run_id, 1)
        assert [r.grader_type for r in rows] == ["transcript"]
        assert rows[0].passed is False
        assert rows[0].payload["breached"] == "max_turns"
        # Command grader was NOT run — budget breach short-circuits validation.
        assert all(r.grader_type != "command" for r in rows)

    def test_budget_event_emitted_with_observed_totals(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                ),
                TranscriptGrader(max_total_tokens=10, name="tok-cap"),
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-budget-evt")
        messages: tuple[Message, ...] = (
            _assistant(usage={"input_tokens": 30, "output_tokens": 20}),
            _result_msg(num_turns=1),
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=messages,
                )
            ]
        )

        _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke))

        events = [
            e
            for e in sink.events(lifecycle.run_id)
            if e.kind == "harness.budget_exceeded"
        ]
        assert len(events) == 1
        assert events[0].payload["breached"] == "max_total_tokens"
        assert events[0].payload["observed"]["total_tokens"] == 50


# --- Continue loop --------------------------------------------------------


class TestContinueLoop:
    def test_continue_loops_within_cap_then_verifies(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-continue")
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.CONTINUE),
                    messages=(_assistant(),),
                ),
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.CONTINUE),
                    messages=(_assistant(),),
                ),
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg(num_turns=3)),
                ),
            ]
        )
        config = HarnessConfig(max_iterations_per_attempt=3)

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        assert outcome.lifecycle.status == Status.DONE
        assert len(invoke.calls) == 3  # type: ignore[attr-defined]
        # All three iterations belong to the same Attempt.
        assert len(outcome.attempts) == 1

    def test_continue_past_cap_fails_validation_without_grading(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-continue-cap")
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.CONTINUE),
                    messages=(_assistant(),),
                ),
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.CONTINUE),
                    messages=(_assistant(),),
                ),
            ]
        )
        config = HarnessConfig(max_iterations_per_attempt=2)

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        assert outcome.lifecycle.status == Status.FAILED
        attempt = outcome.attempts[0]
        assert attempt.outcome == Outcome.AGENT_ERROR
        assert "did not converge" in attempt.error
        # No command grader rows — validation didn't progress past the
        # envelope mismatch.
        assert store.list_grader_results(lifecycle.run_id, 1) == []


# --- Sole-owner invariant -------------------------------------------------


class TestSoleOwnerOfTransitions:
    def test_no_module_outside_harness_calls_transition_to(self) -> None:
        """Static check: only ``harness.py`` calls ``Lifecycle.transition_to``.

        The lifecycle module defines the method; the harness module is
        the only legitimate caller in production code. Tests are
        exempt — they exercise the state machine directly.
        """
        import ast

        src_root = Path(__file__).resolve().parents[1] / "src" / "flywheel_core"
        violations: list[tuple[str, int]] = []
        for py in src_root.glob("*.py"):
            if py.name in ("lifecycle.py", "harness.py"):
                continue
            tree = ast.parse(py.read_text())
            for node in ast.walk(tree):
                if (
                    isinstance(node, ast.Attribute)
                    and node.attr == "transition_to"
                ):
                    violations.append((py.name, node.lineno))
        assert violations == [], (
            f"transition_to is called outside harness.py: {violations}"
        )

    def test_no_module_outside_lifecycle_assigns_to_status(self) -> None:
        """No production module mutates ``Lifecycle.status`` directly.

        Lifecycle owns the assignment inside :meth:`transition_to`. The
        harness reaches it only through that method. Any other module
        assigning to a ``.status`` attribute would bypass the state
        machine.
        """
        import ast

        src_root = Path(__file__).resolve().parents[1] / "src" / "flywheel_core"
        violations: list[tuple[str, int]] = []
        for py in src_root.glob("*.py"):
            if py.name == "lifecycle.py":
                continue
            tree = ast.parse(py.read_text())
            for node in ast.walk(tree):
                if isinstance(node, ast.Assign):
                    for target in node.targets:
                        if (
                            isinstance(target, ast.Attribute)
                            and target.attr == "status"
                        ):
                            violations.append((py.name, node.lineno))
        assert violations == [], (
            f"Lifecycle.status assigned outside lifecycle.py: {violations}"
        )


# --- Pending entry --------------------------------------------------------


class TestEntryNormalization:
    def test_pending_lifecycle_is_brought_to_ready(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                )
            ],
        )
        lifecycle = Lifecycle(
            task_id="t1", run_id="run-pending", status=Status.PENDING
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                )
            ]
        )

        outcome = _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke))

        assert outcome.lifecycle.status == Status.DONE
        # The PENDING → READY → RUNNING → VALIDATING → DONE chain ran.
        assert Status.READY in outcome.lifecycle.timestamps
        assert Status.RUNNING in outcome.lifecycle.timestamps
        assert Status.VALIDATING in outcome.lifecycle.timestamps
        assert Status.DONE in outcome.lifecycle.timestamps

    def test_resume_from_interrupted_runs_a_new_attempt(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                    name="ok",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-resume")

        invoke_first = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(
                        intent=Intent.BLOCKED,
                        reason="need help",
                        requires=(CommandGraderRequirement(name="ok"),),
                    ),
                    messages=(_assistant(),),
                )
            ]
        )
        _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke_first))
        assert lifecycle.status == Status.INTERRUPTED
        # Per the centralized clearer, the resume drain (INTERRUPTED ->
        # READY) drops the structured snapshot back to NULL.
        assert lifecycle.blocked_requires_json is not None

        invoke_second = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                )
            ]
        )
        outcome = _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke_second))

        assert outcome.lifecycle.status == Status.DONE
        assert outcome.lifecycle.blocked_requires_json is None
        assert len(outcome.attempts) == 2


# --- Rate-limit event surface --------------------------------------------


class TestRateLimitSurface:
    def test_rate_limit_event_surfaced_without_classification(self) -> None:
        """rate_limited per docs/loop.md is transient — the harness
        records the signal but does not classify it as a failure."""
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-rate")
        rate_event = RateLimitEvent(
            rate_limit_info=RateLimitInfo(
                status="allowed_warning",
                resets_at=1_700_000_000,
                rate_limit_type="five_hour",
                utilization=0.5,
            ),
            uuid="evt-1",
            session_id="sess-1",
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                    signals=_make_signals(rate_limit_events=(rate_event,)),
                )
            ]
        )

        outcome = _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke))

        assert outcome.lifecycle.status == Status.DONE
        completed_events = [
            e
            for e in sink.events(lifecycle.run_id)
            if e.kind == "harness.iteration_completed"
        ]
        assert completed_events[0].payload["rate_limited"] is True


# --- Context-pressure telemetry ------------------------------------------


class TestIterationCompletedTelemetry:
    """Per-iteration token / cost / turn signals on
    ``harness.iteration_completed``.

    Covers FR-1..FR-4 of ``00009-FEATURE-context-pressure-telemetry``:
    the payload carries a full token breakdown that matches the
    transcript-grader breach math, plus the SDK-reported cost and turn
    counts; token fields are per-iteration deltas, never a running sum.
    """

    def _completed_payload(
        self, sink: _ListSink, run_id: str
    ) -> Mapping[str, Any]:
        events = [
            e
            for e in sink.events(run_id)
            if e.kind == "harness.iteration_completed"
        ]
        assert len(events) == 1, events
        return events[0].payload

    def _completed_payloads(
        self, sink: _ListSink, run_id: str
    ) -> list[Mapping[str, Any]]:
        return [
            e.payload
            for e in sink.events(run_id)
            if e.kind == "harness.iteration_completed"
        ]

    def _passing_task(self) -> Task:
        return Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                )
            ],
        )

    # FR-1: per-iteration token breakdown is present with all five fields.
    def test_usage_breakdown_attached_to_payload(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = self._passing_task()
        lifecycle = Lifecycle(task_id="t1", run_id="run-usage-fr1")
        usage = {
            "input_tokens": 12,
            "output_tokens": 7,
            "cache_creation_input_tokens": 3,
            "cache_read_input_tokens": 100,
        }
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(usage=usage), _result_msg(num_turns=1)),
                )
            ]
        )

        _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke))

        payload = self._completed_payload(sink, lifecycle.run_id)
        assert payload["usage"] == {
            "input_tokens": 12,
            "output_tokens": 7,
            "cache_creation_input_tokens": 3,
            "cache_read_input_tokens": 100,
            "total_tokens": 122,
        }

    # FR-2: cost + turns surfaced verbatim from InvocationSignals.
    def test_cost_and_turns_surfaced_from_signals(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = self._passing_task()
        lifecycle = Lifecycle(task_id="t1", run_id="run-usage-fr2")
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg(num_turns=4)),
                    signals=_make_signals(
                        num_turns=4,
                        total_cost_usd=0.42,
                    ),
                )
            ]
        )

        _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke))

        payload = self._completed_payload(sink, lifecycle.run_id)
        assert payload["num_turns"] == 4
        assert payload["total_cost_usd"] == 0.42

    # FR-2 edge: when the SDK never produced a ResultMessage, cost / turns
    # are absent — surface as null, not zero.
    def test_cost_and_turns_null_when_signals_lack_them(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = self._passing_task()
        lifecycle = Lifecycle(task_id="t1", run_id="run-usage-fr2-null")
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                    signals=_make_signals(
                        num_turns=None,
                        total_cost_usd=None,
                    ),
                )
            ]
        )

        _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke))

        payload = self._completed_payload(sink, lifecycle.run_id)
        assert payload["num_turns"] is None
        assert payload["total_cost_usd"] is None

    # FR-3: emitted total_tokens equals the transcript-grader breach figure
    # (the same _build_observation pipes feed both code paths).
    def test_total_tokens_matches_observation(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = self._passing_task()
        lifecycle = Lifecycle(task_id="t1", run_id="run-usage-fr3")
        messages: tuple[Message, ...] = (
            _assistant(usage={"input_tokens": 30, "output_tokens": 20}),
            _assistant(usage={"input_tokens": 11, "output_tokens": 4}),
            _result_msg(num_turns=2),
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=messages,
                )
            ]
        )

        _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke))

        payload = self._completed_payload(sink, lifecycle.run_id)
        observation = _build_observation(messages, wall_seconds=0.0)
        assert payload["usage"]["total_tokens"] == observation.total_tokens
        # Sanity: total_tokens equals the field-wise sum.
        assert payload["usage"]["total_tokens"] == sum(
            payload["usage"][k]
            for k in (
                "input_tokens",
                "output_tokens",
                "cache_creation_input_tokens",
                "cache_read_input_tokens",
            )
        )

    # FR-4: token fields are per-iteration deltas, never a running sum.
    # Each iteration's event must carry only that iteration's usage; the
    # run total is the sum of the deltas.
    def test_two_iterations_each_carry_own_usage_not_running_sum(
        self,
    ) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = self._passing_task()
        lifecycle = Lifecycle(task_id="t1", run_id="run-usage-fr4")
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.CONTINUE),
                    messages=(
                        _assistant(
                            usage={"input_tokens": 10, "output_tokens": 5}
                        ),
                    ),
                ),
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(
                        _assistant(
                            usage={"input_tokens": 7, "output_tokens": 3}
                        ),
                        _result_msg(num_turns=2),
                    ),
                ),
            ]
        )
        config = HarnessConfig(max_iterations_per_attempt=2)

        _run(run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke))

        payloads = self._completed_payloads(sink, lifecycle.run_id)
        assert len(payloads) == 2
        # First iteration carries only its own 15 tokens.
        assert payloads[0]["usage"]["input_tokens"] == 10
        assert payloads[0]["usage"]["output_tokens"] == 5
        assert payloads[0]["usage"]["total_tokens"] == 15
        # Second iteration carries only its own 10 tokens — NOT 25.
        assert payloads[1]["usage"]["input_tokens"] == 7
        assert payloads[1]["usage"]["output_tokens"] == 3
        assert payloads[1]["usage"]["total_tokens"] == 10
        # Run total is the sum of the per-iteration deltas.
        assert (
            payloads[0]["usage"]["total_tokens"]
            + payloads[1]["usage"]["total_tokens"]
            == 25
        )

    # Edge: an iteration with no usage data anywhere — fields are zero,
    # cost / turns reflect whatever the signals say (here: None).
    def test_iteration_without_usage_data_yields_zero_breakdown(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = self._passing_task()
        lifecycle = Lifecycle(task_id="t1", run_id="run-usage-empty")
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                    signals=_make_signals(
                        num_turns=None,
                        total_cost_usd=None,
                    ),
                )
            ]
        )

        _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke))

        payload = self._completed_payload(sink, lifecycle.run_id)
        assert payload["usage"] == {
            "input_tokens": 0,
            "output_tokens": 0,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0,
            "total_tokens": 0,
        }
        assert payload["num_turns"] is None
        assert payload["total_cost_usd"] is None

    # Edge: failed iteration (invoker raised before ResultMessage) still
    # emits the event with whatever pre-failure usage was observed.
    def test_failed_iteration_still_carries_breakdown(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = self._passing_task()
        lifecycle = Lifecycle(task_id="t1", run_id="run-usage-fail")
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=MissingEnvelope(),
                    messages=(
                        _assistant(
                            usage={
                                "input_tokens": 9,
                                "output_tokens": 4,
                            }
                        ),
                    ),
                    failure=InvocationFailure(
                        error_type="ProcessError",
                        message="agent crashed",
                    ),
                    signals=_make_signals(
                        num_turns=None,
                        total_cost_usd=None,
                    ),
                )
            ]
        )

        _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke))

        payload = self._completed_payload(sink, lifecycle.run_id)
        assert payload["failure"] is not None
        assert payload["failure"]["error_type"] == "ProcessError"
        # Breakdown survives the failed iteration.
        assert payload["usage"]["input_tokens"] == 9
        assert payload["usage"]["output_tokens"] == 4
        assert payload["usage"]["total_tokens"] == 13

    # The _build_usage_breakdown helper agrees with _build_observation on
    # total_tokens for the same messages — a unit-level pinpoint for FR-3
    # so a regression localizes to the helper rather than the event path.
    def test_build_usage_breakdown_total_matches_build_observation(
        self,
    ) -> None:
        messages: tuple[Message, ...] = (
            _assistant(usage={"input_tokens": 12, "output_tokens": 8}),
            _assistant(
                usage={
                    "input_tokens": 5,
                    "cache_read_input_tokens": 100,
                }
            ),
            _result_msg(num_turns=2),
        )
        breakdown = _build_usage_breakdown(messages)
        observation = _build_observation(messages, wall_seconds=0.0)
        assert sum(breakdown.values()) == observation.total_tokens

    # Edge: when a ResultMessage reports a larger aggregate than the summed
    # AssistantMessages, its breakdown wins — matches _build_observation's
    # max(running, total_tokens_from_usage(rm.usage)) reconciliation.
    def test_result_message_breakdown_wins_when_larger(self) -> None:
        messages: tuple[Message, ...] = (
            _assistant(usage={"input_tokens": 5, "output_tokens": 5}),
            _result_msg(
                num_turns=1,
                usage={
                    "input_tokens": 100,
                    "output_tokens": 50,
                    "cache_read_input_tokens": 10,
                },
            ),
        )
        breakdown = _build_usage_breakdown(messages)
        assert breakdown == {
            "input_tokens": 100,
            "output_tokens": 50,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 10,
        }


# --- Attempt aggregate rollup (FR-6) ---------------------------------------


class TestAttemptAggregateRollup:
    """The harness rolls token/iteration/turn/cost/last-activity
    aggregates onto the attempt row at each iteration boundary, through
    the store's versioned ``save_attempt`` write."""

    def _passing_task(self) -> Task:
        return Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                )
            ],
        )

    def test_attempt_row_carries_cumulative_aggregates_after_run(
        self,
    ) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = self._passing_task()
        lifecycle = Lifecycle(task_id="t1", run_id="run-agg-roll")
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.CONTINUE),
                    messages=(
                        _assistant(
                            usage={
                                "input_tokens": 10,
                                "output_tokens": 5,
                                "cache_creation_input_tokens": 2,
                            }
                        ),
                        _result_msg(num_turns=3),
                    ),
                    signals=_make_signals(num_turns=3, total_cost_usd=0.10),
                ),
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(
                        _assistant(
                            usage={
                                "input_tokens": 7,
                                "output_tokens": 3,
                                "cache_read_input_tokens": 50,
                            }
                        ),
                        _result_msg(num_turns=2),
                    ),
                    signals=_make_signals(num_turns=2, total_cost_usd=0.05),
                ),
            ]
        )
        config = HarnessConfig(max_iterations_per_attempt=2)

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        assert outcome.lifecycle.status == Status.DONE
        attempt = store.load_attempt(lifecycle.run_id, 1)
        assert attempt is not None
        assert attempt.input_tokens == 17
        assert attempt.output_tokens == 8
        assert attempt.cache_creation_input_tokens == 2
        assert attempt.cache_read_input_tokens == 50
        assert attempt.total_tokens == 77
        assert attempt.iterations_completed == 2
        assert attempt.turns == 5
        assert attempt.total_cost_usd == pytest.approx(0.15)
        assert attempt.last_activity_at is not None
        # Finalization preserved the rollups alongside the outcome.
        assert attempt.outcome == Outcome.SUCCEEDED
        # The folded lifecycle's attempts carry the same aggregates.
        assert outcome.attempts[0].total_tokens == 77

    def test_none_turns_and_cost_roll_up_as_zero(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = self._passing_task()
        lifecycle = Lifecycle(task_id="t1", run_id="run-agg-none")
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                    signals=_make_signals(
                        num_turns=None,
                        total_cost_usd=None,
                    ),
                )
            ]
        )

        _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke))

        attempt = store.load_attempt(lifecycle.run_id, 1)
        assert attempt is not None
        assert attempt.total_tokens == 0
        assert attempt.iterations_completed == 1
        assert attempt.turns == 0
        assert attempt.total_cost_usd == 0.0
        assert attempt.last_activity_at is not None


# --- Operator interruption ------------------------------------------------


class TestOperatorInterruption:
    def _signal_killed_grader_invocation(self, signal_no: int) -> Task:
        # run_command_graders uses shell=True, so the immediate child is
        # bash, which would normally exit with 128+signal_no when its
        # own child dies by signal. `exec` lets the python child replace
        # the shell, so Popen's returncode reflects the signal directly
        # (negative). SIG_DFL bypasses Python's SIGINT->KeyboardInterrupt
        # conversion so signal 2 terminates like SIGTERM.
        run = (
            f"exec {sys.executable} -c "
            f"\"import os, signal; "
            f"signal.signal({signal_no}, signal.SIG_DFL); "
            f"os.kill(os.getpid(), {signal_no})\""
        )
        return Task(
            goal="g",
            graders=[CommandGrader(run=run, name="signaled")],
        )

    def test_sigint_killed_command_grader_routes_to_interrupted(
        self,
    ) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = self._signal_killed_grader_invocation(2)
        lifecycle = Lifecycle(task_id="t1", run_id="run-sigint-grader")
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                )
            ]
        )
        config = HarnessConfig(max_retries=1)

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        # INTERRUPTED is not a retry-source: the harness must break out
        # of the loop without consuming the retry budget.
        assert outcome.lifecycle.status == Status.INTERRUPTED
        assert outcome.lifecycle.retries == 0
        assert len(outcome.attempts) == 1
        attempt = outcome.attempts[0]
        assert attempt.outcome == Outcome.INTERNAL_ERROR
        assert "operator interrupted" in attempt.error
        assert "signal 2" in attempt.error
        # A single harness.crash event with classification grader_signaled
        # must record the kill — that is the audit-visible distinguisher
        # from a real grader failure.
        events = sink.events(lifecycle.run_id)
        crash = [e for e in events if e.kind == "harness.crash"]
        assert len(crash) == 1
        assert crash[0].payload["classification"] == "grader_signaled"
        assert crash[0].payload["signal"] == 2

    def test_sigterm_killed_command_grader_routes_to_interrupted(
        self,
    ) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = self._signal_killed_grader_invocation(15)
        lifecycle = Lifecycle(task_id="t1", run_id="run-sigterm-grader")
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                )
            ]
        )

        outcome = _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke))

        assert outcome.lifecycle.status == Status.INTERRUPTED
        attempt = outcome.attempts[0]
        assert attempt.outcome == Outcome.INTERNAL_ERROR
        crash = [
            e for e in sink.events(lifecycle.run_id)
            if e.kind == "harness.crash"
        ]
        assert len(crash) == 1
        assert crash[0].payload["signal"] == 15

    def test_non_operator_signal_still_routes_to_failed_validation(
        self,
    ) -> None:
        """A grader that segfaults (signal 11) is a real failure, not
        operator interruption — must consume retry budget like any other
        validation failure."""
        store = InMemoryStore()
        sink = _ListSink()
        task = self._signal_killed_grader_invocation(11)
        lifecycle = Lifecycle(task_id="t1", run_id="run-sigsegv-grader")
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                )
            ]
        )
        config = HarnessConfig(max_retries=0)

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        assert outcome.lifecycle.status == Status.FAILED
        attempt = outcome.attempts[0]
        assert attempt.outcome == Outcome.VALIDATION_FAILED


class TestIterationAwareInterrupt:
    """Spec 00012: an operator SIGINT/SIGTERM while the agent is mid-
    iteration deterministically finalizes the in-flight lifecycle to
    INTERRUPTED, no stranded `running` state, and the run is resumable.

    The invoker's stream loop raises :exc:`asyncio.CancelledError` (the
    signal-handler-cancelled-task shape used in :func:`run_task_object`).
    The harness catches this at the :func:`_run_attempt` boundary,
    finalizes through :func:`_handle_interrupt`, and re-raises so the
    worker stops cleanly. :func:`finalize_stranded_lifecycle` remains the
    SIGKILL/OOM/reboot backstop -- this in-band path closes the graceful
    SIGINT/SIGTERM gap documented in
    ``.flywheel/audits/02-harness-resilience.md``.
    """

    def _cancelling_invoker(
        self,
        *,
        pre_messages: tuple[Message, ...] = (),
    ) -> Callable[[InvocationRequest], Awaitable[IterationResult]]:
        """Build an invoker that persists ``pre_messages`` via on_message,
        then raises :exc:`asyncio.CancelledError` -- the asyncio cancellation
        a SIGINT/SIGTERM-cancelled task produces inside the invoker's
        stream loop."""

        async def _invoke(request: InvocationRequest) -> IterationResult:
            if request.on_message is not None:
                for msg in pre_messages:
                    request.on_message(msg)
            raise asyncio.CancelledError()

        return _invoke

    def test_cancelled_mid_stream_finalizes_interrupted(self) -> None:
        # FR-1: mid-stream cancellation leaves the lifecycle in INTERRUPTED
        # rather than stranded `running`.
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                    name="ok",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-interrupt-mid")
        invoke = self._cancelling_invoker(pre_messages=(_assistant(),))

        with pytest.raises(asyncio.CancelledError):
            _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke))

        reloaded = store.load_lifecycle(lifecycle.run_id)
        assert reloaded is not None
        assert reloaded.status == Status.INTERRUPTED
        # No retry consumed -- INTERRUPTED is not a retry-source state.
        assert reloaded.retries == 0

    def test_cancelled_attempt_finalizes_as_internal_error(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(goal="g", graders=[])
        lifecycle = Lifecycle(task_id="t1", run_id="run-interrupt-attempt")
        invoke = self._cancelling_invoker()

        with pytest.raises(asyncio.CancelledError):
            _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke))

        attempts = store.list_attempts(lifecycle.run_id)
        assert len(attempts) == 1
        attempt = attempts[0]
        assert attempt.ended_at is not None
        assert attempt.outcome == Outcome.INTERNAL_ERROR
        assert "operator interrupted" in attempt.error

    def test_pre_interrupt_messages_preserved(self) -> None:
        # FR-2: messages persisted before the interrupt (live via 00010)
        # remain in the store after finalization.
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(goal="g", graders=[])
        lifecycle = Lifecycle(task_id="t1", run_id="run-interrupt-msgs")
        pre = (
            _assistant(text="pre-1"),
            _assistant(text="pre-2"),
        )
        invoke = self._cancelling_invoker(pre_messages=pre)

        with pytest.raises(asyncio.CancelledError):
            _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke))

        rows = sink.messages(lifecycle.run_id)
        # Both assistant messages observed before cancellation must still be
        # in the audit trail; on_message persists immediately.
        assert len(rows) == 2
        assert rows[0].payload.get("message_type") == "AssistantMessage"
        assert rows[1].payload.get("message_type") == "AssistantMessage"

    def test_harness_interrupted_event_emitted(self) -> None:
        # FR-4: an observability event records the exogenous stop.
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(goal="g", graders=[])
        lifecycle = Lifecycle(task_id="t1", run_id="run-interrupt-event")
        invoke = self._cancelling_invoker()

        with pytest.raises(asyncio.CancelledError):
            _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke))

        events = sink.events(lifecycle.run_id)
        interrupted = [e for e in events if e.kind == "harness.interrupted"]
        assert len(interrupted) == 1
        payload = interrupted[0].payload
        assert payload["classification"] == "worker_interrupted"
        # mid-stream cancellation lands while still in RUNNING.
        assert payload["from_status"] == "running"
        assert interrupted[0].attempt_number == 1
        # The attempt's terminal event ordering: attempt_finalized lands
        # before the lifecycle transitions to INTERRUPTED so the audit
        # stream is internally consistent.
        kinds = [e.kind for e in events]
        assert "harness.attempt_started" in kinds
        assert "harness.attempt_finalized" in kinds

    def test_interrupted_lifecycle_is_resumable(self) -> None:
        # FR-3: an INTERRUPTED lifecycle can return to READY and run
        # again without losing prior execution history.
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(goal="g", graders=[])
        lifecycle = Lifecycle(task_id="t1", run_id="run-interrupt-resume")
        invoke_cancel = self._cancelling_invoker(pre_messages=(_assistant(),))

        with pytest.raises(asyncio.CancelledError):
            _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke_cancel))

        # Sanity: first attempt interrupted, lifecycle parked at INTERRUPTED.
        first = store.load_lifecycle(lifecycle.run_id)
        assert first is not None
        assert first.status == Status.INTERRUPTED
        first_messages = sink.messages(lifecycle.run_id)
        assert len(first_messages) == 1

        # Resume on the same run_id with a fresh, non-cancelling invoker;
        # the harness's entry-time normalization carries INTERRUPTED -> READY
        # before the next attempt starts.
        invoke_ok = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(text="resumed"), _result_msg()),
                )
            ]
        )
        # Stale in-memory copy: replace_from inside run_task reconciles it
        # against the persisted INTERRUPTED row.
        resume_lifecycle = Lifecycle(
            task_id="t1", run_id=lifecycle.run_id
        )
        outcome = _run(
            run_task(task, resume_lifecycle, store, sink=sink, invoke=invoke_ok)
        )
        assert outcome.lifecycle.status == Status.DONE
        attempts = store.list_attempts(lifecycle.run_id)
        # Two attempts: the interrupted one plus the resumed one. Prior
        # execution history is preserved -- the interrupt did not clobber
        # the first attempt's row.
        assert len(attempts) == 2
        assert attempts[0].outcome == Outcome.INTERNAL_ERROR
        assert attempts[1].outcome == Outcome.SUCCEEDED
        # SDK messages from both attempts remain.
        all_messages = sink.messages(lifecycle.run_id)
        assert len(all_messages) >= 2

    def test_no_stranded_running_after_interrupt(self) -> None:
        # FR-5 (in-band guarantee): after an interrupt the in-band finalizer
        # leaves no lifecycle in `running` -- finalize_stranded_lifecycle is
        # a no-op because there is nothing left to finalize.
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(goal="g", graders=[])
        lifecycle = Lifecycle(task_id="t1", run_id="run-interrupt-clean")
        invoke = self._cancelling_invoker()

        with pytest.raises(asyncio.CancelledError):
            _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke))

        reloaded = store.load_lifecycle(lifecycle.run_id)
        assert reloaded is not None
        assert reloaded.status == Status.INTERRUPTED
        # finalize_stranded_lifecycle should find nothing to do.
        assert finalize_stranded_lifecycle(store, lifecycle.run_id) is False
        # Status untouched by the no-op backstop call.
        still = store.load_lifecycle(lifecycle.run_id)
        assert still is not None
        assert still.status == Status.INTERRUPTED

    def test_handle_interrupt_is_idempotent(self) -> None:
        # FR-5 idempotency: a second signal during shutdown must not corrupt
        # the finalization or raise. _handle_interrupt is the sole writer of
        # the interruption path, so calling it twice on the same lifecycle
        # is the unit-level test of the idempotency contract.
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(goal="g", graders=[])
        lifecycle = Lifecycle(task_id="t1", run_id="run-interrupt-idemp")
        invoke = self._cancelling_invoker()

        with pytest.raises(asyncio.CancelledError):
            _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke))

        # Capture state after first finalization.
        first_events = sink.events(lifecycle.run_id)
        first_attempts = store.list_attempts(lifecycle.run_id)
        first_version = store.load_lifecycle(lifecycle.run_id).version  # type: ignore[union-attr]
        first_interrupted_count = sum(
            1 for e in first_events if e.kind == "harness.interrupted"
        )

        # Simulate a second signal arriving during shutdown by calling the
        # helper again on the now-INTERRUPTED lifecycle. Must not raise,
        # must not double-write events, must not advance state.
        reloaded = store.load_lifecycle(lifecycle.run_id)
        assert reloaded is not None
        _handle_interrupt(
            store=store,
            telemetry=_RunTelemetry(
                sink,
                run_id=lifecycle.run_id,
                clock=lambda: datetime.now(timezone.utc),
            ),
            lifecycle=reloaded,
            attempt=None,
            clock=lambda: datetime.now(timezone.utc),
        )

        second_events = sink.events(lifecycle.run_id)
        second_attempts = store.list_attempts(lifecycle.run_id)
        second_version = store.load_lifecycle(lifecycle.run_id).version  # type: ignore[union-attr]
        second_interrupted_count = sum(
            1 for e in second_events if e.kind == "harness.interrupted"
        )
        assert len(second_events) == len(first_events)
        assert len(second_attempts) == len(first_attempts)
        assert second_version == first_version
        assert second_interrupted_count == first_interrupted_count == 1

    def test_interrupt_during_validating_finalizes_interrupted(
        self, tmp_path: Path
    ) -> None:
        # Edge case from the spec: interrupt during `validating` (after
        # invoke, before / inside graders) finalizes INTERRUPTED; graders
        # do not get an opportunity to mark the attempt validation_failed.
        # The rubric judge's await is the only cancellation point inside
        # validating -- command graders are synchronous, so cancellation
        # cannot land between _transition(VALIDATING) and the judge call.
        store = InMemoryStore()
        sink = _ListSink()

        async def _cancelling_judge(
            prompt: str, grader: RubricGrader, worktree: Any
        ) -> str:
            raise asyncio.CancelledError()

        task = Task(
            goal="g",
            graders=[
                _ok_command(),
                RubricGrader(assertions=["a"], name="r0"),
            ],
        )
        lifecycle = Lifecycle(
            task_id="t1", run_id="run-interrupt-validating"
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                )
            ]
        )

        with pytest.raises(asyncio.CancelledError):
            _run(
                run_task(
                    task,
                    lifecycle,
                    store,
                    sink=sink,
                    config=HarnessConfig(
                        worktree=str(tmp_path),
                        rubric_judge_invoke=_cancelling_judge,
                    ),
                    invoke=invoke,
                )
            )

        reloaded = store.load_lifecycle(lifecycle.run_id)
        assert reloaded is not None
        assert reloaded.status == Status.INTERRUPTED
        attempt = store.list_attempts(lifecycle.run_id)[0]
        assert attempt.outcome == Outcome.INTERNAL_ERROR
        interrupted_events = [
            e
            for e in sink.events(lifecycle.run_id)
            if e.kind == "harness.interrupted"
        ]
        assert len(interrupted_events) == 1
        # Cancellation landed after the harness had advanced to VALIDATING
        # to run graders.
        assert interrupted_events[0].payload["from_status"] == "validating"


class TestFinalizeStranded:
    def _seed_running(self, store: InMemoryStore, run_id: str) -> Lifecycle:
        now = datetime.now(timezone.utc)
        lc = Lifecycle(task_id="t", run_id=run_id)
        lc.transition_to(Status.READY, now=now)
        lc.transition_to(Status.RUNNING, now=now)
        store.create_lifecycle(lc)
        store.save_attempt(
            run_id,
            Attempt(number=1, started_at=now, run_id=run_id),
        )
        return lc

    def _seed_validating(
        self, store: InMemoryStore, run_id: str
    ) -> Lifecycle:
        now = datetime.now(timezone.utc)
        lc = Lifecycle(task_id="t", run_id=run_id)
        lc.transition_to(Status.READY, now=now)
        lc.transition_to(Status.RUNNING, now=now)
        lc.transition_to(Status.VALIDATING, now=now)
        store.create_lifecycle(lc)
        store.save_attempt(
            run_id,
            Attempt(number=1, started_at=now, run_id=run_id),
        )
        return lc

    def test_finalize_running_transitions_to_interrupted(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        lc = self._seed_running(store, "run-strand-r")
        ok = finalize_stranded_lifecycle(store, lc.run_id, sink=sink)
        assert ok is True
        reloaded = store.load_lifecycle(lc.run_id)
        assert reloaded is not None
        assert reloaded.status == Status.INTERRUPTED
        attempts = store.list_attempts(lc.run_id)
        assert attempts[0].ended_at is not None
        assert attempts[0].outcome == Outcome.INTERNAL_ERROR
        crash = [
            e for e in sink.events(lc.run_id)
            if e.kind == "harness.crash"
        ]
        assert len(crash) == 1
        assert crash[0].payload["classification"] == "worker_interrupted"
        assert crash[0].payload["from_status"] == "running"

    def test_finalize_validating_transitions_to_interrupted(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        lc = self._seed_validating(store, "run-strand-v")
        ok = finalize_stranded_lifecycle(store, lc.run_id, sink=sink)
        assert ok is True
        reloaded = store.load_lifecycle(lc.run_id)
        assert reloaded is not None
        assert reloaded.status == Status.INTERRUPTED

    def test_finalize_done_lifecycle_is_noop(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        now = datetime.now(timezone.utc)
        lc = Lifecycle(task_id="t", run_id="run-strand-done")
        lc.transition_to(Status.READY, now=now)
        lc.transition_to(Status.RUNNING, now=now)
        lc.transition_to(Status.VALIDATING, now=now)
        lc.transition_to(Status.DONE, now=now)
        store.create_lifecycle(lc)
        ok = finalize_stranded_lifecycle(store, lc.run_id, sink=sink)
        assert ok is False
        reloaded = store.load_lifecycle(lc.run_id)
        assert reloaded is not None
        assert reloaded.status == Status.DONE

    def test_finalize_missing_lifecycle_is_noop(self) -> None:
        store = InMemoryStore()

        ok = finalize_stranded_lifecycle(store, "run-does-not-exist")
        assert ok is False

    def test_finalize_awaiting_approval_lifecycle_is_noop(self) -> None:
        """An ``AWAITING_APPROVAL`` lifecycle is a durable park, not a
        stranded mid-attempt — the attempt was finalized ``SUCCEEDED``
        at gate entry per spec 00016 FR-4. ``finalize_stranded_lifecycle``
        must leave the parked status, the awaiting-gate ordinal, and the
        already-finalized attempt untouched so a manual gate survives
        worker restart (FR-9)."""
        store = InMemoryStore()
        sink = _ListSink()
        now = datetime.now(timezone.utc)
        lc = Lifecycle(task_id="t", run_id="run-strand-awaiting")
        lc.transition_to(Status.READY, now=now)
        lc.transition_to(Status.RUNNING, now=now)
        lc.transition_to(Status.VALIDATING, now=now)
        lc.transition_to(Status.AWAITING_APPROVAL, now=now)
        # The harness's gate-entry path sets this via an AwaitingApproval
        # domain event; the seed bypasses event sourcing and writes it
        # directly so the persisted column matches what the resolver
        # would later read.
        lc.awaiting_manual_ordinal = 1
        store.create_lifecycle(lc)
        # The attempt was finalized SUCCEEDED at gate entry (FR-4), so
        # the open-attempt strand rule is unaffected — only the parked
        # status needs exempting.
        store.save_attempt(
            lc.run_id,
            Attempt(
                number=1,
                started_at=now,
                run_id=lc.run_id,
                ended_at=now,
                outcome=Outcome.SUCCEEDED,
            ),
        )

        ok = finalize_stranded_lifecycle(store, lc.run_id, sink=sink)
        assert ok is False

        reloaded = store.load_lifecycle(lc.run_id)
        assert reloaded is not None
        assert reloaded.status == Status.AWAITING_APPROVAL
        assert reloaded.awaiting_manual_ordinal == 1

        # The finalized attempt is not re-finalized: ended_at and
        # outcome are preserved verbatim from gate entry.
        attempts = store.list_attempts(lc.run_id)
        assert len(attempts) == 1
        assert attempts[0].outcome == Outcome.SUCCEEDED
        assert attempts[0].ended_at == now

        # No crash event was emitted — the no-op returns before any
        # _emit / _transition / _finalize_attempt call.
        crash = [
            e for e in sink.events(lc.run_id)
            if e.kind == "harness.crash"
        ]
        assert crash == []


# --- Entry-time crash recording ------------------------------------------


class TestEntryTimeCrash:
    """run_task must persist a lifecycle row and a harness.crash event
    when a Python exception escapes after the lifecycle row exists.

    Backs the audit finding in
    ``.flywheel/audits/08-recoverable-blocked-lifecycles.md``: 76
    crashed run_ids produced zero rows in lifecycles/attempts/events
    because the failure happened before any DB write. With the
    create-first ordering and the top-level handler in run_task, the
    lifecycle is always persisted and harness.crash records the
    classification before the exception propagates.
    """

    def _raising_invoke(
        self, exc: BaseException
    ) -> Callable[[InvocationRequest], Awaitable[IterationResult]]:
        async def _invoke(_request: InvocationRequest) -> IterationResult:
            raise exc

        return _invoke

    def test_invoke_exception_writes_crash_event_and_lifecycle_row(
        self,
    ) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-entry-crash")
        invoke = self._raising_invoke(RuntimeError("transport blew up"))

        with pytest.raises(RuntimeError, match="transport blew up"):
            _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke))

        # Lifecycle row exists despite the crash -- the create-first
        # ordering is the whole point.
        reloaded = store.load_lifecycle(lifecycle.run_id)
        assert reloaded is not None
        # Terminal status: we walk RUNNING -> FAILED.
        assert reloaded.status == Status.FAILED
        assert "RuntimeError" in reloaded.error
        assert "transport blew up" in reloaded.error

        # Exactly one harness.crash event with the entry_error
        # classification.
        crash_events = [
            e
            for e in sink.events(lifecycle.run_id)
            if e.kind == "harness.crash"
        ]
        assert len(crash_events) == 1
        payload = crash_events[0].payload
        assert payload["classification"] == "entry_error"
        assert payload["exception_type"] == "RuntimeError"
        assert payload["message"] == "transport blew up"

    def test_no_duplicate_crash_event_when_invoke_raises_mid_attempt(
        self,
    ) -> None:
        """attempt_started fired, then invoke raises -- exactly one
        harness.crash event must land, never two."""
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-crash-no-dup")
        invoke = self._raising_invoke(ValueError("boom"))

        with pytest.raises(ValueError, match="boom"):
            _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke))

        events = sink.events(lifecycle.run_id)
        kinds = [e.kind for e in events]
        # attempt_started must have fired before the crash (proves the
        # crash happened mid-attempt, not before any harness work).
        assert "harness.attempt_started" in kinds
        # But only one crash event, despite the attempt being open at
        # the time the exception was raised.
        crash_events = [e for e in events if e.kind == "harness.crash"]
        assert len(crash_events) == 1
        assert crash_events[0].payload["classification"] == "entry_error"

    def test_resume_with_persisted_row_does_not_emit_crash(self) -> None:
        """A caller-supplied stale Lifecycle whose row is already
        persisted must reconcile to the persisted state, run normally,
        and produce no harness.crash event."""
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                    name="ok",
                )
            ],
        )
        # First run pauses on blocked; persists row at version > 1.
        lifecycle = Lifecycle(task_id="t1", run_id="run-resume-clean")
        invoke_first = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(
                        intent=Intent.BLOCKED,
                        reason="paused",
                        requires=(CommandGraderRequirement(name="ok"),),
                    ),
                    messages=(_assistant(),),
                )
            ]
        )
        _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke_first))
        assert lifecycle.status == Status.INTERRUPTED
        persisted = store.load_lifecycle(lifecycle.run_id)
        assert persisted is not None
        canonical_version = persisted.version
        assert canonical_version > 1

        # Build a stale Lifecycle for the same run_id; the caller may
        # have constructed it fresh without loading the row.
        stale = Lifecycle(task_id="t1", run_id="run-resume-clean")
        assert stale.status == Status.PENDING
        assert stale.version == 1
        invoke_second = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                )
            ]
        )
        outcome = _run(
            run_task(task, stale, store, sink=sink, invoke=invoke_second)
        )

        # Reconciliation: the stale Lifecycle's status/version were
        # overwritten with the persisted row's values before the loop
        # started.
        assert outcome.lifecycle.status == Status.DONE
        # No harness.crash event from the resume path.
        crash_events = [
            e
            for e in sink.events(stale.run_id)
            if e.kind == "harness.crash"
        ]
        assert crash_events == []

    def test_original_exception_propagates_after_crash_recorded(
        self,
    ) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-propagate")
        sentinel = RuntimeError("propagate me unchanged")
        invoke = self._raising_invoke(sentinel)

        # The caller observes the original exception, not a wrapper.
        with pytest.raises(RuntimeError) as excinfo:
            _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke))
        assert excinfo.value is sentinel

    def test_entry_crash_finalizes_open_attempt(self) -> None:
        """A mid-attempt entry crash walks the lifecycle to terminal FAILED;
        the open attempt must be finalized (INTERNAL_ERROR), never left with
        ``ended_at=None`` under a terminal lifecycle that finalize_stranded_
        lifecycle (RUNNING/VALIDATING only) would never repair."""
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-strand")
        invoke = self._raising_invoke(OSError("too many open files"))

        with pytest.raises(OSError):
            _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke))

        reloaded = store.load_lifecycle(lifecycle.run_id)
        assert reloaded is not None
        assert reloaded.status == Status.FAILED

        attempts = store.list_attempts(lifecycle.run_id)
        assert len(attempts) == 1
        # The attempt is closed, not stranded.
        assert attempts[0].ended_at is not None
        assert attempts[0].outcome == Outcome.INTERNAL_ERROR

        # And the crash event landed before the propagation.
        crash_events = [
            e
            for e in sink.events(lifecycle.run_id)
            if e.kind == "harness.crash"
        ]
        assert len(crash_events) == 1


# --- Audit-stream: SDK message persistence + strict-audit failure --------


class _RaisingSink:
    """TelemetrySink that raises on every append (or only the first N).

    Used by the telemetry-failure-semantics tests (spec 00025 FR-7): a
    sink failure must never abort or finalize the attempt. With
    ``fail_first`` set, only the first ``fail_first`` appends raise and
    the sink recovers afterwards, capturing later records — that mode
    exercises the best-effort marker line the harness drops into the
    sink after the first failure.
    """

    def __init__(self, *, fail_first: int | None = None) -> None:
        self.attempts = 0
        self.fail_first = fail_first
        self.records: list[TelemetryRecord] = []

    def append_telemetry(self, record: TelemetryRecord) -> None:
        self.attempts += 1
        if self.fail_first is None or self.attempts <= self.fail_first:
            raise RuntimeError("simulated sink append failure")
        self.records.append(record)


class _DomainRaisingStore(InMemoryStore):
    """In-memory store whose ledger append raises on a chosen event kind.

    Used to pin the strict half of the FR-7 split: a domain-event write
    failure is a ledger failure and must keep aborting the run, unlike
    telemetry loss.
    """

    def __init__(self, *, raise_on_kind: str) -> None:
        super().__init__()
        self.raise_on_kind = raise_on_kind

    def append_domain_event(self, event, *, expected_version):  # type: ignore[override]
        if event.KIND.value == self.raise_on_kind:
            raise RuntimeError("simulated ledger append failure")
        return super().append_domain_event(
            event, expected_version=expected_version
        )


class TestSdkMessagePersistence:
    def test_each_iteration_persists_its_message_batch(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-sdk-persist")
        iter1_msgs: tuple[Message, ...] = (
            _assistant(text="iter-1-a"),
        )
        iter2_msgs: tuple[Message, ...] = (
            _assistant(text="iter-2-a"),
            _assistant(text="iter-2-b"),
            _result_msg(num_turns=2),
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.CONTINUE),
                    messages=iter1_msgs,
                ),
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=iter2_msgs,
                ),
            ]
        )
        config = HarnessConfig(max_iterations_per_attempt=2)

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )
        assert outcome.lifecycle.status == Status.DONE

        listed = sink.messages(lifecycle.run_id)
        # All 4 messages across the two iterations, in input order.
        assert len(listed) == len(iter1_msgs) + len(iter2_msgs)
        types = [m.kind for m in listed]
        assert types == [
            "AssistantMessage",
            "AssistantMessage",
            "AssistantMessage",
            "ResultMessage",
        ]
        # Iteration scoping is preserved.
        iter1 = [m for m in listed if m.iteration_number == 1]
        iter2 = [m for m in listed if m.iteration_number == 2]
        assert len(iter1) == 1
        assert len(iter2) == 3

    def test_zero_message_iteration_persists_empty_batch(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-sdk-empty")
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(),
                )
            ]
        )

        _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke))
        # No SDK messages recorded but iteration_completed still fired.
        assert sink.messages(lifecycle.run_id) == []
        completed = [
            e
            for e in sink.events(lifecycle.run_id)
            if e.kind == "harness.iteration_completed"
        ]
        assert len(completed) == 1


class TestTelemetryStreamOrdering:
    def test_run_stream_interleaves_messages_telemetry_and_domain_mirrors(
        self,
    ) -> None:
        """Spec 00025: sink emission order is the canonical observability
        ordering — SDK messages land before their iteration's
        ``harness.iteration_completed``, every ledger append is mirrored
        as a ``domain.*`` line, and the store row count matches the
        mirror count (row first, line second)."""
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-interleaved")
        messages: tuple[Message, ...] = (
            _assistant(text="a"),
            _assistant(text="b"),
            _result_msg(num_turns=2),
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=messages,
                )
            ]
        )

        _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke))

        kinds = [r.kind for r in sink.records]
        # Per-message cadence: every streamed SDK message precedes the
        # iteration_completed record of its iteration.
        completed_idx = kinds.index("harness.iteration_completed")
        message_indices = [
            i
            for i, r in enumerate(sink.records)
            if r.kind in ("AssistantMessage", "ResultMessage")
        ]
        assert len(message_indices) == len(messages)
        assert all(i < completed_idx for i in message_indices)
        # Every ledger append is mirrored as a domain.* line, store row
        # first: counts agree and the mirrored kinds match the rows.
        domain_rows = store.list_domain_events(lifecycle.run_id)
        mirrors = sink.domain_mirrors(lifecycle.run_id)
        assert [m.kind for m in mirrors] == [
            f"domain.{e.KIND.value}" for e in domain_rows
        ]
        # The store holds no telemetry: the legacy telemetry verbs are
        # gone from the surface entirely (spec 00025 FR-5).
        assert not hasattr(store, "list_events")
        assert not hasattr(store, "list_sdk_messages")

    def test_harness_domain_log_replays_to_the_stored_lifecycle(
        self,
    ) -> None:
        """End-to-end determinism oracle: folding the domain-event log the
        harness produced reconstructs the persisted lifecycle exactly. This
        is the event-sourcing guarantee at the harness level — state is the
        fold of the log, with no separate authoritative row.

        The attempt aggregate counters (tokens, iterations, turns, cost,
        last activity) are the one deliberate exception: they are OLTP
        rollups written at iteration boundaries outside the ledger, so the
        oracle compares lifecycles with aggregates normalized to their
        defaults."""
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-oracle")
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                )
            ]
        )

        outcome = _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke))
        assert outcome.lifecycle.status is Status.DONE

        loaded = store.load_lifecycle("run-oracle")
        folded = replay(store.list_domain_events("run-oracle"))
        assert loaded is not None
        _strip_attempt_aggregates(loaded, folded)
        assert loaded == folded
        # version is the domain-event offset.
        assert loaded.version == len(store.list_domain_events("run-oracle"))


class TestTelemetryFailureSemantics:
    """Spec 00025 FR-7: telemetry loss is non-fatal; ledger loss stays fatal."""

    def _task(self) -> Task:
        return Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                )
            ],
        )

    def test_sink_failure_never_aborts_the_run(
        self, capsys: pytest.CaptureFixture[str]
    ) -> None:
        """A sink that raises on every append yields a completed DONE run
        with correct lifecycle state; the first failure is recorded once
        on stderr and subsequent failures are silent."""
        store = InMemoryStore()
        sink = _RaisingSink()
        task = self._task()
        lifecycle = Lifecycle(task_id="t1", run_id="run-sink-fail")
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                )
            ]
        )

        outcome = _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke))

        assert outcome.lifecycle.status == Status.DONE
        assert outcome.attempts[0].outcome == Outcome.SUCCEEDED
        # The sink was attempted repeatedly (messages, telemetry,
        # mirrors) but the failure was reported exactly once.
        assert sink.attempts > 1
        err = capsys.readouterr().err
        assert err.count("telemetry sink append failed") == 1
        assert "run-sink-fail" in err
        # Grader receipts (ledger) are unaffected.
        assert len(store.list_grader_results(lifecycle.run_id, 1)) == 1

    def test_first_failure_drops_marker_line_when_sink_recovers(
        self, capsys: pytest.CaptureFixture[str]
    ) -> None:
        """After the first failed append the harness attempts a marker
        line in the sink itself; a sink that recovers carries it."""
        store = InMemoryStore()
        sink = _RaisingSink(fail_first=1)
        task = self._task()
        lifecycle = Lifecycle(task_id="t1", run_id="run-sink-marker")
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(),),
                )
            ]
        )

        outcome = _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke))

        assert outcome.lifecycle.status == Status.DONE
        marker = [
            r
            for r in sink.records
            if r.kind == "harness.telemetry_sink_failed"
        ]
        assert len(marker) == 1
        assert marker[0].payload["error_type"] == "RuntimeError"
        assert capsys.readouterr().err.count(
            "telemetry sink append failed"
        ) == 1

    def test_domain_append_failure_stays_fatal(self) -> None:
        """The strict half of the split: a ledger write failure aborts
        the run — the simulated store error propagates to the caller
        rather than being swallowed like telemetry loss."""
        store = _DomainRaisingStore(raise_on_kind="attempt_finalized")
        sink = _ListSink()
        task = self._task()
        lifecycle = Lifecycle(task_id="t1", run_id="run-ledger-fail")
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(),),
                )
            ]
        )

        with pytest.raises(RuntimeError, match="simulated ledger append"):
            _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke))


class TestFinalizeStrandedTelemetryOrdering:
    def test_finalize_stranded_streams_events_in_emission_order(
        self,
    ) -> None:
        """Sink emission order is the canonical observability ordering:
        the stranded finalize streams attempt_finalized before crash,
        and the ledger appends it makes are mirrored as domain.* lines
        in the same stream."""
        store = InMemoryStore()
        sink = _ListSink()
        now = datetime.now(timezone.utc)
        lc = Lifecycle(task_id="t", run_id="run-strand-seq")
        lc.transition_to(Status.READY, now=now)
        lc.transition_to(Status.RUNNING, now=now)
        store.create_lifecycle(lc)
        store.save_attempt(
            lc.run_id,
            Attempt(number=1, started_at=now, run_id=lc.run_id),
        )

        ok = finalize_stranded_lifecycle(store, lc.run_id, sink=sink)
        assert ok is True

        kinds = [e.kind for e in sink.events(lc.run_id)]
        assert kinds == ["harness.attempt_finalized", "harness.crash"]
        mirror_kinds = [m.kind for m in sink.domain_mirrors(lc.run_id)]
        assert mirror_kinds == [
            "domain.attempt_finalized",
            "domain.transitioned_to",
        ]


# --- TODO subsystems remain deferred --------------------------------------


class TestDeferredSubsystems:
    def test_deferred_subsystems_are_listed_and_not_silently_implemented(
        self,
    ) -> None:
        """The harness must not silently implement loop.md TODO subsystems.

        The exported deferred-subsystems list is the canonical statement
        that these remain TODO. Confirms the list is present and matches
        the spec.
        """
        from flywheel_core import harness as harness_module

        expected = {
            "thrash net-diff detection (sub-problem b)",
            "thrash input-novelty score (sub-problem c)",
            "hang threshold default value (mechanism shipped, value ungrounded)",
            "fine-grained crash classification",
            "blocked_implicit same-question-re-asked detection",
        }
        assert set(harness_module._DEFERRED_LOOP_SUBSYSTEMS) == expected


# --- Rubric grader integration -------------------------------------------


def _rubric_wrap(payload: str) -> str:
    return f"{RUBRIC_OPENING_FENCE}\n{payload}\n{RUBRIC_CLOSING_FENCE}"


class _ScriptedJudge:
    """Fake ``judge_invoke`` returning canned responses, recording calls."""

    def __init__(self, responses: list[str | Exception]) -> None:
        self._responses: list[str | Exception] = list(responses)
        self.calls: list[tuple[str, RubricGrader, Any]] = []

    async def __call__(
        self,
        prompt: str,
        grader: RubricGrader,
        worktree: Any,
    ) -> str:
        self.calls.append((prompt, grader, worktree))
        if not self._responses:
            raise AssertionError("scripted judge ran out of responses")
        head = self._responses.pop(0)
        if isinstance(head, Exception):
            raise head
        return head


def _ok_command() -> CommandGrader:
    return CommandGrader(
        run=f"{sys.executable} -c 'raise SystemExit(0)'",
        name="ok",
    )


class TestRubricIntegration:
    # Command graders run in config.worktree, so the sandbox must be a real
    # directory. Each test gets a fresh tmp_path-backed worktree via this
    # autouse fixture rather than a hardcoded path that may not exist.
    @pytest.fixture(autouse=True)
    def _worktree(self, tmp_path: Path) -> None:
        self._wt = str(tmp_path)

    def test_all_pass_rubric_reaches_done(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                _ok_command(),
                RubricGrader(assertions=["a"], name="r0"),
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-rubric-done")
        judge = _ScriptedJudge(
            [_rubric_wrap('{"passed": true, "summary": "ok"}')]
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                )
            ]
        )
        config = HarnessConfig(
            worktree=self._wt,
            rubric_judge_invoke=judge,
        )

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        assert outcome.lifecycle.status == Status.DONE
        assert outcome.attempts[-1].outcome == Outcome.SUCCEEDED
        rows = store.list_grader_results(lifecycle.run_id, 1)
        assert [r.grader_type for r in rows] == ["command", "rubric"]
        assert all(r.passed for r in rows)

    def test_rubric_fail_with_retry_on_fail_true_consumes_retry(
        self,
    ) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                _ok_command(),
                RubricGrader(
                    assertions=["a"], name="semantics", retry_on_fail=True
                ),
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-rubric-retry")
        judge = _ScriptedJudge(
            [
                _rubric_wrap('{"passed": false, "summary": "wrong file"}'),
                _rubric_wrap('{"passed": true, "summary": "fixed"}'),
            ]
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                ),
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                ),
            ]
        )
        config = HarnessConfig(
            max_retries=1,
            worktree=self._wt,
            rubric_judge_invoke=judge,
        )

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        assert outcome.lifecycle.status == Status.DONE
        assert outcome.lifecycle.retries == 1
        assert len(outcome.attempts) == 2
        assert outcome.attempts[0].outcome == Outcome.VALIDATION_FAILED
        assert outcome.attempts[0].error == (
            "rubric grader 'semantics' failed"
        )

    def test_rubric_fail_with_retry_on_fail_false_interrupts(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                _ok_command(),
                RubricGrader(
                    assertions=["a"], name="halt-me", retry_on_fail=False
                ),
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-rubric-interrupt")
        judge = _ScriptedJudge(
            [_rubric_wrap('{"passed": false, "summary": "park me"}')]
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                )
            ]
        )
        config = HarnessConfig(
            max_retries=5,
            worktree=self._wt,
            rubric_judge_invoke=judge,
        )

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        assert outcome.lifecycle.status == Status.INTERRUPTED
        assert outcome.lifecycle.retries == 0  # no retry consumed
        assert outcome.attempts[0].outcome == Outcome.VALIDATION_FAILED
        assert "halt-me" in outcome.attempts[0].error

    def test_rubric_fail_with_retries_exhausted_reaches_failed(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                _ok_command(),
                RubricGrader(
                    assertions=["a"], name="strict", retry_on_fail=True
                ),
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-rubric-exhaust")
        judge = _ScriptedJudge(
            [
                _rubric_wrap('{"passed": false, "summary": "first"}'),
                _rubric_wrap('{"passed": false, "summary": "second"}'),
            ]
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                ),
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                ),
            ]
        )
        config = HarnessConfig(
            max_retries=1,
            worktree=self._wt,
            rubric_judge_invoke=judge,
        )

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        assert outcome.lifecycle.status == Status.FAILED
        assert "strict" in outcome.lifecycle.error
        assert outcome.lifecycle.retries == 1
        assert len(outcome.attempts) == 2

    def test_command_fail_short_circuits_before_rubric(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(1)'",
                    name="bad",
                ),
                RubricGrader(assertions=["a"], name="r0"),
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-rubric-skip")
        judge = _ScriptedJudge(
            [_rubric_wrap('{"passed": true, "summary": "should not run"}')]
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                )
            ]
        )
        config = HarnessConfig(
            worktree=self._wt,
            rubric_judge_invoke=judge,
        )

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        assert outcome.lifecycle.status == Status.FAILED
        assert judge.calls == []
        events = [
            e
            for e in sink.events(lifecycle.run_id)
            if e.kind == "harness.rubric_invoked"
        ]
        assert events == []

    def test_rubric_invoked_and_verdict_events_emit_with_metadata(
        self,
    ) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                _ok_command(),
                RubricGrader(
                    assertions=["a"],
                    name="semantics",
                    judge_model="claude-x",
                ),
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-rubric-events")
        judge = _ScriptedJudge(
            [_rubric_wrap('{"passed": true, "summary": "great"}')]
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                )
            ]
        )
        config = HarnessConfig(
            worktree=self._wt,
            rubric_judge_invoke=judge,
        )

        _run(run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke))

        events = sink.events(lifecycle.run_id)
        invoked = [e for e in events if e.kind == "harness.rubric_invoked"]
        assert len(invoked) == 1
        assert invoked[0].payload["grader_name"] == "semantics"
        assert invoked[0].payload["judge_model"] == "claude-x"
        assert invoked[0].payload["attempt_number"] == 1
        assert invoked[0].attempt_number == 1

        verdicts = [e for e in events if e.kind == "harness.rubric_verdict"]
        assert len(verdicts) == 1
        assert verdicts[0].payload["grader_name"] == "semantics"
        assert verdicts[0].payload["passed"] is True
        assert verdicts[0].payload["summary"] == "great"
        assert verdicts[0].payload["unknown"] is False

    def test_judge_crash_routes_to_internal_error(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                _ok_command(),
                RubricGrader(assertions=["a"], name="rcrash"),
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-rubric-crash")
        crash = RubricJudgeError(
            grader_name="rcrash", reason="network down"
        )
        judge = _ScriptedJudge([crash])
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                )
            ]
        )
        config = HarnessConfig(
            max_retries=0,
            worktree=self._wt,
            rubric_judge_invoke=judge,
        )

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        # With max_retries=0, INTERNAL_ERROR exhausts immediately -> FAILED.
        assert outcome.lifecycle.status == Status.FAILED
        attempt = outcome.attempts[0]
        assert attempt.outcome == Outcome.INTERNAL_ERROR
        assert "rubric judge failed" in attempt.error
        assert "rcrash" in attempt.error
        crash_events = [
            e
            for e in sink.events(lifecycle.run_id)
            if e.kind == "harness.crash"
        ]
        assert len(crash_events) == 1
        assert (
            crash_events[0].payload["classification"]
            == "rubric_judge_error"
        )
        assert crash_events[0].payload["grader_name"] == "rcrash"
        assert crash_events[0].payload["reason"] == "network down"

    def test_judge_crash_repeated_retries_exhaust_to_failed(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                _ok_command(),
                RubricGrader(assertions=["a"], name="rcrash"),
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-rubric-crash-x")
        judge = _ScriptedJudge(
            [
                RubricJudgeError(
                    grader_name="rcrash", reason="boom-1"
                ),
                RubricJudgeError(
                    grader_name="rcrash", reason="boom-2"
                ),
            ]
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                ),
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                ),
            ]
        )
        config = HarnessConfig(
            max_retries=1,
            worktree=self._wt,
            rubric_judge_invoke=judge,
        )

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        assert outcome.lifecycle.status == Status.FAILED
        assert len(outcome.attempts) == 2
        assert outcome.attempts[0].outcome == Outcome.INTERNAL_ERROR
        assert outcome.attempts[1].outcome == Outcome.INTERNAL_ERROR
        assert outcome.lifecycle.retries == 1

    def test_unknown_verdict_reaches_done_and_emits_warning(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                _ok_command(),
                RubricGrader(assertions=["a"], name="speculative"),
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-rubric-unknown")
        judge = _ScriptedJudge(
            [
                _rubric_wrap(
                    '{"passed": false, "summary": "punted",'
                    ' "unknown": true}'
                )
            ]
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                )
            ]
        )
        config = HarnessConfig(
            worktree=self._wt,
            rubric_judge_invoke=judge,
        )

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        assert outcome.lifecycle.status == Status.DONE
        unknown_events = [
            e
            for e in sink.events(lifecycle.run_id)
            if e.kind == "harness.rubric_unknown"
        ]
        assert len(unknown_events) == 1
        assert unknown_events[0].payload["grader_name"] == "speculative"
        assert unknown_events[0].payload["summary"] == "punted"

    def test_retry_attempt_carries_prior_rubric_findings_into_prompt(
        self,
    ) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                _ok_command(),
                RubricGrader(
                    assertions=["a"], name="semantics", retry_on_fail=True
                ),
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-rubric-feedback")
        judge = _ScriptedJudge(
            [
                _rubric_wrap(
                    '{"passed": false, "summary":'
                    ' "modified wrong file"}'
                ),
                _rubric_wrap('{"passed": true, "summary": "fixed"}'),
            ]
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                ),
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                ),
            ]
        )
        config = HarnessConfig(
            max_retries=1,
            worktree=self._wt,
            rubric_judge_invoke=judge,
        )

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        assert outcome.lifecycle.status == Status.DONE
        # Attempt #2's prompt must include the # Reviewer feedback section.
        second_prompt = invoke.calls[1].prompt  # type: ignore[attr-defined]
        assert "# Reviewer feedback" in second_prompt
        assert "semantics" in second_prompt
        assert "modified wrong file" in second_prompt
        # Attempt #1's prompt must NOT include the section.
        first_prompt = invoke.calls[0].prompt  # type: ignore[attr-defined]
        assert "# Reviewer feedback" not in first_prompt


class TestManualGateEntry:
    """Validation seat enters the manual-approval gate after all automated
    graders pass when the task declares any :class:`ManualGrader`."""

    @pytest.fixture(autouse=True)
    def _worktree(self, tmp_path: Path) -> None:
        self._wt = str(tmp_path)

    def test_all_pass_with_manual_gate_parks_at_awaiting_approval(
        self,
    ) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                _ok_command(),
                RubricGrader(assertions=["a"], name="r0"),
                ManualGrader(
                    instruction="Confirm the migration is safe.",
                    name="confirm-migration",
                ),
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-manual-park")
        judge = _ScriptedJudge(
            [_rubric_wrap('{"passed": true, "summary": "ok"}')]
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                )
            ]
        )
        config = HarnessConfig(
            worktree=self._wt,
            rubric_judge_invoke=judge,
        )

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        # Lifecycle parks at AWAITING_APPROVAL pinned to the manual gate.
        assert outcome.lifecycle.status == Status.AWAITING_APPROVAL
        assert outcome.lifecycle.awaiting_manual_ordinal == 2

        # Attempt is finalized SUCCEEDED — the agent passed every
        # automated grader; the human wait is a lifecycle-level gate.
        assert len(outcome.attempts) == 1
        attempt = outcome.attempts[0]
        assert attempt.outcome == Outcome.SUCCEEDED
        assert attempt.ended_at is not None
        assert attempt.error == ""

        # The harness.awaiting_approval audit event carries the gate
        # instruction plus the documented context pointers.
        events = [
            e
            for e in sink.events(lifecycle.run_id)
            if e.kind == "harness.awaiting_approval"
        ]
        assert len(events) == 1
        payload = events[0].payload
        assert payload["instructions"] == "Confirm the migration is safe."
        assert payload["awaiting_ordinal"] == 2
        assert payload["grader_name"] == "confirm-migration"
        assert payload["run_id"] == lifecycle.run_id
        assert payload["attempt_number"] == 1
        # artifacts_dir is the per-attempt directory string when set,
        # the empty string otherwise; here no artifacts root is
        # configured so it is empty.
        assert payload["artifacts_dir"] == ""

        # No DONE event fires while the gate is parked.
        assert Status.DONE not in outcome.lifecycle.timestamps

        # The awaiting_manual_ordinal persists across a reload (the
        # AwaitingApproval domain event is event-sourced so loaded ==
        # folded, matching the audit-stream oracle).
        loaded = store.load_lifecycle(lifecycle.run_id)
        assert loaded is not None
        assert loaded.status == Status.AWAITING_APPROVAL
        assert loaded.awaiting_manual_ordinal == 2
        folded = replay(store.list_domain_events(lifecycle.run_id))
        assert folded.awaiting_manual_ordinal == 2
        _strip_attempt_aggregates(loaded, folded)
        assert loaded == folded

    def test_all_pass_with_no_manual_gate_still_reaches_done(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                _ok_command(),
                RubricGrader(assertions=["a"], name="r0"),
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-no-manual")
        judge = _ScriptedJudge(
            [_rubric_wrap('{"passed": true, "summary": "ok"}')]
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                )
            ]
        )
        config = HarnessConfig(
            worktree=self._wt,
            rubric_judge_invoke=judge,
        )

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        # Byte-identical to today's all-pass-no-manual path: DONE with no
        # awaiting_approval audit event and the ordinal column untouched.
        assert outcome.lifecycle.status == Status.DONE
        assert outcome.lifecycle.awaiting_manual_ordinal is None
        assert outcome.attempts[-1].outcome == Outcome.SUCCEEDED

        events = [
            e
            for e in sink.events(lifecycle.run_id)
            if e.kind == "harness.awaiting_approval"
        ]
        assert events == []

        # No AWAITING_APPROVAL state ever entered.
        assert Status.AWAITING_APPROVAL not in outcome.lifecycle.timestamps

    def test_rubric_failure_never_enters_manual_gate(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                _ok_command(),
                RubricGrader(
                    assertions=["a"], name="strict", retry_on_fail=False
                ),
                ManualGrader(
                    instruction="Should never be reached.",
                    name="never",
                ),
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-rubric-fail-no-gate")
        judge = _ScriptedJudge(
            [_rubric_wrap('{"passed": false, "summary": "bad"}')]
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                )
            ]
        )
        config = HarnessConfig(
            worktree=self._wt,
            rubric_judge_invoke=judge,
        )

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        # Rubric failure with retry_on_fail=False routes to INTERRUPTED
        # and short-circuits the manual gate entirely.
        assert outcome.lifecycle.status == Status.INTERRUPTED
        assert outcome.lifecycle.awaiting_manual_ordinal is None
        events = [
            e
            for e in sink.events(lifecycle.run_id)
            if e.kind == "harness.awaiting_approval"
        ]
        assert events == []

    def test_command_failure_never_enters_manual_gate(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(1)'",
                    name="bad",
                ),
                ManualGrader(
                    instruction="Should never be reached.",
                    name="never",
                ),
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-cmd-fail-no-gate")
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                )
            ]
        )
        config = HarnessConfig(worktree=self._wt)

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        # Command grader failure exhausts retries and reaches FAILED
        # without ever entering AWAITING_APPROVAL.
        assert outcome.lifecycle.status == Status.FAILED
        assert outcome.lifecycle.awaiting_manual_ordinal is None
        events = [
            e
            for e in sink.events(lifecycle.run_id)
            if e.kind == "harness.awaiting_approval"
        ]
        assert events == []

    def test_transcript_failure_never_enters_manual_gate(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                _ok_command(),
                TranscriptGrader(max_turns=0, name="caps"),
                ManualGrader(
                    instruction="Should never be reached.",
                    name="never",
                ),
            ],
        )
        lifecycle = Lifecycle(
            task_id="t1", run_id="run-transcript-fail-no-gate"
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg(num_turns=5)),
                    signals=_make_signals(num_turns=5),
                )
            ]
        )
        config = HarnessConfig(worktree=self._wt, max_retries=0)

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        # Transcript breach drives FAILED_VALIDATION -> FAILED with no
        # manual gate ever evaluated.
        assert outcome.lifecycle.status == Status.FAILED
        assert outcome.lifecycle.awaiting_manual_ordinal is None
        events = [
            e
            for e in sink.events(lifecycle.run_id)
            if e.kind == "harness.awaiting_approval"
        ]
        assert events == []


class TestResolveManualApproval:
    """``resolve_manual_approval`` claims the oldest pending approve /
    reject command on an ``AWAITING_APPROVAL`` lifecycle and drives the
    follow-on transition (re-park / DONE / FAILED_VALIDATION + retry arm)
    per spec 00016 FR-5 and FR-6.
    """

    @pytest.fixture(autouse=True)
    def _worktree(self, tmp_path: Path) -> None:
        self._wt = str(tmp_path)

    def _park_single_gate(
        self,
        *,
        run_id: str = "run-resolver",
        gate_name: str = "confirm-migration",
        gate_instruction: str = "Confirm the migration is safe.",
    ) -> tuple[InMemoryStore, _ListSink, Task, Lifecycle]:
        """Drive a one-iteration task with command + rubric + one manual
        gate to its parked ``AWAITING_APPROVAL`` state.

        Centralizes the harness fixture so every resolver test starts
        from the same FR-4 entry shape: the attempt is finalized
        ``SUCCEEDED``, ``awaiting_manual_ordinal`` is set, and exactly one
        ``harness.awaiting_approval`` event has been emitted.
        """
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                _ok_command(),
                RubricGrader(assertions=["a"], name="r0"),
                ManualGrader(
                    instruction=gate_instruction,
                    name=gate_name,
                ),
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id=run_id)
        judge = _ScriptedJudge(
            [_rubric_wrap('{"passed": true, "summary": "ok"}')]
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                )
            ]
        )
        config = HarnessConfig(
            worktree=self._wt,
            rubric_judge_invoke=judge,
        )

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )
        assert outcome.lifecycle.status == Status.AWAITING_APPROVAL
        return store, sink, task, outcome.lifecycle

    def _park_two_gates(
        self,
        *,
        run_id: str = "run-resolver-multi",
        first_name: str = "review-migration",
        second_name: str = "review-rollout",
    ) -> tuple[InMemoryStore, _ListSink, Task, Lifecycle]:
        """Drive a one-iteration task with two manual gates to its
        first-gate ``AWAITING_APPROVAL`` park."""
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                _ok_command(),
                RubricGrader(assertions=["a"], name="r0"),
                ManualGrader(
                    instruction="Confirm the migration is safe.",
                    name=first_name,
                ),
                ManualGrader(
                    instruction="Confirm the rollout cadence.",
                    name=second_name,
                ),
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id=run_id)
        judge = _ScriptedJudge(
            [_rubric_wrap('{"passed": true, "summary": "ok"}')]
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                )
            ]
        )
        config = HarnessConfig(
            worktree=self._wt,
            rubric_judge_invoke=judge,
        )

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )
        assert outcome.lifecycle.status == Status.AWAITING_APPROVAL
        assert outcome.lifecycle.awaiting_manual_ordinal == 2
        return store, sink, task, outcome.lifecycle

    # --- FR-5: approve ---------------------------------------------------

    def test_approve_single_gate_reaches_done_with_passed_receipt(
        self,
    ) -> None:
        from flywheel_core import resolve_manual_approval
        from flywheel_core.invoker_client import CONTROL_COMMAND_APPROVE

        store, sink, task, lifecycle = self._park_single_gate()
        # Snapshot the attempt outcome the gate-entry path finalized so
        # the resolver's no-second-finalize claim can be asserted.
        attempt = store.list_attempts(lifecycle.run_id)[-1]
        assert attempt.outcome == Outcome.SUCCEEDED

        store.enqueue_command(
            lifecycle.run_id,
            CONTROL_COMMAND_APPROVE,
            {},
            now=datetime.now(timezone.utc),
        )

        result = resolve_manual_approval(lifecycle, store, task, sink=sink)

        # The resolver applied the approve and promoted to DONE.
        assert result.applied is True
        assert result.reason == "approved_done"
        assert result.command_id is not None

        # Lifecycle is DONE with the awaiting-ordinal cleared on the
        # -> DONE edge (centralized clear in Lifecycle.transition_to).
        assert lifecycle.status == Status.DONE
        assert lifecycle.awaiting_manual_ordinal is None

        # Exactly one passed=True manual receipt was appended for the
        # gate; the rubric and command receipts from the validation
        # seat are unchanged. Manual rows are append-only — one per
        # approve.
        rows = store.list_grader_results(lifecycle.run_id, attempt.number)
        manual_rows = [r for r in rows if r.grader_type == "manual"]
        assert len(manual_rows) == 1
        manual_row = manual_rows[0]
        assert manual_row.passed is True
        assert manual_row.grader_name == "confirm-migration"
        assert manual_row.ordinal == 2
        assert manual_row.attempt_number == attempt.number

        # Attempt was NOT re-finalized: outcome and ended_at survive
        # unchanged from gate entry (no second AttemptFinalized event,
        # no second harness.attempt_finalized).
        reloaded = store.list_attempts(lifecycle.run_id)
        assert len(reloaded) == 1
        assert reloaded[0].outcome == Outcome.SUCCEEDED
        assert reloaded[0].ended_at == attempt.ended_at

        events = sink.events(lifecycle.run_id)
        finalized_events = [
            e for e in events if e.kind == "harness.attempt_finalized"
        ]
        assert len(finalized_events) == 1

        # Audit event shape carries the documented {grader_name,
        # awaiting_ordinal} keys plus the harness.control_command_applied
        # row attributing the operator decision.
        approved = [e for e in events if e.kind == "harness.manual_approved"]
        assert len(approved) == 1
        assert approved[0].payload == {
            "grader_name": "confirm-migration",
            "awaiting_ordinal": 2,
        }
        applied = [
            e for e in events if e.kind == "harness.control_command_applied"
        ]
        assert len(applied) == 1
        assert applied[0].payload["kind"] == "approve"
        assert applied[0].payload["command_id"] == result.command_id

    def test_approve_multi_gate_reparks_then_final_approve_reaches_done(
        self,
    ) -> None:
        from flywheel_core import resolve_manual_approval
        from flywheel_core.invoker_client import CONTROL_COMMAND_APPROVE

        store, sink, task, lifecycle = self._park_two_gates()
        attempt = store.list_attempts(lifecycle.run_id)[-1]

        # First approve: applies to gate A (ordinal 2), re-parks on B
        # (ordinal 3), stays AWAITING_APPROVAL.
        store.enqueue_command(
            lifecycle.run_id,
            CONTROL_COMMAND_APPROVE,
            {},
            now=datetime.now(timezone.utc),
        )
        first = resolve_manual_approval(lifecycle, store, task, sink=sink)
        assert first.applied is True
        assert first.reason == "approved_next_gate"
        assert lifecycle.status == Status.AWAITING_APPROVAL
        assert lifecycle.awaiting_manual_ordinal == 3

        # Exactly one manual receipt so far, for gate A.
        manual_rows = [
            r
            for r in store.list_grader_results(lifecycle.run_id, attempt.number)
            if r.grader_type == "manual"
        ]
        assert len(manual_rows) == 1
        assert manual_rows[0].passed is True
        assert manual_rows[0].grader_name == "review-migration"
        assert manual_rows[0].ordinal == 2

        # A fresh harness.awaiting_approval event was emitted for B so
        # operators learn the next decision is owed.
        awaiting_events = [
            e
            for e in sink.events(lifecycle.run_id)
            if e.kind == "harness.awaiting_approval"
        ]
        assert len(awaiting_events) == 2  # initial + re-park
        assert awaiting_events[-1].payload["awaiting_ordinal"] == 3
        assert awaiting_events[-1].payload["grader_name"] == "review-rollout"

        # Second approve: applies to gate B, no further gate -> DONE.
        store.enqueue_command(
            lifecycle.run_id,
            CONTROL_COMMAND_APPROVE,
            {},
            now=datetime.now(timezone.utc),
        )
        second = resolve_manual_approval(lifecycle, store, task, sink=sink)
        assert second.applied is True
        assert second.reason == "approved_done"
        assert lifecycle.status == Status.DONE
        assert lifecycle.awaiting_manual_ordinal is None

        # Two manual receipts total — one per approve, append-only.
        manual_rows = [
            r
            for r in store.list_grader_results(lifecycle.run_id, attempt.number)
            if r.grader_type == "manual"
        ]
        assert len(manual_rows) == 2
        assert all(r.passed for r in manual_rows)
        assert [r.ordinal for r in manual_rows] == [2, 3]
        assert [r.grader_name for r in manual_rows] == [
            "review-migration",
            "review-rollout",
        ]

        # Attempt still SUCCEEDED — finalized once at gate entry, never
        # re-finalized.
        attempts = store.list_attempts(lifecycle.run_id)
        assert len(attempts) == 1
        assert attempts[0].outcome == Outcome.SUCCEEDED

    def test_resolver_noop_when_no_pending_command_keeps_park(self) -> None:
        from flywheel_core import resolve_manual_approval

        store, sink, task, lifecycle = self._park_single_gate(
            run_id="run-resolver-idle"
        )
        before = lifecycle.version

        result = resolve_manual_approval(lifecycle, store, task, sink=sink)

        assert result.applied is False
        assert result.reason == "no_pending_command"
        assert result.command_id is None
        assert lifecycle.status == Status.AWAITING_APPROVAL
        assert lifecycle.awaiting_manual_ordinal == 2
        assert lifecycle.version == before

        # No manual receipt, no new events emitted for the no-op tick.
        manual_rows = [
            r
            for r in store.list_grader_results(
                lifecycle.run_id, lifecycle.attempts[-1].number
            )
            if r.grader_type == "manual"
        ]
        assert manual_rows == []
        approved = [
            e
            for e in sink.events(lifecycle.run_id)
            if e.kind == "harness.manual_approved"
        ]
        assert approved == []

    def test_resolver_noop_when_lifecycle_not_awaiting(self) -> None:
        from flywheel_core import resolve_manual_approval

        # Drive an automated-pass-no-manual lifecycle straight to DONE
        # so the resolver sees a non-AWAITING_APPROVAL status.
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[_ok_command(), RubricGrader(assertions=["a"], name="r")],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-resolver-done")
        judge = _ScriptedJudge(
            [_rubric_wrap('{"passed": true, "summary": "ok"}')]
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                )
            ]
        )
        config = HarnessConfig(worktree=self._wt, rubric_judge_invoke=judge)
        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )
        assert outcome.lifecycle.status == Status.DONE

        result = resolve_manual_approval(outcome.lifecycle, store, task)
        assert result.applied is False
        assert result.reason == "not_awaiting"

    # --- FR-6: reject ----------------------------------------------------

    def test_reject_with_retries_remaining_transitions_to_ready(self) -> None:
        from flywheel_core import resolve_manual_approval
        from flywheel_core.invoker_client import CONTROL_COMMAND_REJECT

        store, sink, task, lifecycle = self._park_single_gate(
            run_id="run-reject-retry"
        )
        attempt = store.list_attempts(lifecycle.run_id)[-1]
        assert attempt.outcome == Outcome.SUCCEEDED
        retries_before = lifecycle.retries

        feedback = (
            "The migration drops a column still read by the billing service."
        )
        store.enqueue_command(
            lifecycle.run_id,
            CONTROL_COMMAND_REJECT,
            {"feedback": feedback},
            now=datetime.now(timezone.utc),
        )

        result = resolve_manual_approval(
            lifecycle, store, task, max_retries=1, sink=sink
        )

        # The reject lands FAILED_VALIDATION -> READY (the retry arm
        # consumes one retry on the second edge, matching
        # Lifecycle.apply_transition's retry-counter logic).
        assert result.applied is True
        assert result.reason == "rejected_retry"
        assert lifecycle.status == Status.READY
        assert lifecycle.retries == retries_before + 1
        # The READY edge centralizes the awaiting-ordinal clear.
        assert lifecycle.awaiting_manual_ordinal is None
        # READY clears the FAILED_VALIDATION error inherited on the
        # earlier edge (per the retry-counter clear in
        # Lifecycle.apply_transition).
        assert lifecycle.error == ""

        # The rejected attempt's outcome stays SUCCEEDED — the agent
        # passed every automated grader; the human rejection is
        # captured by the manual receipt + the transition, NOT by
        # mutating the attempt outcome.
        attempts = store.list_attempts(lifecycle.run_id)
        assert len(attempts) == 1
        assert attempts[0].outcome == Outcome.SUCCEEDED
        assert attempts[0].ended_at == attempt.ended_at

        # The manual receipt carries the operator feedback verbatim
        # as its summary, keyed to the SUCCEEDED attempt.
        manual_rows = [
            r
            for r in store.list_grader_results(lifecycle.run_id, attempt.number)
            if r.grader_type == "manual"
        ]
        assert len(manual_rows) == 1
        assert manual_rows[0].passed is False
        assert manual_rows[0].payload["summary"] == feedback
        assert manual_rows[0].grader_name == "confirm-migration"

        # harness.manual_rejected carries the documented payload shape;
        # harness.retry_scheduled witnesses the budget consumption;
        # harness.control_command_applied attributes the operator action.
        events = sink.events(lifecycle.run_id)
        rejected = [e for e in events if e.kind == "harness.manual_rejected"]
        assert len(rejected) == 1
        assert rejected[0].payload == {
            "grader_name": "confirm-migration",
            "awaiting_ordinal": 2,
            "feedback": feedback,
        }
        retry_events = [
            e for e in events if e.kind == "harness.retry_scheduled"
        ]
        assert len(retry_events) == 1
        applied_events = [
            e for e in events if e.kind == "harness.control_command_applied"
        ]
        assert len(applied_events) == 1
        assert applied_events[0].payload["kind"] == "reject"

    def test_reject_with_retries_exhausted_reaches_failed(self) -> None:
        from flywheel_core import resolve_manual_approval
        from flywheel_core.invoker_client import CONTROL_COMMAND_REJECT

        store, sink, task, lifecycle = self._park_single_gate(
            run_id="run-reject-fail"
        )
        attempt = store.list_attempts(lifecycle.run_id)[-1]

        store.enqueue_command(
            lifecycle.run_id,
            CONTROL_COMMAND_REJECT,
            {"feedback": "not good enough"},
            now=datetime.now(timezone.utc),
        )

        # max_retries=0 -> retries exhausted on the first reject.
        result = resolve_manual_approval(
            lifecycle, store, task, max_retries=0, sink=sink
        )

        assert result.applied is True
        assert result.reason == "rejected_failed"
        assert lifecycle.status == Status.FAILED
        # The terminal error preserves the rejection reason so consumers
        # can audit why the lifecycle failed without joining the
        # grader_results table.
        assert (
            lifecycle.error
            == "manual grader 'confirm-migration' rejected by operator"
        )

        # No retry_scheduled audit event when retries are exhausted.
        events = sink.events(lifecycle.run_id)
        retry_events = [
            e for e in events if e.kind == "harness.retry_scheduled"
        ]
        assert retry_events == []

        # Attempt still SUCCEEDED — never re-finalized on a reject.
        attempts = store.list_attempts(lifecycle.run_id)
        assert len(attempts) == 1
        assert attempts[0].outcome == Outcome.SUCCEEDED

        # Manual receipt carries the feedback text and keys to the
        # SUCCEEDED attempt.
        manual_rows = [
            r
            for r in store.list_grader_results(lifecycle.run_id, attempt.number)
            if r.grader_type == "manual"
        ]
        assert len(manual_rows) == 1
        assert manual_rows[0].passed is False
        assert manual_rows[0].payload["summary"] == "not good enough"

    def test_reject_on_first_gate_skips_subsequent_gates(self) -> None:
        from flywheel_core import resolve_manual_approval
        from flywheel_core.invoker_client import CONTROL_COMMAND_REJECT

        store, sink, task, lifecycle = self._park_two_gates(
            run_id="run-reject-short-circuit"
        )
        attempt = store.list_attempts(lifecycle.run_id)[-1]
        assert lifecycle.awaiting_manual_ordinal == 2

        store.enqueue_command(
            lifecycle.run_id,
            CONTROL_COMMAND_REJECT,
            {"feedback": "halt"},
            now=datetime.now(timezone.utc),
        )

        # Retries exhausted so the resolver terminates on the first
        # reject; the second gate (ordinal 3) is never evaluated.
        result = resolve_manual_approval(
            lifecycle, store, task, max_retries=0, sink=sink
        )

        assert result.applied is True
        assert result.reason == "rejected_failed"
        assert lifecycle.status == Status.FAILED

        # Only one manual receipt landed — for gate A. Gate B's
        # ordinal (3) never produced a receipt because the reject
        # short-circuited the chain.
        manual_rows = [
            r
            for r in store.list_grader_results(lifecycle.run_id, attempt.number)
            if r.grader_type == "manual"
        ]
        assert len(manual_rows) == 1
        assert manual_rows[0].ordinal == 2
        assert manual_rows[0].grader_name == "review-migration"
        assert manual_rows[0].passed is False

        # The rejection error names the first gate (the parked one), not
        # the unevaluated second.
        assert (
            lifecycle.error
            == "manual grader 'review-migration' rejected by operator"
        )

    def test_reject_with_absent_feedback_records_placeholder(self) -> None:
        from flywheel_core import resolve_manual_approval
        from flywheel_core.invoker_client import CONTROL_COMMAND_REJECT

        store, sink, task, lifecycle = self._park_single_gate(
            run_id="run-reject-no-feedback"
        )
        attempt = store.list_attempts(lifecycle.run_id)[-1]

        # No feedback key in the payload (the CLI producer permits
        # this when the operator omits --feedback).
        store.enqueue_command(
            lifecycle.run_id,
            CONTROL_COMMAND_REJECT,
            {},
            now=datetime.now(timezone.utc),
        )

        result = resolve_manual_approval(
            lifecycle, store, task, max_retries=0, sink=sink
        )
        assert result.applied is True
        assert result.reason == "rejected_failed"

        manual_rows = [
            r
            for r in store.list_grader_results(lifecycle.run_id, attempt.number)
            if r.grader_type == "manual"
        ]
        assert len(manual_rows) == 1
        # The documented placeholder substitutes for absent / empty
        # feedback so the reviewer-feedback prompt section still has a
        # rendering hook.
        assert manual_rows[0].payload["summary"] == "(no feedback provided)"
        rejected = [
            e
            for e in sink.events(lifecycle.run_id)
            if e.kind == "harness.manual_rejected"
        ]
        assert len(rejected) == 1
        assert rejected[0].payload["feedback"] == "(no feedback provided)"

    def test_reject_with_empty_feedback_records_placeholder(self) -> None:
        from flywheel_core import resolve_manual_approval
        from flywheel_core.invoker_client import CONTROL_COMMAND_REJECT

        store, sink, task, lifecycle = self._park_single_gate(
            run_id="run-reject-empty-feedback"
        )
        attempt = store.list_attempts(lifecycle.run_id)[-1]

        # Empty string feedback should be treated as no feedback, per
        # the spec error-handling table's empty-summary handling.
        store.enqueue_command(
            lifecycle.run_id,
            CONTROL_COMMAND_REJECT,
            {"feedback": ""},
            now=datetime.now(timezone.utc),
        )

        result = resolve_manual_approval(
            lifecycle, store, task, max_retries=0, sink=sink
        )
        assert result.applied is True

        manual_rows = [
            r
            for r in store.list_grader_results(lifecycle.run_id, attempt.number)
            if r.grader_type == "manual"
        ]
        assert manual_rows[0].payload["summary"] == "(no feedback provided)"

    # --- FR-7: rejection feedback flows into the retry prompt ----------

    def test_retry_after_manual_reject_carries_operator_feedback_into_prompt(
        self,
    ) -> None:
        """End-to-end: a rejected attempt #1 retries, and attempt #2's
        prompt carries the operator feedback in the reviewer-feedback
        section with the documented ``manual <name> (operator):`` label.

        Drives the full pipeline:

        1. Run attempt #1 to ``AWAITING_APPROVAL`` (all automated
           graders pass, the manual gate parks the lifecycle).
        2. Enqueue a reject with operator feedback.
        3. ``resolve_manual_approval`` writes the ``passed=False`` manual
           receipt and drives ``AWAITING_APPROVAL -> FAILED_VALIDATION
           -> READY`` (one retry consumed).
        4. Re-enter ``run_task`` on the now-READY lifecycle; the
           prompt-collection step picks up the manual receipt via
           ``_collect_prior_manual_findings`` and the renderer emits the
           operator-labeled bullet.
        """
        from flywheel_core import resolve_manual_approval
        from flywheel_core.invoker_client import CONTROL_COMMAND_REJECT

        store = InMemoryStore()

        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                _ok_command(),
                RubricGrader(assertions=["a"], name="r0"),
                ManualGrader(
                    instruction="Confirm the migration is safe.",
                    name="confirm-migration",
                ),
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-reject-retry-prompt")
        judge = _ScriptedJudge(
            [
                # Both attempts pass the rubric — the rejection is
                # operator-driven, not a rubric verdict.
                _rubric_wrap('{"passed": true, "summary": "ok"}'),
                _rubric_wrap('{"passed": true, "summary": "ok"}'),
            ]
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                ),
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                ),
            ]
        )
        config = HarnessConfig(
            max_retries=1,
            worktree=self._wt,
            rubric_judge_invoke=judge,
        )

        # --- attempt #1: drive to AWAITING_APPROVAL --------------------
        first_outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )
        assert first_outcome.lifecycle.status == Status.AWAITING_APPROVAL
        assert first_outcome.attempts[-1].outcome == Outcome.SUCCEEDED

        # --- reject with feedback --------------------------------------
        feedback = (
            "The migration drops a column still read by the billing "
            "service. Gate it behind a feature flag first."
        )
        store.enqueue_command(
            lifecycle.run_id,
            CONTROL_COMMAND_REJECT,
            {"feedback": feedback},
            now=datetime.now(timezone.utc),
        )

        # Reuse the same mutated lifecycle the harness handed back so
        # the in-memory ``version`` stays aligned with the persisted row
        # when the resolver applies its transitions.
        live = first_outcome.lifecycle
        resolve_result = resolve_manual_approval(
            live, store, task, max_retries=config.max_retries
        )
        assert resolve_result.applied is True
        assert resolve_result.reason == "rejected_retry"
        assert live.status == Status.READY

        # --- attempt #2: re-enter run_task, capture the prompt ---------
        second_outcome = _run(
            run_task(task, live, store, sink=sink, config=config, invoke=invoke)
        )

        # The second attempt ran and (with the manual gate still
        # un-approved) parks again at AWAITING_APPROVAL.
        assert len(second_outcome.attempts) == 2
        assert second_outcome.lifecycle.status == Status.AWAITING_APPROVAL

        # Two scripted invocations consumed: one per attempt.
        calls = invoke.calls  # type: ignore[attr-defined]
        assert len(calls) == 2

        first_prompt = calls[0].prompt
        second_prompt = calls[1].prompt

        # Attempt #1 had no prior findings -> no reviewer-feedback
        # section. Attempt #2 carries the operator feedback verbatim
        # under the documented label.
        assert "# Reviewer feedback" not in first_prompt
        assert "# Reviewer feedback" in second_prompt
        assert "## attempt #1" in second_prompt
        assert (
            "- manual `confirm-migration` (operator): "
            + feedback
        ) in second_prompt
        # No rubric bullet — the rubric passed on attempt #1, so the
        # collector should pick up only the manual rejection.
        assert "- rubric `r0`" not in second_prompt


class TestHarnessConfigDefaults:
    def test_default_rubric_config_fields(self) -> None:
        cfg = HarnessConfig()
        assert cfg.rubric_judge_model is None
        assert cfg.rubric_judge_max_turns == 8
        assert cfg.worktree is None
        assert cfg.rubric_judge_invoke is None

    def test_default_context_recovery_fields(self) -> None:
        # Recovery is disabled by default (capacity None) so existing
        # consumers see no behavior change; the other knobs carry the
        # spec-mandated defaults.
        cfg = HarnessConfig()
        assert cfg.context_window_tokens is None
        assert cfg.context_recovery_trigger_ratio == 0.9
        assert cfg.max_context_recoveries == 1
        assert cfg.recovery_summarizer_invoke is None

    def test_context_window_tokens_accepts_positive_int(self) -> None:
        cfg = HarnessConfig(context_window_tokens=200_000)
        assert cfg.context_window_tokens == 200_000

    def test_context_recovery_trigger_ratio_accepts_boundary_one(self) -> None:
        # Edge case: ratio of exactly 1.0 is valid.
        cfg = HarnessConfig(context_recovery_trigger_ratio=1.0)
        assert cfg.context_recovery_trigger_ratio == 1.0

    @pytest.mark.parametrize("bad_ratio", [0.0, -0.1, 1.0001, 2.0])
    def test_rejects_out_of_range_trigger_ratio(
        self, bad_ratio: float
    ) -> None:
        with pytest.raises(ValueError, match="trigger_ratio"):
            HarnessConfig(context_recovery_trigger_ratio=bad_ratio)

    @pytest.mark.parametrize("bad_capacity", [0, -1, -100_000])
    def test_rejects_non_positive_context_window_tokens(
        self, bad_capacity: int
    ) -> None:
        with pytest.raises(ValueError, match="context_window_tokens"):
            HarnessConfig(context_window_tokens=bad_capacity)

    def test_context_window_tokens_none_constructs_cleanly(self) -> None:
        # None is the disabled sentinel and must not raise.
        cfg = HarnessConfig(context_window_tokens=None)
        assert cfg.context_window_tokens is None

    def test_recovery_summarizer_invoke_seam_accepted(self) -> None:
        async def fake_invoke(prompt: str, worktree: object) -> str:
            return ""

        cfg = HarnessConfig(recovery_summarizer_invoke=fake_invoke)
        assert cfg.recovery_summarizer_invoke is fake_invoke


class TestTaskPersistence:
    def test_run_persists_task_and_pins_content_hash(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(id="persist-me", goal="g", graders=[])
        lifecycle = Lifecycle(task_id="persist-me", run_id="run-persist")
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(
                        intent=Intent.VERIFY, reason="done"
                    ),
                    messages=(_assistant(), _result_msg(num_turns=1)),
                    transcript=_wrap('{"intent": "verify", "reason": "done"}'),
                    signals=_make_signals(num_turns=1),
                )
            ]
        )

        outcome = _run(run_task(task, lifecycle, store, sink=sink, invoke=invoke))

        # Graderless run reaches DONE on the agent's own claim.
        assert outcome.lifecycle.status == Status.DONE
        # The run pins the exact task version it executed, and that version
        # is retrievable both by hash and via the run.
        digest = task_digest(task)
        assert outcome.lifecycle.task_content_hash == digest
        assert store.load_task("persist-me", digest) == task
        assert store.load_task_for_run("run-persist") == task


# --- LoopGuard wiring (FR-1 STUCK / FR-2 THRASH / FR-5 / FR-6 / FR-7) -----


def _tool_call(
    *,
    tool_use_id: str,
    tool_name: str = "Bash",
    tool_input: dict[str, Any] | None = None,
    is_error: bool | None = False,
) -> ToolInteraction:
    """Build one :class:`ToolInteraction` for a loop-guard stream.

    Mirrors what the SDK-backed invoker projects from a paired
    ``ToolUseBlock`` / ``ToolResultBlock``. ``tool_input`` defaults to a
    stable command so byte-identical repeats collide on the digest.
    """
    if tool_input is None:
        tool_input = {"command": "ls"}
    return ToolInteraction(
        tool_use_id=tool_use_id,
        tool_name=tool_name,
        tool_input=tool_input,
        result=ToolResultObservation(
            tool_use_id=tool_use_id,
            is_error=is_error,
            content=None,
        ),
    )


def _signals_with_tools(
    interactions: tuple[ToolInteraction, ...],
) -> InvocationSignals:
    """Default-shaped signals carrying the supplied tool tuples."""
    return _make_signals(tool_interactions=interactions)


class TestLoopGuardStuck:
    """FR-1: repeated-failure -> interrupted, retry budget preserved."""

    def test_three_consecutive_failing_calls_route_to_interrupted(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-loop-stuck")
        interactions = (
            _tool_call(tool_use_id="t1", is_error=True),
            _tool_call(tool_use_id="t2", is_error=True),
            _tool_call(tool_use_id="t3", is_error=True),
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.CONTINUE),
                    messages=(_assistant(),),
                    signals=_signals_with_tools(interactions),
                )
            ]
        )
        config = HarnessConfig(
            max_retries=1,
            loop_guard=LoopGuardConfig(
                repeated_tool_failure_threshold=3,
                thrash_repeat_threshold=None,
                thrash_window=None,
            ),
        )

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        # Lifecycle paused in INTERRUPTED with the retry budget unspent
        # (INTERRUPTED is not a retry-source state, so an operator must
        # transition through READY -- mirrors the explicit-blocked path).
        assert outcome.lifecycle.status == Status.INTERRUPTED
        assert outcome.lifecycle.retries == 0
        # Attempt finalized as cancelled, mirroring the explicit-blocked
        # path's outcome.
        attempt = outcome.attempts[0]
        assert attempt.outcome == Outcome.CANCELLED
        # No grader rows -- validation never ran.
        assert store.list_grader_results(lifecycle.run_id, 1) == []
        # Exactly one harness.stuck event naming the tool and the digest.
        stuck_events = [
            e for e in sink.events(lifecycle.run_id)
            if e.kind == "harness.stuck"
        ]
        assert len(stuck_events) == 1
        payload = stuck_events[0].payload
        assert payload["tool_name"] == "Bash"
        assert isinstance(payload["input_digest"], str)
        assert len(payload["input_digest"]) == 64  # sha256 hex
        assert payload["requires"] == [
            {
                "type": "tool_loop_block",
                "tool_name": "Bash",
                "input_digest": payload["input_digest"],
            }
        ]
        # blocked_requires_json carries the synthesized predicate so
        # operator recovery semantics apply.
        assert outcome.lifecycle.blocked_requires_json == json.dumps(
            payload["requires"]
        )

    def test_operator_transition_to_ready_clears_blocked_requires_json(
        self,
    ) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-stuck-recover")
        interactions = (
            _tool_call(tool_use_id="t1", is_error=True),
            _tool_call(tool_use_id="t2", is_error=True),
            _tool_call(tool_use_id="t3", is_error=True),
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.CONTINUE),
                    messages=(_assistant(),),
                    signals=_signals_with_tools(interactions),
                )
            ]
        )
        config = HarnessConfig(
            max_retries=1,
            loop_guard=LoopGuardConfig(
                repeated_tool_failure_threshold=3,
                thrash_repeat_threshold=None,
                thrash_window=None,
            ),
        )

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        assert outcome.lifecycle.status == Status.INTERRUPTED
        assert outcome.lifecycle.blocked_requires_json is not None

        # Operator-style transition to READY mirrors the recoverable path
        # used by recheck_blocked_lifecycle: the lifecycle reducer clears
        # blocked_requires_json on the -> READY edge.
        now = datetime.now(timezone.utc)
        reloaded = store.load_lifecycle("run-stuck-recover")
        assert reloaded is not None
        reloaded.transition_to(Status.READY, now=now)
        assert reloaded.blocked_requires_json is None

    def test_stuck_trips_on_final_iteration_and_preempts_cap(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-stuck-on-cap")
        # Two CONTINUE iterations with the final one carrying the trip.
        good = _tool_call(tool_use_id="g1", is_error=False)
        bad_block = (
            _tool_call(tool_use_id="b1", is_error=True),
            _tool_call(tool_use_id="b2", is_error=True),
            _tool_call(tool_use_id="b3", is_error=True),
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.CONTINUE),
                    messages=(_assistant(),),
                    signals=_signals_with_tools((good,)),
                ),
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.CONTINUE),
                    messages=(_assistant(),),
                    signals=_signals_with_tools(bad_block),
                ),
            ]
        )
        config = HarnessConfig(
            max_retries=2,
            max_iterations_per_attempt=2,
            loop_guard=LoopGuardConfig(
                repeated_tool_failure_threshold=3,
                thrash_repeat_threshold=None,
                thrash_window=None,
            ),
        )

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        # Cap-reached would route to FAILED_VALIDATION; STUCK preempts it.
        assert outcome.lifecycle.status == Status.INTERRUPTED
        assert outcome.attempts[0].outcome == Outcome.CANCELLED
        kinds = [e.kind for e in sink.events(lifecycle.run_id)]
        assert "harness.stuck" in kinds
        # No protocol-failure / continue-past-cap routing.
        assert "harness.protocol_failure" not in kinds


class TestLoopGuardThrash:
    """FR-2: identical-tuple repetition -> failed_validation + retry arm."""

    def test_thrash_routes_to_ready_when_retries_remain(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-thrash-retry")
        # Four identical successful calls trip thrash at threshold 4.
        interactions = tuple(
            _tool_call(tool_use_id=f"t{i}") for i in range(4)
        )
        # After the retry transition to READY the run schedules a fresh
        # attempt; supply a clean VERIFY result for that one so the run
        # terminates deterministically.
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.CONTINUE),
                    messages=(_assistant(),),
                    signals=_signals_with_tools(interactions),
                ),
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                    signals=_make_signals(),
                ),
            ]
        )
        config = HarnessConfig(
            max_retries=1,
            loop_guard=LoopGuardConfig(
                repeated_tool_failure_threshold=None,
                thrash_repeat_threshold=4,
                thrash_window=12,
            ),
        )

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        # Second attempt's VERIFY result + passing command grader -> DONE.
        assert outcome.lifecycle.status == Status.DONE
        # The retry was consumed exactly once.
        assert outcome.lifecycle.retries == 1
        # First attempt was the thrash trip.
        first = outcome.attempts[0]
        assert first.outcome == Outcome.AGENT_ERROR
        kinds = [e.kind for e in sink.events(lifecycle.run_id)]
        assert "harness.thrash_detected" in kinds
        assert "harness.retry_scheduled" in kinds
        # Exactly one thrash event with the tool name + digest in payload.
        thrash_events = [
            e for e in sink.events(lifecycle.run_id)
            if e.kind == "harness.thrash_detected"
        ]
        assert len(thrash_events) == 1
        payload = thrash_events[0].payload
        assert payload["tool_name"] == "Bash"
        assert len(payload["input_digest"]) == 64

    def test_thrash_routes_to_failed_when_no_retries_remain(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-thrash-final")
        interactions = tuple(
            _tool_call(tool_use_id=f"t{i}") for i in range(4)
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.CONTINUE),
                    messages=(_assistant(),),
                    signals=_signals_with_tools(interactions),
                )
            ]
        )
        config = HarnessConfig(
            max_retries=0,
            loop_guard=LoopGuardConfig(
                repeated_tool_failure_threshold=None,
                thrash_repeat_threshold=4,
                thrash_window=12,
            ),
        )

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        # No retries budget -> terminal FAILED via the standard retry arm.
        assert outcome.lifecycle.status == Status.FAILED
        attempt = outcome.attempts[0]
        assert attempt.outcome == Outcome.AGENT_ERROR
        kinds = [e.kind for e in sink.events(lifecycle.run_id)]
        assert "harness.thrash_detected" in kinds
        assert "harness.retry_scheduled" not in kinds


class TestLoopGuardPrecedence:
    """FR-6: a stream that satisfies both detectors lands STUCK."""

    def test_failing_repeats_satisfy_both_but_route_to_interrupted(
        self,
    ) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-both")
        # Four identical failing calls satisfy threshold=3 STUCK and
        # threshold=4 THRASH simultaneously; STUCK must win.
        interactions = tuple(
            _tool_call(tool_use_id=f"t{i}", is_error=True) for i in range(4)
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.CONTINUE),
                    messages=(_assistant(),),
                    signals=_signals_with_tools(interactions),
                )
            ]
        )
        config = HarnessConfig(
            max_retries=1,
            loop_guard=LoopGuardConfig(
                repeated_tool_failure_threshold=3,
                thrash_repeat_threshold=4,
                thrash_window=12,
            ),
        )

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        assert outcome.lifecycle.status == Status.INTERRUPTED
        assert outcome.attempts[0].outcome == Outcome.CANCELLED
        kinds = [e.kind for e in sink.events(lifecycle.run_id)]
        assert "harness.stuck" in kinds
        # THRASH never emits when STUCK wins on the same observation.
        assert "harness.thrash_detected" not in kinds


class TestLoopGuardDisabled:
    """FR-5: detectors disabled -> normal cap-reached behavior."""

    def test_thrashing_stream_runs_to_cap_when_detectors_disabled(self) -> None:
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-loop-off")
        # Would trip thrash at threshold 4 if enabled; with detectors off
        # the run continues past the cap and lands in FAILED_VALIDATION.
        interactions = tuple(
            _tool_call(tool_use_id=f"t{i}") for i in range(4)
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.CONTINUE),
                    messages=(_assistant(),),
                    signals=_signals_with_tools(interactions),
                )
            ]
        )
        config = HarnessConfig(
            max_retries=0,
            max_iterations_per_attempt=1,
            loop_guard=LoopGuardConfig(
                repeated_tool_failure_threshold=None,
                thrash_repeat_threshold=None,
                thrash_window=None,
            ),
        )

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        # No safety-net detection means the iteration's CONTINUE intent
        # past the cap drives the standard agent-error finalize path.
        assert outcome.lifecycle.status == Status.FAILED
        attempt = outcome.attempts[0]
        assert attempt.outcome == Outcome.AGENT_ERROR
        kinds = [e.kind for e in sink.events(lifecycle.run_id)]
        assert "harness.stuck" not in kinds
        assert "harness.thrash_detected" not in kinds


class TestLoopGuardAuditStream:
    """FR-7: harness.stuck / harness.thrash_detected round-trip via audit."""

    def test_stuck_event_lands_in_telemetry_stream(self) -> None:
        store = InMemoryStore()

        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-audit-stuck")
        interactions = (
            _tool_call(tool_use_id="t1", is_error=True),
            _tool_call(tool_use_id="t2", is_error=True),
            _tool_call(tool_use_id="t3", is_error=True),
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.CONTINUE),
                    messages=(_assistant(),),
                    signals=_signals_with_tools(interactions),
                )
            ]
        )
        config = HarnessConfig(
            max_retries=1,
            loop_guard=LoopGuardConfig(
                repeated_tool_failure_threshold=3,
                thrash_repeat_threshold=None,
                thrash_window=None,
            ),
        )

        _run(run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke))

        # The sink stream carries the stuck record in emission order
        # (file write order is the canonical observability ordering).
        stuck = [
            r
            for r in sink.events("run-audit-stuck")
            if r.kind == "harness.stuck"
        ]
        assert len(stuck) == 1
        assert stuck[0].payload["tool_name"] == "Bash"
        assert len(stuck[0].payload["input_digest"]) == 64

    def test_thrash_event_lands_in_telemetry_stream(self) -> None:
        store = InMemoryStore()

        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-audit-thrash")
        interactions = tuple(
            _tool_call(tool_use_id=f"t{i}") for i in range(4)
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.CONTINUE),
                    messages=(_assistant(),),
                    signals=_signals_with_tools(interactions),
                )
            ]
        )
        config = HarnessConfig(
            max_retries=0,
            loop_guard=LoopGuardConfig(
                repeated_tool_failure_threshold=None,
                thrash_repeat_threshold=4,
                thrash_window=12,
            ),
        )

        _run(run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke))

        thrash = [
            r
            for r in sink.events("run-audit-thrash")
            if r.kind == "harness.thrash_detected"
        ]
        assert len(thrash) == 1
        assert thrash[0].payload["tool_name"] == "Bash"
        assert len(thrash[0].payload["input_digest"]) == 64


class TestLoopGuardDefaultConfig:
    """SHARED-INVARIANT: defaults are tuned so existing fixtures don't trip."""

    def test_default_loop_guard_config_has_detectors_enabled(self) -> None:
        cfg = HarnessConfig()
        # The deterministic detectors ship on by default (the cap is the
        # operator-set threshold). Hang watchdog stays None per FR-5.
        assert cfg.loop_guard.repeated_tool_failure_threshold == 3
        assert cfg.loop_guard.thrash_repeat_threshold == 4
        assert cfg.loop_guard.thrash_window == 12
        assert cfg.loop_guard.hang_timeout_seconds is None


# --- Hang watchdog (FR-3 / FR-4 / FR-7) -----------------------------------


def _ratelimit_event(uuid_: str) -> RateLimitEvent:
    """Build a RateLimitEvent for the watchdog-liveness round-trip.

    The watchdog treats every SDK message as liveness (FR-3 + Out of
    Scope: "RateLimitInfo.ResetsAt-driven ETA suppression"); a steady
    rate-limit stream must not trip even though no agent progress is
    happening, because the underlying connection is alive.
    """
    return RateLimitEvent(
        rate_limit_info=RateLimitInfo(
            status="allowed_warning",
            resets_at=1_700_000_000,
            rate_limit_type="five_hour",
            utilization=0.5,
        ),
        uuid=uuid_,
        session_id="sess-1",
    )


class TestHangWatchdog:
    """FR-3: hang watchdog -> internal_error; FR-4: disambiguated from the
    operator-interrupt path; FR-7: harness.hang_detected round-trips
    through ``flywheel_core.audit.stream``.

    Drives the timing comparison via a controllable monotonic clock and
    the existing ``on_message`` seam rather than real wall-clock sleeps,
    so the assertions are deterministic. The only real wall-clock cost is
    the watchdog's own tick floor (``_HANG_WATCHDOG_MIN_TICK_SECONDS``);
    each test completes in well under a second.
    """

    def _fake_mclock(self) -> tuple[Callable[[], float], dict[str, float]]:
        """A controllable monotonic clock.

        Returns a ``(mclock, state)`` pair: ``mclock()`` reads
        ``state['t']``; the test mutates the dict to drive the watchdog
        comparison without waiting for wall-clock time.
        """
        state: dict[str, float] = {"t": 0.0}

        def mclock() -> float:
            return state["t"]

        return mclock, state

    def test_stalled_stream_routes_to_internal_error(self) -> None:
        # FR-3: a message-stream stall past the threshold finalizes the
        # attempt as INTERNAL_ERROR and emits exactly one
        # harness.hang_detected event. FR-4: no harness.interrupted event
        # is emitted -- the watchdog cancel does NOT reach
        # _handle_interrupt.
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(goal="g", graders=[])
        lifecycle = Lifecycle(task_id="t1", run_id="run-hang-stall")
        mclock, state = self._fake_mclock()
        hang_timeout = 0.04  # tick = max(0.01, min(0.01, 0.5)) = 0.01s

        config = HarnessConfig(
            max_retries=0,
            loop_guard=LoopGuardConfig(
                repeated_tool_failure_threshold=None,
                thrash_repeat_threshold=None,
                thrash_window=None,
                hang_timeout_seconds=hang_timeout,
            ),
        )

        async def main() -> HarnessOutcome:
            invocation_started = asyncio.Event()

            async def stalling_invoker(
                request: InvocationRequest,
            ) -> IterationResult:
                # Signal that we have entered the invocation, then stall
                # forever; the watchdog must cancel us.
                invocation_started.set()
                await asyncio.Future()
                raise RuntimeError("unreachable")  # pragma: no cover

            async def advance_clock_when_ready() -> None:
                # Once the invocation is in flight, advance the fake
                # clock past the threshold so the next watchdog tick trips.
                # Real wall-clock has not advanced; the watchdog comparison
                # is against mclock() only.
                await invocation_started.wait()
                state["t"] = 1000.0

            advance = asyncio.create_task(advance_clock_when_ready())
            try:
                return await run_task(
                    task,
                    lifecycle,
                    store,
                    sink=sink,
                    config=config,
                    invoke=stalling_invoker,
                    monotonic=mclock,
                )
            finally:
                if not advance.done():
                    advance.cancel()
                    try:
                        await advance
                    except BaseException:
                        pass

        outcome = asyncio.run(main())

        # max_retries=0: INTERNAL_ERROR is not retry-eligible, so the
        # outer retry arm walks the lifecycle to FAILED.
        assert outcome.lifecycle.status == Status.FAILED
        assert len(outcome.attempts) == 1
        attempt = outcome.attempts[0]
        assert attempt.outcome == Outcome.INTERNAL_ERROR
        assert "hang watchdog" in attempt.error

        events = sink.events(lifecycle.run_id)
        hang_events = [
            e for e in events if e.kind == "harness.hang_detected"
        ]
        # FR-3 acceptance: exactly one hang event with the threshold and
        # iteration captured in the payload.
        assert len(hang_events) == 1
        payload = hang_events[0].payload
        assert payload["iteration"] == 1
        assert payload["hang_timeout_seconds"] == hang_timeout
        assert payload["silence_seconds"] >= hang_timeout
        assert hang_events[0].attempt_number == 1

        # FR-4 acceptance: zero harness.interrupted events; a watchdog
        # cancel must NOT reach the operator-interrupt path.
        interrupted = [e for e in events if e.kind == "harness.interrupted"]
        assert interrupted == []

    def test_heartbeat_under_threshold_runs_to_completion(self) -> None:
        # FR-3 acceptance: a stream that produces a heartbeat message
        # whose mclock delta stays under hang_timeout each tick reaches
        # normal completion; the watchdog never trips.
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                    name="ok",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-hang-heartbeat")
        mclock, state = self._fake_mclock()
        hang_timeout = 0.1

        config = HarnessConfig(
            max_retries=0,
            loop_guard=LoopGuardConfig(
                repeated_tool_failure_threshold=None,
                thrash_repeat_threshold=None,
                thrash_window=None,
                hang_timeout_seconds=hang_timeout,
            ),
        )

        async def heartbeating_invoker(
            request: InvocationRequest,
        ) -> IterationResult:
            msgs: tuple[Message, ...] = (
                _assistant(text="m1"),
                _assistant(text="m2"),
                _assistant(text="m3"),
                _assistant(text="m4"),
                _assistant(),
                _result_msg(),
            )
            for msg in msgs:
                # Real-time yield so the watchdog can tick between
                # heartbeats. Each heartbeat advances the fake clock by
                # well under hang_timeout, then calls on_message -- which
                # the watchdog wrapper uses to reset last_activity to the
                # new mclock value.
                await asyncio.sleep(0.005)
                state["t"] += hang_timeout * 0.5
                if request.on_message is not None:
                    request.on_message(msg)
            return _iteration(
                envelope=ValidEnvelope(intent=Intent.VERIFY),
                messages=msgs,
            )

        outcome = _run(
            run_task(
                task,
                lifecycle,
                store,
                sink=sink,
                config=config,
                invoke=heartbeating_invoker,
                monotonic=mclock,
            )
        )

        assert outcome.lifecycle.status == Status.DONE
        events = sink.events(lifecycle.run_id)
        assert all(e.kind != "harness.hang_detected" for e in events)
        assert all(e.kind != "harness.interrupted" for e in events)

    def test_steady_rate_limit_stream_never_trips_watchdog(self) -> None:
        # FR-3 + Out of Scope: a RateLimitEvent is an SDK message and so
        # counts as liveness. A steady rate-limit stream must not trip
        # the watchdog even though no agent text is produced.
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                    name="ok",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-hang-ratelimit")
        mclock, state = self._fake_mclock()
        hang_timeout = 0.1

        config = HarnessConfig(
            max_retries=0,
            loop_guard=LoopGuardConfig(
                repeated_tool_failure_threshold=None,
                thrash_repeat_threshold=None,
                thrash_window=None,
                hang_timeout_seconds=hang_timeout,
            ),
        )

        rate_events = tuple(_ratelimit_event(f"evt-{i}") for i in range(6))
        terminal = (_assistant(), _result_msg())
        all_msgs: tuple[Message, ...] = rate_events + terminal

        async def rate_limit_invoker(
            request: InvocationRequest,
        ) -> IterationResult:
            for msg in all_msgs:
                await asyncio.sleep(0.005)
                state["t"] += hang_timeout * 0.5
                if request.on_message is not None:
                    request.on_message(msg)
            return _iteration(
                envelope=ValidEnvelope(intent=Intent.VERIFY),
                messages=all_msgs,
                signals=_make_signals(rate_limit_events=rate_events),
            )

        outcome = _run(
            run_task(
                task,
                lifecycle,
                store,
                sink=sink,
                config=config,
                invoke=rate_limit_invoker,
                monotonic=mclock,
            )
        )

        assert outcome.lifecycle.status == Status.DONE
        events = sink.events(lifecycle.run_id)
        # Rate-limit-driven liveness must be visible: the
        # iteration_completed payload's rate_limited flag is True (the
        # observable that "rate-limit counted as a message" holds).
        completed = [
            e for e in events if e.kind == "harness.iteration_completed"
        ]
        assert len(completed) == 1
        assert completed[0].payload["rate_limited"] is True
        # And the watchdog never declared a hang.
        assert all(e.kind != "harness.hang_detected" for e in events)

    def test_operator_cancel_with_watchdog_disabled_is_unchanged(
        self,
    ) -> None:
        # FR-4 acceptance: an operator cancel with the watchdog disabled
        # (hang_timeout_seconds is None) must route to the existing
        # INTERRUPTED behavior with a harness.interrupted event -- and no
        # harness.hang_detected event because the watchdog never started.
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(goal="g", graders=[])
        lifecycle = Lifecycle(
            task_id="t1", run_id="run-cancel-watchdog-off"
        )

        async def cancelling_invoker(
            request: InvocationRequest,
        ) -> IterationResult:
            raise asyncio.CancelledError()

        # Default LoopGuardConfig leaves hang_timeout_seconds None.
        config = HarnessConfig(max_retries=0)
        assert config.loop_guard.hang_timeout_seconds is None

        with pytest.raises(asyncio.CancelledError):
            _run(
                run_task(
                    task,
                    lifecycle,
                    store,
                    sink=sink,
                    config=config,
                    invoke=cancelling_invoker,
                )
            )

        # Existing operator-interrupt path holds: lifecycle INTERRUPTED,
        # exactly one harness.interrupted event, retries preserved.
        reloaded = store.load_lifecycle(lifecycle.run_id)
        assert reloaded is not None
        assert reloaded.status == Status.INTERRUPTED
        assert reloaded.retries == 0

        events = sink.events(lifecycle.run_id)
        interrupted = [
            e for e in events if e.kind == "harness.interrupted"
        ]
        assert len(interrupted) == 1
        # Watchdog never started, so no hang event surfaces.
        assert all(e.kind != "harness.hang_detected" for e in events)

    def test_hang_detected_lands_in_telemetry_stream(self) -> None:
        # FR-7 acceptance, retargeted by spec 00025: harness.hang_detected
        # streams to the run's telemetry sink in emission order.
        store = InMemoryStore()

        sink = _ListSink()
        task = Task(goal="g", graders=[])
        lifecycle = Lifecycle(task_id="t1", run_id="run-audit-hang")
        mclock, state = self._fake_mclock()
        hang_timeout = 0.04

        config = HarnessConfig(
            max_retries=0,
            loop_guard=LoopGuardConfig(
                repeated_tool_failure_threshold=None,
                thrash_repeat_threshold=None,
                thrash_window=None,
                hang_timeout_seconds=hang_timeout,
            ),
        )

        async def main() -> None:
            invocation_started = asyncio.Event()

            async def stalling_invoker(
                request: InvocationRequest,
            ) -> IterationResult:
                invocation_started.set()
                await asyncio.Future()
                raise RuntimeError("unreachable")  # pragma: no cover

            async def advance_clock_when_ready() -> None:
                await invocation_started.wait()
                state["t"] = 1000.0

            advance = asyncio.create_task(advance_clock_when_ready())
            try:
                await run_task(
                    task,
                    lifecycle,
                    store,
                    sink=sink,
                    config=config,
                    invoke=stalling_invoker,
                    monotonic=mclock,
                )
            finally:
                if not advance.done():
                    advance.cancel()
                    try:
                        await advance
                    except BaseException:
                        pass

        asyncio.run(main())

        hang = [
            r
            for r in sink.events("run-audit-hang")
            if r.kind == "harness.hang_detected"
        ]
        assert len(hang) == 1
        assert hang[0].payload["iteration"] == 1
        assert hang[0].payload["hang_timeout_seconds"] == hang_timeout


# --- Context-recovery policy (spec 00018) ---------------------------------


from flywheel_core.prompt import RecoveryHandoff
from flywheel_core.recovery_summarizer import (
    CLOSING_FENCE as RECOVERY_CLOSING_FENCE,
    OPENING_FENCE as RECOVERY_OPENING_FENCE,
)


def _usage_dict(input_tokens: int = 0) -> dict[str, int]:
    """Build a usage breakdown dict where the iteration's input-side
    occupancy equals ``input_tokens``."""
    return {
        "input_tokens": input_tokens,
        "output_tokens": 0,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0,
    }


def _scripted_summarizer(
    handoff: RecoveryHandoff | Exception,
) -> Callable[[str, Any], Awaitable[str]]:
    """Build a recovery_summarizer_invoke seam that returns one envelope."""
    calls: list[tuple[str, Any]] = []

    async def _invoke(prompt: str, worktree: Any) -> str:
        calls.append((prompt, worktree))
        if isinstance(handoff, Exception):
            raise handoff
        envelope = {
            "work_done": handoff.work_done,
            "work_remaining": handoff.work_remaining,
            "key_decisions": handoff.key_decisions,
            "suggested_next_step": handoff.suggested_next_step,
        }
        return (
            f"{RECOVERY_OPENING_FENCE}\n"
            f"{json.dumps(envelope)}\n"
            f"{RECOVERY_CLOSING_FENCE}\n"
        )

    _invoke.calls = calls  # type: ignore[attr-defined]
    return _invoke


class TestContextRecovery:
    """Spec 00018: summarize-restart context recovery."""

    def test_over_ratio_continue_triggers_recovery_and_fresh_attempt(
        self, tmp_path: Path
    ) -> None:
        """FR-1 / FR-3 happy path: an over-ratio CONTINUE iteration
        finalizes RECOVERED and schedules a fresh attempt whose prompt
        carries the # Recovery handoff section."""
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(goal="g", graders=[])
        lifecycle = Lifecycle(task_id="t1", run_id="run-recover-happy")
        handoff = RecoveryHandoff(
            work_done="probed handler X",
            work_remaining="implement handler Y",
            key_decisions="use approach Z",
            suggested_next_step="open handler.py and patch dispatch",
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.CONTINUE),
                    messages=(
                        _assistant(usage=_usage_dict(input_tokens=95)),
                        _result_msg(num_turns=1, usage=_usage_dict(95)),
                    ),
                    transcript="iteration-one transcript",
                    signals=_make_signals(num_turns=1),
                ),
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY, reason="done"),
                    messages=(_assistant(), _result_msg(num_turns=1)),
                    transcript=_wrap('{"intent": "verify", "reason": "done"}'),
                    signals=_make_signals(num_turns=1),
                ),
            ]
        )
        summarizer = _scripted_summarizer(handoff)
        config = HarnessConfig(
            max_retries=0,
            context_window_tokens=100,
            context_recovery_trigger_ratio=0.9,
            max_context_recoveries=1,
            recovery_summarizer_invoke=summarizer,
            worktree=tmp_path,
        )

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        assert outcome.lifecycle.status == Status.DONE
        # Attempt #1 finalized RECOVERED, attempt #2 is the recovery
        # attempt that succeeded.
        assert len(outcome.attempts) == 2
        assert outcome.attempts[0].outcome == Outcome.RECOVERED
        assert outcome.attempts[1].outcome == Outcome.SUCCEEDED
        # Recovery does NOT consume max_retries.
        assert outcome.lifecycle.retries == 0
        # Exactly one harness.context_recovery event with the expected payload.
        events = sink.events(lifecycle.run_id)
        recoveries = [
            e for e in events if e.kind == "harness.context_recovery"
        ]
        assert len(recoveries) == 1
        payload = recoveries[0].payload
        assert payload["occupancy_tokens"] == 95
        assert payload["context_window_tokens"] == 100
        assert payload["context_recovery_trigger_ratio"] == 0.9
        assert payload["recoveries_used"] == 1
        assert payload["recoveries_remaining"] == 0
        assert payload["attempt_number"] == 1
        # Spec 00019 FR-6: the trigger field attributes the recovery
        # to its producing path -- a boundary crossing here, mid_turn
        # in the spec 00019 mid-turn-recovery tests below.
        assert payload["trigger"] == "boundary"
        # Digest carries per-field lengths so audit consumers can
        # confirm the handoff was structurally non-empty.
        digest = payload["summary_digest"]
        assert digest["work_done_length"] == len(handoff.work_done)
        assert digest["suggested_next_step_length"] == len(
            handoff.suggested_next_step
        )
        # FR-5 ordering: the context_recovery event precedes the
        # recovery attempt's attempt_started.
        recovery_idx = events.index(recoveries[0])
        started_indices = [
            i
            for i, e in enumerate(events)
            if e.kind == "harness.attempt_started"
            and e.payload.get("number") == 2
        ]
        assert started_indices
        assert recovery_idx < started_indices[0]
        # FR-3: the recovery attempt's prompt carries the # Recovery
        # handoff section assembled from the summarizer's structured
        # output.
        recovery_prompt = invoke.calls[1].prompt  # type: ignore[attr-defined]
        assert "# Recovery handoff" in recovery_prompt
        assert handoff.work_done in recovery_prompt
        assert handoff.suggested_next_step in recovery_prompt
        # First-attempt prompt has no recovery section.
        first_prompt = invoke.calls[0].prompt  # type: ignore[attr-defined]
        assert "# Recovery handoff" not in first_prompt
        # Summarizer seam was used once.
        assert len(summarizer.calls) == 1  # type: ignore[attr-defined]

    def test_below_ratio_does_not_recover(self, tmp_path: Path) -> None:
        """FR-1 acceptance: a below-ratio iteration does not recover."""
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(goal="g", graders=[])
        lifecycle = Lifecycle(task_id="t1", run_id="run-recover-below")
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(
                        intent=Intent.VERIFY, reason="done"
                    ),
                    messages=(
                        _assistant(usage=_usage_dict(input_tokens=10)),
                        _result_msg(num_turns=1, usage=_usage_dict(10)),
                    ),
                    transcript=_wrap('{"intent": "verify", "reason": "done"}'),
                    signals=_make_signals(num_turns=1),
                ),
            ]
        )
        summarizer = _scripted_summarizer(
            RecoveryHandoff(
                work_done="",
                work_remaining="",
                key_decisions="",
                suggested_next_step="",
            )
        )
        config = HarnessConfig(
            max_retries=0,
            context_window_tokens=100,
            context_recovery_trigger_ratio=0.9,
            max_context_recoveries=1,
            recovery_summarizer_invoke=summarizer,
            worktree=tmp_path,
        )

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        assert outcome.lifecycle.status == Status.DONE
        assert len(outcome.attempts) == 1
        assert outcome.attempts[0].outcome == Outcome.SUCCEEDED
        events = sink.events(lifecycle.run_id)
        assert all(e.kind != "harness.context_recovery" for e in events)
        # Summarizer was never invoked.
        assert summarizer.calls == []  # type: ignore[attr-defined]

    def test_disabled_when_capacity_none(self) -> None:
        """FR-2: capacity None disables recovery; massive usage does not fire."""
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(goal="g", graders=[])
        lifecycle = Lifecycle(task_id="t1", run_id="run-recover-disabled")
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(
                        intent=Intent.VERIFY, reason="done"
                    ),
                    messages=(
                        _assistant(
                            usage=_usage_dict(input_tokens=999_999_999)
                        ),
                        _result_msg(
                            num_turns=1,
                            usage=_usage_dict(999_999_999),
                        ),
                    ),
                    transcript=_wrap('{"intent": "verify", "reason": "done"}'),
                    signals=_make_signals(num_turns=1),
                ),
            ]
        )
        # No context_window_tokens, no summarizer seam set.
        config = HarnessConfig()
        assert config.context_window_tokens is None

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        assert outcome.lifecycle.status == Status.DONE
        events = sink.events(lifecycle.run_id)
        assert all(e.kind != "harness.context_recovery" for e in events)

    def test_recovery_budget_caps_at_max(self, tmp_path: Path) -> None:
        """FR-4: with max_context_recoveries=1, a run whose every
        iteration is over-ratio recovers exactly once."""
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(goal="g", graders=[])
        lifecycle = Lifecycle(task_id="t1", run_id="run-recover-budget")
        handoff = RecoveryHandoff(
            work_done="a",
            work_remaining="b",
            key_decisions="c",
            suggested_next_step="d",
        )
        # Iteration #1 (attempt 1): over-ratio, intent=continue ->
        # recovery fires. Iteration #2 (attempt 2): over-ratio,
        # intent=continue -> budget exhausted, no recovery; normal
        # cap-reached path -> AGENT_ERROR -> FAILED_VALIDATION
        # (max_retries=0 so the run terminates FAILED).
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.CONTINUE),
                    messages=(
                        _assistant(usage=_usage_dict(input_tokens=95)),
                        _result_msg(num_turns=1, usage=_usage_dict(95)),
                    ),
                    transcript="t1",
                    signals=_make_signals(num_turns=1),
                ),
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.CONTINUE),
                    messages=(
                        _assistant(usage=_usage_dict(input_tokens=95)),
                        _result_msg(num_turns=1, usage=_usage_dict(95)),
                    ),
                    transcript="t2",
                    signals=_make_signals(num_turns=1),
                ),
            ]
        )
        summarizer = _scripted_summarizer(handoff)
        config = HarnessConfig(
            max_retries=0,
            context_window_tokens=100,
            context_recovery_trigger_ratio=0.9,
            max_context_recoveries=1,
            recovery_summarizer_invoke=summarizer,
            worktree=tmp_path,
        )

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        # The run terminates: attempt 1 RECOVERED, attempt 2 ran past
        # cap-with-continue -> AGENT_ERROR -> FAILED_VALIDATION ->
        # FAILED.
        assert outcome.lifecycle.status == Status.FAILED
        assert len(outcome.attempts) == 2
        assert outcome.attempts[0].outcome == Outcome.RECOVERED
        assert outcome.attempts[1].outcome == Outcome.AGENT_ERROR
        # Exactly one recovery event.
        events = sink.events(lifecycle.run_id)
        recoveries = [
            e for e in events if e.kind == "harness.context_recovery"
        ]
        assert len(recoveries) == 1
        assert recoveries[0].payload["recoveries_used"] == 1
        assert recoveries[0].payload["recoveries_remaining"] == 0
        # Spec 00019 FR-6: the single recovery is attributed to the
        # boundary path -- mid-turn act is not wired in the scripted
        # invoker (it runs to natural completion on every iteration).
        assert recoveries[0].payload["trigger"] == "boundary"
        # Summarizer was called exactly once.
        assert len(summarizer.calls) == 1  # type: ignore[attr-defined]

    def test_loop_guard_verdict_preempts_recovery(
        self, tmp_path: Path
    ) -> None:
        """FR-6: a LoopGuard STUCK verdict + over-ratio iteration halts
        with no recovery event."""
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(goal="g", graders=[])
        lifecycle = Lifecycle(
            task_id="t1", run_id="run-recover-precedence-stuck"
        )
        interactions = (
            _tool_call(tool_use_id="t1", is_error=True),
            _tool_call(tool_use_id="t2", is_error=True),
            _tool_call(tool_use_id="t3", is_error=True),
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.CONTINUE),
                    messages=(
                        _assistant(usage=_usage_dict(input_tokens=95)),
                        _result_msg(num_turns=1, usage=_usage_dict(95)),
                    ),
                    transcript="stuck-and-over-ratio",
                    signals=_signals_with_tools(interactions),
                )
            ]
        )
        summarizer = _scripted_summarizer(
            RecoveryHandoff(
                work_done="",
                work_remaining="",
                key_decisions="",
                suggested_next_step="",
            )
        )
        config = HarnessConfig(
            max_retries=0,
            context_window_tokens=100,
            context_recovery_trigger_ratio=0.9,
            max_context_recoveries=1,
            recovery_summarizer_invoke=summarizer,
            worktree=tmp_path,
            loop_guard=LoopGuardConfig(
                repeated_tool_failure_threshold=3,
                thrash_repeat_threshold=None,
                thrash_window=None,
            ),
        )

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        # Loop-guard STUCK preempts recovery: lifecycle ends INTERRUPTED
        # and the recovery event never emits.
        assert outcome.lifecycle.status == Status.INTERRUPTED
        events = sink.events(lifecycle.run_id)
        assert all(e.kind != "harness.context_recovery" for e in events)
        assert summarizer.calls == []  # type: ignore[attr-defined]

    def test_completion_claim_preempts_recovery(
        self, tmp_path: Path
    ) -> None:
        """FR-6: a VERIFY iteration + over-ratio occupancy validates
        normally and does not recover."""
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(goal="g", graders=[])
        lifecycle = Lifecycle(
            task_id="t1", run_id="run-recover-precedence-verify"
        )
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(
                        intent=Intent.VERIFY, reason="done"
                    ),
                    messages=(
                        _assistant(usage=_usage_dict(input_tokens=95)),
                        _result_msg(num_turns=1, usage=_usage_dict(95)),
                    ),
                    transcript=_wrap('{"intent": "verify", "reason": "done"}'),
                    signals=_make_signals(num_turns=1),
                )
            ]
        )
        summarizer = _scripted_summarizer(
            RecoveryHandoff(
                work_done="",
                work_remaining="",
                key_decisions="",
                suggested_next_step="",
            )
        )
        config = HarnessConfig(
            max_retries=0,
            context_window_tokens=100,
            context_recovery_trigger_ratio=0.9,
            max_context_recoveries=1,
            recovery_summarizer_invoke=summarizer,
            worktree=tmp_path,
        )

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        # Completion claim wins: graderless task reaches DONE.
        assert outcome.lifecycle.status == Status.DONE
        events = sink.events(lifecycle.run_id)
        assert all(e.kind != "harness.context_recovery" for e in events)
        assert summarizer.calls == []  # type: ignore[attr-defined]

    def test_summarizer_failure_routes_to_internal_error(
        self, tmp_path: Path
    ) -> None:
        """Error Handling: a summarizer raise aborts recovery for the
        iteration and routes through INTERNAL_ERROR. No recovery event
        is emitted; no empty handoff is silently restarted."""
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(goal="g", graders=[])
        lifecycle = Lifecycle(task_id="t1", run_id="run-recover-summ-fail")
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.CONTINUE),
                    messages=(
                        _assistant(usage=_usage_dict(input_tokens=95)),
                        _result_msg(num_turns=1, usage=_usage_dict(95)),
                    ),
                    transcript="over-ratio-and-broken-summarizer",
                    signals=_make_signals(num_turns=1),
                )
            ]
        )
        summarizer = _scripted_summarizer(RuntimeError("summarizer crashed"))
        config = HarnessConfig(
            max_retries=0,
            context_window_tokens=100,
            context_recovery_trigger_ratio=0.9,
            max_context_recoveries=1,
            recovery_summarizer_invoke=summarizer,
            worktree=tmp_path,
        )

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        # No recovery event; attempt finalized INTERNAL_ERROR; lifecycle
        # reaches FAILED via the standard internal-error -> failed arm.
        assert outcome.lifecycle.status == Status.FAILED
        assert len(outcome.attempts) == 1
        assert outcome.attempts[0].outcome == Outcome.INTERNAL_ERROR
        events = sink.events(lifecycle.run_id)
        assert all(e.kind != "harness.context_recovery" for e in events)
        crashes = [
            e
            for e in events
            if e.kind == "harness.crash"
            and e.payload.get("classification")
            == "recovery_summarizer_error"
        ]
        assert len(crashes) == 1


# --- Mid-turn context-occupancy observe (spec 00019) --------------------


def _scripted_invoker_with_observer_pumps(
    pairs: list[tuple[IterationResult, tuple[dict[str, Any], ...]]],
) -> Callable[[InvocationRequest], Awaitable[IterationResult]]:
    """Build a scripted invoker that also pumps SDK context readings.

    Each call pops the next ``(IterationResult, readings)`` pair. The
    ``readings`` tuple is delivered to ``request.context_observer`` (one
    call per reading) BEFORE the iteration's messages are played through
    ``request.on_message`` so a test can assert the SDK reading wins
    over the accumulated ``AssistantMessage.usage`` estimate when both
    sources arrive in the same iteration.
    """
    calls: list[InvocationRequest] = []

    async def _invoker(request: InvocationRequest) -> IterationResult:
        calls.append(request)
        result, readings = pairs.pop(0)
        if request.context_observer is not None:
            for reading in readings:
                try:
                    request.context_observer(reading)  # type: ignore[arg-type]
                except Exception:  # noqa: BLE001 - mirror the production swallow.
                    pass
        if request.on_message is not None:
            for msg in result.messages:
                try:
                    request.on_message(msg)
                except Exception:  # noqa: BLE001 - mirror the production swallow.
                    pass
        return result

    _invoker.calls = calls  # type: ignore[attr-defined]
    return _invoker


class TestMidTurnContextObserve:
    """Spec 00019 FR-1/FR-2/FR-3: mid-turn occupancy threshold observe."""

    def test_three_tiers_fire_in_order_from_streamed_usage(self) -> None:
        """FR-3: streamed usage that climbs through 50/75/90 fires three
        events in tier order, each carrying iteration / tier / occupancy
        / capacity / percentage / capacity_source."""
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(goal="g", graders=[])
        lifecycle = Lifecycle(task_id="t1", run_id="run-observe-tiers")
        # Capacity 100, occupancy climbs 40 -> 60 -> 80 -> 95. The
        # 60 / 80 / 95 messages each first-cross the next tier; 40 is
        # below the lowest tier and emits nothing.
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(
                        intent=Intent.VERIFY, reason="done"
                    ),
                    messages=(
                        _assistant(usage=_usage_dict(input_tokens=40)),
                        _assistant(usage=_usage_dict(input_tokens=60)),
                        _assistant(usage=_usage_dict(input_tokens=80)),
                        _assistant(usage=_usage_dict(input_tokens=95)),
                        _result_msg(num_turns=1, usage=_usage_dict(95)),
                    ),
                    transcript=_wrap(
                        '{"intent": "verify", "reason": "done"}'
                    ),
                    signals=_make_signals(num_turns=1),
                ),
            ]
        )
        config = HarnessConfig(context_window_tokens=100)

        _run(run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke))

        events = sink.events(lifecycle.run_id)
        crossings = [
            e for e in events if e.kind == "harness.context_threshold_crossed"
        ]
        assert [c.payload["tier"] for c in crossings] == [0.5, 0.75, 0.9]
        # All three fired mid-iteration (before harness.iteration_completed).
        iter_done = next(
            e for e in events if e.kind == "harness.iteration_completed"
        )
        iter_done_idx = events.index(iter_done)
        assert all(events.index(c) < iter_done_idx for c in crossings)
        # Payload shape: every required field present on every event.
        for c in crossings:
            assert c.payload["iteration"] == 1
            assert c.payload["capacity_tokens"] == 100
            assert c.payload["capacity_source"] == "operator"
            assert isinstance(c.payload["occupancy_tokens"], int)
            assert isinstance(c.payload["percentage"], float)
        # Per-tier occupancy reflects the streamed AssistantMessage
        # that first crossed it (60 -> 0.5, 80 -> 0.75, 95 -> 0.9).
        assert crossings[0].payload["occupancy_tokens"] == 60
        assert crossings[1].payload["occupancy_tokens"] == 80
        assert crossings[2].payload["occupancy_tokens"] == 95
        assert crossings[0].payload["percentage"] == 60.0
        assert crossings[1].payload["percentage"] == 80.0
        assert crossings[2].payload["percentage"] == 95.0

    def test_no_capacity_no_events_even_with_huge_usage(self) -> None:
        """FR-2 off-by-default: with no capacity from either source, an
        oversize usage stream emits zero threshold events."""
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(goal="g", graders=[])
        lifecycle = Lifecycle(task_id="t1", run_id="run-observe-disabled")
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(
                        intent=Intent.VERIFY, reason="done"
                    ),
                    messages=(
                        _assistant(
                            usage=_usage_dict(input_tokens=999_999_999)
                        ),
                        _result_msg(
                            num_turns=1, usage=_usage_dict(999_999_999)
                        ),
                    ),
                    transcript=_wrap(
                        '{"intent": "verify", "reason": "done"}'
                    ),
                    signals=_make_signals(num_turns=1),
                ),
            ]
        )
        # Default config: context_window_tokens=None, no SDK reading
        # (scripted invoker does not pump context_observer).
        config = HarnessConfig()
        assert config.context_window_tokens is None

        _run(run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke))

        events = sink.events(lifecycle.run_id)
        assert all(
            e.kind != "harness.context_threshold_crossed" for e in events
        )
        # And no recovery side-effect either.
        assert all(e.kind != "harness.context_recovery" for e in events)

    def test_re_cross_in_same_iteration_does_not_duplicate_event(
        self,
    ) -> None:
        """FR-3: an oscillating estimate that re-crosses an already-emitted
        tier within the same iteration does not produce a duplicate event."""
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(goal="g", graders=[])
        lifecycle = Lifecycle(task_id="t1", run_id="run-observe-recross")
        # Capacity 100. Estimate climbs to 60 (crosses 0.5), drops to 40
        # (would re-arm naive logic), climbs to 60 again (re-cross 0.5),
        # ends at 60. Only one 0.5 event must fire; 0.75 / 0.9 never
        # reach.
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(
                        intent=Intent.VERIFY, reason="done"
                    ),
                    messages=(
                        _assistant(usage=_usage_dict(input_tokens=60)),
                        _assistant(usage=_usage_dict(input_tokens=40)),
                        _assistant(usage=_usage_dict(input_tokens=60)),
                        _result_msg(num_turns=1, usage=_usage_dict(60)),
                    ),
                    transcript=_wrap(
                        '{"intent": "verify", "reason": "done"}'
                    ),
                    signals=_make_signals(num_turns=1),
                ),
            ]
        )
        config = HarnessConfig(context_window_tokens=100)

        _run(run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke))

        events = sink.events(lifecycle.run_id)
        crossings = [
            e for e in events if e.kind == "harness.context_threshold_crossed"
        ]
        assert [c.payload["tier"] for c in crossings] == [0.5]

    def test_one_message_jumps_all_three_tiers_emits_three_events(
        self,
    ) -> None:
        """FR-3: a single message that crosses 50 / 75 / 90 simultaneously
        emits all three events in tier order on that message."""
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(goal="g", graders=[])
        lifecycle = Lifecycle(task_id="t1", run_id="run-observe-jump")
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(
                        intent=Intent.VERIFY, reason="done"
                    ),
                    messages=(
                        _assistant(usage=_usage_dict(input_tokens=95)),
                        _result_msg(num_turns=1, usage=_usage_dict(95)),
                    ),
                    transcript=_wrap(
                        '{"intent": "verify", "reason": "done"}'
                    ),
                    signals=_make_signals(num_turns=1),
                ),
            ]
        )
        config = HarnessConfig(context_window_tokens=100)

        _run(run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke))

        events = sink.events(lifecycle.run_id)
        crossings = [
            e for e in events if e.kind == "harness.context_threshold_crossed"
        ]
        assert [c.payload["tier"] for c in crossings] == [0.5, 0.75, 0.9]
        # All three carry the same occupancy / capacity reading because
        # they all crossed on the same message.
        for c in crossings:
            assert c.payload["occupancy_tokens"] == 95
            assert c.payload["capacity_tokens"] == 100
            assert c.payload["percentage"] == 95.0

    def test_emitted_tiers_reset_per_iteration(self) -> None:
        """FR-3: the per-tier emitted set resets at each new iteration so
        a fresh iteration can re-emit tiers a prior iteration consumed."""
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(goal="g", graders=[])
        lifecycle = Lifecycle(task_id="t1", run_id="run-observe-reset")
        # Two iterations under one attempt (max_iterations_per_attempt=2).
        # Each iteration's usage independently crosses all three tiers.
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.CONTINUE),
                    messages=(
                        _assistant(usage=_usage_dict(input_tokens=95)),
                        _result_msg(num_turns=1, usage=_usage_dict(95)),
                    ),
                    transcript="iteration-one",
                    signals=_make_signals(num_turns=1),
                ),
                _iteration(
                    envelope=ValidEnvelope(
                        intent=Intent.VERIFY, reason="done"
                    ),
                    messages=(
                        _assistant(usage=_usage_dict(input_tokens=95)),
                        _result_msg(num_turns=1, usage=_usage_dict(95)),
                    ),
                    transcript=_wrap(
                        '{"intent": "verify", "reason": "done"}'
                    ),
                    signals=_make_signals(num_turns=1),
                ),
            ]
        )
        # max_context_recoveries=0 so the over-ratio iteration does not
        # trigger spec 00018's boundary recovery (which would short-
        # circuit the second iteration). This test only exercises
        # observe behavior, not the recovery path.
        config = HarnessConfig(
            context_window_tokens=100,
            max_iterations_per_attempt=2,
            max_context_recoveries=0,
        )

        _run(run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke))

        events = sink.events(lifecycle.run_id)
        crossings = [
            e for e in events if e.kind == "harness.context_threshold_crossed"
        ]
        # Two iterations, each crossing all three tiers -> 6 events.
        assert len(crossings) == 6
        by_iter: dict[int, list[float]] = {}
        for c in crossings:
            by_iter.setdefault(int(c.payload["iteration"]), []).append(
                float(c.payload["tier"])
            )
        assert by_iter[1] == [0.5, 0.75, 0.9]
        assert by_iter[2] == [0.5, 0.75, 0.9]

    def test_sdk_reading_wins_over_estimate_and_marks_source_sdk(
        self,
    ) -> None:
        """FR-1: when the watcher pumps a ContextUsageResponse, its
        ``totalTokens`` / ``maxTokens`` win over the accumulated estimate
        and the operator capacity, and the event records capacity_source
        ``sdk``."""
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(goal="g", graders=[])
        lifecycle = Lifecycle(task_id="t1", run_id="run-observe-sdk-wins")
        # The streamed message reports tiny usage (would not cross 0.5
        # by itself); the SDK reading puts occupancy at 80 / 100 (0.8),
        # crossing 0.5 and 0.75. Operator capacity is also set to 1000
        # to prove the SDK maxTokens wins over it (an operator capacity
        # of 1000 would put 80 tokens at 8%, no tier).
        reading: dict[str, Any] = {
            "totalTokens": 80,
            "maxTokens": 100,
            "rawMaxTokens": 100,
            "percentage": 80.0,
            "categories": [],
            "model": "claude-test",
            "isAutoCompactEnabled": False,
            "memoryFiles": [],
            "mcpTools": [],
            "agents": [],
            "gridRows": [],
        }
        invoke = _scripted_invoker_with_observer_pumps(
            [
                (
                    _iteration(
                        envelope=ValidEnvelope(
                            intent=Intent.VERIFY, reason="done"
                        ),
                        messages=(
                            _assistant(usage=_usage_dict(input_tokens=5)),
                            _result_msg(num_turns=1, usage=_usage_dict(5)),
                        ),
                        transcript=_wrap(
                            '{"intent": "verify", "reason": "done"}'
                        ),
                        signals=_make_signals(num_turns=1),
                    ),
                    (reading,),
                ),
            ]
        )
        config = HarnessConfig(context_window_tokens=1000)

        _run(run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke))

        events = sink.events(lifecycle.run_id)
        crossings = [
            e for e in events if e.kind == "harness.context_threshold_crossed"
        ]
        # SDK reading at 80% fires 0.5 and 0.75 on the observer call.
        assert [c.payload["tier"] for c in crossings] == [0.5, 0.75]
        for c in crossings:
            assert c.payload["capacity_source"] == "sdk"
            assert c.payload["capacity_tokens"] == 100
            assert c.payload["occupancy_tokens"] == 80
            assert c.payload["percentage"] == 80.0

    def test_sdk_reading_alone_provides_capacity_without_operator_knob(
        self,
    ) -> None:
        """FR-1 hybrid capacity: with no operator knob set, a single SDK
        reading is sufficient to enable threshold observation."""
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(goal="g", graders=[])
        lifecycle = Lifecycle(task_id="t1", run_id="run-observe-sdk-only")
        reading: dict[str, Any] = {
            "totalTokens": 60,
            "maxTokens": 100,
            "rawMaxTokens": 100,
            "percentage": 60.0,
            "categories": [],
            "model": "claude-test",
            "isAutoCompactEnabled": False,
            "memoryFiles": [],
            "mcpTools": [],
            "agents": [],
            "gridRows": [],
        }
        invoke = _scripted_invoker_with_observer_pumps(
            [
                (
                    _iteration(
                        envelope=ValidEnvelope(
                            intent=Intent.VERIFY, reason="done"
                        ),
                        messages=(_assistant(), _result_msg(num_turns=1)),
                        transcript=_wrap(
                            '{"intent": "verify", "reason": "done"}'
                        ),
                        signals=_make_signals(num_turns=1),
                    ),
                    (reading,),
                ),
            ]
        )
        # No context_window_tokens -- the SDK reading alone supplies
        # capacity.
        config = HarnessConfig()

        _run(run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke))

        events = sink.events(lifecycle.run_id)
        crossings = [
            e for e in events if e.kind == "harness.context_threshold_crossed"
        ]
        assert [c.payload["tier"] for c in crossings] == [0.5]
        assert crossings[0].payload["capacity_source"] == "sdk"
        assert crossings[0].payload["capacity_tokens"] == 100
        assert crossings[0].payload["occupancy_tokens"] == 60


# --- Mid-turn context-recovery act (spec 00019) --------------------------


from flywheel_core.invoker_client import HarnessRecoveryRequested


def _scripted_invoker_midturn_act(
    pre_messages: tuple[Message, ...],
    iteration_after_resume: IterationResult,
) -> Callable[[InvocationRequest], Awaitable[IterationResult]]:
    """Build an invoker that honors ``recovery_interrupt_event`` mid-stream.

    On the first call, the invoker pumps ``pre_messages`` through
    ``request.on_message`` (one message at a time) and, after each
    delivery, checks whether the harness has set
    ``request.recovery_interrupt_event``. The first set event causes
    the invoker to raise :class:`HarnessRecoveryRequested` -- mirrors
    the production :func:`invoke_iteration_with_client` translating a
    watcher-induced cancel into the distinguishable mid-turn signal.

    On subsequent calls (the recovery attempt), the invoker returns
    ``iteration_after_resume`` after pumping its messages through
    ``on_message`` for parity with :func:`_scripted_invoker`.
    """
    calls: list[InvocationRequest] = []

    async def _invoker(request: InvocationRequest) -> IterationResult:
        calls.append(request)
        if len(calls) == 1:
            if request.on_message is not None:
                for msg in pre_messages:
                    try:
                        request.on_message(msg)
                    except Exception:  # noqa: BLE001 - mirror invoker swallow.
                        pass
                    if (
                        request.recovery_interrupt_event is not None
                        and request.recovery_interrupt_event.is_set()
                    ):
                        raise HarnessRecoveryRequested()
            # No event set after all messages: fall through (the test
            # is exercising the natural-completion race). Return a
            # plain result so the boundary check can still fire.
            return iteration_after_resume
        if request.on_message is not None:
            for msg in iteration_after_resume.messages:
                try:
                    request.on_message(msg)
                except Exception:  # noqa: BLE001 - mirror invoker swallow.
                    pass
        return iteration_after_resume

    _invoker.calls = calls  # type: ignore[attr-defined]
    return _invoker


class TestMidTurnContextRecoveryAct:
    """Spec 00019 FR-4 / FR-5 / FR-6 / FR-7: mid-turn act -> summarize-restart."""

    def test_over_ratio_midturn_triggers_recovery_with_trigger_marker(
        self, tmp_path: Path
    ) -> None:
        """FR-4 / FR-6 happy path: an over-ratio mid-iteration crossing
        interrupts the iteration, finalizes the attempt RECOVERED, emits
        ``harness.context_recovery`` with ``trigger="mid_turn"`` ordered
        BEFORE the recovery attempt's ``AttemptStarted``, and the recovery
        attempt's prompt carries the ``# Recovery handoff`` section."""
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(goal="g", graders=[])
        lifecycle = Lifecycle(task_id="t1", run_id="run-midturn-happy")
        handoff = RecoveryHandoff(
            work_done="probed handler X mid-turn",
            work_remaining="finish handler Y",
            key_decisions="approach Z",
            suggested_next_step="patch handler.py",
        )
        # First call pumps two messages: the first at 40% (no cross),
        # the second at 95% (crosses 50/75/90 observe tiers AND the
        # 0.9 act ratio). The act crossing sets the recovery event;
        # the invoker raises HarnessRecoveryRequested on its next
        # tick. The recovery attempt then completes normally.
        invoke = _scripted_invoker_midturn_act(
            pre_messages=(
                _assistant(
                    text="early-work ",
                    usage=_usage_dict(input_tokens=40),
                ),
                _assistant(
                    text="late-work-before-interrupt",
                    usage=_usage_dict(input_tokens=95),
                ),
            ),
            iteration_after_resume=_iteration(
                envelope=ValidEnvelope(intent=Intent.VERIFY, reason="done"),
                messages=(_assistant(), _result_msg(num_turns=1)),
                transcript=_wrap('{"intent": "verify", "reason": "done"}'),
                signals=_make_signals(num_turns=1),
            ),
        )
        summarizer = _scripted_summarizer(handoff)
        config = HarnessConfig(
            max_retries=0,
            context_window_tokens=100,
            context_recovery_trigger_ratio=0.9,
            max_context_recoveries=1,
            recovery_summarizer_invoke=summarizer,
            worktree=tmp_path,
        )

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        # Attempt #1 finalized RECOVERED (mid-turn), attempt #2 is the
        # recovery attempt that succeeded.
        assert outcome.lifecycle.status == Status.DONE
        assert len(outcome.attempts) == 2
        assert outcome.attempts[0].outcome == Outcome.RECOVERED
        assert outcome.attempts[1].outcome == Outcome.SUCCEEDED
        # Recovery does NOT consume max_retries (shared budget is its own).
        assert outcome.lifecycle.retries == 0
        # Exactly one harness.context_recovery event, mid_turn trigger.
        events = sink.events(lifecycle.run_id)
        recoveries = [
            e for e in events if e.kind == "harness.context_recovery"
        ]
        assert len(recoveries) == 1
        payload = recoveries[0].payload
        assert payload["trigger"] == "mid_turn"
        assert payload["occupancy_tokens"] == 95
        assert payload["context_window_tokens"] == 100
        assert payload["context_recovery_trigger_ratio"] == 0.9
        assert payload["recoveries_used"] == 1
        assert payload["recoveries_remaining"] == 0
        assert payload["attempt_number"] == 1
        assert payload["iteration"] == 1
        digest = payload["summary_digest"]
        assert digest["work_done_length"] == len(handoff.work_done)
        # FR-6 ordering: the harness.context_recovery event precedes
        # the recovery attempt's AttemptStarted in the per-run audit
        # sequence.
        recovery_idx = events.index(recoveries[0])
        started_indices = [
            i
            for i, e in enumerate(events)
            if e.kind == "harness.attempt_started"
            and e.payload.get("number") == 2
        ]
        assert started_indices
        assert recovery_idx < started_indices[0]
        # FR-7 no double-fire: no boundary recovery event on the
        # same attempt (the mid-turn act preempts it).
        assert len(recoveries) == 1
        # FR-4: the recovery attempt's prompt carries the # Recovery
        # handoff section, threaded from the summarizer's structured
        # output.
        recovery_prompt = invoke.calls[1].prompt  # type: ignore[attr-defined]
        assert "# Recovery handoff" in recovery_prompt
        assert handoff.work_done in recovery_prompt
        assert handoff.suggested_next_step in recovery_prompt
        # First-attempt prompt has no recovery section.
        first_prompt = invoke.calls[0].prompt  # type: ignore[attr-defined]
        assert "# Recovery handoff" not in first_prompt
        # Summarizer seam was used once.
        assert len(summarizer.calls) == 1  # type: ignore[attr-defined]

    def test_observe_event_ordered_before_recovery_event(
        self, tmp_path: Path
    ) -> None:
        """Spec edge case: when the same message crosses 90% observe and
        the act ratio together, the 90% threshold_crossed event is
        ordered BEFORE the harness.context_recovery event in the
        per-run audit sequence (the observe event is emitted in the
        same _check_context_thresholds call that arms the act)."""
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(goal="g", graders=[])
        lifecycle = Lifecycle(task_id="t1", run_id="run-midturn-ordering")
        handoff = RecoveryHandoff(
            work_done="x",
            work_remaining="y",
            key_decisions="z",
            suggested_next_step="next",
        )
        # The single mid-turn message is at 95% -- it crosses 50, 75,
        # AND 90 observe tiers AND the 0.9 act ratio simultaneously.
        invoke = _scripted_invoker_midturn_act(
            pre_messages=(
                _assistant(
                    text="big-jump",
                    usage=_usage_dict(input_tokens=95),
                ),
            ),
            iteration_after_resume=_iteration(
                envelope=ValidEnvelope(intent=Intent.VERIFY, reason="done"),
                messages=(_assistant(), _result_msg(num_turns=1)),
                transcript=_wrap('{"intent": "verify", "reason": "done"}'),
                signals=_make_signals(num_turns=1),
            ),
        )
        summarizer = _scripted_summarizer(handoff)
        config = HarnessConfig(
            max_retries=0,
            context_window_tokens=100,
            context_recovery_trigger_ratio=0.9,
            max_context_recoveries=1,
            recovery_summarizer_invoke=summarizer,
            worktree=tmp_path,
        )

        _run(run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke))

        events = sink.events(lifecycle.run_id)
        observe_90 = next(
            e
            for e in events
            if e.kind == "harness.context_threshold_crossed"
            and e.payload.get("tier") == 0.9
        )
        recovery = next(
            e for e in events if e.kind == "harness.context_recovery"
        )
        assert events.index(observe_90) < events.index(recovery)

    def test_budget_exhausted_skips_midturn_act(
        self, tmp_path: Path
    ) -> None:
        """Spec edge case: when budget is already exhausted, no
        interrupt arms; observe events still fire and the iteration
        runs to its natural end. The invoker returns an
        ``iteration_after_resume`` result on its first call because
        the recovery event is never set."""
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(goal="g", graders=[])
        lifecycle = Lifecycle(task_id="t1", run_id="run-midturn-exhausted")
        # max_context_recoveries=0 means the budget check
        # (recoveries_used < max_context_recoveries) fails on the first
        # crossing -> no event is set -> the invoker completes naturally.
        invoke = _scripted_invoker_midturn_act(
            pre_messages=(
                _assistant(
                    text="late-work",
                    usage=_usage_dict(input_tokens=95),
                ),
            ),
            iteration_after_resume=_iteration(
                envelope=ValidEnvelope(intent=Intent.VERIFY, reason="done"),
                messages=(_assistant(), _result_msg(num_turns=1)),
                transcript=_wrap('{"intent": "verify", "reason": "done"}'),
                signals=_make_signals(num_turns=1),
            ),
        )
        summarizer = _scripted_summarizer(
            RecoveryHandoff(
                work_done="", work_remaining="",
                key_decisions="", suggested_next_step="",
            )
        )
        config = HarnessConfig(
            max_retries=0,
            context_window_tokens=100,
            context_recovery_trigger_ratio=0.9,
            max_context_recoveries=0,
            recovery_summarizer_invoke=summarizer,
            worktree=tmp_path,
        )

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        # No recovery, no summarizer invocation, attempt reaches DONE
        # via the natural-completion path.
        assert outcome.lifecycle.status == Status.DONE
        assert len(outcome.attempts) == 1
        assert outcome.attempts[0].outcome == Outcome.SUCCEEDED
        events = sink.events(lifecycle.run_id)
        assert all(e.kind != "harness.context_recovery" for e in events)
        # Observe events still fire -- act is gated, observe is not.
        crossings = [
            e
            for e in events
            if e.kind == "harness.context_threshold_crossed"
        ]
        assert {c.payload["tier"] for c in crossings} == {0.5, 0.75, 0.9}
        # Summarizer never ran.
        assert summarizer.calls == []  # type: ignore[attr-defined]

    def test_shared_budget_blocks_followup_boundary_recovery(
        self, tmp_path: Path
    ) -> None:
        """FR-5: mid-turn and boundary recovery share
        ``max_context_recoveries``. With the budget set to 1, a run
        that recovers mid-turn cannot also recover at a later
        boundary, even if the second attempt would itself be
        over-ratio."""
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(goal="g", graders=[])
        lifecycle = Lifecycle(
            task_id="t1", run_id="run-midturn-shared-budget"
        )
        handoff = RecoveryHandoff(
            work_done="midturn-done",
            work_remaining="r",
            key_decisions="d",
            suggested_next_step="n",
        )

        # First call: mid-turn over-ratio (95) interrupts via the
        # recovery event. Second call: returns an over-ratio CONTINUE
        # iteration that WOULD trigger boundary recovery if budget
        # remained -- it must not (budget consumed by the mid-turn
        # recovery), so the run terminates AGENT_ERROR via the
        # cap-reached path on the recovery attempt.
        calls: list[InvocationRequest] = []

        async def _invoker(request: InvocationRequest) -> IterationResult:
            calls.append(request)
            if len(calls) == 1:
                if request.on_message is not None:
                    msg = _assistant(
                        text="late-work",
                        usage=_usage_dict(input_tokens=95),
                    )
                    request.on_message(msg)
                    if (
                        request.recovery_interrupt_event is not None
                        and request.recovery_interrupt_event.is_set()
                    ):
                        raise HarnessRecoveryRequested()
                return _iteration(
                    envelope=ValidEnvelope(intent=Intent.CONTINUE),
                    messages=(),
                    transcript="unused",
                )
            # Second call: produce an over-ratio CONTINUE iteration.
            second = _iteration(
                envelope=ValidEnvelope(intent=Intent.CONTINUE),
                messages=(
                    _assistant(usage=_usage_dict(input_tokens=95)),
                    _result_msg(num_turns=1, usage=_usage_dict(95)),
                ),
                transcript="over-ratio-second",
                signals=_make_signals(num_turns=1),
            )
            if request.on_message is not None:
                for m in second.messages:
                    request.on_message(m)
            return second

        _invoker.calls = calls  # type: ignore[attr-defined]
        summarizer = _scripted_summarizer(handoff)
        config = HarnessConfig(
            max_retries=0,
            context_window_tokens=100,
            context_recovery_trigger_ratio=0.9,
            max_context_recoveries=1,
            recovery_summarizer_invoke=summarizer,
            worktree=tmp_path,
        )

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=_invoker)
        )

        # Mid-turn recovery used the budget; boundary recovery is
        # blocked. Attempt 2 hits the cap-with-continue path and
        # finalizes AGENT_ERROR; the run terminates FAILED.
        assert outcome.lifecycle.status == Status.FAILED
        assert len(outcome.attempts) == 2
        assert outcome.attempts[0].outcome == Outcome.RECOVERED
        assert outcome.attempts[1].outcome == Outcome.AGENT_ERROR
        events = sink.events(lifecycle.run_id)
        recoveries = [
            e for e in events if e.kind == "harness.context_recovery"
        ]
        # Exactly one recovery event across BOTH paths.
        assert len(recoveries) == 1
        assert recoveries[0].payload["trigger"] == "mid_turn"
        # Summarizer was called exactly once.
        assert len(summarizer.calls) == 1  # type: ignore[attr-defined]

    def test_midturn_summarizer_failure_routes_to_internal_error(
        self, tmp_path: Path
    ) -> None:
        """Error Handling: a summarizer raise during mid-turn recovery
        aborts the recovery for this crossing and routes through
        INTERNAL_ERROR. Same routing as a failed boundary recovery --
        we never restart with an empty handoff."""
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(goal="g", graders=[])
        lifecycle = Lifecycle(
            task_id="t1", run_id="run-midturn-summ-fail"
        )
        invoke = _scripted_invoker_midturn_act(
            pre_messages=(
                _assistant(
                    text="late-work",
                    usage=_usage_dict(input_tokens=95),
                ),
            ),
            iteration_after_resume=_iteration(
                envelope=ValidEnvelope(intent=Intent.VERIFY, reason="done"),
                messages=(_assistant(), _result_msg(num_turns=1)),
                transcript=_wrap('{"intent": "verify", "reason": "done"}'),
                signals=_make_signals(num_turns=1),
            ),
        )
        summarizer = _scripted_summarizer(
            RuntimeError("midturn summarizer crashed")
        )
        config = HarnessConfig(
            max_retries=0,
            context_window_tokens=100,
            context_recovery_trigger_ratio=0.9,
            max_context_recoveries=1,
            recovery_summarizer_invoke=summarizer,
            worktree=tmp_path,
        )

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        # No recovery event; attempt finalized INTERNAL_ERROR.
        assert outcome.lifecycle.status == Status.FAILED
        assert len(outcome.attempts) == 1
        assert outcome.attempts[0].outcome == Outcome.INTERNAL_ERROR
        events = sink.events(lifecycle.run_id)
        assert all(e.kind != "harness.context_recovery" for e in events)
        crashes = [
            e
            for e in events
            if e.kind == "harness.crash"
            and e.payload.get("classification")
            == "recovery_summarizer_error"
        ]
        assert len(crashes) == 1
        # The crash event records which trigger produced the failed
        # recovery so an operator can attribute it to the mid-turn
        # path rather than the boundary path.
        assert crashes[0].payload.get("trigger") == "mid_turn"

    def test_plain_path_invoker_degrades_to_observe_only(
        self, tmp_path: Path
    ) -> None:
        """FR-4 plain-path degradation: when the invoker does NOT honor
        ``recovery_interrupt_event`` (the plain ``query`` path, scripted
        test invokers), mid-turn act simply does not fire. Observe
        events still emit, the iteration completes naturally, and the
        boundary recovery check covers the over-ratio tail."""
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(goal="g", graders=[])
        lifecycle = Lifecycle(
            task_id="t1", run_id="run-midturn-plain-degrade"
        )
        handoff = RecoveryHandoff(
            work_done="boundary-done",
            work_remaining="",
            key_decisions="",
            suggested_next_step="next",
        )
        # _scripted_invoker is the plain test invoker -- it pumps
        # messages but never polls request.recovery_interrupt_event,
        # so a set event is a no-op and the iteration completes
        # normally. The boundary check then sees the over-ratio
        # CONTINUE and recovers.
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.CONTINUE),
                    messages=(
                        _assistant(usage=_usage_dict(input_tokens=95)),
                        _result_msg(num_turns=1, usage=_usage_dict(95)),
                    ),
                    transcript="over-ratio-plain",
                    signals=_make_signals(num_turns=1),
                ),
                _iteration(
                    envelope=ValidEnvelope(
                        intent=Intent.VERIFY, reason="done"
                    ),
                    messages=(_assistant(), _result_msg(num_turns=1)),
                    transcript=_wrap(
                        '{"intent": "verify", "reason": "done"}'
                    ),
                    signals=_make_signals(num_turns=1),
                ),
            ]
        )
        summarizer = _scripted_summarizer(handoff)
        config = HarnessConfig(
            max_retries=0,
            context_window_tokens=100,
            context_recovery_trigger_ratio=0.9,
            max_context_recoveries=1,
            recovery_summarizer_invoke=summarizer,
            worktree=tmp_path,
        )

        outcome = _run(
            run_task(task, lifecycle, store, sink=sink, config=config, invoke=invoke)
        )

        # Boundary recovery covered the over-ratio tail -- the run
        # reaches DONE via attempt 2.
        assert outcome.lifecycle.status == Status.DONE
        assert len(outcome.attempts) == 2
        assert outcome.attempts[0].outcome == Outcome.RECOVERED
        assert outcome.attempts[1].outcome == Outcome.SUCCEEDED
        events = sink.events(lifecycle.run_id)
        recoveries = [
            e for e in events if e.kind == "harness.context_recovery"
        ]
        # Exactly one boundary recovery, no mid-turn one.
        assert len(recoveries) == 1
        assert recoveries[0].payload["trigger"] == "boundary"


# --- Steering ledger (spec 00025 FR-10) -------------------------------------


class TestSteeringLedger:
    """Applying a control command appends a ``CommandApplied`` domain
    event to the ledger and deletes the applied queue row; an append
    failure retains the row as the visible trace."""

    def _steering_invoker(
        self, store: InMemoryStore, run_id: str
    ) -> Callable[[InvocationRequest], Awaitable[IterationResult]]:
        """Invoker standing in for the live watcher: claims the run's
        pending commands and feeds each through the harness's
        ``on_command_applied`` seam before returning the iteration."""

        async def _invoker(request: InvocationRequest) -> IterationResult:
            claimed = store.claim_commands(
                run_id, now=datetime.now(timezone.utc)
            )
            assert request.on_command_applied is not None
            for cmd in claimed:
                request.on_command_applied(cmd)
            result = _iteration(
                envelope=ValidEnvelope(intent=Intent.VERIFY),
                messages=(_assistant(), _result_msg()),
            )
            if request.on_message is not None:
                for msg in result.messages:
                    request.on_message(msg)
            return result

        return _invoker

    def test_applied_command_lands_in_ledger_and_clears_queue(self) -> None:
        from flywheel_core.events import CommandApplied

        store = InMemoryStore()
        sink = _ListSink()
        task = Task(goal="g", graders=[_ok_command()])
        lifecycle = Lifecycle(task_id="t1", run_id="run-steer")
        store.enqueue_command(
            "run-steer",
            "say",
            {"text": "focus on graders"},
            now=datetime.now(timezone.utc),
        )

        outcome = _run(
            run_task(
                task,
                lifecycle,
                store,
                sink=sink,
                invoke=self._steering_invoker(store, "run-steer"),
            )
        )

        assert outcome.lifecycle.status == Status.DONE
        steering = [
            e
            for e in store.list_domain_events("run-steer")
            if isinstance(e, CommandApplied)
        ]
        assert len(steering) == 1
        assert steering[0].command_kind == "say"
        assert dict(steering[0].command_payload) == {
            "text": "focus on graders"
        }
        assert steering[0].command_id is not None
        # The applied queue row was deleted after the event committed.
        assert store._control_commands == []
        # The ledger fact reached the run stream via the existing
        # domain-mirror path (no second mirror).
        mirror_kinds = [r.kind for r in sink.records]
        assert mirror_kinds.count("domain.command_applied") == 1
        # Replay over the augmented log still folds to the stored state.
        folded = replay(store.list_domain_events("run-steer"))
        assert folded.status == outcome.lifecycle.status
        assert folded.version == outcome.lifecycle.version

    def test_applied_command_ledgered_with_hang_watchdog_enabled(self) -> None:
        # Regression: _invoke_with_watchdog rebuilds InvocationRequest to wrap
        # on_message for its heartbeat; it must also thread on_command_applied
        # through, or the steering ledger (the CommandApplied event AND the
        # applied-queue-row delete) silently breaks whenever the watchdog is
        # enabled (hang_timeout_seconds > 0). The _steering_invoker asserts
        # on_command_applied is not None, so the dropped seam fails here.
        from flywheel_core.events import CommandApplied

        store = InMemoryStore()
        sink = _ListSink()
        task = Task(goal="g", graders=[_ok_command()])
        lifecycle = Lifecycle(task_id="t1", run_id="run-steer-wd")
        store.enqueue_command(
            "run-steer-wd",
            "say",
            {"text": "focus on graders"},
            now=datetime.now(timezone.utc),
        )
        config = HarnessConfig(
            max_retries=0,
            loop_guard=LoopGuardConfig(
                repeated_tool_failure_threshold=None,
                thrash_repeat_threshold=None,
                thrash_window=None,
                hang_timeout_seconds=0.1,
            ),
        )

        outcome = _run(
            run_task(
                task,
                lifecycle,
                store,
                sink=sink,
                config=config,
                invoke=self._steering_invoker(store, "run-steer-wd"),
                # Non-advancing monotonic clock: silence stays 0, so the
                # watchdog is active (routing through _invoke_with_watchdog)
                # but never trips.
                monotonic=lambda: 0.0,
            )
        )

        assert outcome.lifecycle.status == Status.DONE
        steering = [
            e
            for e in store.list_domain_events("run-steer-wd")
            if isinstance(e, CommandApplied)
        ]
        assert len(steering) == 1
        assert steering[0].command_kind == "say"
        # The applied queue row was deleted after the event committed.
        assert store._control_commands == []

    def test_append_failure_retains_queue_row(
        self, capsys: pytest.CaptureFixture[str]
    ) -> None:
        from flywheel_core.events import CommandApplied

        class _LedgerDownStore(InMemoryStore):
            def append_domain_event(
                self, event: Any, *, expected_version: int
            ) -> Lifecycle:
                if isinstance(event, CommandApplied):
                    raise RuntimeError("ledger offline")
                return super().append_domain_event(
                    event, expected_version=expected_version
                )

        store = _LedgerDownStore()
        sink = _ListSink()
        task = Task(goal="g", graders=[_ok_command()])
        lifecycle = Lifecycle(task_id="t1", run_id="run-steer-fail")
        store.enqueue_command(
            "run-steer-fail",
            "say",
            {"text": "steer"},
            now=datetime.now(timezone.utc),
        )

        outcome = _run(
            run_task(
                task,
                lifecycle,
                store,
                sink=sink,
                invoke=self._steering_invoker(store, "run-steer-fail"),
            )
        )

        # The run is unaffected; no steering fact was recorded; the
        # claimed row is retained as the visible trace and the failure
        # surfaced on stderr.
        assert outcome.lifecycle.status == Status.DONE
        assert not any(
            isinstance(e, CommandApplied)
            for e in store.list_domain_events("run-steer-fail")
        )
        assert len(store._control_commands) == 1
        assert store._control_commands[0].claimed_at is not None
        assert "steering ledger append failed" in capsys.readouterr().err

    def test_resolver_approve_ledgers_steering_and_deletes_row(self) -> None:
        from flywheel_core import resolve_manual_approval
        from flywheel_core.events import CommandApplied
        from flywheel_core.invoker_client import CONTROL_COMMAND_APPROVE

        helper = TestResolveManualApproval()
        helper._wt = self._wt
        store, sink, task, lifecycle = helper._park_single_gate(
            run_id="run-steer-approve"
        )
        store.enqueue_command(
            lifecycle.run_id,
            CONTROL_COMMAND_APPROVE,
            {},
            now=datetime.now(timezone.utc),
        )

        result = resolve_manual_approval(lifecycle, store, task, sink=sink)

        assert result.applied
        steering = [
            e
            for e in store.list_domain_events(lifecycle.run_id)
            if isinstance(e, CommandApplied)
        ]
        assert len(steering) == 1
        assert steering[0].command_kind == CONTROL_COMMAND_APPROVE
        assert store._control_commands == []

    def test_resolver_reject_ledgers_steering_and_deletes_row(self) -> None:
        from flywheel_core import resolve_manual_approval
        from flywheel_core.events import CommandApplied
        from flywheel_core.invoker_client import CONTROL_COMMAND_REJECT

        helper = TestResolveManualApproval()
        helper._wt = self._wt
        store, sink, task, lifecycle = helper._park_single_gate(
            run_id="run-steer-reject"
        )
        store.enqueue_command(
            lifecycle.run_id,
            CONTROL_COMMAND_REJECT,
            {"feedback": "tighten the rollout plan"},
            now=datetime.now(timezone.utc),
        )

        result = resolve_manual_approval(lifecycle, store, task, sink=sink)

        assert result.applied
        steering = [
            e
            for e in store.list_domain_events(lifecycle.run_id)
            if isinstance(e, CommandApplied)
        ]
        assert len(steering) == 1
        assert steering[0].command_kind == CONTROL_COMMAND_REJECT
        assert dict(steering[0].command_payload) == {
            "feedback": "tighten the rollout plan"
        }
        assert store._control_commands == []

    @pytest.fixture(autouse=True)
    def _worktree(self, tmp_path: Path) -> None:
        self._wt = str(tmp_path)

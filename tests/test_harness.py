"""Contract tests for :mod:`flywheel.harness`.

Each test injects a stub ``invoke`` callable returning canned
:class:`IterationResult` instances rather than spawning a real agent.
The harness owns lifecycle transitions, attempt recording, and grader
dispatch; the tests assert that contract end-to-end across every
state-detection-map branch the MVP handles.
"""

from __future__ import annotations

import asyncio
import sys
from collections.abc import Awaitable, Callable, Coroutine
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

from flywheel import (
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
    Outcome,
    Status,
    Task,
    TranscriptGrader,
    run_task,
)
from flywheel.harness import finalize_stranded_lifecycle
from flywheel.envelope import (
    CLOSING_FENCE,
    DuplicateEnvelope,
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


def _run(coro: Coroutine[Any, Any, HarnessOutcome]) -> HarnessOutcome:
    return asyncio.run(coro)


def _scripted_invoker(
    results: list[IterationResult],
) -> Callable[[InvocationRequest], Awaitable[IterationResult]]:
    """Build an invoker that returns ``results`` in order, recording calls.

    Each call pops the next IterationResult. Tests can inspect the
    accumulated ``InvocationRequest`` list via the function's ``.calls``
    attribute.
    """
    calls: list[InvocationRequest] = []

    async def _invoker(request: InvocationRequest) -> IterationResult:
        calls.append(request)
        return results.pop(0)

    _invoker.calls = calls  # type: ignore[attr-defined]
    return _invoker


# --- Successful run -------------------------------------------------------


class TestSuccessfulRun:
    def test_verify_then_all_graders_pass_reaches_done(self) -> None:
        store = InMemoryStore()
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

        outcome = _run(run_task(task, lifecycle, store, invoke=invoke))

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
            run_task(task, lifecycle, store, config=config, invoke=invoke)
        )

        attempt = outcome.attempts[0]
        assert attempt.agent_context == ctx
        # Mutating the returned agent_context must not affect the store.
        attempt.agent_context["model_id"] = "tampered"
        reloaded = store.list_attempts(lifecycle.run_id)[0]
        assert reloaded.agent_context["model_id"] == "claude-opus-4-7"

    def test_per_attempt_artifact_directory_created(self, tmp_path: Path) -> None:
        store = InMemoryStore()
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

        _run(run_task(task, lifecycle, store, config=config, invoke=invoke))

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
            run_task(task, lifecycle, store, config=config, invoke=invoke)
        )

        assert outcome.lifecycle.status == Status.DONE
        assert outcome.lifecycle.retries == 1
        assert len(outcome.attempts) == 2
        assert outcome.attempts[0].outcome == Outcome.VALIDATION_FAILED
        assert outcome.attempts[1].outcome == Outcome.SUCCEEDED

    def test_validation_failure_terminates_when_retries_exhausted(self) -> None:
        store = InMemoryStore()
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
            run_task(task, lifecycle, store, config=config, invoke=invoke)
        )

        assert outcome.lifecycle.status == Status.FAILED
        assert outcome.lifecycle.retries == 0
        assert len(outcome.attempts) == 1
        assert outcome.attempts[0].outcome == Outcome.VALIDATION_FAILED

    def test_retry_eligibility_delegates_to_lifecycle(self) -> None:
        """``Lifecycle.is_retry_eligible`` is the single source of truth.

        Bumping ``Lifecycle.retries`` past ``max_retries`` (simulated by
        a prior failed run) makes a fresh failure terminal without the
        harness reimplementing the rule.
        """
        store = InMemoryStore()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(1)'",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-elig", retries=2)
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                )
            ]
        )
        config = HarnessConfig(max_retries=2)  # retries (2) >= max (2) — exhausted.

        outcome = _run(
            run_task(task, lifecycle, store, config=config, invoke=invoke)
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

        outcome = _run(run_task(task, lifecycle, store, invoke=invoke))

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
            run_task(task, lifecycle, store, config=config, invoke=invoke)
        )

        assert outcome.lifecycle.status == Status.FAILED
        assert len(invoke.calls) == 1  # type: ignore[attr-defined]


# --- Blocked / abort ------------------------------------------------------


class TestBlockedAndAbort:
    def test_blocked_transitions_to_interrupted_and_preserves_retries(
        self,
    ) -> None:
        store = InMemoryStore()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-blocked")
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(
                        intent=Intent.BLOCKED,
                        reason="need API key",
                    ),
                    messages=(_assistant(),),
                )
            ]
        )
        config = HarnessConfig(max_retries=5)

        outcome = _run(
            run_task(task, lifecycle, store, config=config, invoke=invoke)
        )

        assert outcome.lifecycle.status == Status.INTERRUPTED
        # Blocked must not consume retries.
        assert outcome.lifecycle.retries == 0
        attempt = outcome.attempts[0]
        assert attempt.outcome == Outcome.CANCELLED
        assert "need API key" in attempt.error
        # No grader rows — validation did not run.
        assert store.list_grader_results(lifecycle.run_id, 1) == []

    def test_abort_transitions_to_failed(self) -> None:
        store = InMemoryStore()
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
            run_task(task, lifecycle, store, config=config, invoke=invoke)
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

        outcome = _run(run_task(task, lifecycle, store, invoke=invoke))

        assert outcome.lifecycle.status == Status.FAILED
        attempt = outcome.attempts[0]
        assert attempt.outcome == Outcome.INTERNAL_ERROR
        assert "ProcessError" in attempt.error
        # Crash recorded distinctly via an event.
        events = store.list_events(lifecycle.run_id)
        crash_events = [e for e in events if e.kind == "harness.crash"]
        assert len(crash_events) == 1
        assert crash_events[0].payload["classification"] == "deferred"


# --- Budget breach --------------------------------------------------------


class TestBudgetBreach:
    def test_max_turns_breach_routes_to_failed_validation(self) -> None:
        store = InMemoryStore()
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

        outcome = _run(run_task(task, lifecycle, store, invoke=invoke))

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

        _run(run_task(task, lifecycle, store, invoke=invoke))

        events = [
            e
            for e in store.list_events(lifecycle.run_id)
            if e.kind == "harness.budget_exceeded"
        ]
        assert len(events) == 1
        assert events[0].payload["breached"] == "max_total_tokens"
        assert events[0].payload["observed"]["total_tokens"] == 50


# --- Continue loop --------------------------------------------------------


class TestContinueLoop:
    def test_continue_loops_within_cap_then_verifies(self) -> None:
        store = InMemoryStore()
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
            run_task(task, lifecycle, store, config=config, invoke=invoke)
        )

        assert outcome.lifecycle.status == Status.DONE
        assert len(invoke.calls) == 3  # type: ignore[attr-defined]
        # All three iterations belong to the same Attempt.
        assert len(outcome.attempts) == 1

    def test_continue_past_cap_fails_validation_without_grading(self) -> None:
        store = InMemoryStore()
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
            run_task(task, lifecycle, store, config=config, invoke=invoke)
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

        src_root = Path(__file__).resolve().parents[1] / "src" / "flywheel"
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

        src_root = Path(__file__).resolve().parents[1] / "src" / "flywheel"
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

        outcome = _run(run_task(task, lifecycle, store, invoke=invoke))

        assert outcome.lifecycle.status == Status.DONE
        # The PENDING → READY → RUNNING → VALIDATING → DONE chain ran.
        assert Status.READY in outcome.lifecycle.timestamps
        assert Status.RUNNING in outcome.lifecycle.timestamps
        assert Status.VALIDATING in outcome.lifecycle.timestamps
        assert Status.DONE in outcome.lifecycle.timestamps

    def test_resume_from_interrupted_runs_a_new_attempt(self) -> None:
        store = InMemoryStore()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-resume")

        invoke_first = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(
                        intent=Intent.BLOCKED, reason="need help"
                    ),
                    messages=(_assistant(),),
                )
            ]
        )
        _run(run_task(task, lifecycle, store, invoke=invoke_first))
        assert lifecycle.status == Status.INTERRUPTED

        invoke_second = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(), _result_msg()),
                )
            ]
        )
        outcome = _run(run_task(task, lifecycle, store, invoke=invoke_second))

        assert outcome.lifecycle.status == Status.DONE
        assert len(outcome.attempts) == 2


# --- Rate-limit event surface --------------------------------------------


class TestRateLimitSurface:
    def test_rate_limit_event_surfaced_without_classification(self) -> None:
        """rate_limited per docs/loop.md is transient — the harness
        records the signal but does not classify it as a failure."""
        store = InMemoryStore()
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

        outcome = _run(run_task(task, lifecycle, store, invoke=invoke))

        assert outcome.lifecycle.status == Status.DONE
        completed_events = [
            e
            for e in store.list_events(lifecycle.run_id)
            if e.kind == "harness.iteration_completed"
        ]
        assert completed_events[0].payload["rate_limited"] is True


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
            run_task(task, lifecycle, store, config=config, invoke=invoke)
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
        events = store.list_events(lifecycle.run_id)
        crash = [e for e in events if e.kind == "harness.crash"]
        assert len(crash) == 1
        assert crash[0].payload["classification"] == "grader_signaled"
        assert crash[0].payload["signal"] == 2

    def test_sigterm_killed_command_grader_routes_to_interrupted(
        self,
    ) -> None:
        store = InMemoryStore()
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

        outcome = _run(run_task(task, lifecycle, store, invoke=invoke))

        assert outcome.lifecycle.status == Status.INTERRUPTED
        attempt = outcome.attempts[0]
        assert attempt.outcome == Outcome.INTERNAL_ERROR
        crash = [
            e for e in store.list_events(lifecycle.run_id)
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
            run_task(task, lifecycle, store, config=config, invoke=invoke)
        )

        assert outcome.lifecycle.status == Status.FAILED
        attempt = outcome.attempts[0]
        assert attempt.outcome == Outcome.VALIDATION_FAILED


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
        lc = self._seed_running(store, "run-strand-r")
        ok = finalize_stranded_lifecycle(store, lc.run_id)
        assert ok is True
        reloaded = store.load_lifecycle(lc.run_id)
        assert reloaded is not None
        assert reloaded.status == Status.INTERRUPTED
        attempts = store.list_attempts(lc.run_id)
        assert attempts[0].ended_at is not None
        assert attempts[0].outcome == Outcome.INTERNAL_ERROR
        crash = [
            e for e in store.list_events(lc.run_id)
            if e.kind == "harness.crash"
        ]
        assert len(crash) == 1
        assert crash[0].payload["classification"] == "worker_interrupted"
        assert crash[0].payload["from_status"] == "running"

    def test_finalize_validating_transitions_to_interrupted(self) -> None:
        store = InMemoryStore()
        lc = self._seed_validating(store, "run-strand-v")
        ok = finalize_stranded_lifecycle(store, lc.run_id)
        assert ok is True
        reloaded = store.load_lifecycle(lc.run_id)
        assert reloaded is not None
        assert reloaded.status == Status.INTERRUPTED

    def test_finalize_done_lifecycle_is_noop(self) -> None:
        store = InMemoryStore()
        now = datetime.now(timezone.utc)
        lc = Lifecycle(task_id="t", run_id="run-strand-done")
        lc.transition_to(Status.READY, now=now)
        lc.transition_to(Status.RUNNING, now=now)
        lc.transition_to(Status.VALIDATING, now=now)
        lc.transition_to(Status.DONE, now=now)
        store.create_lifecycle(lc)
        ok = finalize_stranded_lifecycle(store, lc.run_id)
        assert ok is False
        reloaded = store.load_lifecycle(lc.run_id)
        assert reloaded is not None
        assert reloaded.status == Status.DONE

    def test_finalize_missing_lifecycle_is_noop(self) -> None:
        store = InMemoryStore()
        ok = finalize_stranded_lifecycle(store, "run-does-not-exist")
        assert ok is False


# --- Audit-stream: SDK message persistence + strict-audit failure --------


class _RaisingStore(InMemoryStore):
    """In-memory store with selectively-raising audit write paths.

    Used by the strict-audit tests to assert the harness routes both
    ``append_event`` failures and ``save_sdk_messages`` failures through
    the same ``harness.audit_write_failed`` / ``INTERNAL_ERROR``
    finalization path. Callers configure which method raises by setting
    the ``raise_on_append_event`` / ``raise_on_save_sdk_messages``
    attributes; failures are emitted by the harness via the
    best-effort secondary emit, which on this store SUCCEEDS by
    design (so we can assert exactly one audit_write_failed event was
    recorded).
    """

    def __init__(self) -> None:
        super().__init__()
        self.raise_on_save_sdk_messages: bool = False
        self.raise_on_append_event_kind: str | None = None

    def save_sdk_messages(
        self, run_id, attempt_number, iteration_number, messages
    ):  # type: ignore[override]
        if self.raise_on_save_sdk_messages:
            raise RuntimeError("simulated sdk persistence failure")
        return super().save_sdk_messages(
            run_id, attempt_number, iteration_number, messages
        )

    def append_event(self, event):  # type: ignore[override]
        if (
            self.raise_on_append_event_kind is not None
            and event.kind == self.raise_on_append_event_kind
        ):
            raise RuntimeError("simulated event persistence failure")
        return super().append_event(event)


class TestSdkMessagePersistence:
    def test_each_iteration_persists_its_message_batch(self) -> None:
        store = InMemoryStore()
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
            run_task(task, lifecycle, store, config=config, invoke=invoke)
        )
        assert outcome.lifecycle.status == Status.DONE

        listed = store.list_sdk_messages(lifecycle.run_id)
        # All 4 messages across the two iterations, in input order.
        assert len(listed) == len(iter1_msgs) + len(iter2_msgs)
        types = [m.message_type for m in listed]
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

        _run(run_task(task, lifecycle, store, invoke=invoke))
        # No SDK messages recorded but iteration_completed still fired.
        assert store.list_sdk_messages(lifecycle.run_id) == []
        completed = [
            e
            for e in store.list_events(lifecycle.run_id)
            if e.kind == "harness.iteration_completed"
        ]
        assert len(completed) == 1


class TestInterleavedAuditSequence:
    def test_events_and_sdk_messages_share_one_monotonic_run_sequence(
        self,
    ) -> None:
        store = InMemoryStore()
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

        _run(run_task(task, lifecycle, store, invoke=invoke))

        events = store.list_events(lifecycle.run_id)
        sdk = store.list_sdk_messages(lifecycle.run_id)
        all_seqs = [e.sequence for e in events] + [m.sequence for m in sdk]
        assert all(s is not None for s in all_seqs)
        # Strictly ascending across both record types.
        deduped = sorted(set(all_seqs))
        assert deduped == sorted(all_seqs)
        assert len(deduped) == len(all_seqs)
        # The earliest sequence is 1; no gaps.
        assert deduped == list(range(1, len(deduped) + 1))


class TestStrictAuditFailure:
    def test_save_sdk_messages_failure_finalizes_attempt_as_internal_error(
        self,
    ) -> None:
        store = _RaisingStore()
        store.raise_on_save_sdk_messages = True
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-audit-sdk-fail")
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(),),
                )
            ]
        )

        outcome = _run(run_task(task, lifecycle, store, invoke=invoke))

        # Retries default to 0, so lifecycle ends in FAILED with the
        # audit error propagated through the retry policy.
        assert outcome.lifecycle.status == Status.FAILED
        assert "audit write failed" in outcome.lifecycle.error
        # Attempt was finalized as INTERNAL_ERROR per spec.
        attempt = outcome.attempts[0]
        assert attempt.outcome == Outcome.INTERNAL_ERROR
        assert "audit write failed" in attempt.error
        # Exactly one harness.audit_write_failed event was emitted, with
        # the failing_method correctly identifying the broken write path.
        audit_failures = [
            e
            for e in store.list_events(lifecycle.run_id)
            if e.kind == "harness.audit_write_failed"
        ]
        assert len(audit_failures) == 1
        payload = audit_failures[0].payload
        assert payload["failing_method"] == "save_sdk_messages"
        assert payload["error_type"] == "RuntimeError"
        assert "simulated sdk persistence failure" in payload["message"]
        # iteration_number is known at the failure site.
        assert payload["attempt_number"] == 1
        assert payload["iteration_number"] == 1
        # Command grader did NOT run — validation never started.
        assert store.list_grader_results(lifecycle.run_id, 1) == []

    def test_append_event_failure_finalizes_attempt_as_internal_error(
        self,
    ) -> None:
        store = _RaisingStore()
        # Fail on the iteration_completed emit so we are clearly past
        # save_sdk_messages and inside the same iteration's events.
        store.raise_on_append_event_kind = "harness.iteration_completed"
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                )
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-audit-evt-fail")
        invoke = _scripted_invoker(
            [
                _iteration(
                    envelope=ValidEnvelope(intent=Intent.VERIFY),
                    messages=(_assistant(),),
                )
            ]
        )

        outcome = _run(run_task(task, lifecycle, store, invoke=invoke))

        assert outcome.lifecycle.status == Status.FAILED
        attempt = outcome.attempts[0]
        assert attempt.outcome == Outcome.INTERNAL_ERROR
        assert "audit write failed" in attempt.error
        audit_failures = [
            e
            for e in store.list_events(lifecycle.run_id)
            if e.kind == "harness.audit_write_failed"
        ]
        assert len(audit_failures) == 1
        payload = audit_failures[0].payload
        assert payload["failing_method"] == "append_event"
        assert payload["error_type"] == "RuntimeError"
        # The save_sdk_messages call succeeded before the append_event
        # failure, so the iteration's SDK messages should already be in
        # the audit trail.
        assert len(store.list_sdk_messages(lifecycle.run_id)) == 1


class TestFinalizeStrandedReceivesSequenceNumbers:
    def test_finalize_stranded_emits_events_with_assigned_sequences(
        self,
    ) -> None:
        store = InMemoryStore()
        now = datetime.now(timezone.utc)
        lc = Lifecycle(task_id="t", run_id="run-strand-seq")
        lc.transition_to(Status.READY, now=now)
        lc.transition_to(Status.RUNNING, now=now)
        store.create_lifecycle(lc)
        store.save_attempt(
            lc.run_id,
            Attempt(number=1, started_at=now, run_id=lc.run_id),
        )

        ok = finalize_stranded_lifecycle(store, lc.run_id)
        assert ok is True

        # Every event the stranded-finalize emitted carries a
        # per-run monotonic sequence assigned by the store.
        events = store.list_events(lc.run_id)
        assert len(events) >= 1
        seqs = [e.sequence for e in events]
        assert all(s is not None for s in seqs)
        assert seqs == sorted(seqs)
        # No collisions.
        assert len(set(seqs)) == len(seqs)


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
        from flywheel import harness as harness_module

        expected = {
            "thrash detection",
            "hang threshold defaults",
            "context-recovery policy",
            "fine-grained crash classification",
            "blocked_implicit semantic similarity",
        }
        assert set(harness_module._DEFERRED_LOOP_SUBSYSTEMS) == expected

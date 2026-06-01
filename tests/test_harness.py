"""Contract tests for :mod:`flywheel.harness`.

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
    RubricGrader,
    Status,
    Task,
    TranscriptGrader,
    replay,
    run_task,
)
from flywheel.grader_rubric import (
    CLOSING_FENCE as RUBRIC_CLOSING_FENCE,
)
from flywheel.grader_rubric import (
    OPENING_FENCE as RUBRIC_OPENING_FENCE,
)
from flywheel.grader_rubric import (
    RubricJudgeError,
)
from flywheel.harness import (
    _build_observation,
    _build_usage_breakdown,
    finalize_stranded_lifecycle,
)
from flywheel.loaders import task_digest
from flywheel.envelope import (
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


def _run(coro: Coroutine[Any, Any, HarnessOutcome]) -> HarnessOutcome:
    return asyncio.run(coro)


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

    def test_command_grader_runs_in_worktree(self, tmp_path: Path) -> None:
        # A relative-path check passes only if the grader's CWD is the
        # configured sandbox. Regression guard for the harness threading
        # config.worktree into run_command_graders (and not grading its
        # own ambient CWD).
        sandbox = tmp_path / "sandbox"
        sandbox.mkdir()
        (sandbox / "sentinel.flag").write_text("ok", encoding="utf-8")
        store = InMemoryStore()
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
            run_task(task, lifecycle, store, config=config, invoke=invoke)
        )

        # The sentinel only exists inside the sandbox, so reaching DONE
        # proves the grader ran there rather than in the harness CWD.
        assert outcome.lifecycle.status == Status.DONE

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

        With no retry budget (``max_retries=0``), the first validation
        failure is terminal: ``retries`` (0) is not ``< max_retries`` (0),
        so the harness routes to ``FAILED`` without reimplementing the
        rule. (Under event sourcing ``retries`` accumulates only via real
        retry edges in the log, so exhaustion is expressed through the
        budget rather than a constructor-injected count.)
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
            e for e in store.list_events(lifecycle.run_id)
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
            run_task(task, lifecycle, store, config=config, invoke=invoke)
        )

        # Routed through protocol-failure path: FAILED_VALIDATION terminal
        # (no retry budget here), AGENT_ERROR outcome, harness.blocked
        # never emitted, blocked_requires_json never populated.
        assert outcome.lifecycle.status == Status.FAILED
        attempt = outcome.attempts[0]
        assert attempt.outcome == Outcome.AGENT_ERROR
        assert "invalid blocked requires" in attempt.error
        assert outcome.lifecycle.blocked_requires_json is None
        kinds = [e.kind for e in store.list_events(lifecycle.run_id)]
        assert "harness.blocked" not in kinds
        assert "harness.protocol_failure" in kinds

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
        _run(run_task(task, lifecycle, store, invoke=invoke_first))
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
        outcome = _run(run_task(task, lifecycle, store, invoke=invoke_second))

        assert outcome.lifecycle.status == Status.DONE
        assert outcome.lifecycle.blocked_requires_json is None
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
        self, store: InMemoryStore, run_id: str
    ) -> Mapping[str, Any]:
        events = [
            e
            for e in store.list_events(run_id)
            if e.kind == "harness.iteration_completed"
        ]
        assert len(events) == 1, events
        return events[0].payload

    def _completed_payloads(
        self, store: InMemoryStore, run_id: str
    ) -> list[Mapping[str, Any]]:
        return [
            e.payload
            for e in store.list_events(run_id)
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

        _run(run_task(task, lifecycle, store, invoke=invoke))

        payload = self._completed_payload(store, lifecycle.run_id)
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

        _run(run_task(task, lifecycle, store, invoke=invoke))

        payload = self._completed_payload(store, lifecycle.run_id)
        assert payload["num_turns"] == 4
        assert payload["total_cost_usd"] == 0.42

    # FR-2 edge: when the SDK never produced a ResultMessage, cost / turns
    # are absent — surface as null, not zero.
    def test_cost_and_turns_null_when_signals_lack_them(self) -> None:
        store = InMemoryStore()
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

        _run(run_task(task, lifecycle, store, invoke=invoke))

        payload = self._completed_payload(store, lifecycle.run_id)
        assert payload["num_turns"] is None
        assert payload["total_cost_usd"] is None

    # FR-3: emitted total_tokens equals the transcript-grader breach figure
    # (the same _build_observation pipes feed both code paths).
    def test_total_tokens_matches_observation(self) -> None:
        store = InMemoryStore()
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

        _run(run_task(task, lifecycle, store, invoke=invoke))

        payload = self._completed_payload(store, lifecycle.run_id)
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

        _run(run_task(task, lifecycle, store, config=config, invoke=invoke))

        payloads = self._completed_payloads(store, lifecycle.run_id)
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

        _run(run_task(task, lifecycle, store, invoke=invoke))

        payload = self._completed_payload(store, lifecycle.run_id)
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

        _run(run_task(task, lifecycle, store, invoke=invoke))

        payload = self._completed_payload(store, lifecycle.run_id)
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


# --- Entry-time crash recording ------------------------------------------


class TestEntryTimeCrash:
    """run_task must persist a lifecycle row and a harness.crash event
    when a Python exception escapes after the lifecycle row exists.

    Backs the audit finding in
    ``.workflow/audits/08-recoverable-blocked-lifecycles.md``: 76
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
            _run(run_task(task, lifecycle, store, invoke=invoke))

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
            for e in store.list_events(lifecycle.run_id)
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
            _run(run_task(task, lifecycle, store, invoke=invoke))

        events = store.list_events(lifecycle.run_id)
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
        _run(run_task(task, lifecycle, store, invoke=invoke_first))
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
            run_task(task, stale, store, invoke=invoke_second)
        )

        # Reconciliation: the stale Lifecycle's status/version were
        # overwritten with the persisted row's values before the loop
        # started.
        assert outcome.lifecycle.status == Status.DONE
        # No harness.crash event from the resume path.
        crash_events = [
            e
            for e in store.list_events(stale.run_id)
            if e.kind == "harness.crash"
        ]
        assert crash_events == []

    def test_original_exception_propagates_after_crash_recorded(
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
        lifecycle = Lifecycle(task_id="t1", run_id="run-propagate")
        sentinel = RuntimeError("propagate me unchanged")
        invoke = self._raising_invoke(sentinel)

        # The caller observes the original exception, not a wrapper.
        with pytest.raises(RuntimeError) as excinfo:
            _run(run_task(task, lifecycle, store, invoke=invoke))
        assert excinfo.value is sentinel

        # And the crash event landed before the propagation.
        crash_events = [
            e
            for e in store.list_events(lifecycle.run_id)
            if e.kind == "harness.crash"
        ]
        assert len(crash_events) == 1


# --- Audit-stream: SDK message persistence + strict-audit failure --------


class _RaisingStore(InMemoryStore):
    """In-memory store with selectively-raising audit write paths.

    Used by the strict-audit tests to assert the harness routes both
    ``append_event`` failures and ``append_sdk_message`` failures through
    the same ``harness.audit_write_failed`` / ``INTERNAL_ERROR``
    finalization path. Callers configure which method raises by setting
    the ``raise_on_append_event_kind`` / ``raise_on_append_sdk_message``
    attributes; failures are emitted by the harness via the
    best-effort secondary emit, which on this store SUCCEEDS by
    design (so we can assert exactly one audit_write_failed event was
    recorded).
    """

    def __init__(self) -> None:
        super().__init__()
        self.raise_on_append_sdk_message: bool = False
        self.raise_on_append_event_kind: str | None = None

    def append_sdk_message(self, message):  # type: ignore[override]
        if self.raise_on_append_sdk_message:
            raise RuntimeError("simulated sdk persistence failure")
        return super().append_sdk_message(message)

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
        # Strictly ascending and unique across both record types: the
        # telemetry events and SDK messages share one per-run counter.
        deduped = sorted(set(all_seqs))
        assert deduped == sorted(all_seqs)
        assert len(deduped) == len(all_seqs)
        # The counter is also shared with state-bearing domain events
        # (transitions, attempt start/finalize), which the audit stream
        # does not surface — so the telemetry/SDK sequences are strictly
        # ascending but not contiguous from 1.
        domain = store.list_domain_events(lifecycle.run_id)
        domain_seqs = [e.sequence for e in domain]
        assert domain_seqs  # the run produced domain events
        # No sequence value is shared between the two partitions.
        assert set(domain_seqs).isdisjoint(set(all_seqs))

    def test_harness_domain_log_replays_to_the_stored_lifecycle(
        self,
    ) -> None:
        """End-to-end determinism oracle: folding the domain-event log the
        harness produced reconstructs the persisted lifecycle exactly. This
        is the event-sourcing guarantee at the harness level — state is the
        fold of the log, with no separate authoritative row."""
        store = InMemoryStore()
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

        outcome = _run(run_task(task, lifecycle, store, invoke=invoke))
        assert outcome.lifecycle.status is Status.DONE

        loaded = store.load_lifecycle("run-oracle")
        folded = replay(store.list_domain_events("run-oracle"))
        assert loaded == folded
        # version is the domain-event offset.
        assert loaded is not None
        assert loaded.version == len(store.list_domain_events("run-oracle"))


class TestStrictAuditFailure:
    def test_append_sdk_message_failure_finalizes_attempt_as_internal_error(
        self,
    ) -> None:
        store = _RaisingStore()
        store.raise_on_append_sdk_message = True
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
        assert payload["failing_method"] == "append_sdk_message"
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
            run_task(task, lifecycle, store, config=config, invoke=invoke)
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
            run_task(task, lifecycle, store, config=config, invoke=invoke)
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
            run_task(task, lifecycle, store, config=config, invoke=invoke)
        )

        assert outcome.lifecycle.status == Status.INTERRUPTED
        assert outcome.lifecycle.retries == 0  # no retry consumed
        assert outcome.attempts[0].outcome == Outcome.VALIDATION_FAILED
        assert "halt-me" in outcome.attempts[0].error

    def test_rubric_fail_with_retries_exhausted_reaches_failed(self) -> None:
        store = InMemoryStore()
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
            run_task(task, lifecycle, store, config=config, invoke=invoke)
        )

        assert outcome.lifecycle.status == Status.FAILED
        assert "strict" in outcome.lifecycle.error
        assert outcome.lifecycle.retries == 1
        assert len(outcome.attempts) == 2

    def test_command_fail_short_circuits_before_rubric(self) -> None:
        store = InMemoryStore()
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
            run_task(task, lifecycle, store, config=config, invoke=invoke)
        )

        assert outcome.lifecycle.status == Status.FAILED
        assert judge.calls == []
        events = [
            e
            for e in store.list_events(lifecycle.run_id)
            if e.kind == "harness.rubric_invoked"
        ]
        assert events == []

    def test_rubric_invoked_and_verdict_events_emit_with_metadata(
        self,
    ) -> None:
        store = InMemoryStore()
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

        _run(run_task(task, lifecycle, store, config=config, invoke=invoke))

        events = store.list_events(lifecycle.run_id)
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
            run_task(task, lifecycle, store, config=config, invoke=invoke)
        )

        # With max_retries=0, INTERNAL_ERROR exhausts immediately -> FAILED.
        assert outcome.lifecycle.status == Status.FAILED
        attempt = outcome.attempts[0]
        assert attempt.outcome == Outcome.INTERNAL_ERROR
        assert "rubric judge failed" in attempt.error
        assert "rcrash" in attempt.error
        crash_events = [
            e
            for e in store.list_events(lifecycle.run_id)
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
            run_task(task, lifecycle, store, config=config, invoke=invoke)
        )

        assert outcome.lifecycle.status == Status.FAILED
        assert len(outcome.attempts) == 2
        assert outcome.attempts[0].outcome == Outcome.INTERNAL_ERROR
        assert outcome.attempts[1].outcome == Outcome.INTERNAL_ERROR
        assert outcome.lifecycle.retries == 1

    def test_unknown_verdict_reaches_done_and_emits_warning(self) -> None:
        store = InMemoryStore()
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
            run_task(task, lifecycle, store, config=config, invoke=invoke)
        )

        assert outcome.lifecycle.status == Status.DONE
        unknown_events = [
            e
            for e in store.list_events(lifecycle.run_id)
            if e.kind == "harness.rubric_unknown"
        ]
        assert len(unknown_events) == 1
        assert unknown_events[0].payload["grader_name"] == "speculative"
        assert unknown_events[0].payload["summary"] == "punted"

    def test_retry_attempt_carries_prior_rubric_findings_into_prompt(
        self,
    ) -> None:
        store = InMemoryStore()
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
            run_task(task, lifecycle, store, config=config, invoke=invoke)
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


class TestHarnessConfigDefaults:
    def test_default_rubric_config_fields(self) -> None:
        cfg = HarnessConfig()
        assert cfg.rubric_judge_model is None
        assert cfg.rubric_judge_max_turns == 8
        assert cfg.worktree is None
        assert cfg.rubric_judge_invoke is None


class TestTaskPersistence:
    def test_run_persists_task_and_pins_content_hash(self) -> None:
        store = InMemoryStore()
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

        outcome = _run(run_task(task, lifecycle, store, invoke=invoke))

        # Graderless run reaches DONE on the agent's own claim.
        assert outcome.lifecycle.status == Status.DONE
        # The run pins the exact task version it executed, and that version
        # is retrievable both by hash and via the run.
        digest = task_digest(task)
        assert outcome.lifecycle.task_content_hash == digest
        assert store.load_task("persist-me", digest) == task
        assert store.load_task_for_run("run-persist") == task

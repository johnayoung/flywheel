"""In-loop verification of the context-recovery policy (spec 00018 FR-7).

This is the loop-path slot for spec 00018. It does NOT exercise the
parsing rules, the config validation, or the precedence edge cases --
those live in ``tests/test_harness.py::TestContextRecovery`` and
``tests/test_recovery_summarizer.py``. What this file does, and the only
thing it does, is drive a fixture through the real production loop so
the shipped summarize-restart path is observed end-to-end:

* The real ``flywheel.harness.run_task`` runs the attempts.
* A real ``flywheel.store_sqlite.SqliteStore`` persists every event,
  attempt, and lifecycle row. We assert against what is on disk, not
  what the harness happened to return.
* The real ``flywheel.recovery_summarizer.run_recovery_summarizer`` is
  invoked through ``HarnessConfig.recovery_summarizer_invoke`` -- the
  same seam production uses. Only the summarizer's response text is
  scripted (a well-formed handoff envelope); the runner that parses
  and validates it is the production code.
* The agent is scripted through the production
  ``InvokeFunc`` seam: one CONTINUE iteration whose input-side
  occupancy crosses ``context_recovery_trigger_ratio`` (producing the
  recovery), followed by one VERIFY iteration that drives the recovery
  attempt to ``DONE``.

A passing test proves both ends of FR-3 ran on the real loop:

* PRODUCING recovery: attempt #1 finalizes ``Outcome.RECOVERED`` and a
  ``harness.context_recovery`` event lands in the audit stream.
* APPLYING recovery: a second attempt actually starts (an
  ``AttemptStarted`` / ``harness.attempt_started`` event for
  ``number=2`` is persisted) and its prompt carries the
  ``# Recovery handoff`` section populated from the summarizer's
  structured output.

Per the spec's Decisions Log, no schema column or table is added by
this feature, so the FR-4 v(N-1) store-seeding / forward-migration
clause does not apply -- the test exercises the current schema only.
"""

from __future__ import annotations

import asyncio
import json
from pathlib import Path

from claude_agent_sdk import AssistantMessage, ResultMessage, TextBlock

from flywheel import (
    HarnessConfig,
    Intent,
    InvocationRequest,
    InvocationSignals,
    IterationResult,
    Lifecycle,
    Outcome,
    Status,
    Task,
    ValidEnvelope,
    run_task,
)
from flywheel.prompt import RecoveryHandoff
from flywheel.recovery_summarizer import (
    CLOSING_FENCE as RECOVERY_CLOSING_FENCE,
)
from flywheel.recovery_summarizer import (
    OPENING_FENCE as RECOVERY_OPENING_FENCE,
)
from flywheel.store_sqlite import SqliteStore


# --- Scripted SDK message helpers -----------------------------------------

_SESSION_ID = "sess-in-loop-recover"


def _usage(input_tokens: int) -> dict[str, int]:
    """Build a usage breakdown whose input-side occupancy equals
    ``input_tokens``.

    The harness sums ``input_tokens + cache_read_input_tokens +
    cache_creation_input_tokens`` to compute the iteration's occupancy
    (spec 00018 FR-1 / ``_OCCUPANCY_USAGE_KEYS``); putting the full
    occupancy on ``input_tokens`` keeps the fixture readable while
    still routing through the real summing logic.
    """
    return {
        "input_tokens": input_tokens,
        "output_tokens": 0,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0,
    }


def _signals() -> InvocationSignals:
    return InvocationSignals(
        stop_reason="end_turn",
        num_turns=1,
        total_cost_usd=0.0,
        result_is_error=False,
        result_subtype="success",
        api_error_status=None,
        session_id=_SESSION_ID,
    )


def _assistant_msg(*, input_tokens: int, text: str) -> AssistantMessage:
    return AssistantMessage(
        content=[TextBlock(text=text)],
        model="claude-test",
        stop_reason="end_turn",
        session_id=_SESSION_ID,
        usage=_usage(input_tokens),
    )


def _result_msg(*, input_tokens: int) -> ResultMessage:
    return ResultMessage(
        subtype="success",
        duration_ms=1,
        duration_api_ms=1,
        is_error=False,
        num_turns=1,
        session_id=_SESSION_ID,
        stop_reason="end_turn",
        total_cost_usd=0.0,
        usage=_usage(input_tokens),
    )


def _verify_transcript() -> str:
    """A transcript that ends with a real verify-intent loop envelope.

    The harness parses ``IterationResult.envelope`` directly so the
    transcript text is informational, but matching the production
    fencing keeps the fixture honest -- a future check that hashes the
    full transcript would still see a well-formed envelope.
    """
    return (
        "<!-- LOOP_STATUS -->\n"
        '{"intent": "verify", "reason": "done"}\n'
        "<!-- /LOOP_STATUS -->\n"
    )


# --- The slot --------------------------------------------------------------


def test_real_loop_drives_full_summarize_restart(tmp_path: Path) -> None:
    """Spec 00018 FR-7: real production loop runs a full summarize-restart.

    A two-iteration script trips the policy on the first CONTINUE
    iteration (occupancy >= context_window_tokens *
    context_recovery_trigger_ratio) and lets the recovery attempt
    converge on the second VERIFY iteration. The graderless task means
    the only thing standing between VERIFY and DONE is the harness
    state machine itself, so any deviation from the shipped recovery
    path surfaces as a wrong terminal status, a missing event, or a
    prompt without ``# Recovery handoff``.
    """
    db_path = tmp_path / "flywheel.sqlite"
    worktree = tmp_path / "worktree"
    worktree.mkdir()

    # Two iterations, in script order:
    #
    # 1. attempt #1, iter #1: intent=continue, input-side occupancy = 95
    #    tokens. With context_window_tokens=100 and
    #    context_recovery_trigger_ratio=0.9 the threshold is 90; 95 >= 90
    #    so the harness's summarize-restart fires after this iteration
    #    finalizes.
    # 2. attempt #2 (the recovery attempt), iter #1: intent=verify on a
    #    graderless task -> DONE. Occupancy is well below threshold so
    #    no second recovery is even considered; the second recovery
    #    would be rejected by max_context_recoveries=1 in any case.
    iterations: list[IterationResult] = [
        IterationResult(
            transcript="probing handler X; one more step to go",
            messages=(
                _assistant_msg(
                    input_tokens=95,
                    text="probing handler X; one more step to go",
                ),
                _result_msg(input_tokens=95),
            ),
            envelope=ValidEnvelope(intent=Intent.CONTINUE),
            signals=_signals(),
            failure=None,
        ),
        IterationResult(
            transcript=_verify_transcript(),
            messages=(
                _assistant_msg(input_tokens=5, text="all done"),
                _result_msg(input_tokens=5),
            ),
            envelope=ValidEnvelope(intent=Intent.VERIFY, reason="done"),
            signals=_signals(),
            failure=None,
        ),
    ]
    invoke_calls: list[InvocationRequest] = []

    async def invoke(request: InvocationRequest) -> IterationResult:
        """Production-shape invoker seam.

        Records the harness-built prompt before returning so we can
        assert on it post-run, and mirrors the production
        per-message-observer contract by forwarding every Message to
        ``request.on_message`` -- the harness uses that observer for
        its SDK-message persistence and the hang watchdog, so a
        scripted invoker that skips it would leave the audit stream
        diverging from production.
        """
        invoke_calls.append(request)
        result = iterations.pop(0)
        if request.on_message is not None:
            for msg in result.messages:
                request.on_message(msg)
        return result

    # The summarizer seam returns a well-formed handoff envelope; the
    # production ``run_recovery_summarizer`` runner parses it via
    # ``parse_handoff`` and the harness threads the resulting
    # ``RecoveryHandoff`` into the next attempt's ``IterationInputs``.
    # We capture the structured fields here so the assertions can
    # match them against the resulting prompt and the
    # ``summary_digest`` audit payload.
    handoff = RecoveryHandoff(
        work_done="probed handler X and surveyed dispatch module",
        work_remaining="implement the missing branch in dispatch()",
        key_decisions="reuse the existing parser; avoid touching codec",
        suggested_next_step="open dispatch.py and add the new case",
    )
    summarizer_calls: list[tuple[str, object]] = []

    async def summarizer_invoke(prompt: str, summarizer_worktree: object) -> str:
        summarizer_calls.append((prompt, summarizer_worktree))
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

    task = Task(
        id="in-loop-recover-task",
        goal="Verify the summarize-restart recovery path end-to-end.",
        graders=[],
    )
    lifecycle = Lifecycle(
        task_id=task.id, run_id="run-in-loop-recover"
    )

    config = HarnessConfig(
        max_retries=0,
        context_window_tokens=100,
        context_recovery_trigger_ratio=0.9,
        max_context_recoveries=1,
        recovery_summarizer_invoke=summarizer_invoke,
        worktree=worktree,
    )

    store = SqliteStore(db_path)
    try:
        outcome = asyncio.run(
            run_task(
                task,
                lifecycle,
                store,
                config=config,
                invoke=invoke,
            )
        )

        # --- Returned outcome --------------------------------------
        # Two attempts: the recovered one and the recovery one.
        assert outcome.lifecycle.status == Status.DONE
        assert len(outcome.attempts) == 2
        assert outcome.attempts[0].outcome == Outcome.RECOVERED
        assert outcome.attempts[1].outcome == Outcome.SUCCEEDED
        # Recovery budget consumed; validation retry budget untouched.
        assert outcome.lifecycle.retries == 0

        # --- Persisted attempts row (real SqliteStore) -------------
        # The HarnessOutcome and the projected ``attempts`` rows must
        # agree -- the projection is what every other consumer reads.
        persisted_attempts = store.list_attempts(lifecycle.run_id)
        assert [a.number for a in persisted_attempts] == [1, 2]
        assert persisted_attempts[0].outcome == Outcome.RECOVERED
        assert persisted_attempts[1].outcome == Outcome.SUCCEEDED

        # --- Persisted lifecycle row -------------------------------
        persisted_lifecycle = store.load_lifecycle(lifecycle.run_id)
        assert persisted_lifecycle is not None
        assert persisted_lifecycle.status == Status.DONE
        assert persisted_lifecycle.retries == 0

        # --- Persisted audit events --------------------------------
        events = store.list_events(lifecycle.run_id)

        # FR-5: exactly one harness.context_recovery event carrying
        # occupancy/capacity/budget/summary-digest details.
        recoveries = [
            e for e in events if e.kind == "harness.context_recovery"
        ]
        assert len(recoveries) == 1
        rec_payload = recoveries[0].payload
        assert rec_payload["attempt_number"] == 1
        assert rec_payload["occupancy_tokens"] == 95
        assert rec_payload["context_window_tokens"] == 100
        assert rec_payload["context_recovery_trigger_ratio"] == 0.9
        assert rec_payload["recoveries_used"] == 1
        assert rec_payload["recoveries_remaining"] == 0
        # Spec 00019 FR-6: the boundary path attributes the recovery
        # to ``trigger="boundary"``; the mid-turn in-loop test asserts
        # the matching ``trigger="mid_turn"`` marker on its side.
        assert rec_payload["trigger"] == "boundary"
        digest = rec_payload["summary_digest"]
        assert digest["work_done_length"] == len(handoff.work_done)
        assert digest["work_remaining_length"] == len(
            handoff.work_remaining
        )
        assert digest["key_decisions_length"] == len(
            handoff.key_decisions
        )
        assert digest["suggested_next_step_length"] == len(
            handoff.suggested_next_step
        )

        # FR-5 ordering: the harness.context_recovery event is the
        # producing-end signal; the recovery attempt's
        # harness.attempt_started for number=2 is the applying-end
        # signal. The producing event must come first in the
        # per-run audit sequence so a downstream consumer reads
        # "recovery happened" before "next attempt began".
        attempt2_started = [
            e
            for e in events
            if e.kind == "harness.attempt_started"
            and e.payload.get("number") == 2
        ]
        assert len(attempt2_started) == 1
        recovery_seq = recoveries[0].sequence
        started_seq = attempt2_started[0].sequence
        assert recovery_seq is not None
        assert started_seq is not None
        assert recovery_seq < started_seq

        # --- Applied recovery: the second prompt carries the handoff ---
        # The harness threaded the structured summary into the
        # recovery attempt's IterationInputs and prompt.py rendered it
        # under the # Recovery handoff section. Every field round-trips
        # so we know the summarizer's content reached the agent verbatim.
        assert len(invoke_calls) == 2
        first_prompt = invoke_calls[0].prompt
        recovery_prompt = invoke_calls[1].prompt
        assert "# Recovery handoff" not in first_prompt
        assert "# Recovery handoff" in recovery_prompt
        assert handoff.work_done in recovery_prompt
        assert handoff.work_remaining in recovery_prompt
        assert handoff.key_decisions in recovery_prompt
        assert handoff.suggested_next_step in recovery_prompt

        # The second invocation is the recovery attempt, not a
        # within-attempt continuation.
        assert invoke_calls[1].attempt_number == 2
        assert invoke_calls[1].iteration_number == 1

        # --- Production seams were used exactly the expected number ---
        # of times. The summarizer fires only on the actual recovery,
        # never on the second attempt's verify iteration, so the
        # summarizer-invoke call count doubles as a "no spurious
        # recovery" check.
        assert len(summarizer_calls) == 1
        # The summarizer was invoked with the configured worktree.
        # ``_handle_context_recovery`` stringifies the path before
        # handing it to the runner (so a ``Path`` and the equivalent
        # ``str`` both flow through identically), so compare as
        # strings.
        assert summarizer_calls[0][1] == str(worktree)
        # And its prompt carries the task goal and the prior
        # iteration's transcript tail, so the producing call had real
        # material to summarize -- not an empty placeholder.
        summarizer_prompt = summarizer_calls[0][0]
        assert task.goal in summarizer_prompt
        assert "probing handler X" in summarizer_prompt
    finally:
        store.close()

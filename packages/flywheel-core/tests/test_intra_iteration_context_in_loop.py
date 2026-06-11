"""In-loop verification of the mid-turn context-recovery policy (spec 00019 FR-8).

This is the loop-path slot for spec 00019. Its sibling slot
``tests/test_context_recovery_in_loop.py`` covers the spec 00018 boundary
path -- an iteration that finishes naturally and only then crosses the
occupancy ratio at its tail. This file covers the new mid-turn path:
the ratio crosses *while the iteration is still streaming*, the harness
arms the recovery-interrupt event, the invoker raises
:class:`flywheel_core.invoker_client.HarnessRecoveryRequested`, and the
harness produces AND applies the summarize-restart recovery on the real
loop.

What this file does, and the only thing it does, is drive a fixture
through the shipped harness pipeline so the mid-turn interrupt-to-recovery
path is observed end-to-end:

* The real :func:`flywheel_core.harness.run_task` runs the attempts.
* A real :class:`flywheel_core.store_sqlite.SqliteStore` persists every
  event, attempt, and lifecycle row. Assertions read the on-disk store
  rather than relying on the harness's return value.
* The real :func:`flywheel_core.recovery_summarizer.run_recovery_summarizer`
  is invoked through ``HarnessConfig.recovery_summarizer_invoke`` -- the
  same seam production uses. Only the summarizer's response text is
  scripted (a well-formed handoff envelope); the runner that parses
  and validates it is the production code.
* The agent is scripted through the production ``InvokeFunc`` seam --
  the same shape :func:`flywheel_core.invoker_client.invoke_iteration_with_client`
  satisfies. The scripted invoker mirrors the production watcher
  contract by forwarding each streamed :class:`AssistantMessage` to
  ``request.on_message`` and, once the harness arms
  ``request.recovery_interrupt_event``, raising
  :class:`HarnessRecoveryRequested` -- exactly the way the live-client
  watcher translates an SDK ``interrupt`` cancel into the
  distinguishable mid-turn signal.

A passing test proves both ends of FR-4 / FR-6 ran on the real loop:

* PRODUCING the recovery: occupancy crosses
  ``context_recovery_trigger_ratio`` mid-iteration, the harness sets
  the recovery interrupt event, the iteration is interrupted before it
  can return an :class:`IterationResult`, attempt #1 finalizes
  :attr:`Outcome.RECOVERED`, and one ``harness.context_recovery``
  audit event with ``trigger="mid_turn"`` lands in the audit stream.
* APPLYING the recovery: attempt #2 actually starts (a persisted
  ``harness.attempt_started`` event for ``number=2`` ordered after the
  recovery event) and its prompt carries the ``# Recovery handoff``
  section populated from the summarizer's structured output.

Per the spec's Decisions Log this feature adds no schema column or
table, so the FR-4 ``v(N-1)`` store-seeding / forward-migration clause
does not apply -- the test exercises the current schema only.
"""

from __future__ import annotations

import asyncio
import json
from pathlib import Path
from types import SimpleNamespace

from claude_agent_sdk import AssistantMessage, ResultMessage, TextBlock

from flywheel_core import (
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
from flywheel_core.invoker_client import HarnessRecoveryRequested
from flywheel_core.prompt import RecoveryHandoff
from flywheel_core.recovery_summarizer import (
    CLOSING_FENCE as RECOVERY_CLOSING_FENCE,
)
from flywheel_core.recovery_summarizer import (
    OPENING_FENCE as RECOVERY_OPENING_FENCE,
)
from flywheel_core.store_sqlite import SqliteStore
from flywheel_core.telemetry_file import FileTelemetrySink


# --- Scripted SDK message helpers -----------------------------------------

_SESSION_ID = "sess-in-loop-midturn-recover"


def _usage(input_tokens: int) -> dict[str, int]:
    """Build a usage breakdown whose input-side occupancy equals
    ``input_tokens``.

    Mirrors the boundary slot's helper so both in-loop tests stress the
    same input-side summing path: the harness computes
    ``input_tokens + cache_read_input_tokens + cache_creation_input_tokens``
    when it updates the accumulated mid-turn estimate from streamed
    :attr:`AssistantMessage.usage` (spec 00019 FR-1, reusing
    ``_OCCUPANCY_USAGE_KEYS``). Putting the full occupancy on
    ``input_tokens`` keeps the fixture readable while still routing
    through the production summing logic.
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


# --- The slot --------------------------------------------------------------


# Text the streaming agent emits before the harness's mid-turn interrupt
# lands. The harness accumulates ``AssistantMessage`` TextBlock content
# into ``partial_transcript_chunks`` (spec 00019 FR-4 -- the
# mid-turn-interrupted iteration never returns a full ``IterationResult``,
# so the summarizer's only window into the prior work-so-far is whatever
# streamed through ``on_message`` before the interrupt). Splitting the
# text across two streamed assistant messages -- one below the act
# ratio, one above -- exercises both halves of that accumulator while
# also producing a stable, search-able token the summarizer-prompt
# assertion below can pin against.
_EARLY_CHUNK = "probing handler X across dispatch module; "
_LATE_CHUNK = "one more streamed step before the interrupt lands"


def test_real_loop_drives_full_midturn_summarize_restart(
    tmp_path: Path,
) -> None:
    """Spec 00019 FR-8: real production loop runs a full mid-turn summarize-restart.

    Attempt #1 streams two assistant messages within a single iteration:
    the first reports 40 input tokens (below every observe tier and well
    below the 0.9 act ratio); the second reports 95 input tokens, which
    against the operator-supplied ``context_window_tokens=100`` puts
    occupancy at 95% -- above the 0.9 ratio. The harness's threshold
    closure arms ``recovery_interrupt_event`` the moment that second
    message lands. The scripted invoker (modeled on the production
    :func:`invoke_iteration_with_client` watcher) polls the event after
    each delivery and raises :class:`HarnessRecoveryRequested`, which
    the harness catches in ``_drive_iterations`` and routes through the
    spec 00018 summarize-restart action with ``trigger="mid_turn"``.

    Attempt #2 -- the recovery attempt -- runs a single VERIFY iteration
    on a graderless task, which transitions straight to DONE. Any
    deviation from the shipped mid-turn recovery path surfaces as a
    wrong terminal status, a missing event, a missing handoff section,
    or a recovery whose ``trigger`` field does not match.
    """
    db_path = tmp_path / "flywheel.sqlite"
    worktree = tmp_path / "worktree"
    worktree.mkdir()

    # The recovery attempt's verify iteration. Occupancy is well below
    # any threshold so the boundary check (spec 00018) does not also
    # fire on attempt #2 -- and ``max_context_recoveries=1`` would
    # reject a second recovery anyway, which is the FR-5 shared-budget
    # belt-and-braces guarantee.
    verify_iteration = IterationResult(
        transcript=(
            "<!-- LOOP_STATUS -->\n"
            '{"intent": "verify", "reason": "done"}\n'
            "<!-- /LOOP_STATUS -->\n"
        ),
        messages=(
            _assistant_msg(input_tokens=5, text="all done"),
            _result_msg(input_tokens=5),
        ),
        envelope=ValidEnvelope(intent=Intent.VERIFY, reason="done"),
        signals=_signals(),
        failure=None,
    )

    # Streamed messages for attempt #1's interrupted iteration. The
    # first message is below every observe tier; the second crosses the
    # 50 / 75 / 90 observe tiers AND the 0.9 act ratio in a single
    # delivery (spec 00019 edge case "Crossing 90% observe and the act
    # ratio on the same message"). Only the AssistantMessages carry
    # text/usage -- the iteration is interrupted before any
    # ResultMessage would have streamed.
    streamed_pre_interrupt = (
        _assistant_msg(input_tokens=40, text=_EARLY_CHUNK),
        _assistant_msg(input_tokens=95, text=_LATE_CHUNK),
    )

    invoke_calls: list[InvocationRequest] = []

    async def invoke(request: InvocationRequest) -> IterationResult:
        """Production-shape invoker seam with mid-turn interrupt support.

        Records the harness-built prompt so the post-run assertions can
        verify the recovery prompt carries ``# Recovery handoff``. On
        the first call (attempt #1, iter #1) streams the pre-interrupt
        :class:`AssistantMessage`s one at a time through
        ``request.on_message`` and, after each delivery, checks whether
        the harness has set ``request.recovery_interrupt_event``. The
        first set event causes the invoker to raise
        :class:`HarnessRecoveryRequested` without returning an
        :class:`IterationResult` -- exactly mirrors the production
        :func:`invoke_iteration_with_client` translating a
        watcher-induced cancel into the distinguishable mid-turn
        signal. On the second call (attempt #2, iter #1) the invoker
        forwards the verify iteration's messages through ``on_message``
        and returns the iteration result for the graderless task to
        reach DONE.
        """
        invoke_calls.append(request)
        if len(invoke_calls) == 1:
            assert request.on_message is not None, (
                "harness must wire on_message so mid-turn occupancy "
                "tracking observes streamed usage"
            )
            assert request.recovery_interrupt_event is not None, (
                "harness must wire recovery_interrupt_event so the "
                "mid-turn act seam can interrupt this invoker"
            )
            for msg in streamed_pre_interrupt:
                request.on_message(msg)
                if request.recovery_interrupt_event.is_set():
                    # The harness's threshold closure armed the event
                    # in response to this delivery. Mirror the live
                    # watcher: raise the distinguishable signal so the
                    # harness routes the attempt into mid-turn
                    # recovery instead of running the boundary check
                    # over a returned iteration_result.
                    raise HarnessRecoveryRequested()
            raise AssertionError(
                "scripted streamed usage was supposed to arm the "
                "mid-turn recovery event, but the harness never set it"
            )
        if request.on_message is not None:
            for msg in verify_iteration.messages:
                request.on_message(msg)
        return verify_iteration

    handoff = RecoveryHandoff(
        work_done="probed handler X across dispatch module mid-turn",
        work_remaining="implement the missing branch in dispatch()",
        key_decisions="reuse the existing parser; avoid touching codec",
        suggested_next_step="open dispatch.py and add the new case",
    )
    summarizer_calls: list[tuple[str, object]] = []

    async def summarizer_invoke(prompt: str, summarizer_worktree: object) -> str:
        """Scripted summarizer returning a well-formed handoff envelope.

        The production :func:`flywheel_core.recovery_summarizer.run_recovery_summarizer`
        runner parses this string via ``parse_handoff`` and threads the
        resulting :class:`RecoveryHandoff` into the next attempt's
        :class:`IterationInputs`. The handoff fields chosen here let
        the prompt assertion below verify every slot round-trips
        verbatim into the rendered ``# Recovery handoff`` section.
        """
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
        id="in-loop-midturn-recover-task",
        goal=(
            "Verify the mid-turn summarize-restart recovery path "
            "end-to-end on the real loop."
        ),
        graders=[],
    )
    lifecycle = Lifecycle(
        task_id=task.id, run_id="run-in-loop-midturn-recover"
    )

    # Operator-supplied capacity is the only ratio denominator on the
    # scripted-invoker path (the SDK ``get_context_usage`` source only
    # populates when a live :class:`ClaudeSDKClient` is in use). With
    # capacity=100 and ratio=0.9, the threshold is 90 tokens; the late
    # streamed assistant message at 95 tokens crosses it.
    config = HarnessConfig(
        max_retries=0,
        context_window_tokens=100,
        context_recovery_trigger_ratio=0.9,
        max_context_recoveries=1,
        recovery_summarizer_invoke=summarizer_invoke,
        worktree=worktree,
    )

    store = SqliteStore(db_path)
    sink = FileTelemetrySink(db_path.parent / "logs")
    try:
        outcome = asyncio.run(
            run_task(
                task,
                lifecycle,
                store,
                config=config,
                invoke=invoke,
                sink=sink,
            )
        )

        # --- Returned outcome --------------------------------------
        # Two attempts: attempt #1 finalized RECOVERED via the
        # mid-turn path, attempt #2 the recovery attempt that
        # converged to DONE.
        assert outcome.lifecycle.status == Status.DONE
        assert len(outcome.attempts) == 2
        assert outcome.attempts[0].outcome == Outcome.RECOVERED
        assert outcome.attempts[1].outcome == Outcome.SUCCEEDED
        # The recovery budget consumed one slot; the validation retry
        # budget was untouched (FR-5: independent counters).
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
        sink.close()
        run_file = (
            db_path.parent / "logs" / "runs" / f"{lifecycle.run_id}.jsonl"
        )
        assert run_file.exists()
        events = [
            SimpleNamespace(
                kind=line["kind"],
                payload=line.get("payload", {}),
                attempt_number=line.get("attempt_number"),
            )
            for line in (
                json.loads(raw)
                for raw in run_file.read_text(
                    encoding="utf-8"
                ).splitlines()
                if raw.strip()
            )
        ]

        # FR-4 / FR-6: exactly one harness.context_recovery event
        # carrying occupancy / capacity / budget / summary-digest
        # details, attributed to the MID-TURN trigger.
        recoveries = [
            e for e in events if e.kind == "harness.context_recovery"
        ]
        assert len(recoveries) == 1
        rec_payload = recoveries[0].payload
        assert rec_payload["attempt_number"] == 1
        assert rec_payload["iteration"] == 1
        # The accumulated mid-turn estimate equals the input-side
        # tokens reported by the LATE streamed assistant message --
        # the message that crossed the act ratio. The harness arms
        # the interrupt off the occupancy reading that armed it, not
        # off a later refresh.
        assert rec_payload["occupancy_tokens"] == 95
        assert rec_payload["context_window_tokens"] == 100
        assert rec_payload["context_recovery_trigger_ratio"] == 0.9
        assert rec_payload["recoveries_used"] == 1
        assert rec_payload["recoveries_remaining"] == 0
        # Spec 00019 FR-6 the headline assertion of this slot: the
        # mid-turn path attributes its recovery event to the
        # ``"mid_turn"`` marker; the boundary in-loop sibling asserts
        # the matching ``"boundary"`` marker on its side. Operators
        # downstream of the audit stream use this field to attribute
        # a recovery to the path that produced it.
        assert rec_payload["trigger"] == "mid_turn"
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

        # FR-6 ordering: the harness.context_recovery event is the
        # producing-end signal; the recovery attempt's
        # harness.attempt_started for number=2 is the applying-end
        # signal. The producing event MUST come first in the per-run
        # audit sequence so a downstream consumer reads "recovery
        # happened" before "next attempt began".
        attempt2_started = [
            e
            for e in events
            if e.kind == "harness.attempt_started"
            and e.payload.get("number") == 2
        ]
        assert len(attempt2_started) == 1
        assert events.index(recoveries[0]) < events.index(
            attempt2_started[0]
        )

        # The mid-turn path interrupts BEFORE the iteration could emit
        # a ``harness.iteration_completed`` event for attempt #1.
        # Exactly one iteration_completed event lands in the audit
        # stream (from the verify iteration on attempt #2). This is
        # the "iteration was interrupted in flight" signature -- a
        # boundary recovery would have emitted iteration_completed
        # for both attempts.
        iteration_completed = [
            e for e in events if e.kind == "harness.iteration_completed"
        ]
        assert len(iteration_completed) == 1
        assert iteration_completed[0].attempt_number == 2

        # --- Applied recovery: the second prompt carries the handoff ---
        # The harness threaded the structured summary into the
        # recovery attempt's :class:`IterationInputs` and prompt.py
        # rendered it under the ``# Recovery handoff`` section. Every
        # field round-trips so we know the summarizer's content
        # reached the agent verbatim.
        assert len(invoke_calls) == 2
        first_prompt = invoke_calls[0].prompt
        recovery_prompt = invoke_calls[1].prompt
        assert "# Recovery handoff" not in first_prompt
        assert "# Recovery handoff" in recovery_prompt
        assert handoff.work_done in recovery_prompt
        assert handoff.work_remaining in recovery_prompt
        assert handoff.key_decisions in recovery_prompt
        assert handoff.suggested_next_step in recovery_prompt

        # The second invocation is the recovery attempt itself, not a
        # within-attempt continuation of the interrupted one.
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
        # handing it to the runner (so a :class:`Path` and the
        # equivalent ``str`` both flow through identically), so the
        # comparison is on the string form.
        assert summarizer_calls[0][1] == str(worktree)
        # The summarizer's prompt carries the task goal AND the
        # work-so-far transcript text the streaming AssistantMessages
        # produced before the interrupt landed. The mid-turn path's
        # tail is built from ``partial_transcript_chunks`` -- text
        # accumulated through ``on_message`` -- not from any
        # returned :class:`IterationResult.transcript` (the iteration
        # never returned one). Asserting both chunks survived proves
        # the harness's TextBlock-accumulation closure ran on each
        # streamed message before the interrupt landed.
        summarizer_prompt = summarizer_calls[0][0]
        assert task.goal in summarizer_prompt
        assert _EARLY_CHUNK in summarizer_prompt
        assert _LATE_CHUNK in summarizer_prompt
    finally:
        store.close()

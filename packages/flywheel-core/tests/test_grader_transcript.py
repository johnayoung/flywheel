"""Behavioral tests for ``flywheel_core.grader_transcript``.

Two behaviors share one source of truth (``TranscriptGrader``) and the
tests assert that:

* :func:`first_breach` evaluates limits in canonical order so multi-field
  graders converge on a single breached field.
* :func:`enforce_transcript_limits` aborts a streaming message source as
  soon as any limit is breached.
* :func:`run_transcript_graders` persists rows in the documented
  ``{observed, breached}`` shape and honors run-cost-order.
* A hard-limit abort and the persisted ``grader_results`` row agree on
  which field tripped — proven for each of the three limit fields.
"""

from __future__ import annotations

import asyncio
from collections.abc import AsyncIterator
from datetime import datetime, timezone
from typing import Any

import pytest
from claude_agent_sdk import (
    AssistantMessage,
    Message,
    ResultMessage,
    TextBlock,
)

from flywheel_core import (
    Attempt,
    CommandGrader,
    InMemoryStore,
    Lifecycle,
    ManualGrader,
    RubricGrader,
    Task,
    TranscriptCounter,
    TranscriptGrader,
    TranscriptObservation,
    enforce_transcript_limits,
    first_breach,
    run_command_graders,
    run_transcript_graders,
    total_tokens_from_usage,
)

# --- Test helpers ----------------------------------------------------------


def _attempt_run_id(store: InMemoryStore, run_id: str = "r1") -> None:
    if store.load_lifecycle(run_id) is None:
        store.create_lifecycle(Lifecycle(task_id="t", run_id=run_id))
    if store.load_attempt(run_id, 1) is None:
        store.save_attempt(
            run_id,
            Attempt(
                number=1,
                started_at=datetime(2024, 1, 1, tzinfo=timezone.utc),
                run_id=run_id,
            ),
        )


def _task(*graders: object) -> Task:
    return Task(goal="g", graders=list(graders))  # type: ignore[arg-type]


def _assistant(
    *,
    usage: dict[str, Any] | None = None,
    stop_reason: str | None = "end_turn",
    session_id: str = "sess-1",
    text: str = "ok",
) -> AssistantMessage:
    return AssistantMessage(
        content=[TextBlock(text=text)],
        model="claude-test",
        stop_reason=stop_reason,
        session_id=session_id,
        usage=usage,
    )


def _result(
    *,
    num_turns: int = 1,
    usage: dict[str, Any] | None = None,
    session_id: str = "sess-1",
) -> ResultMessage:
    return ResultMessage(
        subtype="success",
        duration_ms=10,
        duration_api_ms=8,
        is_error=False,
        num_turns=num_turns,
        session_id=session_id,
        stop_reason="end_turn",
        total_cost_usd=0.01,
        usage=usage,
    )


async def _stream(*items: Message) -> AsyncIterator[Message]:
    for item in items:
        yield item


class _FakeClock:
    """Monotonic clock returning a fixed value, advanced manually or by step.

    With ``step > 0``, every call advances the clock — useful for
    simulating wall-time breaches without sleeping. With ``step = 0`` the
    clock stays put unless ``advance`` is called explicitly.
    """

    def __init__(self, start: float = 0.0, step: float = 0.0) -> None:
        self.value = start
        self.step = step

    def __call__(self) -> float:
        current = self.value
        self.value += self.step
        return current

    def advance(self, delta: float) -> None:
        self.value += delta


# --- total_tokens_from_usage ----------------------------------------------


class TestTotalTokensFromUsage:
    def test_sums_all_documented_token_fields(self) -> None:
        usage = {
            "input_tokens": 10,
            "output_tokens": 20,
            "cache_creation_input_tokens": 5,
            "cache_read_input_tokens": 3,
        }
        assert total_tokens_from_usage(usage) == 38

    def test_treats_missing_fields_as_zero(self) -> None:
        assert total_tokens_from_usage({"input_tokens": 7}) == 7
        assert total_tokens_from_usage({}) == 0

    def test_none_usage_returns_zero(self) -> None:
        assert total_tokens_from_usage(None) == 0

    def test_ignores_non_numeric_garbage(self) -> None:
        usage = {"input_tokens": "junk", "output_tokens": 5}
        assert total_tokens_from_usage(usage) == 5


# --- first_breach ---------------------------------------------------------


class TestFirstBreach:
    def test_returns_none_when_within_every_limit(self) -> None:
        grader = TranscriptGrader(
            max_turns=10, max_total_tokens=1000, max_wall_seconds=60.0
        )
        observed = TranscriptObservation(
            turns=10, total_tokens=1000, wall_seconds=60.0
        )
        assert first_breach(grader, observed) is None

    def test_strictly_exceeding_max_turns_returns_max_turns(self) -> None:
        grader = TranscriptGrader(max_turns=3)
        observed = TranscriptObservation(
            turns=4, total_tokens=0, wall_seconds=0.0
        )
        assert first_breach(grader, observed) == "max_turns"

    def test_strictly_exceeding_max_total_tokens(self) -> None:
        grader = TranscriptGrader(max_total_tokens=100)
        observed = TranscriptObservation(
            turns=0, total_tokens=101, wall_seconds=0.0
        )
        assert first_breach(grader, observed) == "max_total_tokens"

    def test_strictly_exceeding_max_wall_seconds(self) -> None:
        grader = TranscriptGrader(max_wall_seconds=10.0)
        observed = TranscriptObservation(
            turns=0, total_tokens=0, wall_seconds=10.5
        )
        assert first_breach(grader, observed) == "max_wall_seconds"

    def test_canonical_order_resolves_multi_field_breach(self) -> None:
        # All three breached — canonical order picks max_turns first.
        grader = TranscriptGrader(
            max_turns=1, max_total_tokens=10, max_wall_seconds=1.0
        )
        observed = TranscriptObservation(
            turns=5, total_tokens=100, wall_seconds=60.0
        )
        assert first_breach(grader, observed) == "max_turns"

    def test_unset_limits_are_not_evaluated(self) -> None:
        # max_turns absent — even if turns is huge, no breach reported.
        grader = TranscriptGrader(max_total_tokens=100)
        observed = TranscriptObservation(
            turns=10_000, total_tokens=50, wall_seconds=0.0
        )
        assert first_breach(grader, observed) is None


# --- TranscriptCounter ----------------------------------------------------


class TestTranscriptCounter:
    def test_observe_increments_turns_per_assistant_message(self) -> None:
        counter = TranscriptCounter.start(monotonic=lambda: 0.0)
        counter.observe(_assistant())
        counter.observe(_assistant())
        counter.observe(_assistant())
        assert counter.turns == 3

    def test_observe_sums_assistant_usage_token_fields(self) -> None:
        counter = TranscriptCounter.start(monotonic=lambda: 0.0)
        counter.observe(_assistant(usage={"input_tokens": 10, "output_tokens": 5}))
        counter.observe(_assistant(usage={"input_tokens": 20, "output_tokens": 8}))
        assert counter.total_tokens == 43

    def test_observe_takes_max_with_result_message_totals(self) -> None:
        counter = TranscriptCounter.start(monotonic=lambda: 0.0)
        counter.observe(_assistant(usage={"input_tokens": 10, "output_tokens": 5}))
        # ResultMessage reports a higher total — counter should adopt it.
        counter.observe(
            _result(num_turns=5, usage={"input_tokens": 100, "output_tokens": 50})
        )
        assert counter.total_tokens == 150
        assert counter.turns == 5

    def test_snapshot_uses_monotonic_clock(self) -> None:
        clock = _FakeClock(start=0.0)
        counter = TranscriptCounter.start(monotonic=clock)
        clock.advance(2.5)
        snap = counter.snapshot()
        assert snap.wall_seconds == pytest.approx(2.5)


# --- enforce_transcript_limits --------------------------------------------


def _drain(agen: AsyncIterator[Message]) -> list[Message]:
    async def collect() -> list[Message]:
        out: list[Message] = []
        async for msg in agen:
            out.append(msg)
        return out

    return asyncio.run(collect())


class TestEnforceTranscriptLimits:
    def test_passes_through_when_no_graders(self) -> None:
        messages = [_assistant(), _assistant(), _result(num_turns=2)]
        out = _drain(
            enforce_transcript_limits(
                _stream(*messages), graders=()
            )
        )
        assert out == messages

    def test_aborts_stream_when_max_turns_exceeded(self) -> None:
        # Source emits 5 assistant turns; max_turns=2 stops after turn 3.
        messages = [_assistant() for _ in range(5)]
        grader = TranscriptGrader(max_turns=2)

        out = _drain(
            enforce_transcript_limits(
                _stream(*messages), graders=[grader]
            )
        )
        # Turn 1 (1 not > 2), turn 2 (2 not > 2), turn 3 (3 > 2 → abort).
        assert len(out) == 3
        assert all(isinstance(m, AssistantMessage) for m in out)

    def test_aborts_stream_when_max_total_tokens_exceeded(self) -> None:
        # Each turn adds 50 tokens; max_total_tokens=120 → stop after turn 3
        # (cumulative 150 > 120).
        messages = [
            _assistant(usage={"input_tokens": 30, "output_tokens": 20})
            for _ in range(5)
        ]
        grader = TranscriptGrader(max_total_tokens=120)

        out = _drain(
            enforce_transcript_limits(
                _stream(*messages), graders=[grader]
            )
        )
        assert len(out) == 3

    def test_aborts_stream_when_max_wall_seconds_exceeded(self) -> None:
        # Clock advances 1.0s per call; max_wall_seconds=2.5 → after the
        # third yield wall_seconds is 3.0 which exceeds 2.5.
        clock = _FakeClock(start=0.0, step=1.0)
        messages = [_assistant() for _ in range(5)]
        grader = TranscriptGrader(max_wall_seconds=2.5)

        out = _drain(
            enforce_transcript_limits(
                _stream(*messages), graders=[grader], monotonic=clock
            )
        )
        assert len(out) == 3

    def test_no_abort_when_within_all_limits(self) -> None:
        messages = [_assistant(), _assistant(), _result(num_turns=2)]
        grader = TranscriptGrader(max_turns=10)
        out = _drain(
            enforce_transcript_limits(
                _stream(*messages), graders=[grader]
            )
        )
        assert out == messages

    def test_tightest_grader_among_many_wins(self) -> None:
        # Two graders; the tighter one triggers first.
        messages = [_assistant() for _ in range(5)]
        loose = TranscriptGrader(max_turns=100)
        tight = TranscriptGrader(max_turns=1)
        out = _drain(
            enforce_transcript_limits(
                _stream(*messages), graders=[loose, tight]
            )
        )
        # turn 1 (not > 1), turn 2 (2 > 1 → abort).
        assert len(out) == 2


# --- run_transcript_graders -----------------------------------------------


def _run_observation(
    task: Task,
    obs: TranscriptObservation,
    store: InMemoryStore,
    **kwargs: object,
) -> Any:
    return run_transcript_graders(
        task,
        obs,
        store,
        run_id="r1",
        attempt_number=1,
        **kwargs,  # type: ignore[arg-type]
    )


class TestRunTranscriptGradersValidation:
    def test_passing_observation_records_observed_and_no_breached(self) -> None:
        store = InMemoryStore()
        _attempt_run_id(store)
        task = _task(
            TranscriptGrader(
                max_turns=10, max_total_tokens=1000, max_wall_seconds=60.0
            )
        )
        obs = TranscriptObservation(
            turns=3, total_tokens=120, wall_seconds=4.2
        )

        [row] = _run_observation(task, obs, store)
        assert row.passed is True
        assert row.grader_type == "transcript"
        assert row.payload["observed"] == {
            "turns": 3,
            "total_tokens": 120,
            "wall_seconds": 4.2,
        }
        assert "breached" not in row.payload

    def test_max_turns_breach_records_breached_field(self) -> None:
        store = InMemoryStore()
        _attempt_run_id(store)
        task = _task(TranscriptGrader(max_turns=2))
        obs = TranscriptObservation(
            turns=5, total_tokens=0, wall_seconds=0.0
        )

        [row] = _run_observation(task, obs, store)
        assert row.passed is False
        assert row.payload["breached"] == "max_turns"
        assert row.payload["observed"]["turns"] == 5

    def test_max_total_tokens_breach_records_breached_field(self) -> None:
        store = InMemoryStore()
        _attempt_run_id(store)
        task = _task(TranscriptGrader(max_total_tokens=100))
        obs = TranscriptObservation(
            turns=2, total_tokens=200, wall_seconds=0.0
        )

        [row] = _run_observation(task, obs, store)
        assert row.passed is False
        assert row.payload["breached"] == "max_total_tokens"
        assert row.payload["observed"]["total_tokens"] == 200

    def test_max_wall_seconds_breach_records_breached_field(self) -> None:
        store = InMemoryStore()
        _attempt_run_id(store)
        task = _task(TranscriptGrader(max_wall_seconds=10.0))
        obs = TranscriptObservation(
            turns=1, total_tokens=10, wall_seconds=12.3
        )

        [row] = _run_observation(task, obs, store)
        assert row.passed is False
        assert row.payload["breached"] == "max_wall_seconds"
        assert row.payload["observed"]["wall_seconds"] == 12.3


class TestRunTranscriptGradersOrdering:
    def test_preserves_ordinals_among_mixed_grader_types(self) -> None:
        store = InMemoryStore()
        _attempt_run_id(store)
        task = _task(
            CommandGrader(run="true"),
            TranscriptGrader(max_turns=10),
            RubricGrader(assertions=["x"]),
            TranscriptGrader(max_total_tokens=10_000),
            ManualGrader(instruction="approve"),
        )
        obs = TranscriptObservation(
            turns=1, total_tokens=100, wall_seconds=1.0
        )

        results = _run_observation(task, obs, store)
        assert [r.ordinal for r in results] == [1, 3]
        assert all(r.grader_type == "transcript" for r in results)

    def test_aborts_later_transcript_graders_on_first_failure(self) -> None:
        store = InMemoryStore()
        _attempt_run_id(store)
        task = _task(
            TranscriptGrader(max_turns=1, name="tight"),
            TranscriptGrader(max_turns=1000, name="loose"),
        )
        obs = TranscriptObservation(
            turns=5, total_tokens=0, wall_seconds=0.0
        )

        results = _run_observation(task, obs, store)
        # Only the first (failing) transcript grader runs.
        assert [r.grader_name for r in results] == ["tight"]
        assert results[0].passed is False

    def test_skips_entirely_when_command_failed(self) -> None:
        store = InMemoryStore()
        _attempt_run_id(store)
        task = _task(TranscriptGrader(max_turns=10))
        obs = TranscriptObservation(
            turns=3, total_tokens=0, wall_seconds=0.0
        )

        results = _run_observation(task, obs, store, command_passed=False)
        assert results == []
        # And nothing is persisted.
        assert store.list_grader_results("r1", 1) == []


class TestRunTranscriptGradersPersistence:
    def test_grader_spec_snapshots_input_verbatim(self) -> None:
        store = InMemoryStore()
        _attempt_run_id(store)
        grader = TranscriptGrader(
            max_turns=20,
            max_total_tokens=5000,
            max_wall_seconds=120.0,
            name="caps",
        )
        task = _task(grader)
        obs = TranscriptObservation(
            turns=1, total_tokens=10, wall_seconds=1.0
        )

        [row] = _run_observation(task, obs, store)
        assert dict(row.grader_spec) == {
            "type": "transcript",
            "max_turns": 20,
            "max_total_tokens": 5000,
            "max_wall_seconds": 120.0,
            "name": "caps",
        }

    def test_later_grader_edits_do_not_rewrite_history(self) -> None:
        store = InMemoryStore()
        _attempt_run_id(store)
        grader = TranscriptGrader(max_turns=5, name="snapshot")
        task = _task(grader)
        obs = TranscriptObservation(
            turns=2, total_tokens=10, wall_seconds=1.0
        )

        _run_observation(task, obs, store)
        grader.max_turns = 99
        grader.name = "RENAMED"

        rows = store.list_grader_results("r1", 1)
        assert dict(rows[0].grader_spec) == {
            "type": "transcript",
            "max_turns": 5,
            "name": "snapshot",
        }

    def test_payload_matches_documented_shape(self) -> None:
        store = InMemoryStore()
        _attempt_run_id(store)
        task = _task(TranscriptGrader(max_turns=1))
        obs = TranscriptObservation(
            turns=5, total_tokens=200, wall_seconds=3.0
        )

        [row] = _run_observation(task, obs, store)
        # Documented shape: {observed: {turns, total_tokens, wall_seconds},
        # breached: ...}
        assert set(row.payload.keys()) == {"observed", "breached"}
        assert set(row.payload["observed"].keys()) == {
            "turns",
            "total_tokens",
            "wall_seconds",
        }

    def test_each_row_keys_by_run_id_attempt_number_and_ordinal(self) -> None:
        store = InMemoryStore()
        _attempt_run_id(store)
        task = _task(
            CommandGrader(run="true"),
            TranscriptGrader(max_turns=10),
        )
        obs = TranscriptObservation(
            turns=2, total_tokens=10, wall_seconds=1.0
        )

        _run_observation(task, obs, store)
        rows = store.list_grader_results("r1", 1)
        assert [(r.run_id, r.attempt_number, r.ordinal) for r in rows] == [
            ("r1", 1, 1),
        ]


# --- Hard-limit / validation grader agreement (rubric assertion #4) -------


class TestHardLimitAndValidationAgree:
    """A hard-limit abort and the persisted ``grader_results`` row must
    agree on the breached field for every one of the three limit types.

    The contract: feed a long source stream into
    ``enforce_transcript_limits``; whatever observation the harness
    reconstructs from the partial stream must, when graded, record the
    same breached field that triggered the abort.
    """

    def _observed_after_abort(
        self,
        *,
        source: list[Message],
        grader: TranscriptGrader,
        monotonic: _FakeClock | None = None,
    ) -> TranscriptObservation:
        clock = monotonic or _FakeClock(start=0.0, step=0.0)
        counter = TranscriptCounter.start(monotonic=clock)

        async def consume() -> None:
            async for msg in enforce_transcript_limits(
                _stream(*source),
                graders=[grader],
                monotonic=clock,
            ):
                counter.observe(msg)

        asyncio.run(consume())
        return counter.snapshot()

    def test_max_turns_abort_and_row_agree(self) -> None:
        source = [_assistant() for _ in range(10)]
        grader = TranscriptGrader(max_turns=2)

        observation = self._observed_after_abort(
            source=source, grader=grader
        )
        # The wrapper aborted after observing the breach — observation
        # reflects only what was yielded.
        assert observation.turns > 2

        store = InMemoryStore()
        _attempt_run_id(store)
        task = _task(grader)
        [row] = _run_observation(task, observation, store)
        assert row.passed is False
        assert row.payload["breached"] == "max_turns"

    def test_max_total_tokens_abort_and_row_agree(self) -> None:
        source = [
            _assistant(usage={"input_tokens": 30, "output_tokens": 20})
            for _ in range(10)
        ]
        grader = TranscriptGrader(max_total_tokens=120)

        observation = self._observed_after_abort(
            source=source, grader=grader
        )
        assert observation.total_tokens > 120

        store = InMemoryStore()
        _attempt_run_id(store)
        task = _task(grader)
        [row] = _run_observation(task, observation, store)
        assert row.passed is False
        assert row.payload["breached"] == "max_total_tokens"

    def test_max_wall_seconds_abort_and_row_agree(self) -> None:
        clock = _FakeClock(start=0.0, step=1.0)
        source = [_assistant() for _ in range(10)]
        grader = TranscriptGrader(max_wall_seconds=2.5)

        observation = self._observed_after_abort(
            source=source, grader=grader, monotonic=clock
        )
        assert observation.wall_seconds > 2.5

        store = InMemoryStore()
        _attempt_run_id(store)
        task = _task(grader)
        [row] = _run_observation(task, observation, store)
        assert row.passed is False
        assert row.payload["breached"] == "max_wall_seconds"


# --- Cost-order integration with run_command_graders ----------------------


class TestCostOrderIntegration:
    """Wire ``run_command_graders`` then ``run_transcript_graders`` —
    transcript graders only run when command graders all passed.
    """

    def test_command_pass_then_transcript_evaluated(self) -> None:
        store = InMemoryStore()
        _attempt_run_id(store)
        task = _task(
            CommandGrader(run="true"),
            TranscriptGrader(max_turns=10),
        )
        observation = TranscriptObservation(
            turns=2, total_tokens=10, wall_seconds=1.0
        )

        cmd_rows = run_command_graders(
            task, store, run_id="r1", attempt_number=1
        )
        command_passed = all(r.passed for r in cmd_rows)
        ts_rows = run_transcript_graders(
            task,
            observation,
            store,
            run_id="r1",
            attempt_number=1,
            command_passed=command_passed,
        )

        assert command_passed is True
        assert len(ts_rows) == 1
        assert ts_rows[0].grader_type == "transcript"
        # Two rows total: command + transcript.
        assert len(store.list_grader_results("r1", 1)) == 2

    def test_command_fail_skips_transcript_entirely(self) -> None:
        store = InMemoryStore()
        _attempt_run_id(store)
        task = _task(
            CommandGrader(run="false"),
            TranscriptGrader(max_turns=10),
        )
        observation = TranscriptObservation(
            turns=2, total_tokens=10, wall_seconds=1.0
        )

        cmd_rows = run_command_graders(
            task, store, run_id="r1", attempt_number=1
        )
        command_passed = all(r.passed for r in cmd_rows)
        ts_rows = run_transcript_graders(
            task,
            observation,
            store,
            run_id="r1",
            attempt_number=1,
            command_passed=command_passed,
        )

        assert command_passed is False
        assert ts_rows == []
        # Only the failed command row was persisted.
        rows = store.list_grader_results("r1", 1)
        assert len(rows) == 1
        assert rows[0].grader_type == "command"
        assert rows[0].passed is False

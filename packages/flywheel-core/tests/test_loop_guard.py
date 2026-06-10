"""Behavioral coverage for :mod:`flywheel.loop_guard`.

Cross-references the FRs in ``.flywheel/specs/00015-FEATURE-loop-safety-net.md``:

- FR-1 (STUCK) -- three consecutive identical failing calls trip at threshold 3.
- FR-2 (THRASH) -- identical tuple repeating >= K within trailing W trips.
- FR-6 precedence -- a stream that satisfies both detectors returns STUCK.
- Edge cases from spec "Behavior Specification > Edge Cases".
- FR-5 disabling -- each knob set to ``None`` / ``0`` short-circuits its detector.
"""

from typing import Any

from flywheel.invoker import ToolInteraction, ToolResultObservation
from flywheel.loop_guard import (
    LoopGuard,
    LoopGuardConfig,
    LoopGuardVerdictKind,
)


def _interaction(
    *,
    tool_name: str = "Bash",
    tool_input: dict[str, Any] | None = None,
    is_error: bool | None = False,
    tool_use_id: str = "use-1",
    has_result: bool = True,
) -> ToolInteraction:
    """Build a ``ToolInteraction`` for a stream test.

    ``has_result=False`` simulates the "no matching result block" edge case
    from the spec (pending tool call): ``result`` is ``None``.
    """

    if tool_input is None:
        tool_input = {"cmd": "ls"}
    result: ToolResultObservation | None = None
    if has_result:
        result = ToolResultObservation(
            tool_use_id=tool_use_id,
            is_error=is_error,
            content=None,
        )
    return ToolInteraction(
        tool_use_id=tool_use_id,
        tool_name=tool_name,
        tool_input=tool_input,
        result=result,
    )


def _failing(
    tool_use_id: str,
    tool_input: dict[str, Any] | None = None,
    tool_name: str = "Bash",
) -> ToolInteraction:
    return _interaction(
        tool_use_id=tool_use_id,
        tool_input=tool_input,
        tool_name=tool_name,
        is_error=True,
    )


def _succeeding(
    tool_use_id: str,
    tool_input: dict[str, Any] | None = None,
    tool_name: str = "Bash",
) -> ToolInteraction:
    return _interaction(
        tool_use_id=tool_use_id,
        tool_input=tool_input,
        tool_name=tool_name,
        is_error=False,
    )


# --- FR-1: repeated-failure (STUCK) ---


def test_three_consecutive_identical_failures_trip_stuck_at_threshold_three() -> None:
    guard = LoopGuard(
        LoopGuardConfig(
            repeated_tool_failure_threshold=3,
            thrash_repeat_threshold=None,
            thrash_window=None,
        )
    )
    verdict = guard.observe(
        [_failing("u1"), _failing("u2"), _failing("u3")]
    )
    assert verdict is not None
    assert verdict.kind is LoopGuardVerdictKind.STUCK
    assert verdict.tool_name == "Bash"
    assert verdict.input_digest  # populated with the digest of {"cmd": "ls"}
    assert "Bash" in verdict.reason


def test_repeated_failure_accumulates_across_observe_calls() -> None:
    guard = LoopGuard(
        LoopGuardConfig(
            repeated_tool_failure_threshold=3,
            thrash_repeat_threshold=None,
            thrash_window=None,
        )
    )
    assert guard.observe([_failing("u1")]) is None
    assert guard.observe([_failing("u2")]) is None
    verdict = guard.observe([_failing("u3")])
    assert verdict is not None
    assert verdict.kind is LoopGuardVerdictKind.STUCK


def test_alternating_failures_do_not_trip_stuck() -> None:
    """Spec edge case: A,B,A,B failures break the consecutive run."""

    guard = LoopGuard(
        LoopGuardConfig(
            repeated_tool_failure_threshold=3,
            thrash_repeat_threshold=None,
            thrash_window=None,
        )
    )
    a_in = {"cmd": "a"}
    b_in = {"cmd": "b"}
    stream = [
        _failing("u1", tool_input=a_in),
        _failing("u2", tool_input=b_in),
        _failing("u3", tool_input=a_in),
        _failing("u4", tool_input=b_in),
    ]
    assert guard.observe(stream) is None


def test_success_in_the_middle_breaks_the_consecutive_failing_run() -> None:
    guard = LoopGuard(
        LoopGuardConfig(
            repeated_tool_failure_threshold=3,
            thrash_repeat_threshold=None,
            thrash_window=None,
        )
    )
    stream = [
        _failing("u1"),
        _failing("u2"),
        _succeeding("u3"),
        _failing("u4"),
        _failing("u5"),
    ]
    assert guard.observe(stream) is None


def test_pending_result_does_not_count_for_repeated_failure() -> None:
    """Spec edge case: result is ``None`` is not a failure for FR-1."""

    guard = LoopGuard(
        LoopGuardConfig(
            repeated_tool_failure_threshold=3,
            thrash_repeat_threshold=None,
            thrash_window=None,
        )
    )
    pending = _interaction(tool_use_id="u1", has_result=False)
    # Three pending interactions must not trip the failure counter.
    assert guard.observe([pending, pending, pending]) is None


def test_key_order_noise_does_not_split_digests() -> None:
    """Identical mappings with different key orders must collide."""

    guard = LoopGuard(
        LoopGuardConfig(
            repeated_tool_failure_threshold=3,
            thrash_repeat_threshold=None,
            thrash_window=None,
        )
    )
    stream = [
        _failing("u1", tool_input={"a": 1, "b": 2}),
        _failing("u2", tool_input={"b": 2, "a": 1}),
        _failing("u3", tool_input={"a": 1, "b": 2}),
    ]
    verdict = guard.observe(stream)
    assert verdict is not None
    assert verdict.kind is LoopGuardVerdictKind.STUCK


# --- FR-2: thrash (THRASH) ---


def test_thrash_trips_on_identical_successes_within_window() -> None:
    guard = LoopGuard(
        LoopGuardConfig(
            repeated_tool_failure_threshold=None,
            thrash_repeat_threshold=3,
            thrash_window=5,
        )
    )
    stream = [_succeeding(f"u{i}") for i in range(3)]
    verdict = guard.observe(stream)
    assert verdict is not None
    assert verdict.kind is LoopGuardVerdictKind.THRASH
    assert verdict.tool_name == "Bash"
    assert verdict.input_digest


def test_identical_tuple_outside_window_does_not_trip_thrash() -> None:
    """Spec edge case: repeats spread wider than the window are tolerated."""

    guard = LoopGuard(
        LoopGuardConfig(
            repeated_tool_failure_threshold=None,
            thrash_repeat_threshold=3,
            thrash_window=3,
        )
    )
    target = {"cmd": "target"}
    # Distinct decoys -- using identical decoy inputs would themselves
    # satisfy the thrash threshold and obscure what this test asserts.
    stream = [
        _succeeding("u1", tool_input=target),
        _succeeding("u2", tool_input=target),
        _succeeding("u3", tool_input={"cmd": "decoy-a"}),
        _succeeding("u4", tool_input={"cmd": "decoy-b"}),
        _succeeding("u5", tool_input={"cmd": "decoy-c"}),
        # Trailing 3-call window now holds only decoys; this new ``target``
        # contributes one occurrence -- well under the threshold.
        _succeeding("u6", tool_input=target),
    ]
    assert guard.observe(stream) is None


def test_pending_results_still_count_toward_thrash_repetition() -> None:
    """Spec edge case: result-is-None still counts toward thrash."""

    guard = LoopGuard(
        LoopGuardConfig(
            repeated_tool_failure_threshold=None,
            thrash_repeat_threshold=3,
            thrash_window=5,
        )
    )
    pending = _interaction(tool_use_id="u1", has_result=False)
    verdict = guard.observe([pending, pending, pending])
    assert verdict is not None
    assert verdict.kind is LoopGuardVerdictKind.THRASH


# --- FR-6: precedence ---


def test_precedence_stuck_wins_when_both_detectors_would_trip() -> None:
    """A stream that is both failing-and-repeating and thrashing returns STUCK."""

    guard = LoopGuard(
        LoopGuardConfig(
            repeated_tool_failure_threshold=3,
            thrash_repeat_threshold=3,
            thrash_window=10,
        )
    )
    stream = [_failing(f"u{i}") for i in range(3)]
    verdict = guard.observe(stream)
    assert verdict is not None
    assert verdict.kind is LoopGuardVerdictKind.STUCK


# --- Edge cases ---


def test_empty_interactions_is_a_noop() -> None:
    guard = LoopGuard(LoopGuardConfig())
    assert guard.observe([]) is None


# --- FR-5: each disabled threshold never trips ---


def test_disabled_repeated_failure_threshold_none_never_trips() -> None:
    guard = LoopGuard(
        LoopGuardConfig(
            repeated_tool_failure_threshold=None,
            thrash_repeat_threshold=None,
            thrash_window=None,
        )
    )
    assert guard.observe([_failing(f"u{i}") for i in range(50)]) is None


def test_disabled_repeated_failure_threshold_zero_never_trips() -> None:
    guard = LoopGuard(
        LoopGuardConfig(
            repeated_tool_failure_threshold=0,
            thrash_repeat_threshold=None,
            thrash_window=None,
        )
    )
    assert guard.observe([_failing(f"u{i}") for i in range(50)]) is None


def test_disabled_thrash_repeat_threshold_never_trips() -> None:
    guard = LoopGuard(
        LoopGuardConfig(
            repeated_tool_failure_threshold=None,
            thrash_repeat_threshold=0,
            thrash_window=5,
        )
    )
    assert guard.observe([_succeeding(f"u{i}") for i in range(20)]) is None


def test_disabled_thrash_window_never_trips() -> None:
    guard = LoopGuard(
        LoopGuardConfig(
            repeated_tool_failure_threshold=None,
            thrash_repeat_threshold=3,
            thrash_window=0,
        )
    )
    assert guard.observe([_succeeding(f"u{i}") for i in range(20)]) is None


def test_thrash_repeat_threshold_none_never_trips() -> None:
    guard = LoopGuard(
        LoopGuardConfig(
            repeated_tool_failure_threshold=None,
            thrash_repeat_threshold=None,
            thrash_window=5,
        )
    )
    assert guard.observe([_succeeding(f"u{i}") for i in range(20)]) is None


def test_thrash_window_none_never_trips() -> None:
    guard = LoopGuard(
        LoopGuardConfig(
            repeated_tool_failure_threshold=None,
            thrash_repeat_threshold=3,
            thrash_window=None,
        )
    )
    assert guard.observe([_succeeding(f"u{i}") for i in range(20)]) is None


# --- Config carries the watchdog knob as pure data ---


def test_hang_timeout_seconds_defaults_to_none_and_is_a_pure_data_field() -> None:
    """The pure module owns the knob; the harness consumes it."""

    cfg = LoopGuardConfig()
    assert cfg.hang_timeout_seconds is None
    # Setting it does not affect ``observe`` -- this is a config-only field.
    guard = LoopGuard(LoopGuardConfig(hang_timeout_seconds=0.5))
    assert guard.observe([_succeeding("u1")]) is None

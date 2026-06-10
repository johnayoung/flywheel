"""Behavioral tests for ``flywheel.grader_manual``.

Covers the pure gate-selection contract (``next_pending_manual_gate``)
and the manual receipt assembly contract (``build_manual_result``) per
the acceptance criteria in
``.flywheel/specs/00016-FEATURE-manual-grader-approval-gate.md``
(FR-3).
"""

from __future__ import annotations

from datetime import datetime, timezone

from flywheel import (
    CommandGrader,
    ManualGrader,
    RubricGrader,
    Task,
    TranscriptGrader,
)
from flywheel.grader_manual import (
    ManualGate,
    build_manual_result,
    next_pending_manual_gate,
)


def _task(*graders: object) -> Task:
    return Task(goal="g", graders=list(graders))  # type: ignore[arg-type]


# --- next_pending_manual_gate ---------------------------------------------


def test_first_gate_when_after_ordinal_is_none() -> None:
    """``after_ordinal=None`` returns the first manual gate in list order."""

    task = _task(
        CommandGrader(run="true"),
        ManualGrader(instruction="confirm A", name="gate-a"),
        ManualGrader(instruction="confirm B", name="gate-b"),
    )

    gate = next_pending_manual_gate(task, after_ordinal=None)

    assert gate == ManualGate(
        ordinal=1,
        instruction="confirm A",
        grader_name="gate-a",
    )


def test_next_gate_strictly_after_given_ordinal() -> None:
    """``after_ordinal=n`` returns the first manual gate at index ``> n``,
    not ``>= n``."""

    task = _task(
        ManualGrader(instruction="confirm A", name="gate-a"),
        ManualGrader(instruction="confirm B", name="gate-b"),
        ManualGrader(instruction="confirm C", name="gate-c"),
    )

    # after_ordinal=0 must skip gate-a (ordinal 0) and return gate-b.
    gate = next_pending_manual_gate(task, after_ordinal=0)
    assert gate is not None
    assert gate.ordinal == 1
    assert gate.grader_name == "gate-b"

    # after_ordinal=1 returns gate-c, not gate-b.
    gate = next_pending_manual_gate(task, after_ordinal=1)
    assert gate is not None
    assert gate.ordinal == 2
    assert gate.grader_name == "gate-c"


def test_none_when_no_gate_remains() -> None:
    """No remaining manual gate returns ``None``, never an error."""

    # Past the last manual gate.
    task = _task(
        CommandGrader(run="true"),
        ManualGrader(instruction="only one", name="solo"),
        RubricGrader(assertions=["x"]),
    )
    assert next_pending_manual_gate(task, after_ordinal=1) is None

    # Task with no manual graders at all.
    no_manual = _task(
        CommandGrader(run="true"),
        RubricGrader(assertions=["x"]),
        TranscriptGrader(max_turns=5),
    )
    assert next_pending_manual_gate(no_manual, after_ordinal=None) is None
    assert next_pending_manual_gate(no_manual, after_ordinal=0) is None

    # Empty grader list returns None for both starting points.
    empty = _task()
    assert next_pending_manual_gate(empty, after_ordinal=None) is None
    assert next_pending_manual_gate(empty, after_ordinal=42) is None


def test_skips_non_manual_graders_while_preserving_ordinals() -> None:
    """Non-manual graders are skipped without shifting the returned
    ordinal: it is the manual grader's literal index in ``task.graders``,
    not the count of manual graders preceding it."""

    task = _task(
        CommandGrader(run="true"),
        RubricGrader(assertions=["x"]),
        TranscriptGrader(max_turns=5),
        ManualGrader(instruction="confirm A", name="gate-a"),
        CommandGrader(run="true"),
        ManualGrader(instruction="confirm B", name="gate-b"),
    )

    first = next_pending_manual_gate(task, after_ordinal=None)
    assert first is not None
    # Three non-manual graders precede gate-a; its ordinal is 3, not 0.
    assert first.ordinal == 3
    assert first.grader_name == "gate-a"
    assert first.instruction == "confirm A"

    # Walking forward from gate-a must skip the intervening command
    # grader and land on gate-b at its literal index 5.
    second = next_pending_manual_gate(task, after_ordinal=first.ordinal)
    assert second is not None
    assert second.ordinal == 5
    assert second.grader_name == "gate-b"
    assert second.instruction == "confirm B"

    # Past the last manual gate -> None.
    assert next_pending_manual_gate(task, after_ordinal=second.ordinal) is None


def test_manual_gate_without_name_carries_none() -> None:
    """``ManualGrader.name`` is optional; ``ManualGate.grader_name`` mirrors it."""

    task = _task(ManualGrader(instruction="unlabeled"))
    gate = next_pending_manual_gate(task, after_ordinal=None)
    assert gate == ManualGate(
        ordinal=0,
        instruction="unlabeled",
        grader_name=None,
    )


# --- build_manual_result ---------------------------------------------------


def test_build_manual_result_approve_payload() -> None:
    """``passed=True`` (operator approve) builds a manual receipt with the
    documented payload shape and snapshots the gate's grader spec."""

    gate = ManualGate(
        ordinal=2,
        instruction="confirm migration is safe",
        grader_name="confirm-migration",
    )
    ts = datetime(2024, 5, 1, 12, 0, tzinfo=timezone.utc)

    record = build_manual_result(
        gate,
        run_id="run-1",
        attempt_number=1,
        passed=True,
        summary="looks good",
        now=ts,
    )

    assert record.run_id == "run-1"
    assert record.attempt_number == 1
    assert record.ordinal == 2
    assert record.grader_type == "manual"
    assert record.grader_name == "confirm-migration"
    assert record.passed is True
    assert record.duration_ms == 0
    assert record.ts == ts
    assert record.id is None
    assert dict(record.grader_spec) == {
        "type": "manual",
        "instruction": "confirm migration is safe",
        "name": "confirm-migration",
    }
    assert dict(record.payload) == {
        "instruction": "confirm migration is safe",
        "summary": "looks good",
    }


def test_build_manual_result_reject_payload_carries_feedback() -> None:
    """``passed=False`` (operator reject) carries the feedback summary
    verbatim in the payload and keeps ``grader_type="manual"``."""

    gate = ManualGate(
        ordinal=4,
        instruction="confirm migration is safe",
        grader_name="confirm-migration",
    )
    ts = datetime(2024, 5, 2, 9, 30, tzinfo=timezone.utc)
    feedback = (
        "The migration drops a column still read by the billing service. "
        "Gate it behind a feature flag first."
    )

    record = build_manual_result(
        gate,
        run_id="run-2",
        attempt_number=3,
        passed=False,
        summary=feedback,
        now=ts,
    )

    assert record.passed is False
    assert record.grader_type == "manual"
    assert record.attempt_number == 3
    assert record.ordinal == 4
    assert record.duration_ms == 0
    assert record.ts == ts
    assert record.payload["summary"] == feedback
    assert record.payload["instruction"] == "confirm migration is safe"
    assert dict(record.grader_spec) == {
        "type": "manual",
        "instruction": "confirm migration is safe",
        "name": "confirm-migration",
    }


def test_build_manual_result_unnamed_gate_omits_name_from_spec() -> None:
    """An unnamed gate produces a ``grader_spec`` without a ``name`` key,
    matching the on-task shape, and ``grader_name`` on the record is None."""

    gate = ManualGate(
        ordinal=0,
        instruction="confirm",
        grader_name=None,
    )
    ts = datetime(2024, 1, 1, tzinfo=timezone.utc)

    record = build_manual_result(
        gate,
        run_id="run-3",
        attempt_number=1,
        passed=True,
        summary="ok",
        now=ts,
    )

    assert record.grader_name is None
    assert "name" not in dict(record.grader_spec)
    assert dict(record.grader_spec) == {
        "type": "manual",
        "instruction": "confirm",
    }

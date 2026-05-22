from dataclasses import fields
from datetime import datetime, timezone

import pytest

from flywheel import (
    Attempt,
    Lifecycle,
    LifecycleTransitionError,
    Outcome,
    Status,
)


# --- Type fidelity ---------------------------------------------------------


def test_status_enumerates_exactly_the_eight_spec_states() -> None:
    expected = {
        "pending",
        "ready",
        "running",
        "validating",
        "failed_validation",
        "done",
        "failed",
        "interrupted",
    }
    assert {member.value for member in Status} == expected


def test_outcome_enumerates_exactly_the_five_spec_outcomes() -> None:
    expected = {
        "succeeded",
        "validation_failed",
        "agent_error",
        "cancelled",
        "internal_error",
    }
    assert {member.value for member in Outcome} == expected


def test_lifecycle_fields_match_spec_table() -> None:
    expected = {
        "task_id",
        "run_id",
        "worker_id",
        "status",
        "timestamps",
        "version",
        "retries",
        "error",
        "agent_output",
        "attempts",
        "session_id",
        "artifacts_dir",
    }
    assert {f.name for f in fields(Lifecycle)} == expected


def test_attempt_fields_match_spec_table() -> None:
    expected = {
        "number",
        "started_at",
        "ended_at",
        "outcome",
        "agent_output",
        "error",
        "agent_context",
        "run_id",
    }
    assert {f.name for f in fields(Attempt)} == expected


def test_attempt_constructs_directly_with_agent_context() -> None:
    a = Attempt(
        number=1,
        started_at=datetime(2024, 1, 1, tzinfo=timezone.utc),
        run_id="r1",
        agent_context={"model_id": "claude-opus-4-7"},
    )
    assert a.agent_context == {"model_id": "claude-opus-4-7"}


def test_default_lifecycle_in_pending_state_with_empty_attempts() -> None:
    lc = Lifecycle(task_id="t1")
    assert lc.status is Status.PENDING
    assert lc.retries == 0
    assert lc.attempts == []


# --- Valid transitions -----------------------------------------------------


def test_pending_to_ready() -> None:
    lc = Lifecycle(task_id="t")
    lc.transition_to(Status.READY)
    assert lc.status is Status.READY
    assert Status.READY in lc.timestamps


def test_ready_to_running() -> None:
    lc = Lifecycle(task_id="t", status=Status.READY)
    lc.transition_to(Status.RUNNING)
    assert lc.status is Status.RUNNING


def test_running_to_validating() -> None:
    lc = Lifecycle(task_id="t", status=Status.RUNNING)
    lc.transition_to(Status.VALIDATING)
    assert lc.status is Status.VALIDATING


def test_running_to_failed_with_error() -> None:
    lc = Lifecycle(task_id="t", status=Status.RUNNING)
    lc.transition_to(Status.FAILED, error="boom")
    assert lc.status is Status.FAILED
    assert lc.error == "boom"


def test_running_to_interrupted_no_error_required() -> None:
    lc = Lifecycle(task_id="t", status=Status.RUNNING)
    lc.transition_to(Status.INTERRUPTED)
    assert lc.status is Status.INTERRUPTED


def test_validating_to_done() -> None:
    lc = Lifecycle(task_id="t", status=Status.VALIDATING)
    lc.transition_to(Status.DONE)
    assert lc.status is Status.DONE


def test_validating_to_failed_validation_requires_error() -> None:
    lc = Lifecycle(task_id="t", status=Status.VALIDATING)
    lc.transition_to(Status.FAILED_VALIDATION, error="assert failed")
    assert lc.status is Status.FAILED_VALIDATION
    assert lc.error == "assert failed"


def test_failed_validation_to_failed_terminates() -> None:
    lc = Lifecycle(
        task_id="t",
        status=Status.FAILED_VALIDATION,
        error="prior",
        retries=3,
    )
    lc.transition_to(Status.FAILED, error="give up")
    assert lc.status is Status.FAILED
    assert lc.error == "give up"


def test_interrupted_to_ready() -> None:
    lc = Lifecycle(task_id="t", status=Status.INTERRUPTED)
    lc.transition_to(Status.READY)
    assert lc.status is Status.READY


# --- Illegal transitions ---------------------------------------------------


def test_done_is_terminal_no_outbound_transitions() -> None:
    for target in Status:
        lc = Lifecycle(task_id="t", status=Status.DONE)
        with pytest.raises(LifecycleTransitionError, match="illegal transition"):
            lc.transition_to(target, error="x")


def test_failed_is_terminal_no_outbound_transitions() -> None:
    for target in Status:
        lc = Lifecycle(task_id="t", status=Status.FAILED)
        with pytest.raises(LifecycleTransitionError, match="illegal transition"):
            lc.transition_to(target, error="x")


def test_pending_cannot_skip_directly_to_running() -> None:
    lc = Lifecycle(task_id="t")
    with pytest.raises(LifecycleTransitionError, match="illegal transition"):
        lc.transition_to(Status.RUNNING)


def test_ready_cannot_jump_to_done() -> None:
    lc = Lifecycle(task_id="t", status=Status.READY)
    with pytest.raises(LifecycleTransitionError, match="illegal transition"):
        lc.transition_to(Status.DONE)


def test_interrupted_cannot_resume_directly_to_running() -> None:
    lc = Lifecycle(task_id="t", status=Status.INTERRUPTED)
    with pytest.raises(LifecycleTransitionError, match="illegal transition"):
        lc.transition_to(Status.RUNNING)


def test_running_cannot_go_directly_to_failed_validation() -> None:
    lc = Lifecycle(task_id="t", status=Status.RUNNING)
    with pytest.raises(LifecycleTransitionError, match="illegal transition"):
        lc.transition_to(Status.FAILED_VALIDATION, error="x")


def test_running_cannot_go_directly_to_done() -> None:
    lc = Lifecycle(task_id="t", status=Status.RUNNING)
    with pytest.raises(LifecycleTransitionError, match="illegal transition"):
        lc.transition_to(Status.DONE)


def test_validating_cannot_go_back_to_running() -> None:
    lc = Lifecycle(task_id="t", status=Status.VALIDATING)
    with pytest.raises(LifecycleTransitionError, match="illegal transition"):
        lc.transition_to(Status.RUNNING)


# --- Error requirement -----------------------------------------------------


def test_transition_to_failed_without_error_rejected() -> None:
    lc = Lifecycle(task_id="t", status=Status.RUNNING)
    with pytest.raises(LifecycleTransitionError, match="requires a non-empty error"):
        lc.transition_to(Status.FAILED)
    assert lc.status is Status.RUNNING


def test_transition_to_failed_validation_without_error_rejected() -> None:
    lc = Lifecycle(task_id="t", status=Status.VALIDATING)
    with pytest.raises(LifecycleTransitionError, match="requires a non-empty error"):
        lc.transition_to(Status.FAILED_VALIDATION)
    assert lc.status is Status.VALIDATING


def test_transition_to_failed_with_empty_string_rejected() -> None:
    lc = Lifecycle(task_id="t", status=Status.RUNNING)
    with pytest.raises(LifecycleTransitionError, match="requires a non-empty error"):
        lc.transition_to(Status.FAILED, error="")


# --- Retry semantics -------------------------------------------------------


def test_retry_transition_increments_retries_and_clears_error() -> None:
    history = [
        Attempt(
            number=1,
            started_at=datetime(2024, 1, 1, tzinfo=timezone.utc),
            run_id="run-a",
            outcome=Outcome.VALIDATION_FAILED,
            error="grader failed",
        ),
    ]
    lc = Lifecycle(
        task_id="t",
        status=Status.FAILED_VALIDATION,
        error="grader failed",
        retries=0,
        attempts=history,
    )
    lc.transition_to(Status.READY)
    assert lc.status is Status.READY
    assert lc.retries == 1
    assert lc.error == ""
    assert lc.attempts == history  # history preserved


def test_retry_transition_preserves_multi_attempt_history() -> None:
    history = [
        Attempt(
            number=1,
            started_at=datetime(2024, 1, 1, tzinfo=timezone.utc),
            run_id="run-a",
            outcome=Outcome.VALIDATION_FAILED,
        ),
        Attempt(
            number=2,
            started_at=datetime(2024, 1, 1, 0, 1, tzinfo=timezone.utc),
            run_id="run-a",
            outcome=Outcome.VALIDATION_FAILED,
        ),
    ]
    lc = Lifecycle(
        task_id="t",
        status=Status.FAILED_VALIDATION,
        error="x",
        attempts=history,
    )
    lc.transition_to(Status.READY)
    assert len(lc.attempts) == 2
    assert [a.number for a in lc.attempts] == [1, 2]


# --- Retry eligibility -----------------------------------------------------


def test_is_retry_eligible_true_when_status_failed_validation_and_budget_left() -> None:
    lc = Lifecycle(task_id="t", status=Status.FAILED_VALIDATION, retries=1)
    assert lc.is_retry_eligible(max_retries=3) is True


def test_is_retry_eligible_false_when_status_is_wrong() -> None:
    lc = Lifecycle(task_id="t", status=Status.FAILED, retries=0)
    assert lc.is_retry_eligible(max_retries=3) is False
    lc2 = Lifecycle(task_id="t", status=Status.RUNNING, retries=0)
    assert lc2.is_retry_eligible(max_retries=3) is False


def test_is_retry_eligible_false_when_budget_exhausted() -> None:
    lc = Lifecycle(task_id="t", status=Status.FAILED_VALIDATION, retries=3)
    assert lc.is_retry_eligible(max_retries=3) is False


def test_is_retry_eligible_false_when_both_conditions_wrong() -> None:
    lc = Lifecycle(task_id="t", status=Status.DONE, retries=5)
    assert lc.is_retry_eligible(max_retries=3) is False


# --- consecutive_failed_runs ----------------------------------------------


def _attempt(
    number: int,
    run_id: str,
    outcome: Outcome | None,
) -> Attempt:
    return Attempt(
        number=number,
        started_at=datetime(2024, 1, 1, tzinfo=timezone.utc),
        run_id=run_id,
        outcome=outcome,
    )


def test_consecutive_failed_runs_empty_returns_zero() -> None:
    lc = Lifecycle(task_id="t")
    assert lc.consecutive_failed_runs() == 0


def test_consecutive_failed_runs_successful_tail_returns_zero() -> None:
    lc = Lifecycle(
        task_id="t",
        attempts=[
            _attempt(1, "run-a", Outcome.VALIDATION_FAILED),
            _attempt(2, "run-a", Outcome.SUCCEEDED),
        ],
    )
    assert lc.consecutive_failed_runs() == 0


def test_consecutive_failed_runs_single_failed_tail_returns_one() -> None:
    lc = Lifecycle(
        task_id="t",
        attempts=[_attempt(1, "run-a", Outcome.VALIDATION_FAILED)],
    )
    assert lc.consecutive_failed_runs() == 1


def test_consecutive_failed_runs_same_run_id_chain_counts_all() -> None:
    lc = Lifecycle(
        task_id="t",
        attempts=[
            _attempt(1, "run-a", Outcome.AGENT_ERROR),
            _attempt(2, "run-a", Outcome.VALIDATION_FAILED),
            _attempt(3, "run-a", Outcome.INTERNAL_ERROR),
        ],
    )
    assert lc.consecutive_failed_runs() == 3


def test_consecutive_failed_runs_resets_on_run_id_change() -> None:
    lc = Lifecycle(
        task_id="t",
        attempts=[
            _attempt(1, "run-a", Outcome.VALIDATION_FAILED),
            _attempt(2, "run-b", Outcome.VALIDATION_FAILED),
        ],
    )
    assert lc.consecutive_failed_runs() == 1


def test_consecutive_failed_runs_stops_at_successful_predecessor() -> None:
    lc = Lifecycle(
        task_id="t",
        attempts=[
            _attempt(1, "run-a", Outcome.SUCCEEDED),
            _attempt(2, "run-a", Outcome.VALIDATION_FAILED),
        ],
    )
    assert lc.consecutive_failed_runs() == 1


def test_consecutive_failed_runs_cancelled_is_not_failure() -> None:
    lc = Lifecycle(
        task_id="t",
        attempts=[_attempt(1, "run-a", Outcome.CANCELLED)],
    )
    assert lc.consecutive_failed_runs() == 0

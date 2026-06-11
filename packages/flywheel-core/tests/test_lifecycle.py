from dataclasses import fields
from datetime import datetime, timezone

import pytest

from flywheel_core import (
    Attempt,
    Lifecycle,
    LifecycleTransitionError,
    Outcome,
    Status,
)


# --- Type fidelity ---------------------------------------------------------


def test_status_enumerates_exactly_the_ten_spec_states() -> None:
    expected = {
        "pending",
        "ready",
        "running",
        "validating",
        "awaiting_approval",
        "failed_validation",
        "internal_error",
        "done",
        "failed",
        "interrupted",
    }
    assert {member.value for member in Status} == expected


def test_outcome_enumerates_exactly_the_six_spec_outcomes() -> None:
    expected = {
        "succeeded",
        "validation_failed",
        "agent_error",
        "cancelled",
        "internal_error",
        "recovered",
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
        "blocked_requires_json",
        "awaiting_manual_ordinal",
        "task_content_hash",
    }
    assert {f.name for f in fields(Lifecycle)} == expected


def test_default_lifecycle_blocked_requires_json_is_none() -> None:
    lc = Lifecycle(task_id="t1")
    assert lc.blocked_requires_json is None


def test_default_lifecycle_awaiting_manual_ordinal_is_none() -> None:
    lc = Lifecycle(task_id="t1")
    assert lc.awaiting_manual_ordinal is None


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
        # Boundary-rolled aggregates (FR-6): updated by the harness at
        # iteration boundaries through the versioned save_attempt write.
        "input_tokens",
        "output_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
        "iterations_completed",
        "turns",
        "total_cost_usd",
        "last_activity_at",
    }
    assert {f.name for f in fields(Attempt)} == expected


def test_attempt_aggregates_default_to_zero_with_no_activity() -> None:
    a = Attempt(
        number=1,
        started_at=datetime(2024, 1, 1, tzinfo=timezone.utc),
        run_id="r1",
    )
    assert a.input_tokens == 0
    assert a.output_tokens == 0
    assert a.cache_creation_input_tokens == 0
    assert a.cache_read_input_tokens == 0
    assert a.total_tokens == 0
    assert a.iterations_completed == 0
    assert a.turns == 0
    assert a.total_cost_usd == 0.0
    assert a.last_activity_at is None


def test_attempt_total_tokens_sums_the_four_usage_fields() -> None:
    a = Attempt(
        number=1,
        started_at=datetime(2024, 1, 1, tzinfo=timezone.utc),
        run_id="r1",
        input_tokens=1000,
        output_tokens=200,
        cache_creation_input_tokens=30,
        cache_read_input_tokens=4,
    )
    assert a.total_tokens == 1234


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


def test_validating_to_awaiting_approval_no_error_required() -> None:
    lc = Lifecycle(task_id="t", status=Status.VALIDATING)
    lc.transition_to(Status.AWAITING_APPROVAL)
    assert lc.status is Status.AWAITING_APPROVAL
    assert Status.AWAITING_APPROVAL in lc.timestamps
    assert lc.error == ""


def test_awaiting_approval_to_done() -> None:
    lc = Lifecycle(task_id="t", status=Status.AWAITING_APPROVAL)
    lc.transition_to(Status.DONE)
    assert lc.status is Status.DONE


def test_awaiting_approval_to_failed_validation_requires_error() -> None:
    lc = Lifecycle(task_id="t", status=Status.AWAITING_APPROVAL)
    lc.transition_to(
        Status.FAILED_VALIDATION,
        error="manual grader 'confirm-migration' rejected by operator",
    )
    assert lc.status is Status.FAILED_VALIDATION
    assert lc.error == "manual grader 'confirm-migration' rejected by operator"


def test_awaiting_approval_to_failed_validation_without_error_rejected() -> None:
    lc = Lifecycle(task_id="t", status=Status.AWAITING_APPROVAL)
    with pytest.raises(LifecycleTransitionError, match="requires a non-empty error"):
        lc.transition_to(Status.FAILED_VALIDATION)
    assert lc.status is Status.AWAITING_APPROVAL


def test_validating_to_failed_validation_requires_error() -> None:
    lc = Lifecycle(task_id="t", status=Status.VALIDATING)
    lc.transition_to(Status.FAILED_VALIDATION, error="assert failed")
    assert lc.status is Status.FAILED_VALIDATION
    assert lc.error == "assert failed"


def test_validating_to_interrupted_no_error_required() -> None:
    lc = Lifecycle(task_id="t", status=Status.VALIDATING)
    lc.transition_to(Status.INTERRUPTED)
    assert lc.status is Status.INTERRUPTED


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


# --- blocked_requires_json centralized clear ------------------------------


def test_interrupted_to_ready_clears_blocked_requires_json() -> None:
    lc = Lifecycle(
        task_id="t",
        status=Status.INTERRUPTED,
        blocked_requires_json='[{"type":"env_var_set","name":"FOO"}]',
    )
    lc.transition_to(Status.READY)
    assert lc.blocked_requires_json is None


def test_failed_validation_retry_clears_blocked_requires_json() -> None:
    lc = Lifecycle(
        task_id="t",
        status=Status.FAILED_VALIDATION,
        error="x",
        blocked_requires_json='[{"type":"env_var_set","name":"FOO"}]',
    )
    lc.transition_to(Status.READY)
    assert lc.blocked_requires_json is None


def test_internal_error_retry_clears_blocked_requires_json() -> None:
    lc = Lifecycle(
        task_id="t",
        status=Status.INTERNAL_ERROR,
        error="x",
        blocked_requires_json='[{"type":"env_var_set","name":"FOO"}]',
    )
    lc.transition_to(Status.READY)
    assert lc.blocked_requires_json is None


def test_pending_to_ready_clears_blocked_requires_json() -> None:
    """Entry-time normalization edge — should also drop the snapshot
    so a never-cleared payload on a fresh lifecycle row cannot survive
    the very first transition into the active state machine."""
    lc = Lifecycle(
        task_id="t",
        blocked_requires_json='[{"type":"env_var_set","name":"FOO"}]',
    )
    lc.transition_to(Status.READY)
    assert lc.blocked_requires_json is None


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


def test_awaiting_approval_other_transitions_rejected() -> None:
    """AWAITING_APPROVAL is non-terminal but only exits to DONE or
    FAILED_VALIDATION. Every other target — including reflexive
    AWAITING_APPROVAL — must raise LifecycleTransitionError."""
    allowed = {Status.DONE, Status.FAILED_VALIDATION}
    for target in Status:
        if target in allowed:
            continue
        lc = Lifecycle(task_id="t", status=Status.AWAITING_APPROVAL)
        with pytest.raises(LifecycleTransitionError, match="illegal transition"):
            lc.transition_to(target, error="x")


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


def test_is_retry_eligible_false_in_awaiting_approval() -> None:
    """AWAITING_APPROVAL is a parked, non-terminal state; it is not a
    retry source. is_retry_eligible must report False regardless of
    remaining budget so the harness never schedules a fresh attempt
    while a human gate is pending."""
    lc = Lifecycle(task_id="t", status=Status.AWAITING_APPROVAL, retries=0)
    assert lc.is_retry_eligible(max_retries=3) is False


def test_entering_awaiting_approval_does_not_consume_retry_budget() -> None:
    """Entering AWAITING_APPROVAL from VALIDATING is an automated-graders-
    passed park, not a failure. It must not bump `retries` and must not
    clear `error` as a retry edge would."""
    lc = Lifecycle(task_id="t", status=Status.VALIDATING, retries=2)
    lc.transition_to(Status.AWAITING_APPROVAL)
    assert lc.status is Status.AWAITING_APPROVAL
    assert lc.retries == 2


def test_awaiting_approval_to_failed_validation_does_not_consume_retry_budget() -> None:
    """The rejection edge itself does not consume budget; the
    subsequent FAILED_VALIDATION -> READY edge is the retry-source
    edge. Confirm budget is unchanged on the rejection step."""
    lc = Lifecycle(
        task_id="t",
        status=Status.AWAITING_APPROVAL,
        retries=1,
    )
    lc.transition_to(Status.FAILED_VALIDATION, error="rejected")
    assert lc.retries == 1


# --- awaiting_manual_ordinal centralized clear ----------------------------


def test_awaiting_approval_to_done_clears_awaiting_manual_ordinal() -> None:
    lc = Lifecycle(
        task_id="t",
        status=Status.AWAITING_APPROVAL,
        awaiting_manual_ordinal=3,
    )
    lc.transition_to(Status.DONE)
    assert lc.awaiting_manual_ordinal is None


def test_awaiting_approval_to_failed_validation_clears_awaiting_manual_ordinal() -> None:
    lc = Lifecycle(
        task_id="t",
        status=Status.AWAITING_APPROVAL,
        awaiting_manual_ordinal=2,
    )
    lc.transition_to(Status.FAILED_VALIDATION, error="rejected")
    assert lc.awaiting_manual_ordinal is None


def test_failed_validation_retry_clears_awaiting_manual_ordinal() -> None:
    """A leaked ordinal on a FAILED_VALIDATION row (e.g. inherited via
    replace_from from a prior parked snapshot) must not survive the
    retry edge into READY."""
    lc = Lifecycle(
        task_id="t",
        status=Status.FAILED_VALIDATION,
        error="x",
        awaiting_manual_ordinal=4,
    )
    lc.transition_to(Status.READY)
    assert lc.awaiting_manual_ordinal is None


def test_pending_to_ready_clears_awaiting_manual_ordinal() -> None:
    """Entry-time normalization edge — a never-cleared ordinal on a
    fresh lifecycle must not survive the first transition into the
    active state machine."""
    lc = Lifecycle(task_id="t", awaiting_manual_ordinal=7)
    lc.transition_to(Status.READY)
    assert lc.awaiting_manual_ordinal is None


def test_validating_to_awaiting_approval_preserves_awaiting_manual_ordinal() -> None:
    """The harness writes awaiting_manual_ordinal at gate entry; the
    transition into AWAITING_APPROVAL must not clobber it. Only the
    three exit edges (-> READY, -> DONE, -> FAILED_VALIDATION) clear it."""
    lc = Lifecycle(
        task_id="t",
        status=Status.VALIDATING,
        awaiting_manual_ordinal=2,
    )
    lc.transition_to(Status.AWAITING_APPROVAL)
    assert lc.awaiting_manual_ordinal == 2


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

from dataclasses import fields
from datetime import datetime, timezone

from flywheel import (
    Attempt,
    Lifecycle,
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

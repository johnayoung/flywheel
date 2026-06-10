from datetime import datetime, timedelta, timezone

import pytest

from flywheel_core.events import (
    AttemptFinalized,
    AttemptStarted,
    Blocked,
    DomainEventKind,
    EventReplayError,
    GraderEvaluated,
    LifecycleInitialized,
    RetryScheduled,
    SessionRecorded,
    TransitionedTo,
    Unblocked,
    apply,
    replay,
)
from flywheel_core.lifecycle import Lifecycle, LifecycleTransitionError, Outcome, Status


_BASE = datetime(2026, 5, 28, 12, 0, 0, tzinfo=timezone.utc)


def _ts(seconds: int) -> datetime:
    return _BASE + timedelta(seconds=seconds)


def _init(seq: int = 0) -> LifecycleInitialized:
    return LifecycleInitialized(
        run_id="run-1",
        ts=_ts(seq),
        task_id="task-1",
        worker_id="worker-1",
        artifacts_dir="/artifacts/run-1",
    )


def _happy_path() -> list:
    """init -> ready -> running -> attempt -> validating -> grader -> done."""
    return [
        _init(0),
        TransitionedTo(run_id="run-1", ts=_ts(1), target=Status.READY),
        TransitionedTo(run_id="run-1", ts=_ts(2), target=Status.RUNNING),
        AttemptStarted(
            run_id="run-1",
            ts=_ts(3),
            attempt_number=1,
            number=1,
            attempt_run_id="run-1",
            started_at=_ts(3),
            agent_context={"model": "claude-opus-4-8"},
        ),
        TransitionedTo(run_id="run-1", ts=_ts(4), target=Status.VALIDATING),
        GraderEvaluated(
            run_id="run-1",
            ts=_ts(5),
            attempt_number=1,
            ordinal=0,
            grader_type="command",
            passed=True,
            duration_ms=12,
            grader_name="pytest",
            grader_spec={"run": "uv run pytest"},
            payload={"exit_code": 0},
        ),
        AttemptFinalized(
            run_id="run-1",
            ts=_ts(6),
            attempt_number=1,
            number=1,
            outcome=Outcome.SUCCEEDED,
            ended_at=_ts(6),
            agent_output="all done",
        ),
        TransitionedTo(run_id="run-1", ts=_ts(7), target=Status.DONE),
    ]


def test_replay_rejects_empty_stream() -> None:
    with pytest.raises(EventReplayError):
        replay([])


def test_replay_requires_leading_initialized() -> None:
    with pytest.raises(EventReplayError):
        replay([TransitionedTo(run_id="run-1", ts=_ts(1), target=Status.READY)])


def test_duplicate_initialization_raises() -> None:
    with pytest.raises(EventReplayError):
        replay([_init(0), _init(1)])


def test_version_equals_domain_event_count() -> None:
    events = _happy_path()
    lc = replay(events)
    assert lc.version == len(events)


def test_replay_is_deterministic() -> None:
    assert replay(_happy_path()) == replay(_happy_path())


def test_happy_path_folds_to_done_with_finalized_attempt() -> None:
    lc = replay(_happy_path())
    assert lc.status is Status.DONE
    assert lc.task_id == "task-1"
    assert lc.worker_id == "worker-1"
    assert lc.artifacts_dir == "/artifacts/run-1"
    assert lc.agent_output == "all done"
    assert len(lc.attempts) == 1
    only = lc.attempts[0]
    assert only.number == 1
    assert only.outcome is Outcome.SUCCEEDED
    assert only.ended_at == _ts(6)
    assert only.agent_context == {"model": "claude-opus-4-8"}


def test_illegal_transition_surfaces_loudly() -> None:
    with pytest.raises(LifecycleTransitionError):
        replay(
            [
                _init(0),
                TransitionedTo(run_id="run-1", ts=_ts(1), target=Status.RUNNING),
            ]
        )


def test_transition_to_failure_requires_error() -> None:
    # VALIDATING -> FAILED_VALIDATION with an empty error is illegal; the
    # reducer must not paper over it.
    events = [
        _init(0),
        TransitionedTo(run_id="run-1", ts=_ts(1), target=Status.READY),
        TransitionedTo(run_id="run-1", ts=_ts(2), target=Status.RUNNING),
        TransitionedTo(run_id="run-1", ts=_ts(3), target=Status.VALIDATING),
        TransitionedTo(
            run_id="run-1", ts=_ts(4), target=Status.FAILED_VALIDATION
        ),
    ]
    with pytest.raises(LifecycleTransitionError):
        replay(events)


def test_retry_edge_increments_retries_and_clears_error() -> None:
    events = [
        _init(0),
        TransitionedTo(run_id="run-1", ts=_ts(1), target=Status.READY),
        TransitionedTo(run_id="run-1", ts=_ts(2), target=Status.RUNNING),
        TransitionedTo(run_id="run-1", ts=_ts(3), target=Status.VALIDATING),
        TransitionedTo(
            run_id="run-1",
            ts=_ts(4),
            target=Status.FAILED_VALIDATION,
            error="grader failed",
        ),
        RetryScheduled(
            run_id="run-1",
            ts=_ts(5),
            retries_used=0,
            max_retries=2,
        ),
        TransitionedTo(run_id="run-1", ts=_ts(6), target=Status.READY),
    ]
    lc = replay(events)
    assert lc.status is Status.READY
    assert lc.retries == 1
    assert lc.error == ""
    assert lc.version == len(events)


def test_blocked_snapshot_set_then_cleared_on_ready() -> None:
    snapshot = '[{"type": "command_grader", "name": "build"}]'
    events = [
        _init(0),
        TransitionedTo(run_id="run-1", ts=_ts(1), target=Status.READY),
        TransitionedTo(run_id="run-1", ts=_ts(2), target=Status.RUNNING),
        Blocked(run_id="run-1", ts=_ts(3), requires_json=snapshot),
        TransitionedTo(run_id="run-1", ts=_ts(4), target=Status.INTERRUPTED),
    ]
    interrupted = replay(events)
    assert interrupted.status is Status.INTERRUPTED
    assert interrupted.blocked_requires_json == snapshot

    events.append(
        Unblocked(run_id="run-1", ts=_ts(5)),
    )
    events.append(
        TransitionedTo(run_id="run-1", ts=_ts(6), target=Status.READY),
    )
    recovered = replay(events)
    assert recovered.status is Status.READY
    assert recovered.blocked_requires_json is None


def test_session_recorded_folds_session_id() -> None:
    events = [
        _init(0),
        SessionRecorded(run_id="run-1", ts=_ts(1), session_id="sess-42"),
    ]
    assert replay(events).session_id == "sess-42"


def test_identity_fold_events_only_advance_version() -> None:
    base_events = [
        _init(0),
        TransitionedTo(run_id="run-1", ts=_ts(1), target=Status.READY),
    ]
    before = replay(base_events)
    for identity_event in (
        Unblocked(run_id="run-1", ts=_ts(2)),
        RetryScheduled(
            run_id="run-1", ts=_ts(2), retries_used=0, max_retries=1
        ),
        GraderEvaluated(
            run_id="run-1",
            ts=_ts(2),
            ordinal=0,
            grader_type="command",
            passed=True,
            duration_ms=1,
        ),
    ):
        after = apply(before, identity_event)
        assert after.version == before.version + 1
        assert after.status == before.status
        assert after.retries == before.retries
        assert after.error == before.error
        assert after.attempts == before.attempts
        assert after.blocked_requires_json == before.blocked_requires_json
        assert after.agent_output == before.agent_output


def test_apply_does_not_mutate_input() -> None:
    before = replay(
        [
            _init(0),
            TransitionedTo(run_id="run-1", ts=_ts(1), target=Status.READY),
        ]
    )
    snapshot_version = before.version
    snapshot_status = before.status
    after = apply(
        before,
        TransitionedTo(run_id="run-1", ts=_ts(2), target=Status.RUNNING),
    )
    assert before.version == snapshot_version
    assert before.status == snapshot_status
    assert after is not before
    assert after.status is Status.RUNNING


def test_finalize_unknown_attempt_raises() -> None:
    events = [
        _init(0),
        TransitionedTo(run_id="run-1", ts=_ts(1), target=Status.READY),
        TransitionedTo(run_id="run-1", ts=_ts(2), target=Status.RUNNING),
        AttemptFinalized(
            run_id="run-1",
            ts=_ts(3),
            number=99,
            outcome=Outcome.SUCCEEDED,
            ended_at=_ts(3),
        ),
    ]
    with pytest.raises(EventReplayError):
        replay(events)


def test_reducer_matches_imperative_transition_to() -> None:
    # Folding transitions through the reducer must produce the same lifecycle
    # as driving transition_to imperatively over the same edges and clocks.
    imperative = Lifecycle(
        task_id="task-1",
        run_id="run-1",
        worker_id="worker-1",
        artifacts_dir="/artifacts/run-1",
    )
    imperative.transition_to(Status.READY, now=_ts(1))
    imperative.transition_to(Status.RUNNING, now=_ts(2))

    folded = replay(
        [
            _init(0),
            TransitionedTo(run_id="run-1", ts=_ts(1), target=Status.READY),
            TransitionedTo(run_id="run-1", ts=_ts(2), target=Status.RUNNING),
        ]
    )
    assert folded == imperative


def test_domain_event_kind_discriminator_is_stable() -> None:
    assert LifecycleInitialized.KIND is DomainEventKind.LIFECYCLE_INITIALIZED
    assert TransitionedTo.KIND is DomainEventKind.TRANSITIONED_TO
    assert GraderEvaluated.KIND is DomainEventKind.GRADER_EVALUATED

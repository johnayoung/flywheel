from datetime import datetime, timedelta, timezone

import pytest

from flywheel_core.events import (
    AttemptFinalized,
    AttemptStarted,
    Blocked,
    CommandApplied,
    DomainEventKind,
    EventReplayError,
    GateGraderReceipt,
    GraderEvaluated,
    HeldOutGateEvaluated,
    Landed,
    LandingParked,
    LifecycleInitialized,
    RetryScheduled,
    SessionRecorded,
    TransitionedTo,
    Unblocked,
    apply,
    replay,
)
from flywheel_core.event_serde import event_from_record, event_kind, event_payload
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
        CommandApplied(
            run_id="run-1",
            ts=_ts(2),
            command_kind="say",
            command_payload={"text": "focus on graders"},
            command_id=7,
        ),
        LandingParked(
            run_id="run-1",
            ts=_ts(2),
            park_kind="uncommitted-work",
            detail="DONE with an uncommitted tree on flywheel/01/t1",
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
    assert CommandApplied.KIND is DomainEventKind.COMMAND_APPLIED
    assert LandingParked.KIND is DomainEventKind.LANDING_PARKED
    # Wire tag is the stable persisted discriminator (SI-12).
    assert DomainEventKind.LANDING_PARKED.value == "landing_parked"


def test_landing_parked_folds_to_identity_leaving_terminal_done() -> None:
    # A DONE run that parks at submit: the audit-witness event advances version
    # only and never moves the run off its terminal DONE status (D-6).
    done = replay(
        [
            _init(0),
            TransitionedTo(run_id="run-1", ts=_ts(1), target=Status.READY),
            TransitionedTo(run_id="run-1", ts=_ts(2), target=Status.RUNNING),
            TransitionedTo(run_id="run-1", ts=_ts(3), target=Status.VALIDATING),
            TransitionedTo(run_id="run-1", ts=_ts(4), target=Status.DONE),
        ]
    )
    assert done.status is Status.DONE

    parked = apply(
        done,
        LandingParked(
            run_id="run-1",
            ts=_ts(4),
            park_kind="divergent-base",
            detail="base advanced; rebase conflicted",
        ),
    )
    assert parked.status is Status.DONE
    assert parked.version == done.version + 1


def test_landing_parked_round_trips_through_serde() -> None:
    event = LandingParked(
        run_id="run-9",
        ts=_ts(5),
        attempt_number=None,
        park_kind="divergent-base",
        detail="cannot fast-forward landing-base",
    )
    assert event_kind(event) == "landing_parked"
    payload = event_payload(event)
    assert payload == {
        "park_kind": "divergent-base",
        "detail": "cannot fast-forward landing-base",
    }
    restored = event_from_record(
        kind=event_kind(event),
        payload=payload,
        run_id=event.run_id,
        ts=event.ts,
        attempt_number=event.attempt_number,
        sequence=None,
        id=None,
    )
    assert isinstance(restored, LandingParked)
    assert restored == event


def test_landed_kind_discriminator_is_stable() -> None:
    assert Landed.KIND is DomainEventKind.LANDED
    # Wire tag is the stable persisted discriminator.
    assert DomainEventKind.LANDED.value == "landed"


def test_landed_folds_to_identity_leaving_terminal_done() -> None:
    # A successful land is recorded after the run finalized DONE: the
    # audit-witness event advances version only and never moves off DONE.
    done = replay(
        [
            _init(0),
            TransitionedTo(run_id="run-1", ts=_ts(1), target=Status.READY),
            TransitionedTo(run_id="run-1", ts=_ts(2), target=Status.RUNNING),
            TransitionedTo(run_id="run-1", ts=_ts(3), target=Status.VALIDATING),
            TransitionedTo(run_id="run-1", ts=_ts(4), target=Status.DONE),
        ]
    )
    assert done.status is Status.DONE

    landed = apply(
        done,
        Landed(
            run_id="run-1",
            ts=_ts(5),
            strategy="merge",
            landed_ref="a" * 40,
        ),
    )
    assert landed.status is Status.DONE
    assert landed.version == done.version + 1


def test_landed_round_trips_through_serde() -> None:
    event = Landed(
        run_id="run-9",
        ts=_ts(5),
        attempt_number=None,
        strategy="merge",
        landed_ref="0123456789abcdef0123456789abcdef01234567",
    )
    assert event_kind(event) == "landed"
    payload = event_payload(event)
    assert payload == {
        "strategy": "merge",
        "landed_ref": "0123456789abcdef0123456789abcdef01234567",
    }
    restored = event_from_record(
        kind=event_kind(event),
        payload=payload,
        run_id=event.run_id,
        ts=event.ts,
        attempt_number=event.attempt_number,
        sequence=None,
        id=None,
    )
    assert isinstance(restored, Landed)
    assert restored == event


def test_landed_pr_reference_round_trips_through_serde() -> None:
    # A PR land carries the pull-request identifier, not a commit sha; the wire
    # shape must round-trip either reference under the one ``strategy`` tag.
    event = Landed(
        run_id="run-9",
        ts=_ts(6),
        attempt_number=None,
        strategy="pr",
        landed_ref="https://example.test/pr/7",
    )
    payload = event_payload(event)
    assert payload == {
        "strategy": "pr",
        "landed_ref": "https://example.test/pr/7",
    }
    restored = event_from_record(
        kind=event_kind(event),
        payload=payload,
        run_id=event.run_id,
        ts=event.ts,
        attempt_number=event.attempt_number,
        sequence=None,
        id=None,
    )
    assert isinstance(restored, Landed)
    assert restored == event


def test_held_out_gate_evaluated_kind_discriminator_is_stable() -> None:
    assert (
        HeldOutGateEvaluated.KIND
        is DomainEventKind.HELD_OUT_GATE_EVALUATED
    )
    # Wire tag is the stable persisted discriminator.
    assert (
        DomainEventKind.HELD_OUT_GATE_EVALUATED.value
        == "held_out_gate_evaluated"
    )


def test_held_out_gate_evaluated_folds_to_identity_leaving_terminal_done() -> None:
    # The gate runs after the run finalized DONE; recording its verdict is an
    # audit witness that advances version only and never moves off DONE (D-2).
    done = replay(
        [
            _init(0),
            TransitionedTo(run_id="run-1", ts=_ts(1), target=Status.READY),
            TransitionedTo(run_id="run-1", ts=_ts(2), target=Status.RUNNING),
            TransitionedTo(run_id="run-1", ts=_ts(3), target=Status.VALIDATING),
            TransitionedTo(run_id="run-1", ts=_ts(4), target=Status.DONE),
        ]
    )
    assert done.status is Status.DONE

    evaluated = apply(
        done,
        HeldOutGateEvaluated(
            run_id="run-1",
            ts=_ts(5),
            outcome="fail",
            reason="held-out gate FAILED",
            receipts=(
                GateGraderReceipt(
                    grader_name="oracle",
                    passed=False,
                    output_excerpt="boom\n",
                ),
            ),
        ),
    )
    assert evaluated.status is Status.DONE
    assert evaluated.version == done.version + 1


def test_held_out_gate_evaluated_round_trips_through_serde() -> None:
    event = HeldOutGateEvaluated(
        run_id="run-9",
        ts=_ts(5),
        attempt_number=None,
        outcome="fail",
        reason="held-out gate FAILED: 2 of 2 grader(s) ran; [oracle-b] "
        "exited exit_code=1",
        receipts=(
            GateGraderReceipt(
                grader_name="oracle-a",
                passed=True,
                output_excerpt="all good\n",
            ),
            # A grader with no declared name and a distinct excerpt: the wire
            # shape must round-trip a None name and preserve per-grader tails.
            GateGraderReceipt(
                grader_name=None,
                passed=False,
                output_excerpt="assertion failed\n",
            ),
        ),
    )
    assert event_kind(event) == "held_out_gate_evaluated"
    payload = event_payload(event)
    assert payload == {
        "outcome": "fail",
        "reason": "held-out gate FAILED: 2 of 2 grader(s) ran; [oracle-b] "
        "exited exit_code=1",
        "receipts": [
            {
                "grader_name": "oracle-a",
                "passed": True,
                "output_excerpt": "all good\n",
            },
            {
                "grader_name": None,
                "passed": False,
                "output_excerpt": "assertion failed\n",
            },
        ],
    }
    restored = event_from_record(
        kind=event_kind(event),
        payload=payload,
        run_id=event.run_id,
        ts=event.ts,
        attempt_number=event.attempt_number,
        sequence=None,
        id=None,
    )
    assert isinstance(restored, HeldOutGateEvaluated)
    assert restored == event


def test_held_out_gate_evaluated_no_gate_round_trips_with_empty_receipts() -> None:
    # A no-gate evaluation is a positive record with no receipts -- it must
    # round-trip as a distinct, present fact (D-1).
    event = HeldOutGateEvaluated(
        run_id="run-9",
        ts=_ts(6),
        attempt_number=None,
        outcome="no_gate",
        reason="no held-out graders registered for this task",
    )
    payload = event_payload(event)
    assert payload == {
        "outcome": "no_gate",
        "reason": "no held-out graders registered for this task",
        "receipts": [],
    }
    restored = event_from_record(
        kind=event_kind(event),
        payload=payload,
        run_id=event.run_id,
        ts=event.ts,
        attempt_number=event.attempt_number,
        sequence=None,
        id=None,
    )
    assert isinstance(restored, HeldOutGateEvaluated)
    assert restored == event
    assert restored.receipts == ()

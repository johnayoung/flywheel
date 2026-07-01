"""The bounded dangling-prerequisite re-driver (spec 00069, criteria #7/#8/#13).

A task whose declared prerequisite resolves to no work item this pass stays out
of the ready set -- it is never dispatched against an unsatisfied prereq --
exactly as before. This re-driver turns that permanent dead-end into a bounded
one:

* #7 -- when the missing prerequisite later appears in the work source, the
  scheduler's per-pass graph rebuild resolves the edge, the once-dangling task
  becomes eligible, and it is driven. The re-driver never keeps it permanently
  ineligible.
* #8 -- when the prerequisite stays missing past a fixed bound of cycles, the
  referencing task is routed ONCE to the single human-review queue with the
  machine-readable ``prerequisite-missing`` reason and the missing prerequisite
  id named in the detail, and it is never dispatched.
* #13 -- the path is bounded: exactly ``bound`` dangling witnesses then exactly
  one queue entry; a prerequisite that never appears produces neither an
  infinite ineligible spin nor a growing pile of queue entries.

The direct-unit cases drive ``redrive_missing_prerequisites`` with a frozen clock
over an ``SqliteClaimStore``; the integration cases drive the real ``orchestrate``
loop against a mutable directory source so the graph rebuild and eligibility are
genuine. Nothing about lifecycle state is forged -- the re-driver only appends
ledger rows (criterion #14).
"""

from __future__ import annotations

import asyncio
import io
import json
from datetime import datetime, timezone
from pathlib import Path

from claude_agent_sdk import AssistantMessage, ResultMessage, TextBlock

from flywheel_core import (
    Intent,
    InvocationRequest,
    InvocationSignals,
    IterationResult,
    Status,
    ValidEnvelope,
)
from flywheel_core.store_sqlite import SqliteStore
from flywheel_orchestrator import (
    DEFAULT_PREREQ_REDRIVE_BOUND,
    REASON_PREREQUISITE_MISSING,
    GraphValidationIssue,
    SqliteClaimStore,
    orchestrate,
    redrive_missing_prerequisites,
)
from flywheel_orchestrator._claims import STOP_DANGLING_PREREQUISITE

_BASE = datetime(2026, 7, 1, 12, 0, 0, tzinfo=timezone.utc)


# --- helpers ----------------------------------------------------------------


def _frozen(at: datetime):
    def _now() -> datetime:
        return at

    return _now


def _issue(referencing_id: str, missing_id: str) -> GraphValidationIssue:
    return GraphValidationIssue(
        referencing_id=referencing_id, missing_id=missing_id
    )


def _dangling_witnesses(
    claims: SqliteClaimStore, referencing_id: str
) -> list:
    return [
        e
        for e in claims.list_subject_stop_events(referencing_id)
        if e.kind == STOP_DANGLING_PREREQUISITE
    ]


def _write_task(
    phase: Path,
    task_id: str,
    *,
    prerequisites: list[str] | None = None,
    grader_run: str = "true",
) -> None:
    phase.mkdir(parents=True, exist_ok=True)
    payload: dict[str, object] = {
        "id": task_id,
        "goal": f"Goal for {task_id}.",
        "graders": [{"type": "command", "run": grader_run}],
    }
    if prerequisites:
        payload["prerequisites"] = prerequisites
    (phase / f"{task_id}.json").write_text(json.dumps(payload))


def _signals() -> InvocationSignals:
    return InvocationSignals(
        stop_reason="end_turn",
        num_turns=1,
        total_cost_usd=0.0,
        result_is_error=False,
        result_subtype="success",
        api_error_status=None,
        session_id="sess",
    )


def _messages() -> tuple[object, ...]:
    return (
        AssistantMessage(
            content=[TextBlock(text="done")],
            model="claude-test",
            stop_reason="end_turn",
            session_id="sess",
            usage=None,
        ),
        ResultMessage(
            subtype="success",
            duration_ms=1,
            duration_api_ms=1,
            is_error=False,
            num_turns=1,
            session_id="sess",
            stop_reason="end_turn",
            total_cost_usd=0.0,
            usage=None,
        ),
    )


def _always_verify():
    async def _invoke(request: InvocationRequest) -> IterationResult:
        return IterationResult(
            transcript="ok",
            messages=_messages(),  # type: ignore[arg-type]
            envelope=ValidEnvelope(intent=Intent.VERIFY),
            signals=_signals(),
            failure=None,
        )

    return _invoke


def _orchestrate(tmp_path: Path, *, bound: int = DEFAULT_PREREQ_REDRIVE_BOUND):
    # A single loop-to-quiescence pass over a persistent store (one file across
    # calls), so successive invocations accumulate cycles on the same ledger.
    return asyncio.run(
        orchestrate(
            tasks_dir=tmp_path / "tasks",
            db_path=tmp_path / "flywheel.sqlite",
            sandbox_root=tmp_path / "sandboxes",
            invoke=_always_verify(),
            max_retries=0,
            max_turns=4,
            prereq_redrive_bound=bound,
            stream=io.StringIO(),
        )
    )


# --- #8/#13: bounded routing (direct unit) ----------------------------------


def test_below_bound_waits_and_never_queues(tmp_path: Path) -> None:
    """While the prerequisite is missing but the bound is not yet reached, the
    re-driver records a witness and waits -- it never queues and never
    dispatches (criterion #8, the ineligible-but-not-yet-given-up state)."""
    claims = SqliteClaimStore(tmp_path / "c.db")
    try:
        for cycle in range(2):
            outcomes = redrive_missing_prerequisites(
                claims,
                issues=[_issue("B", "A")],
                bound=3,
                now=_frozen(_BASE),
            )
            assert [o.result for o in outcomes] == ["waiting"]
            assert outcomes[0].referencing_id == "B"
            assert outcomes[0].missing_id == "A"
            assert outcomes[0].cycles == cycle + 1
        # Two cycles missing, bound 3: two witnesses, still no queue entry.
        assert len(_dangling_witnesses(claims, "B")) == 2
        assert claims.list_human_review_queue() == []
    finally:
        claims.close()


def test_missing_past_bound_routes_to_queue_naming_prereq(
    tmp_path: Path,
) -> None:
    """When the prerequisite stays missing through the bound, the referencing
    task is routed to the human-review queue with ``prerequisite-missing`` and
    the missing prerequisite id named in the detail (criterion #8)."""
    claims = SqliteClaimStore(tmp_path / "c.db")
    try:
        results: list[str] = []
        for _ in range(3):
            outcomes = redrive_missing_prerequisites(
                claims,
                issues=[_issue("B", "A")],
                bound=3,
                now=_frozen(_BASE),
            )
            results.append(outcomes[0].result)
        assert results == ["waiting", "waiting", "queued"]

        queue = claims.list_human_review_queue()
        assert len(queue) == 1
        entry = queue[0]
        assert entry.reason == REASON_PREREQUISITE_MISSING
        assert entry.task_id == "B"
        assert entry.run_id == ""  # a missing prereq has no run yet
        # The machine-readable reason plus a detail naming the missing id.
        assert "'A'" in entry.detail
    finally:
        claims.close()


def test_bounded_exactly_one_queue_entry_no_infinite_spin(
    tmp_path: Path,
) -> None:
    """A prerequisite that never appears costs exactly ``bound`` witnesses and
    exactly one queue entry -- the terminal guard stops further witnessing and
    re-queuing, so there is no infinite ineligible spin (criteria #8/#13)."""
    claims = SqliteClaimStore(tmp_path / "c.db")
    try:
        bound = 2
        # Pump well past the bound.
        for _ in range(6):
            redrive_missing_prerequisites(
                claims,
                issues=[_issue("B", "A")],
                bound=bound,
                now=_frozen(_BASE),
            )
        # Exactly ``bound`` witnesses -- no further witnessing after routing.
        assert len(_dangling_witnesses(claims, "B")) == bound
        # Exactly one queue entry -- never re-queued.
        queue = claims.list_human_review_queue()
        assert len(queue) == 1
        assert queue[0].reason == REASON_PREREQUISITE_MISSING
        # An already-queued edge reports its terminal state, not a fresh route.
        outcomes = redrive_missing_prerequisites(
            claims,
            issues=[_issue("B", "A")],
            bound=bound,
            now=_frozen(_BASE),
        )
        assert [o.result for o in outcomes] == ["queued"]
        assert len(claims.list_human_review_queue()) == 1
    finally:
        claims.close()


def test_distinct_missing_prereqs_bounded_independently(
    tmp_path: Path,
) -> None:
    """A task with two distinct dangling prerequisites bounds each edge on its
    own count and names each missing id in its own queue entry."""
    claims = SqliteClaimStore(tmp_path / "c.db")
    try:
        for _ in range(2):
            redrive_missing_prerequisites(
                claims,
                issues=[_issue("B", "A"), _issue("B", "A2")],
                bound=2,
                now=_frozen(_BASE),
            )
        queue = claims.list_human_review_queue()
        assert {e.reason for e in queue} == {REASON_PREREQUISITE_MISSING}
        named = sorted(
            missing
            for missing in ("A", "A2")
            for e in queue
            if repr(missing) in e.detail
        )
        assert named == ["A", "A2"]
        assert len(queue) == 2
    finally:
        claims.close()


def test_prereq_appearing_before_bound_never_queues(tmp_path: Path) -> None:
    """If the prerequisite appears before the bound (no issue is produced for
    it), the re-driver is simply not called for that edge, so it is never
    queued -- the bound is only reached by a genuinely absent prerequisite
    (criterion #7's guard against a false give-up)."""
    claims = SqliteClaimStore(tmp_path / "c.db")
    try:
        # Two dangling cycles under bound 3...
        for _ in range(2):
            redrive_missing_prerequisites(
                claims,
                issues=[_issue("B", "A")],
                bound=3,
                now=_frozen(_BASE),
            )
        # ...then the prerequisite appears: the edge resolves, no issue this
        # pass, so the re-driver sees nothing for B.
        outcomes = redrive_missing_prerequisites(
            claims, issues=[], bound=3, now=_frozen(_BASE)
        )
        assert outcomes == ()
        assert claims.list_human_review_queue() == []
        # No third witness was appended for the resolved edge.
        assert len(_dangling_witnesses(claims, "B")) == 2
    finally:
        claims.close()


# --- #7: re-drive when the prerequisite appears (integration) ---------------


def test_once_dangling_task_is_driven_when_prereq_appears(
    tmp_path: Path,
) -> None:
    """A task referencing an absent prerequisite is not driven while the
    prerequisite is missing; once the prerequisite appears in the source it
    becomes eligible and is driven (criterion #7) -- never permanently
    ineligible."""
    phase = tmp_path / "tasks" / "active" / "01-phase"
    # B requires A, which does not exist yet.
    _write_task(phase, "B", prerequisites=["A"])

    first = _orchestrate(tmp_path)
    # B stays out of the ready set: nothing dispatched, nothing queued yet.
    assert first.runs == ()
    claims = SqliteClaimStore(tmp_path / "flywheel.sqlite")
    try:
        assert claims.list_human_review_queue() == []
        assert len(_dangling_witnesses(claims, "B")) == 1
    finally:
        claims.close()

    # A now appears in the work source.
    _write_task(phase, "A")

    second = _orchestrate(tmp_path)
    driven = [r.task_id for r in second.runs]
    # A runs first (no prereqs), then the once-dangling B becomes eligible.
    assert driven == ["A", "B"]
    assert all(r.status is Status.DONE for r in second.runs)

    control = SqliteStore(tmp_path / "flywheel.sqlite")
    claims = SqliteClaimStore(tmp_path / "flywheel.sqlite")
    try:
        b_lc = control.list_lifecycles()
        assert any(
            lc.task_id == "B" and lc.status is Status.DONE for lc in b_lc
        )
        # B was never routed to the queue -- it recovered, it did not give up.
        assert claims.list_human_review_queue() == []
    finally:
        control.close()
        claims.close()


# --- #8: prereq missing past the bound in the real loop ---------------------


def test_loop_routes_to_queue_and_never_dispatches_past_bound(
    tmp_path: Path,
) -> None:
    """In the real orchestrate loop, a prerequisite that stays missing past the
    bound routes the referencing task to the human-review queue naming the
    missing id, and the task is never dispatched -- across every cycle (criteria
    #8/#13)."""
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "B", prerequisites=["A"])

    bound = 2
    all_driven: list[str] = []
    # Run more than ``bound`` cycles; A never appears.
    for _ in range(bound + 2):
        report = _orchestrate(tmp_path, bound=bound)
        all_driven.extend(r.task_id for r in report.runs)

    # B was never dispatched against its unsatisfied prerequisite.
    assert "B" not in all_driven
    assert all_driven == []

    claims = SqliteClaimStore(tmp_path / "flywheel.sqlite")
    try:
        # Exactly ``bound`` witnesses then exactly one queue entry (bounded).
        assert len(_dangling_witnesses(claims, "B")) == bound
        queue = claims.list_human_review_queue()
        assert len(queue) == 1
        assert queue[0].reason == REASON_PREREQUISITE_MISSING
        assert queue[0].task_id == "B"
        assert "'A'" in queue[0].detail
    finally:
        claims.close()

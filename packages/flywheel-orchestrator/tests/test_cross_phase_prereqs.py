"""Cross-phase prerequisite scheduling: a prerequisite absent from the active
work-source listing but carrying a ``DONE`` lifecycle in the store is satisfied.

The regression: when a prerequisite's phase archives, its task JSON moves out of
``.flywheel/tasks/active/`` and ``DirectoryWorkSource`` stops listing it -- yet
its ``DONE`` lifecycle stays in the store. Before this change the scheduler
treated the (now unlisted) prerequisite as *dangling*: the dependent stayed out
of the ready set, a ``dangling-prerequisite`` witness accrued, and the edge was
eventually routed to the human-review queue with ``prerequisite-missing``. The
operator workaround was copying the archived prerequisite JSON back.

The store is the authoritative record of completion (``docs/data-taxonomy.md``);
the listing is an input surface. These tests pin the new contract at three
layers:

* the pure eligibility predicates (``WorkGraph.ready_set`` and
  ``select_next_task``) honor a caller-supplied ``satisfied_prerequisites`` set;
* ``satisfied_prerequisites_from_store`` resolves exactly the unlisted ids with a
  ``DONE`` lifecycle -- a ``FAILED`` id never satisfies, and a listed id is never
  consulted;
* the real ``orchestrate`` loop dispatches a dependent whose only unlisted
  prerequisite is store-``DONE`` (recording no dangling witness and no
  ``prerequisite-missing`` queue entry), keeps a dependent whose unlisted
  prerequisite only ``FAILED`` ineligible and drives it down the bounded
  dangling path, and self-heals a dependent already routed to the queue once the
  store resolves the edge.
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
from flywheel_core.lifecycle import Lifecycle
from flywheel_core.store_sqlite import SqliteStore
from flywheel_orchestrator import (
    DEFAULT_PREREQ_REDRIVE_BOUND,
    REASON_PREREQUISITE_MISSING,
    SqliteClaimStore,
    WorkGraph,
    orchestrate,
)
from flywheel_orchestrator._claims import STOP_DANGLING_PREREQUISITE
from flywheel_orchestrator._sources import DirectoryWorkSource
from flywheel_orchestrator._workflow import (
    TaskState,
    build_status_rows,
    satisfied_prerequisites_from_store,
    select_next_task,
)

_BASE = datetime(2026, 7, 1, 12, 0, 0, tzinfo=timezone.utc)


# --- fixtures / helpers -----------------------------------------------------


def _write_task(
    phase: Path,
    task_id: str,
    *,
    prerequisites: list[str] | None = None,
    grader_run: str = "true",
) -> Path:
    phase.mkdir(parents=True, exist_ok=True)
    payload: dict[str, object] = {
        "id": task_id,
        "goal": f"Goal for {task_id}.",
        "graders": [{"type": "command", "run": grader_run}],
    }
    if prerequisites:
        payload["prerequisites"] = prerequisites
    path = phase / f"{task_id}.json"
    path.write_text(json.dumps(payload))
    return path


def _seed_done(store: SqliteStore, task_id: str) -> None:
    """Persist a lifecycle for ``task_id`` ending in DONE (an unlisted,
    already-completed prerequisite, e.g. one whose phase archived)."""
    now = datetime.now(timezone.utc)
    lc = Lifecycle(task_id=task_id, run_id=f"run-{task_id}-ok")
    lc.transition_to(Status.READY, now=now)
    lc.transition_to(Status.RUNNING, now=now)
    lc.transition_to(Status.VALIDATING, now=now)
    lc.transition_to(Status.DONE, now=now)
    store.create_lifecycle(lc)


def _seed_failed(store: SqliteStore, task_id: str) -> None:
    """Persist a lifecycle for ``task_id`` ending in FAILED -- a non-DONE
    lifecycle that must NOT satisfy a dependent's prerequisite."""
    now = datetime.now(timezone.utc)
    lc = Lifecycle(task_id=task_id, run_id=f"run-{task_id}-fail")
    lc.transition_to(Status.READY, now=now)
    lc.transition_to(Status.RUNNING, now=now)
    lc.transition_to(Status.FAILED, error="boom", now=now)
    store.create_lifecycle(lc)


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
    """One loop-to-quiescence pass over a persistent store (one sqlite file
    across calls), mirroring ``test_redriver_prereq``'s integration harness."""
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


def _dangling_witnesses(claims: SqliteClaimStore, referencing_id: str) -> list:
    return [
        e
        for e in claims.list_subject_stop_events(referencing_id)
        if e.kind == STOP_DANGLING_PREREQUISITE
    ]


def _prereq_missing_queue_entries(
    claims: SqliteClaimStore, task_id: str
) -> list:
    return [
        entry
        for entry in claims.list_human_review_queue()
        if entry.task_id == task_id
        and entry.reason == REASON_PREREQUISITE_MISSING
    ]


def _build_graph(tasks_dir: Path) -> WorkGraph:
    items = list(DirectoryWorkSource(tasks_dir).list_work())
    return WorkGraph.build(items).graph


# --- pure predicate: WorkGraph.ready_set ------------------------------------


def test_ready_set_store_done_prereq_makes_unlisted_dependent_runnable(
    tmp_path: Path,
) -> None:
    """``ready_set`` treats a declared prerequisite absent from the listing as
    satisfied iff its id is in ``satisfied_prerequisites`` (the caller's
    store-DONE set). Without it the dependent dangles; with it the dependent is
    runnable. Only a DONE id belongs in that set -- a bare 'any state' id does
    not, which the failed-case integration test below exercises end to end."""
    phase = tmp_path / "tasks" / "active" / "01-phase"
    # Only B is listed; its declared prerequisite A resolves to no node.
    _write_task(phase, "B", prerequisites=["A"])
    graph = _build_graph(tmp_path / "tasks")
    states = {"B": TaskState.FRESH}

    # Default (empty satisfied set) -> A dangles -> B is not runnable.
    assert graph.ready_set(states) == ()
    # A supplied as store-satisfied -> B becomes runnable.
    ready = graph.ready_set(
        states, satisfied_prerequisites=frozenset({"A"})
    )
    assert [item.task.id for item in ready] == ["B"]
    # A satisfied set that does NOT name A leaves B dangling (only the exact
    # missing id satisfies, never a blanket bypass).
    assert graph.ready_set(
        states, satisfied_prerequisites=frozenset({"other"})
    ) == ()


def test_ready_set_fully_listed_graph_is_byte_identical(
    tmp_path: Path,
) -> None:
    """When the prerequisite is listed and DONE, eligibility is unchanged and
    passing ``satisfied_prerequisites`` never alters the result (the fully-listed
    path stays byte-identical to today)."""
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "A")
    _write_task(phase, "B", prerequisites=["A"])
    graph = _build_graph(tmp_path / "tasks")
    states = {"A": TaskState.DONE, "B": TaskState.FRESH}

    baseline = [item.task.id for item in graph.ready_set(states)]
    with_set = [
        item.task.id
        for item in graph.ready_set(
            states, satisfied_prerequisites=frozenset({"A"})
        )
    ]
    assert baseline == ["B"]
    assert with_set == baseline


# --- pure predicate: select_next_task ---------------------------------------


def test_select_next_task_honors_store_done_prereq(tmp_path: Path) -> None:
    """``select_next_task`` withholds a dependent whose unlisted prerequisite is
    unresolved, but selects it when the missing id is supplied as store-DONE."""
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "B", prerequisites=["A"])
    store = SqliteStore(tmp_path / "unit.sqlite")
    try:
        rows = build_status_rows(tmp_path / "tasks", store)
        # A missing from the workspace -> B ineligible by default.
        assert select_next_task(rows) is None
        # A supplied as store-satisfied -> B selected.
        pick = select_next_task(
            rows, satisfied_prerequisites=frozenset({"A"})
        )
        assert pick is not None and pick.task.id == "B"
    finally:
        store.close()


# --- helper: satisfied_prerequisites_from_store discrimination --------------


def test_satisfied_from_store_returns_only_unlisted_done_ids(
    tmp_path: Path,
) -> None:
    """The helper resolves an unlisted prerequisite iff it has a DONE lifecycle:
    a DONE id is returned, a FAILED id is not (the discrimination screen)."""
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "B", prerequisites=["A"])

    done_store = SqliteStore(tmp_path / "done.sqlite")
    try:
        _seed_done(done_store, "A")
        rows = build_status_rows(tmp_path / "tasks", done_store)
        assert satisfied_prerequisites_from_store(rows, done_store) == (
            frozenset({"A"})
        )
    finally:
        done_store.close()

    failed_store = SqliteStore(tmp_path / "failed.sqlite")
    try:
        _seed_failed(failed_store, "A")
        rows = build_status_rows(tmp_path / "tasks", failed_store)
        # Only a DONE lifecycle satisfies -- FAILED never does.
        assert satisfied_prerequisites_from_store(rows, failed_store) == (
            frozenset()
        )
    finally:
        failed_store.close()


def test_satisfied_from_store_never_consults_a_listed_prereq(
    tmp_path: Path,
) -> None:
    """A prerequisite present in the listing is resolved off its row state, not
    the store, so the helper returns nothing for it even when it is DONE -- no
    per-listed-task store read is implied, and the fully-listed graph is
    unaffected."""
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "A")
    _write_task(phase, "B", prerequisites=["A"])
    store = SqliteStore(tmp_path / "unit.sqlite")
    try:
        _seed_done(store, "A")
        rows = build_status_rows(tmp_path / "tasks", store)
        # A is listed -> not a candidate for store consultation.
        assert satisfied_prerequisites_from_store(rows, store) == frozenset()
        # And selection still works via A's DONE row state (byte-identical).
        pick = select_next_task(rows)
        assert pick is not None and pick.task.id == "B"
    finally:
        store.close()


# --- integration: the real orchestrate loop ---------------------------------


def test_loop_dispatches_dependent_when_prereq_unlisted_but_store_done(
    tmp_path: Path,
) -> None:
    """A prerequisite that ran to DONE and then left the listing (its JSON
    removed, as archiving does) still satisfies its dependent: the dependent is
    dispatched, and the claim store records NO dangling witness and NO
    prerequisite-missing queue entry for that edge."""
    phase = tmp_path / "tasks" / "active" / "01-phase"
    # A alone runs first and reaches DONE in the store.
    a_file = _write_task(phase, "A")
    first = _orchestrate(tmp_path)
    assert [r.task_id for r in first.runs] == ["A"]
    assert all(r.status is Status.DONE for r in first.runs)

    # A's phase "archives": its JSON leaves the listing while its DONE lifecycle
    # stays in the store. B, which requires A, now appears.
    a_file.unlink()
    _write_task(phase, "B", prerequisites=["A"])

    second = _orchestrate(tmp_path)
    # B is dispatched off the store-satisfied prerequisite and completes.
    assert [r.task_id for r in second.runs] == ["B"]
    assert all(r.status is Status.DONE for r in second.runs)

    control = SqliteStore(tmp_path / "flywheel.sqlite")
    claims = SqliteClaimStore(tmp_path / "flywheel.sqlite")
    try:
        assert any(
            lc.task_id == "B" and lc.status is Status.DONE
            for lc in control.list_lifecycles()
        )
        # The store-DONE edge never reached the dangling re-driver.
        assert _dangling_witnesses(claims, "B") == []
        assert _prereq_missing_queue_entries(claims, "B") == []
    finally:
        control.close()
        claims.close()


def test_loop_keeps_dependent_ineligible_when_unlisted_prereq_only_failed(
    tmp_path: Path,
) -> None:
    """A prerequisite that is unlisted and has only a FAILED lifecycle does NOT
    satisfy: the dependent stays out of the ready set and the bounded
    dangling-prerequisite path still applies -- witnesses accrue and, past the
    bound, the edge routes once to the human-review queue naming the missing
    id. The dependent is never dispatched."""
    phase = tmp_path / "tasks" / "active" / "01-phase"
    bound = 2
    # A fails first (grader returns non-zero) and is left FAILED in the store.
    a_file = _write_task(phase, "A", grader_run="false")
    first = _orchestrate(tmp_path, bound=bound)
    assert [r.task_id for r in first.runs] == ["A"]
    assert all(r.status is Status.FAILED for r in first.runs)

    # A leaves the listing (only its FAILED lifecycle remains); B requires A.
    a_file.unlink()
    _write_task(phase, "B", prerequisites=["A"])

    driven: list[str] = list(r.task_id for r in first.runs)
    for _ in range(bound + 1):
        report = _orchestrate(tmp_path, bound=bound)
        driven.extend(r.task_id for r in report.runs)

    # B was never dispatched against its unsatisfied (failed) prerequisite.
    assert "B" not in driven

    claims = SqliteClaimStore(tmp_path / "flywheel.sqlite")
    try:
        # Exactly ``bound`` witnesses then exactly one queue entry (bounded).
        assert len(_dangling_witnesses(claims, "B")) == bound
        queue = _prereq_missing_queue_entries(claims, "B")
        assert len(queue) == 1
        assert queue[0].reason == REASON_PREREQUISITE_MISSING
        assert "'A'" in queue[0].detail
    finally:
        claims.close()


def test_queued_dependent_self_heals_when_store_resolves_prereq(
    tmp_path: Path,
) -> None:
    """A dependent already routed to the human-review queue with
    prerequisite-missing self-heals: nothing reads the queue for scheduling
    exclusion, so once the store resolves the edge (the prerequisite's DONE
    lifecycle appears while it stays unlisted) the next pass selects and drives
    the dependent."""
    phase = tmp_path / "tasks" / "active" / "01-phase"
    bound = 2
    # B requires A; A is entirely absent, so B is routed to the queue.
    _write_task(phase, "B", prerequisites=["A"])
    for _ in range(bound + 1):
        report = _orchestrate(tmp_path, bound=bound)
        assert report.runs == ()  # B never dispatched while A dangles

    claims = SqliteClaimStore(tmp_path / "flywheel.sqlite")
    try:
        assert len(_prereq_missing_queue_entries(claims, "B")) == 1
    finally:
        claims.close()

    # A's DONE lifecycle now appears in the store while A stays unlisted (its
    # phase archived elsewhere). B must self-heal despite its queue entry.
    seed = SqliteStore(tmp_path / "flywheel.sqlite")
    try:
        _seed_done(seed, "A")
    finally:
        seed.close()

    report = _orchestrate(tmp_path, bound=bound)
    assert [r.task_id for r in report.runs] == ["B"]
    assert all(r.status is Status.DONE for r in report.runs)

    control = SqliteStore(tmp_path / "flywheel.sqlite")
    claims = SqliteClaimStore(tmp_path / "flywheel.sqlite")
    try:
        assert any(
            lc.task_id == "B" and lc.status is Status.DONE
            for lc in control.list_lifecycles()
        )
        # No further dangling witness was appended once the edge resolved
        # (exactly the bound recorded before self-heal, none after).
        assert len(_dangling_witnesses(claims, "B")) == bound
    finally:
        control.close()
        claims.close()


# --- cross-store continuity: archived task files satisfy (bug 3) ------------


def test_archived_task_file_satisfies_prereq_absent_from_store(
    tmp_path: Path,
) -> None:
    """A prerequisite completed under an earlier store backend (pre-flip
    sqlite) has no lifecycle row in the policy-selected store -- but its
    ARCHIVED task file is durable proof of verified completion (a phase
    archives only past the landed predicate and exit gates), so the
    dependent must schedule instead of dead-ending at prerequisite-missing.
    """
    tasks_dir = tmp_path / "tasks"
    _write_task(
        tasks_dir / "active" / "02-next",
        "dependent",
        prerequisites=["shipped-under-sqlite"],
    )
    # The prerequisite's phase archived under the OLD store regime; only the
    # archived task JSON survives the store cutover.
    _write_task(tasks_dir / "archive" / "01-shipped", "shipped-under-sqlite")

    fresh_store = SqliteStore(tmp_path / "post-flip.sqlite")
    try:
        rows = build_status_rows(tasks_dir, fresh_store)
        satisfied = satisfied_prerequisites_from_store(rows, fresh_store)
    finally:
        fresh_store.close()

    assert satisfied == frozenset({"shipped-under-sqlite"})


def test_prereq_absent_from_store_and_archive_stays_unsatisfied(
    tmp_path: Path,
) -> None:
    """Fail closed: no lifecycle row AND no archived task file means the
    prerequisite is genuinely missing, never blessed."""
    tasks_dir = tmp_path / "tasks"
    _write_task(
        tasks_dir / "active" / "02-next",
        "dependent",
        prerequisites=["never-ran"],
    )
    (tasks_dir / "archive").mkdir(parents=True)

    fresh_store = SqliteStore(tmp_path / "post-flip.sqlite")
    try:
        rows = build_status_rows(tasks_dir, fresh_store)
        satisfied = satisfied_prerequisites_from_store(rows, fresh_store)
    finally:
        fresh_store.close()

    assert satisfied == frozenset()


def test_dependent_dispatches_when_prereq_only_in_archive(
    tmp_path: Path,
) -> None:
    """End to end: the loop runs the dependent to DONE off the archived-file
    satisfaction alone -- no lifecycle row for the prerequisite exists in the
    (fresh, post-cutover) store the loop opens."""
    tasks_dir = tmp_path / "tasks"
    _write_task(
        tasks_dir / "active" / "02-next",
        "dependent",
        prerequisites=["shipped-under-sqlite"],
    )
    _write_task(tasks_dir / "archive" / "01-shipped", "shipped-under-sqlite")

    report = _orchestrate(tmp_path)
    assert [r.task_id for r in report.runs] == ["dependent"]
    assert all(r.status is Status.DONE for r in report.runs)

    store = SqliteStore(tmp_path / "flywheel.sqlite")
    claims = SqliteClaimStore(tmp_path / "flywheel.sqlite")
    try:
        rows = build_status_rows(tasks_dir, store)
        assert [(r.task.id, r.state) for r in rows] == [
            ("dependent", TaskState.DONE)
        ]
        assert _prereq_missing_queue_entries(claims, "dependent") == []
    finally:
        claims.close()
        store.close()

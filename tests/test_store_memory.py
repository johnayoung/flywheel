"""Contract tests for ``flywheel.store_memory.InMemoryStore``.

Exercises every Protocol round-trip (lifecycle, attempt, event,
grader-result, session), enforces optimistic concurrency on
``Lifecycle.version``, and pins down the append-only contract on
``grader_results``.
"""

from __future__ import annotations

from datetime import datetime, timezone

import pytest

from flywheel import (
    AgentSessionStore,
    Attempt,
    AttemptStore,
    ClaudeSessionEntry,
    EventRecord,
    EventStore,
    GraderResultRecord,
    GraderResultStore,
    InMemoryStore,
    Lifecycle,
    LifecycleAlreadyExistsError,
    LifecycleNotFoundError,
    LifecycleStore,
    OptimisticConcurrencyError,
    Outcome,
    Status,
)


# --- Protocol satisfaction -------------------------------------------------


def test_in_memory_store_satisfies_every_protocol() -> None:
    s = InMemoryStore()
    assert isinstance(s, LifecycleStore)
    assert isinstance(s, AttemptStore)
    assert isinstance(s, EventStore)
    assert isinstance(s, GraderResultStore)
    assert isinstance(s, AgentSessionStore)


def test_in_memory_store_exposes_no_grader_result_mutators() -> None:
    s = InMemoryStore()
    for forbidden in (
        "update_grader_result",
        "delete_grader_result",
        "remove_grader_result",
        "set_grader_result",
        "replace_grader_result",
    ):
        assert not hasattr(s, forbidden), (
            f"InMemoryStore must be append-only on grader_results; "
            f"exposes {forbidden!r}"
        )


# --- Lifecycle round-trip & concurrency ------------------------------------


def test_load_missing_lifecycle_returns_none() -> None:
    s = InMemoryStore()
    assert s.load_lifecycle("nope") is None


def test_create_then_load_lifecycle_round_trips() -> None:
    s = InMemoryStore()
    lc = Lifecycle(task_id="t", run_id="r1", worker_id="w1")
    lc.transition_to(Status.READY)  # version becomes 2
    s.create_lifecycle(lc)

    loaded = s.load_lifecycle("r1")
    assert loaded is not None
    assert loaded.task_id == "t"
    assert loaded.run_id == "r1"
    assert loaded.worker_id == "w1"
    assert loaded.status is Status.READY
    assert loaded.version == 2
    assert loaded.attempts == []


def test_create_lifecycle_twice_raises_already_exists() -> None:
    s = InMemoryStore()
    lc = Lifecycle(task_id="t", run_id="r1")
    s.create_lifecycle(lc)
    with pytest.raises(LifecycleAlreadyExistsError) as exc_info:
        s.create_lifecycle(lc)
    assert exc_info.value.run_id == "r1"


def test_update_missing_lifecycle_raises_not_found() -> None:
    s = InMemoryStore()
    lc = Lifecycle(task_id="t", run_id="r1", version=2)
    with pytest.raises(LifecycleNotFoundError) as exc_info:
        s.update_lifecycle(lc, expected_version=1)
    assert exc_info.value.run_id == "r1"


def test_update_lifecycle_with_matching_version_succeeds() -> None:
    s = InMemoryStore()
    lc = Lifecycle(task_id="t", run_id="r1")  # version=1
    s.create_lifecycle(lc)

    lc.transition_to(Status.READY)  # version=2
    s.update_lifecycle(lc, expected_version=1)

    loaded = s.load_lifecycle("r1")
    assert loaded is not None
    assert loaded.status is Status.READY
    assert loaded.version == 2


def test_update_lifecycle_with_stale_version_raises_conflict() -> None:
    s = InMemoryStore()
    lc = Lifecycle(task_id="t", run_id="r1")  # version=1
    s.create_lifecycle(lc)

    # First writer bumps to v2 successfully.
    lc.transition_to(Status.READY)
    s.update_lifecycle(lc, expected_version=1)

    # Second writer holds an old snapshot at v1 and tries to update.
    stale = Lifecycle(task_id="t", run_id="r1", status=Status.READY, version=2)
    with pytest.raises(OptimisticConcurrencyError) as exc_info:
        s.update_lifecycle(stale, expected_version=1)
    assert exc_info.value.run_id == "r1"
    assert exc_info.value.expected_version == 1
    assert exc_info.value.actual_version == 2

    # The store row is unchanged.
    loaded = s.load_lifecycle("r1")
    assert loaded is not None
    assert loaded.version == 2


def test_lifecycle_stored_row_is_isolated_from_caller_mutations() -> None:
    s = InMemoryStore()
    lc = Lifecycle(task_id="t", run_id="r1")
    s.create_lifecycle(lc)

    # Caller mutates their copy after save.
    lc.task_id = "MUTATED"
    lc.timestamps[Status.READY] = datetime(2024, 1, 1, tzinfo=timezone.utc)

    loaded = s.load_lifecycle("r1")
    assert loaded is not None
    assert loaded.task_id == "t"
    assert Status.READY not in loaded.timestamps

    # Mutating the loaded copy also does not corrupt the store.
    loaded.task_id = "ALSO_MUTATED"
    again = s.load_lifecycle("r1")
    assert again is not None
    assert again.task_id == "t"


def test_load_lifecycle_attaches_attempts_in_number_order() -> None:
    s = InMemoryStore()
    lc = Lifecycle(task_id="t", run_id="r1")
    s.create_lifecycle(lc)

    base = datetime(2024, 1, 1, tzinfo=timezone.utc)
    s.save_attempt("r1", Attempt(number=3, started_at=base, run_id="r1"))
    s.save_attempt("r1", Attempt(number=1, started_at=base, run_id="r1"))
    s.save_attempt("r1", Attempt(number=2, started_at=base, run_id="r1"))

    loaded = s.load_lifecycle("r1")
    assert loaded is not None
    assert [a.number for a in loaded.attempts] == [1, 2, 3]


# --- Attempt round-trip ----------------------------------------------------


def test_load_missing_attempt_returns_none() -> None:
    s = InMemoryStore()
    assert s.load_attempt("r1", 1) is None


def test_save_then_load_attempt_round_trips_with_agent_context() -> None:
    s = InMemoryStore()
    a = Attempt(
        number=1,
        started_at=datetime(2024, 1, 1, tzinfo=timezone.utc),
        run_id="r1",
        agent_context={"model_id": "claude-opus-4-7"},
    )
    s.save_attempt("r1", a)

    loaded = s.load_attempt("r1", 1)
    assert loaded is not None
    assert loaded.number == 1
    assert loaded.run_id == "r1"
    assert loaded.agent_context == {"model_id": "claude-opus-4-7"}


def test_save_attempt_is_upsert_keyed_by_run_id_and_number() -> None:
    s = InMemoryStore()
    start = datetime(2024, 1, 1, tzinfo=timezone.utc)
    end = datetime(2024, 1, 1, 0, 1, tzinfo=timezone.utc)
    s.save_attempt(
        "r1",
        Attempt(number=1, started_at=start, run_id="r1"),
    )
    s.save_attempt(
        "r1",
        Attempt(
            number=1,
            started_at=start,
            run_id="r1",
            ended_at=end,
            outcome=Outcome.SUCCEEDED,
        ),
    )

    loaded = s.load_attempt("r1", 1)
    assert loaded is not None
    assert loaded.ended_at == end
    assert loaded.outcome is Outcome.SUCCEEDED
    assert s.list_attempts("r1") == [loaded]


def test_list_attempts_returns_in_number_order_regardless_of_insertion() -> None:
    s = InMemoryStore()
    base = datetime(2024, 1, 1, tzinfo=timezone.utc)
    for n in (5, 2, 4, 1, 3):
        s.save_attempt("r1", Attempt(number=n, started_at=base, run_id="r1"))
    listed = s.list_attempts("r1")
    assert [a.number for a in listed] == [1, 2, 3, 4, 5]


def test_list_attempts_scoped_by_run_id() -> None:
    s = InMemoryStore()
    base = datetime(2024, 1, 1, tzinfo=timezone.utc)
    s.save_attempt("r1", Attempt(number=1, started_at=base, run_id="r1"))
    s.save_attempt("r2", Attempt(number=1, started_at=base, run_id="r2"))
    assert [a.run_id for a in s.list_attempts("r1")] == ["r1"]
    assert [a.run_id for a in s.list_attempts("r2")] == ["r2"]


def test_attempt_stored_row_isolated_from_caller_mutations() -> None:
    s = InMemoryStore()
    a = Attempt(
        number=1,
        started_at=datetime(2024, 1, 1, tzinfo=timezone.utc),
        run_id="r1",
        agent_context={"model_id": "x"},
    )
    s.save_attempt("r1", a)
    a.agent_context["model_id"] = "MUTATED"
    loaded = s.load_attempt("r1", 1)
    assert loaded is not None
    assert loaded.agent_context == {"model_id": "x"}


# --- Event round-trip ------------------------------------------------------


def test_append_event_assigns_monotonic_id_and_returns_record() -> None:
    s = InMemoryStore()
    ts = datetime(2024, 1, 1, tzinfo=timezone.utc)
    r1 = s.append_event(EventRecord(run_id="r1", ts=ts, kind="started"))
    r2 = s.append_event(EventRecord(run_id="r1", ts=ts, kind="completed"))
    assert r1.id is not None
    assert r2.id is not None
    assert r2.id > r1.id


def test_list_events_returns_chronological_order_regardless_of_insertion() -> None:
    s = InMemoryStore()
    later = datetime(2024, 1, 1, 0, 0, 10, tzinfo=timezone.utc)
    earlier = datetime(2024, 1, 1, 0, 0, 0, tzinfo=timezone.utc)
    middle = datetime(2024, 1, 1, 0, 0, 5, tzinfo=timezone.utc)
    s.append_event(EventRecord(run_id="r1", ts=later, kind="c"))
    s.append_event(EventRecord(run_id="r1", ts=earlier, kind="a"))
    s.append_event(EventRecord(run_id="r1", ts=middle, kind="b"))
    listed = s.list_events("r1")
    assert [e.kind for e in listed] == ["a", "b", "c"]


def test_list_events_scoped_by_run_id() -> None:
    s = InMemoryStore()
    ts = datetime(2024, 1, 1, tzinfo=timezone.utc)
    s.append_event(EventRecord(run_id="r1", ts=ts, kind="x"))
    s.append_event(EventRecord(run_id="r2", ts=ts, kind="y"))
    assert [e.kind for e in s.list_events("r1")] == ["x"]
    assert [e.kind for e in s.list_events("r2")] == ["y"]


def test_event_payload_isolated_from_caller_mutations() -> None:
    s = InMemoryStore()
    payload: dict[str, int] = {"turns": 1}
    s.append_event(
        EventRecord(
            run_id="r1",
            ts=datetime(2024, 1, 1, tzinfo=timezone.utc),
            kind="progress",
            payload=payload,
        )
    )
    payload["turns"] = 999
    listed = s.list_events("r1")
    assert dict(listed[0].payload) == {"turns": 1}


# --- Grader-result round-trip ----------------------------------------------


def _gr(
    *,
    run_id: str = "r1",
    attempt_number: int = 1,
    ordinal: int = 0,
    grader_type: str = "command",
    passed: bool = True,
) -> GraderResultRecord:
    return GraderResultRecord(
        run_id=run_id,
        attempt_number=attempt_number,
        ordinal=ordinal,
        grader_type=grader_type,  # type: ignore[arg-type]
        grader_spec={"type": grader_type, "run": "true"},
        passed=passed,
        duration_ms=1,
        payload={"exit_code": 0 if passed else 1},
        ts=datetime(2024, 1, 1, tzinfo=timezone.utc),
    )


def test_append_grader_result_assigns_monotonic_id() -> None:
    s = InMemoryStore()
    r1 = s.append_grader_result(_gr(ordinal=0))
    r2 = s.append_grader_result(_gr(ordinal=1, grader_type="rubric"))
    assert r1.id is not None and r2.id is not None
    assert r2.id > r1.id


def test_list_grader_results_returns_in_ordinal_order_scoped_by_attempt() -> None:
    s = InMemoryStore()
    s.append_grader_result(_gr(ordinal=2, grader_type="rubric"))
    s.append_grader_result(_gr(ordinal=0, grader_type="command"))
    s.append_grader_result(_gr(ordinal=1, grader_type="transcript"))
    # Different attempt — must not bleed into the queried list.
    s.append_grader_result(_gr(attempt_number=2, ordinal=0))

    listed = s.list_grader_results("r1", 1)
    assert [r.ordinal for r in listed] == [0, 1, 2]
    assert all(r.attempt_number == 1 for r in listed)


def test_list_grader_results_for_missing_attempt_returns_empty_list() -> None:
    s = InMemoryStore()
    assert s.list_grader_results("nope", 999) == []


def test_grader_spec_and_payload_isolated_from_caller_mutations() -> None:
    s = InMemoryStore()
    spec: dict[str, object] = {"type": "command", "run": "true"}
    payload: dict[str, object] = {"exit_code": 0}
    s.append_grader_result(
        GraderResultRecord(
            run_id="r1",
            attempt_number=1,
            ordinal=0,
            grader_type="command",
            grader_spec=spec,
            passed=True,
            duration_ms=1,
            payload=payload,
            ts=datetime(2024, 1, 1, tzinfo=timezone.utc),
        )
    )
    spec["run"] = "MUTATED"
    payload["exit_code"] = 1
    listed = s.list_grader_results("r1", 1)
    assert dict(listed[0].grader_spec) == {"type": "command", "run": "true"}
    assert dict(listed[0].payload) == {"exit_code": 0}


# --- Session round-trip ----------------------------------------------------


def test_append_session_entry_assigns_monotonic_seq() -> None:
    s = InMemoryStore()
    e1 = s.append_session_entry(
        ClaudeSessionEntry(
            project_key="proj",
            session_id="sess",
            entry="a",
            mtime=1,
        )
    )
    e2 = s.append_session_entry(
        ClaudeSessionEntry(
            project_key="proj",
            session_id="sess",
            entry="b",
            mtime=2,
        )
    )
    assert e1.seq is not None and e2.seq is not None
    assert e2.seq > e1.seq


def test_list_session_entries_filters_by_subpath_and_orders_by_seq() -> None:
    s = InMemoryStore()
    s.append_session_entry(
        ClaudeSessionEntry(
            project_key="proj", session_id="sess", entry="main-1", mtime=1
        )
    )
    s.append_session_entry(
        ClaudeSessionEntry(
            project_key="proj",
            session_id="sess",
            entry="sub-1",
            mtime=2,
            subpath="agent-a",
        )
    )
    s.append_session_entry(
        ClaudeSessionEntry(
            project_key="proj", session_id="sess", entry="main-2", mtime=3
        )
    )

    main = s.list_session_entries("proj", "sess")
    sub = s.list_session_entries("proj", "sess", subpath="agent-a")
    assert [e.entry for e in main] == ["main-1", "main-2"]
    assert [e.entry for e in sub] == ["sub-1"]


def test_list_session_entries_scoped_by_project_and_session() -> None:
    s = InMemoryStore()
    s.append_session_entry(
        ClaudeSessionEntry(
            project_key="p1", session_id="s1", entry="x", mtime=1
        )
    )
    s.append_session_entry(
        ClaudeSessionEntry(
            project_key="p2", session_id="s1", entry="y", mtime=1
        )
    )
    s.append_session_entry(
        ClaudeSessionEntry(
            project_key="p1", session_id="s2", entry="z", mtime=1
        )
    )
    assert [e.entry for e in s.list_session_entries("p1", "s1")] == ["x"]
    assert [e.entry for e in s.list_session_entries("p2", "s1")] == ["y"]
    assert [e.entry for e in s.list_session_entries("p1", "s2")] == ["z"]


# --- Independent store instances do not share state -----------------------


def test_two_in_memory_stores_have_independent_state() -> None:
    s1 = InMemoryStore()
    s2 = InMemoryStore()
    s1.create_lifecycle(Lifecycle(task_id="t", run_id="shared"))
    assert s1.load_lifecycle("shared") is not None
    assert s2.load_lifecycle("shared") is None

"""Shared contract tests for every store backend.

Parameterized over ``InMemoryStore`` and ``SqliteStore`` to demonstrate
behavioral parity between the test substrate and the durable backend
(constraint from ``roadmap-04-store-sqlite``). A test that lives here is
a statement about the protocol surface, not about a particular
implementation; backend-specific tests live in their own files
(``test_store_memory.py``, ``test_store_sqlite.py``).

Fixture quirks:

* ``store`` yields a freshly bootstrapped store. The SQLite variant is
  file-backed (in ``tmp_path``) so the same DB can be reopened by
  follow-up tests when needed.
* Attempt/event/grader-result tests pre-bootstrap a lifecycle (and, for
  grader_results, an attempt). The SQLite schema declares FKs that
  require parent rows to exist; the in-memory store does not enforce
  FKs but happily accepts the same call shape.
"""

from __future__ import annotations

import importlib.util
from collections.abc import Callable, Iterator
from datetime import datetime, timezone
from pathlib import Path
from uuid import uuid4

import pytest

from flywheel import (
    AgentSessionStore,
    Attempt,
    AttemptFinalized,
    AttemptStarted,
    AttemptStore,
    AuditStore,
    ClaudeSessionEntry,
    DomainEventStore,
    EventRecord,
    EventStore,
    GraderEvaluated,
    GraderResultRecord,
    GraderResultStore,
    InMemoryStore,
    Lifecycle,
    LifecycleAlreadyExistsError,
    LifecycleInitialized,
    LifecycleNotFoundError,
    LifecycleStore,
    OptimisticConcurrencyError,
    Outcome,
    SdkMessageRecord,
    SdkMessageStore,
    SqliteStore,
    Status,
    TransitionedTo,
    replay,
)

# Postgres backend: gated by docker/testcontainers availability. The
# container is session-scoped via a module-level cache so a single
# Postgres instance backs every postgres-parametrized test.
_PG_CONTAINER_STATE: dict[str, object] = {"checked": False}


def _get_postgres_dsn() -> str | None:
    """Return a Postgres DSN backed by a session-scoped testcontainer,
    or ``None`` when Docker/testcontainers is unavailable.

    Caches both success and failure across calls so the container starts
    exactly once per test run and probe failures don't bog down later
    parametrized cases."""
    if _PG_CONTAINER_STATE["checked"]:
        return _PG_CONTAINER_STATE.get("dsn")  # type: ignore[return-value]
    _PG_CONTAINER_STATE["checked"] = True
    # The postgres backend needs the `flywheel[postgres]` extra (psycopg +
    # psycopg_pool) at runtime. Without it, PostgresStore construction
    # would raise ImportError mid-test; skip cleanly instead.
    if importlib.util.find_spec("psycopg") is None:
        _PG_CONTAINER_STATE["reason"] = (
            "flywheel[postgres] extra not installed (psycopg missing)"
        )
        return None
    try:
        from testcontainers.postgres import PostgresContainer
    except ImportError:
        _PG_CONTAINER_STATE["reason"] = "testcontainers not installed"
        return None
    try:
        container = PostgresContainer("postgres:16-alpine")
        container.start()
    except Exception as exc:  # docker daemon missing, image pull failure, etc.
        _PG_CONTAINER_STATE["reason"] = f"Docker unavailable: {exc}"
        return None
    dsn: str = container.get_connection_url(driver=None)
    _PG_CONTAINER_STATE["container"] = container
    _PG_CONTAINER_STATE["dsn"] = dsn

    import atexit

    atexit.register(container.stop)
    return dsn


def _postgres_factory(tmp_path: Path) -> object:
    dsn = _get_postgres_dsn()
    if dsn is None:
        pytest.skip(
            "Postgres backend skipped: "
            f"{_PG_CONTAINER_STATE.get('reason', 'unknown')}"
        )
    from flywheel import PostgresStore

    schema = f"flywheel_test_{uuid4().hex[:12]}"
    return PostgresStore(dsn, schema=schema, pool_min=1, pool_max=4)


# Stores under test. Each value is a callable that, given the test's
# ``tmp_path``, returns a fresh store. Using ``tmp_path`` for the SQLite
# variant gives every parametrized case its own DB file. The postgres
# factory acquires a shared session-scoped container and yields a store
# bound to a per-test schema.
_STORE_FACTORIES: dict[str, Callable[[Path], object]] = {
    "memory": lambda tmp_path: InMemoryStore(),
    "sqlite": lambda tmp_path: SqliteStore(tmp_path / "contract.db"),
    "postgres": _postgres_factory,
}


@pytest.fixture(params=sorted(_STORE_FACTORIES), ids=sorted(_STORE_FACTORIES))
def store(
    request: pytest.FixtureRequest, tmp_path: Path
) -> Iterator[object]:
    factory = _STORE_FACTORIES[request.param]
    instance = factory(tmp_path)
    try:
        yield instance
    finally:
        close = getattr(instance, "close", None)
        if callable(close):
            close()


def _ensure_lifecycle(
    store: object,
    run_id: str = "r1",
    task_id: str = "t",
) -> None:
    """Create a lifecycle row for ``run_id`` if not already present.

    Tests that exercise attempts/events/grader_results need the parent
    lifecycle row so the SQLite FK constraint is satisfied. The
    in-memory store accepts the same call but doesn't enforce FKs.
    """
    assert isinstance(store, LifecycleStore)
    if store.load_lifecycle(run_id) is not None:
        return
    store.create_lifecycle(Lifecycle(task_id=task_id, run_id=run_id))


def _ensure_attempt(
    store: object,
    run_id: str = "r1",
    number: int = 1,
) -> None:
    """Create a parent attempt row needed for grader_results FK."""
    assert isinstance(store, AttemptStore)
    _ensure_lifecycle(store, run_id)
    if store.load_attempt(run_id, number) is not None:
        return
    store.save_attempt(
        run_id,
        Attempt(
            number=number,
            started_at=datetime(2024, 1, 1, tzinfo=timezone.utc),
            run_id=run_id,
        ),
    )


# --- Protocol satisfaction --------------------------------------------------


def test_store_satisfies_every_protocol(store: object) -> None:
    assert isinstance(store, LifecycleStore)
    assert isinstance(store, AttemptStore)
    assert isinstance(store, EventStore)
    assert isinstance(store, GraderResultStore)
    assert isinstance(store, AgentSessionStore)
    assert isinstance(store, SdkMessageStore)
    assert isinstance(store, AuditStore)


def test_store_exposes_no_grader_result_mutators(store: object) -> None:
    for forbidden in (
        "update_grader_result",
        "delete_grader_result",
        "remove_grader_result",
        "set_grader_result",
        "replace_grader_result",
    ):
        assert not hasattr(store, forbidden), (
            f"store must be append-only on grader_results; "
            f"exposes {forbidden!r}"
        )


# --- Lifecycle round-trip & concurrency ------------------------------------


def test_load_missing_lifecycle_returns_none(store: object) -> None:
    assert isinstance(store, LifecycleStore)
    assert store.load_lifecycle("nope") is None


def test_create_then_load_lifecycle_round_trips(store: object) -> None:
    assert isinstance(store, LifecycleStore)
    lc = Lifecycle(task_id="t", run_id="r1", worker_id="w1")
    lc.transition_to(Status.READY)  # version becomes 2
    store.create_lifecycle(lc)

    loaded = store.load_lifecycle("r1")
    assert loaded is not None
    assert loaded.task_id == "t"
    assert loaded.run_id == "r1"
    assert loaded.worker_id == "w1"
    assert loaded.status is Status.READY
    assert loaded.version == 2
    assert loaded.attempts == []
    # Timestamps round-trip with their original tz info.
    assert Status.READY in loaded.timestamps
    assert loaded.timestamps[Status.READY].tzinfo is not None


def test_create_lifecycle_twice_raises_already_exists(store: object) -> None:
    assert isinstance(store, LifecycleStore)
    lc = Lifecycle(task_id="t", run_id="r1")
    store.create_lifecycle(lc)
    with pytest.raises(LifecycleAlreadyExistsError) as exc_info:
        store.create_lifecycle(lc)
    assert exc_info.value.run_id == "r1"


def test_update_missing_lifecycle_raises_not_found(store: object) -> None:
    assert isinstance(store, LifecycleStore)
    lc = Lifecycle(task_id="t", run_id="r1", version=2)
    with pytest.raises(LifecycleNotFoundError) as exc_info:
        store.update_lifecycle(lc, expected_version=1)
    assert exc_info.value.run_id == "r1"


def test_update_lifecycle_with_matching_version_succeeds(
    store: object,
) -> None:
    assert isinstance(store, LifecycleStore)
    lc = Lifecycle(task_id="t", run_id="r1")  # version=1
    store.create_lifecycle(lc)

    lc.transition_to(Status.READY)  # version=2
    store.update_lifecycle(lc, expected_version=1)

    loaded = store.load_lifecycle("r1")
    assert loaded is not None
    assert loaded.status is Status.READY
    assert loaded.version == 2


def test_update_lifecycle_with_stale_version_raises_conflict(
    store: object,
) -> None:
    assert isinstance(store, LifecycleStore)
    lc = Lifecycle(task_id="t", run_id="r1")  # version=1
    store.create_lifecycle(lc)

    # First writer bumps to v2 successfully.
    lc.transition_to(Status.READY)
    store.update_lifecycle(lc, expected_version=1)

    # Second writer holds an old snapshot at v1 and tries to update.
    stale = Lifecycle(
        task_id="t", run_id="r1", status=Status.READY, version=2
    )
    with pytest.raises(OptimisticConcurrencyError) as exc_info:
        store.update_lifecycle(stale, expected_version=1)
    assert exc_info.value.run_id == "r1"
    assert exc_info.value.expected_version == 1
    assert exc_info.value.actual_version == 2

    # The store row is unchanged.
    loaded = store.load_lifecycle("r1")
    assert loaded is not None
    assert loaded.version == 2


def test_lifecycle_stored_row_is_isolated_from_caller_mutations(
    store: object,
) -> None:
    assert isinstance(store, LifecycleStore)
    lc = Lifecycle(task_id="t", run_id="r1")
    store.create_lifecycle(lc)

    # Caller mutates their copy after save.
    lc.task_id = "MUTATED"
    lc.timestamps[Status.READY] = datetime(2024, 1, 1, tzinfo=timezone.utc)

    loaded = store.load_lifecycle("r1")
    assert loaded is not None
    assert loaded.task_id == "t"
    assert Status.READY not in loaded.timestamps

    # Mutating the loaded copy also does not corrupt the store.
    loaded.task_id = "ALSO_MUTATED"
    again = store.load_lifecycle("r1")
    assert again is not None
    assert again.task_id == "t"


def test_blocked_requires_json_round_trips_through_create_update_load(
    store: object,
) -> None:
    """blocked_requires_json must persist verbatim on create, survive
    update (including clearing back to NULL/None), and default to None
    when unset. Mirrors the contract across every backend so no store can
    silently drop the column."""
    assert isinstance(store, LifecycleStore)

    # 1. Default is None on a freshly-created lifecycle.
    lc_default = Lifecycle(task_id="t", run_id="r-default")
    store.create_lifecycle(lc_default)
    loaded_default = store.load_lifecycle("r-default")
    assert loaded_default is not None
    assert loaded_default.blocked_requires_json is None

    # 2. A non-empty JSON string round-trips byte-for-byte on create.
    payload = (
        '[{"type": "command_grader", "name": "full-suite"}, '
        '{"type": "file_exists", "path": ".workflow/lkg/.venv", '
        '"present": true}]'
    )
    lc = Lifecycle(
        task_id="t",
        run_id="r-set",
        blocked_requires_json=payload,
    )
    store.create_lifecycle(lc)
    loaded = store.load_lifecycle("r-set")
    assert loaded is not None
    assert loaded.blocked_requires_json == payload

    # 3. Update can clear blocked_requires_json back to None.
    loaded.transition_to(Status.READY)  # version becomes 2
    loaded.blocked_requires_json = None
    store.update_lifecycle(loaded, expected_version=1)
    reloaded = store.load_lifecycle("r-set")
    assert reloaded is not None
    assert reloaded.blocked_requires_json is None
    assert reloaded.version == 2

    # 4. Update can also set blocked_requires_json on a row where it
    # was previously None (the inverse direction).
    reloaded.blocked_requires_json = '[{"type": "env_var_set", "name": "X"}]'
    # NB: transition not required to update the column, but version bump
    # is needed to satisfy optimistic concurrency.
    reloaded.version = 3
    store.update_lifecycle(reloaded, expected_version=2)
    final = store.load_lifecycle("r-set")
    assert final is not None
    assert final.blocked_requires_json == (
        '[{"type": "env_var_set", "name": "X"}]'
    )


def test_load_lifecycle_attaches_attempts_in_number_order(
    store: object,
) -> None:
    assert isinstance(store, LifecycleStore)
    assert isinstance(store, AttemptStore)
    _ensure_lifecycle(store, "r1")

    base = datetime(2024, 1, 1, tzinfo=timezone.utc)
    store.save_attempt("r1", Attempt(number=3, started_at=base, run_id="r1"))
    store.save_attempt("r1", Attempt(number=1, started_at=base, run_id="r1"))
    store.save_attempt("r1", Attempt(number=2, started_at=base, run_id="r1"))

    loaded = store.load_lifecycle("r1")
    assert loaded is not None
    assert [a.number for a in loaded.attempts] == [1, 2, 3]


# --- Attempt round-trip ----------------------------------------------------


def test_load_missing_attempt_returns_none(store: object) -> None:
    assert isinstance(store, AttemptStore)
    assert store.load_attempt("r1", 1) is None


def test_save_then_load_attempt_round_trips_with_agent_context(
    store: object,
) -> None:
    assert isinstance(store, AttemptStore)
    _ensure_lifecycle(store, "r1")
    a = Attempt(
        number=1,
        started_at=datetime(2024, 1, 1, tzinfo=timezone.utc),
        run_id="r1",
        agent_context={"model_id": "claude-opus-4-7"},
    )
    store.save_attempt("r1", a)

    loaded = store.load_attempt("r1", 1)
    assert loaded is not None
    assert loaded.number == 1
    assert loaded.run_id == "r1"
    assert loaded.agent_context == {"model_id": "claude-opus-4-7"}


def test_save_attempt_is_upsert_keyed_by_run_id_and_number(
    store: object,
) -> None:
    assert isinstance(store, AttemptStore)
    _ensure_lifecycle(store, "r1")
    start = datetime(2024, 1, 1, tzinfo=timezone.utc)
    end = datetime(2024, 1, 1, 0, 1, tzinfo=timezone.utc)
    store.save_attempt(
        "r1",
        Attempt(number=1, started_at=start, run_id="r1"),
    )
    store.save_attempt(
        "r1",
        Attempt(
            number=1,
            started_at=start,
            run_id="r1",
            ended_at=end,
            outcome=Outcome.SUCCEEDED,
        ),
    )

    loaded = store.load_attempt("r1", 1)
    assert loaded is not None
    assert loaded.ended_at == end
    assert loaded.outcome is Outcome.SUCCEEDED
    listed = store.list_attempts("r1")
    assert len(listed) == 1
    assert listed[0].ended_at == end


def test_list_attempts_returns_in_number_order_regardless_of_insertion(
    store: object,
) -> None:
    assert isinstance(store, AttemptStore)
    _ensure_lifecycle(store, "r1")
    base = datetime(2024, 1, 1, tzinfo=timezone.utc)
    for n in (5, 2, 4, 1, 3):
        store.save_attempt(
            "r1", Attempt(number=n, started_at=base, run_id="r1")
        )
    listed = store.list_attempts("r1")
    assert [a.number for a in listed] == [1, 2, 3, 4, 5]


def test_list_attempts_scoped_by_run_id(store: object) -> None:
    assert isinstance(store, AttemptStore)
    _ensure_lifecycle(store, "r1")
    _ensure_lifecycle(store, "r2")
    base = datetime(2024, 1, 1, tzinfo=timezone.utc)
    store.save_attempt("r1", Attempt(number=1, started_at=base, run_id="r1"))
    store.save_attempt("r2", Attempt(number=1, started_at=base, run_id="r2"))
    assert [a.run_id for a in store.list_attempts("r1")] == ["r1"]
    assert [a.run_id for a in store.list_attempts("r2")] == ["r2"]


def test_attempt_stored_row_isolated_from_caller_mutations(
    store: object,
) -> None:
    assert isinstance(store, AttemptStore)
    _ensure_lifecycle(store, "r1")
    a = Attempt(
        number=1,
        started_at=datetime(2024, 1, 1, tzinfo=timezone.utc),
        run_id="r1",
        agent_context={"model_id": "x"},
    )
    store.save_attempt("r1", a)
    a.agent_context["model_id"] = "MUTATED"
    loaded = store.load_attempt("r1", 1)
    assert loaded is not None
    assert loaded.agent_context == {"model_id": "x"}


# --- Event round-trip ------------------------------------------------------


def test_append_event_assigns_monotonic_id_and_returns_record(
    store: object,
) -> None:
    assert isinstance(store, EventStore)
    _ensure_lifecycle(store, "r1")
    ts = datetime(2024, 1, 1, tzinfo=timezone.utc)
    r1 = store.append_event(EventRecord(run_id="r1", ts=ts, kind="started"))
    r2 = store.append_event(EventRecord(run_id="r1", ts=ts, kind="completed"))
    assert r1.id is not None
    assert r2.id is not None
    assert r2.id > r1.id


def test_list_events_returns_chronological_order_regardless_of_insertion(
    store: object,
) -> None:
    assert isinstance(store, EventStore)
    _ensure_lifecycle(store, "r1")
    later = datetime(2024, 1, 1, 0, 0, 10, tzinfo=timezone.utc)
    earlier = datetime(2024, 1, 1, 0, 0, 0, tzinfo=timezone.utc)
    middle = datetime(2024, 1, 1, 0, 0, 5, tzinfo=timezone.utc)
    store.append_event(EventRecord(run_id="r1", ts=later, kind="c"))
    store.append_event(EventRecord(run_id="r1", ts=earlier, kind="a"))
    store.append_event(EventRecord(run_id="r1", ts=middle, kind="b"))
    listed = store.list_events("r1")
    assert [e.kind for e in listed] == ["a", "b", "c"]


def test_list_events_scoped_by_run_id(store: object) -> None:
    assert isinstance(store, EventStore)
    _ensure_lifecycle(store, "r1")
    _ensure_lifecycle(store, "r2")
    ts = datetime(2024, 1, 1, tzinfo=timezone.utc)
    store.append_event(EventRecord(run_id="r1", ts=ts, kind="x"))
    store.append_event(EventRecord(run_id="r2", ts=ts, kind="y"))
    assert [e.kind for e in store.list_events("r1")] == ["x"]
    assert [e.kind for e in store.list_events("r2")] == ["y"]


def test_event_payload_isolated_from_caller_mutations(
    store: object,
) -> None:
    assert isinstance(store, EventStore)
    _ensure_lifecycle(store, "r1")
    payload: dict[str, int] = {"turns": 1}
    store.append_event(
        EventRecord(
            run_id="r1",
            ts=datetime(2024, 1, 1, tzinfo=timezone.utc),
            kind="progress",
            payload=payload,
        )
    )
    payload["turns"] = 999
    listed = store.list_events("r1")
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


def test_append_grader_result_assigns_monotonic_id(store: object) -> None:
    assert isinstance(store, GraderResultStore)
    _ensure_attempt(store, "r1", 1)
    r1 = store.append_grader_result(_gr(ordinal=0))
    r2 = store.append_grader_result(_gr(ordinal=1, grader_type="rubric"))
    assert r1.id is not None and r2.id is not None
    assert r2.id > r1.id


def test_list_grader_results_returns_in_ordinal_order_scoped_by_attempt(
    store: object,
) -> None:
    assert isinstance(store, GraderResultStore)
    _ensure_attempt(store, "r1", 1)
    _ensure_attempt(store, "r1", 2)
    store.append_grader_result(_gr(ordinal=2, grader_type="rubric"))
    store.append_grader_result(_gr(ordinal=0, grader_type="command"))
    store.append_grader_result(_gr(ordinal=1, grader_type="transcript"))
    # Different attempt — must not bleed into the queried list.
    store.append_grader_result(_gr(attempt_number=2, ordinal=0))

    listed = store.list_grader_results("r1", 1)
    assert [r.ordinal for r in listed] == [0, 1, 2]
    assert all(r.attempt_number == 1 for r in listed)


def test_list_grader_results_for_missing_attempt_returns_empty_list(
    store: object,
) -> None:
    assert isinstance(store, GraderResultStore)
    assert store.list_grader_results("nope", 999) == []


def test_grader_spec_and_payload_isolated_from_caller_mutations(
    store: object,
) -> None:
    assert isinstance(store, GraderResultStore)
    _ensure_attempt(store, "r1", 1)
    spec: dict[str, object] = {"type": "command", "run": "true"}
    payload: dict[str, object] = {"exit_code": 0}
    store.append_grader_result(
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
    listed = store.list_grader_results("r1", 1)
    assert dict(listed[0].grader_spec) == {"type": "command", "run": "true"}
    assert dict(listed[0].payload) == {"exit_code": 0}


# --- Session round-trip ----------------------------------------------------


def test_append_session_entry_assigns_monotonic_seq(store: object) -> None:
    assert isinstance(store, AgentSessionStore)
    e1 = store.append_session_entry(
        ClaudeSessionEntry(
            project_key="proj",
            session_id="sess",
            entry="a",
            mtime=1,
        )
    )
    e2 = store.append_session_entry(
        ClaudeSessionEntry(
            project_key="proj",
            session_id="sess",
            entry="b",
            mtime=2,
        )
    )
    assert e1.seq is not None and e2.seq is not None
    assert e2.seq > e1.seq


def test_list_session_entries_filters_by_subpath_and_orders_by_seq(
    store: object,
) -> None:
    assert isinstance(store, AgentSessionStore)
    store.append_session_entry(
        ClaudeSessionEntry(
            project_key="proj", session_id="sess", entry="main-1", mtime=1
        )
    )
    store.append_session_entry(
        ClaudeSessionEntry(
            project_key="proj",
            session_id="sess",
            entry="sub-1",
            mtime=2,
            subpath="agent-a",
        )
    )
    store.append_session_entry(
        ClaudeSessionEntry(
            project_key="proj", session_id="sess", entry="main-2", mtime=3
        )
    )

    main = store.list_session_entries("proj", "sess")
    sub = store.list_session_entries("proj", "sess", subpath="agent-a")
    assert [e.entry for e in main] == ["main-1", "main-2"]
    assert [e.entry for e in sub] == ["sub-1"]


def test_list_session_entries_scoped_by_project_and_session(
    store: object,
) -> None:
    assert isinstance(store, AgentSessionStore)
    store.append_session_entry(
        ClaudeSessionEntry(
            project_key="p1", session_id="s1", entry="x", mtime=1
        )
    )
    store.append_session_entry(
        ClaudeSessionEntry(
            project_key="p2", session_id="s1", entry="y", mtime=1
        )
    )
    store.append_session_entry(
        ClaudeSessionEntry(
            project_key="p1", session_id="s2", entry="z", mtime=1
        )
    )
    assert [e.entry for e in store.list_session_entries("p1", "s1")] == ["x"]
    assert [e.entry for e in store.list_session_entries("p2", "s1")] == ["y"]
    assert [e.entry for e in store.list_session_entries("p1", "s2")] == ["z"]


# --- SDK message persistence -----------------------------------------------


def test_save_sdk_messages_persists_payloads_byte_for_byte(
    store: object,
) -> None:
    assert isinstance(store, SdkMessageStore)
    assert isinstance(store, LifecycleStore)
    _ensure_lifecycle(store, "r1")

    payloads: list[dict[str, object]] = [
        {
            "type": "assistant",
            "content": [{"type": "text", "text": "hi"}],
            "usage": {"input_tokens": 12, "output_tokens": 34},
        },
        {
            "type": "tool_use",
            "id": "call-1",
            "name": "Bash",
            "input": {"command": "echo hello"},
        },
        {
            "type": "result",
            "is_error": False,
            "stop_reason": "end_turn",
        },
    ]
    saved = store.save_sdk_messages(
        run_id="r1",
        attempt_number=1,
        iteration_number=1,
        messages=payloads,
    )
    assert len(saved) == len(payloads)
    # Sequences are per-run monotonic and start from the next run-counter
    # value; we check strict ascending across the batch only.
    seqs = [s.sequence for s in saved]
    assert all(s is not None for s in seqs)
    assert seqs == sorted(seqs)
    assert len(set(seqs)) == len(seqs)
    for got, original in zip(saved, payloads):
        assert dict(got.payload) == original
        assert got.message_type == original["type"]
        assert got.attempt_number == 1
        assert got.iteration_number == 1
        assert got.id is not None


def test_list_sdk_messages_returns_in_sequence_order(store: object) -> None:
    assert isinstance(store, SdkMessageStore)
    assert isinstance(store, LifecycleStore)
    _ensure_lifecycle(store, "r1")

    store.save_sdk_messages(
        "r1", 1, 1, [{"type": "assistant", "n": 1}]
    )
    store.save_sdk_messages(
        "r1", 1, 2, [{"type": "assistant", "n": 2}, {"type": "result", "n": 3}]
    )
    store.save_sdk_messages(
        "r1", 2, 1, [{"type": "assistant", "n": 4}]
    )

    listed = store.list_sdk_messages("r1")
    assert [m.payload["n"] for m in listed] == [1, 2, 3, 4]
    seqs = [m.sequence for m in listed]
    assert all(s is not None for s in seqs)
    assert seqs == sorted(seqs)


def test_sdk_messages_scoped_by_run_id(store: object) -> None:
    assert isinstance(store, SdkMessageStore)
    assert isinstance(store, LifecycleStore)
    _ensure_lifecycle(store, "r1")
    _ensure_lifecycle(store, "r2")
    store.save_sdk_messages("r1", 1, 1, [{"type": "assistant", "tag": "a"}])
    store.save_sdk_messages("r2", 1, 1, [{"type": "assistant", "tag": "b"}])
    assert [m.payload["tag"] for m in store.list_sdk_messages("r1")] == ["a"]
    assert [m.payload["tag"] for m in store.list_sdk_messages("r2")] == ["b"]


def test_sdk_message_payload_isolated_from_caller_mutations(
    store: object,
) -> None:
    assert isinstance(store, SdkMessageStore)
    assert isinstance(store, LifecycleStore)
    _ensure_lifecycle(store, "r1")

    payload: dict[str, object] = {"type": "assistant", "k": "v"}
    saved = store.save_sdk_messages("r1", 1, 1, [payload])
    payload["k"] = "MUTATED"
    payload["new"] = "field"
    # The persisted record snapshots the caller's payload at save time.
    listed = store.list_sdk_messages("r1")
    assert dict(listed[0].payload) == {"type": "assistant", "k": "v"}
    # The returned record from save is similarly insulated.
    assert dict(saved[0].payload) == {"type": "assistant", "k": "v"}
    # And mutating the returned record's payload does not corrupt the
    # store's row.
    cast_payload = dict(saved[0].payload)
    cast_payload["k"] = "ALSO_MUTATED"
    again = store.list_sdk_messages("r1")
    assert dict(again[0].payload) == {"type": "assistant", "k": "v"}


def test_save_sdk_messages_with_empty_batch_is_noop(store: object) -> None:
    assert isinstance(store, SdkMessageStore)
    assert isinstance(store, LifecycleStore)
    _ensure_lifecycle(store, "r1")
    saved = store.save_sdk_messages("r1", 1, 1, [])
    assert saved == []
    assert store.list_sdk_messages("r1") == []


# --- Per-run monotonic sequence shared across events + SDK messages --------


def test_interleaved_events_and_sdk_messages_have_ascending_per_run_sequence(
    store: object,
) -> None:
    """Events and SDK messages share one per-run monotonic counter so a
    single audit ordering exists across both write paths."""
    assert isinstance(store, SdkMessageStore)
    assert isinstance(store, EventStore)
    assert isinstance(store, LifecycleStore)
    _ensure_lifecycle(store, "r1")

    ts = datetime(2024, 1, 1, tzinfo=timezone.utc)
    e1 = store.append_event(EventRecord(run_id="r1", ts=ts, kind="started"))
    m1 = store.save_sdk_messages("r1", 1, 1, [{"type": "assistant", "n": 1}])
    e2 = store.append_event(EventRecord(run_id="r1", ts=ts, kind="progress"))
    m2 = store.save_sdk_messages(
        "r1", 1, 2, [{"type": "tool_use", "n": 2}, {"type": "result", "n": 3}]
    )
    e3 = store.append_event(EventRecord(run_id="r1", ts=ts, kind="completed"))

    seqs: list[int] = []
    for rec in (e1, *m1, e2, *m2, e3):
        assert rec.sequence is not None
        seqs.append(rec.sequence)
    # Strict ascending across both record types within one run.
    assert seqs == sorted(seqs)
    assert len(set(seqs)) == len(seqs)


def test_run_sequences_are_independent_per_run_id(store: object) -> None:
    """The shared per-run counter is scoped to ``run_id``: two runs may
    independently start at 1 without colliding."""
    assert isinstance(store, SdkMessageStore)
    assert isinstance(store, EventStore)
    assert isinstance(store, LifecycleStore)
    _ensure_lifecycle(store, "r1")
    _ensure_lifecycle(store, "r2")

    ts = datetime(2024, 1, 1, tzinfo=timezone.utc)
    a = store.append_event(EventRecord(run_id="r1", ts=ts, kind="started"))
    b = store.append_event(EventRecord(run_id="r2", ts=ts, kind="started"))
    assert a.sequence == 1
    assert b.sequence == 1
    a2 = store.save_sdk_messages("r1", 1, 1, [{"type": "assistant"}])
    b2 = store.save_sdk_messages("r2", 1, 1, [{"type": "assistant"}])
    assert a2[0].sequence == 2
    assert b2[0].sequence == 2


# --- Audit-stream merged read ----------------------------------------------


def test_read_audit_since_cursor_zero_returns_every_record(
    store: object,
) -> None:
    assert isinstance(store, AuditStore)
    assert isinstance(store, EventStore)
    assert isinstance(store, SdkMessageStore)
    assert isinstance(store, LifecycleStore)
    _ensure_lifecycle(store, "r1")

    ts = datetime(2024, 1, 1, tzinfo=timezone.utc)
    store.append_event(EventRecord(run_id="r1", ts=ts, kind="started"))
    store.save_sdk_messages(
        "r1", 1, 1, [{"type": "assistant"}, {"type": "tool_use"}]
    )
    store.append_event(EventRecord(run_id="r1", ts=ts, kind="completed"))

    records = store.read_audit_since("r1", 0)
    assert len(records) == 4
    seqs = [r.sequence for r in records]
    assert all(s is not None for s in seqs)
    assert seqs == sorted(seqs)


def test_read_audit_since_cursor_n_skips_first_n_records(
    store: object,
) -> None:
    assert isinstance(store, AuditStore)
    assert isinstance(store, EventStore)
    assert isinstance(store, SdkMessageStore)
    assert isinstance(store, LifecycleStore)
    _ensure_lifecycle(store, "r1")

    ts = datetime(2024, 1, 1, tzinfo=timezone.utc)
    store.append_event(EventRecord(run_id="r1", ts=ts, kind="started"))
    store.save_sdk_messages(
        "r1", 1, 1, [{"type": "assistant"}, {"type": "tool_use"}]
    )
    store.append_event(EventRecord(run_id="r1", ts=ts, kind="completed"))

    all_records = store.read_audit_since("r1", 0)
    assert len(all_records) == 4

    after_two = store.read_audit_since("r1", 2)
    assert len(after_two) == 2
    assert [r.sequence for r in after_two] == [
        r.sequence for r in all_records[2:]
    ]

    after_all = store.read_audit_since(
        "r1", all_records[-1].sequence or 0
    )
    assert after_all == []


def test_read_audit_since_returns_typed_union_arms(store: object) -> None:
    assert isinstance(store, AuditStore)
    assert isinstance(store, EventStore)
    assert isinstance(store, SdkMessageStore)
    assert isinstance(store, LifecycleStore)
    _ensure_lifecycle(store, "r1")

    ts = datetime(2024, 1, 1, tzinfo=timezone.utc)
    store.append_event(EventRecord(run_id="r1", ts=ts, kind="started"))
    store.save_sdk_messages("r1", 1, 1, [{"type": "assistant"}])

    records = store.read_audit_since("r1", 0)
    assert isinstance(records[0], EventRecord)
    assert isinstance(records[1], SdkMessageRecord)


def test_read_audit_since_for_unknown_run_returns_empty(store: object) -> None:
    assert isinstance(store, AuditStore)
    assert store.read_audit_since("does-not-exist", 0) == []


def test_read_audit_since_scoped_by_run_id(store: object) -> None:
    assert isinstance(store, AuditStore)
    assert isinstance(store, EventStore)
    assert isinstance(store, SdkMessageStore)
    assert isinstance(store, LifecycleStore)
    _ensure_lifecycle(store, "r1")
    _ensure_lifecycle(store, "r2")

    ts = datetime(2024, 1, 1, tzinfo=timezone.utc)
    store.append_event(EventRecord(run_id="r1", ts=ts, kind="x"))
    store.save_sdk_messages("r2", 1, 1, [{"type": "assistant", "tag": "r2"}])

    r1 = store.read_audit_since("r1", 0)
    r2 = store.read_audit_since("r2", 0)
    assert len(r1) == 1
    assert len(r2) == 1
    assert isinstance(r1[0], EventRecord)
    assert isinstance(r2[0], SdkMessageRecord)
    assert dict(r2[0].payload) == {"type": "assistant", "tag": "r2"}


# --- Event-sourced domain-event write path ---------------------------------


def _dts(n: int) -> datetime:
    return datetime(2026, 5, 28, 12, 0, n, tzinfo=timezone.utc)


def _seed(store: object, run_id: str = "r1") -> Lifecycle:
    assert isinstance(store, DomainEventStore)
    return store.append_domain_event(
        LifecycleInitialized(
            run_id=run_id,
            ts=_dts(0),
            task_id="t",
            worker_id="w",
            artifacts_dir="/artifacts",
        ),
        expected_version=0,
    )


def _drive_to_done(store: object, run_id: str = "r1") -> None:
    """Append a full happy-path domain-event stream ending in DONE."""
    assert isinstance(store, DomainEventStore)
    _seed(store, run_id)
    store.append_domain_event(
        TransitionedTo(run_id=run_id, ts=_dts(1), target=Status.READY),
        expected_version=1,
    )
    store.append_domain_event(
        TransitionedTo(run_id=run_id, ts=_dts(2), target=Status.RUNNING),
        expected_version=2,
    )
    store.append_domain_event(
        AttemptStarted(
            run_id=run_id,
            ts=_dts(3),
            attempt_number=1,
            number=1,
            attempt_run_id=run_id,
            started_at=_dts(3),
            agent_context={"model_id": "claude-opus-4-8"},
        ),
        expected_version=3,
    )
    store.append_domain_event(
        TransitionedTo(run_id=run_id, ts=_dts(4), target=Status.VALIDATING),
        expected_version=4,
    )
    store.append_domain_event(
        GraderEvaluated(
            run_id=run_id,
            ts=_dts(5),
            attempt_number=1,
            ordinal=0,
            grader_type="command",
            passed=True,
            duration_ms=7,
            grader_name="pytest",
            grader_spec={"type": "command", "run": "uv run pytest"},
            payload={"exit_code": 0},
        ),
        expected_version=5,
    )
    store.append_domain_event(
        AttemptFinalized(
            run_id=run_id,
            ts=_dts(6),
            attempt_number=1,
            number=1,
            outcome=Outcome.SUCCEEDED,
            ended_at=_dts(6),
            agent_output="done",
        ),
        expected_version=6,
    )
    store.append_domain_event(
        TransitionedTo(run_id=run_id, ts=_dts(7), target=Status.DONE),
        expected_version=7,
    )


def test_store_satisfies_domain_event_store_protocol(store: object) -> None:
    assert isinstance(store, DomainEventStore)


def test_seed_event_creates_projection_row(store: object) -> None:
    assert isinstance(store, DomainEventStore)
    assert isinstance(store, LifecycleStore)
    folded = _seed(store)
    assert folded.status is Status.PENDING
    assert folded.version == 1
    loaded = store.load_lifecycle("r1")
    assert loaded is not None
    assert loaded.status is Status.PENDING
    assert loaded.version == 1
    assert loaded.task_id == "t"
    assert loaded.worker_id == "w"
    assert loaded.artifacts_dir == "/artifacts"


def test_duplicate_seed_raises_already_exists(store: object) -> None:
    assert isinstance(store, DomainEventStore)
    _seed(store)
    with pytest.raises(LifecycleAlreadyExistsError):
        _seed(store)


def test_append_to_unknown_run_raises_not_found(store: object) -> None:
    assert isinstance(store, DomainEventStore)
    with pytest.raises(LifecycleNotFoundError):
        store.append_domain_event(
            TransitionedTo(run_id="ghost", ts=_dts(1), target=Status.READY),
            expected_version=1,
        )


def test_append_with_stale_version_raises_conflict(store: object) -> None:
    assert isinstance(store, DomainEventStore)
    _seed(store)  # version 1
    store.append_domain_event(
        TransitionedTo(run_id="r1", ts=_dts(1), target=Status.READY),
        expected_version=1,
    )  # version 2
    with pytest.raises(OptimisticConcurrencyError) as exc_info:
        store.append_domain_event(
            TransitionedTo(run_id="r1", ts=_dts(2), target=Status.RUNNING),
            expected_version=1,  # stale: store is at version 2
        )
    assert exc_info.value.expected_version == 1
    assert exc_info.value.actual_version == 2


def test_happy_path_folds_to_done_with_projections(store: object) -> None:
    assert isinstance(store, DomainEventStore)
    assert isinstance(store, LifecycleStore)
    assert isinstance(store, GraderResultStore)
    _drive_to_done(store)

    loaded = store.load_lifecycle("r1")
    assert loaded is not None
    assert loaded.status is Status.DONE
    assert loaded.version == 8
    assert loaded.agent_output == "done"
    # attempts projection populated from AttemptStarted/AttemptFinalized.
    assert len(loaded.attempts) == 1
    assert loaded.attempts[0].outcome is Outcome.SUCCEEDED
    assert loaded.attempts[0].agent_context == {"model_id": "claude-opus-4-8"}
    # grader_results projection populated from GraderEvaluated.
    grader_results = store.list_grader_results("r1", 1)
    assert len(grader_results) == 1
    assert grader_results[0].passed
    assert grader_results[0].grader_name == "pytest"


def test_replay_of_domain_events_equals_loaded_projection(
    store: object,
) -> None:
    """The determinism oracle: folding the persisted domain-event log
    reproduces the stored projection exactly, on every backend."""
    assert isinstance(store, DomainEventStore)
    assert isinstance(store, LifecycleStore)
    _drive_to_done(store)
    loaded = store.load_lifecycle("r1")
    folded = replay(store.list_domain_events("r1"))
    assert loaded == folded


def test_domain_events_interleave_with_telemetry_in_audit_stream(
    store: object,
) -> None:
    """Domain and telemetry events share the events table and the per-run
    sequence, so the audit stream is one totally-ordered log."""
    assert isinstance(store, DomainEventStore)
    assert isinstance(store, EventStore)
    assert isinstance(store, AuditStore)
    _seed(store)
    store.append_event(
        EventRecord(run_id="r1", ts=_dts(1), kind="harness.attempt_started")
    )
    store.append_domain_event(
        TransitionedTo(run_id="r1", ts=_dts(2), target=Status.READY),
        expected_version=1,
    )

    records = store.read_audit_since("r1", 0)
    # The seed, the telemetry event, and the domain transition are all here.
    kinds = [r.kind for r in records if isinstance(r, EventRecord)]
    assert "lifecycle_initialized" in kinds
    assert "harness.attempt_started" in kinds
    assert "transitioned_to" in kinds
    categories = {
        r.kind: r.category for r in records if isinstance(r, EventRecord)
    }
    assert categories["harness.attempt_started"] == "telemetry"
    assert categories["transitioned_to"] == "domain"
    seqs = [r.sequence for r in records]
    assert seqs == sorted(seqs)

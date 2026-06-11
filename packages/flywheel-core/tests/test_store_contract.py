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

from collections.abc import Iterator
from datetime import datetime, timezone
from pathlib import Path
from uuid import uuid4

import pytest

from flywheel_core import (
    Attempt,
    AttemptFinalized,
    AttemptStarted,
    AttemptStore,
    ControlCommandStore,
    DomainEventStore,
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
    CommandGrader,
    SqliteStore,
    Status,
    Task,
    TaskStore,
    TransitionedTo,
    replay,
)

# Stores under test. The Postgres container is provided session-scoped by the
# root ``conftest.py`` (``postgres_dsn``); a None DSN skips the postgres cases.
_STORE_BACKENDS = ("memory", "sqlite", "postgres")


@pytest.fixture(params=_STORE_BACKENDS, ids=_STORE_BACKENDS)
def store(
    request: pytest.FixtureRequest,
    tmp_path: Path,
    postgres_dsn: str | None,
) -> Iterator[object]:
    param = request.param
    if param == "memory":
        instance: object = InMemoryStore()
    elif param == "sqlite":
        instance = SqliteStore(tmp_path / "contract.db")
    else:
        if postgres_dsn is None:
            pytest.skip("Postgres backend skipped: no database reachable")
        from flywheel_core import PostgresStore

        instance = PostgresStore(
            postgres_dsn,
            schema=f"flywheel_test_{uuid4().hex[:12]}",
            pool_min=1,
            pool_max=4,
        )
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
    assert isinstance(store, DomainEventStore)
    assert isinstance(store, GraderResultStore)
    assert isinstance(store, TaskStore)
    assert isinstance(store, ControlCommandStore)


def test_store_exposes_no_telemetry_verbs(store: object) -> None:
    """Telemetry flows to a TelemetrySink, never the relational store
    (spec 00025 FR-5). The sdk-message verbs, the events-side telemetry
    write path, and the merged audit-stream read were removed with the
    schema reduction; a store growing one of these back is reintroducing
    telemetry at ledger grade."""
    for forbidden in (
        "append_event",
        "list_events",
        "append_sdk_message",
        "save_sdk_messages",
        "list_sdk_messages",
        "read_audit_since",
    ):
        assert not hasattr(store, forbidden), (
            f"telemetry must not reach the store; exposes {forbidden!r}"
        )


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
        '{"type": "file_exists", "path": ".flywheel/lkg/.venv", '
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


def test_awaiting_manual_ordinal_round_trips_through_create_update_load(
    store: object,
) -> None:
    """awaiting_manual_ordinal must persist verbatim on create, survive
    update (including clearing back to NULL/None), and default to None
    when unset. Mirrors the contract across every backend so no store can
    silently drop the column (the same shape as the
    ``blocked_requires_json`` test above)."""
    assert isinstance(store, LifecycleStore)

    # 1. Default is None on a freshly-created lifecycle.
    lc_default = Lifecycle(task_id="t", run_id="r-default")
    store.create_lifecycle(lc_default)
    loaded_default = store.load_lifecycle("r-default")
    assert loaded_default is not None
    assert loaded_default.awaiting_manual_ordinal is None

    # 2. A non-zero ordinal round-trips on create.
    lc = Lifecycle(
        task_id="t",
        run_id="r-set",
        awaiting_manual_ordinal=3,
    )
    store.create_lifecycle(lc)
    loaded = store.load_lifecycle("r-set")
    assert loaded is not None
    assert loaded.awaiting_manual_ordinal == 3

    # 3. An ordinal of 0 is a real value, not the NULL sentinel.
    lc_zero = Lifecycle(
        task_id="t",
        run_id="r-zero",
        awaiting_manual_ordinal=0,
    )
    store.create_lifecycle(lc_zero)
    loaded_zero = store.load_lifecycle("r-zero")
    assert loaded_zero is not None
    assert loaded_zero.awaiting_manual_ordinal == 0

    # 4. Update can carry the column through a version bump.
    loaded.version = 2
    loaded.awaiting_manual_ordinal = 7
    store.update_lifecycle(loaded, expected_version=1)
    reloaded = store.load_lifecycle("r-set")
    assert reloaded is not None
    assert reloaded.awaiting_manual_ordinal == 7
    assert reloaded.version == 2

    # 5. Update can clear the column back to None.
    reloaded.version = 3
    reloaded.awaiting_manual_ordinal = None
    store.update_lifecycle(reloaded, expected_version=2)
    cleared = store.load_lifecycle("r-set")
    assert cleared is not None
    assert cleared.awaiting_manual_ordinal is None


def test_awaiting_manual_ordinal_cleared_on_ready_done_failed_validation(
    store: object,
) -> None:
    """Lifecycle.transition_to nulls ``awaiting_manual_ordinal`` on every
    -> READY / -> DONE / -> FAILED_VALIDATION edge (mirroring the
    blocked_requires_json clear on -> READY). The contract test pushes the
    cleared value through the store so a backend that drops the column
    during persistence is caught here, not at harness wiring time.
    """
    assert isinstance(store, LifecycleStore)

    # --- -> READY clears -------------------------------------------------
    # Drive a lifecycle far enough that it can land back on READY via the
    # FAILED_VALIDATION retry edge; set the ordinal mid-flight to prove
    # the next -> READY edge nulls it.
    lc_ready = Lifecycle(task_id="t", run_id="r-clear-ready")
    lc_ready.transition_to(Status.READY)  # v=2
    lc_ready.transition_to(Status.RUNNING)  # v=3
    lc_ready.transition_to(Status.VALIDATING)  # v=4
    lc_ready.transition_to(
        Status.FAILED_VALIDATION, error="grader failed"
    )  # v=5
    # Stash an ordinal on the parked-then-retried lifecycle so the next
    # -> READY edge has something to clear (the harness would never set the
    # column from FAILED_VALIDATION, but the clearing rule must not depend
    # on the source state).
    lc_ready.awaiting_manual_ordinal = 2
    store.create_lifecycle(lc_ready)
    lc_ready.transition_to(Status.READY)  # v=6, retry edge clears
    assert lc_ready.awaiting_manual_ordinal is None
    store.update_lifecycle(lc_ready, expected_version=5)
    reloaded_ready = store.load_lifecycle("r-clear-ready")
    assert reloaded_ready is not None
    assert reloaded_ready.awaiting_manual_ordinal is None

    # --- -> DONE clears (via AWAITING_APPROVAL approve) ------------------
    lc_done = Lifecycle(task_id="t", run_id="r-clear-done")
    lc_done.transition_to(Status.READY)
    lc_done.transition_to(Status.RUNNING)
    lc_done.transition_to(Status.VALIDATING)
    lc_done.transition_to(Status.AWAITING_APPROVAL)
    lc_done.awaiting_manual_ordinal = 4
    store.create_lifecycle(lc_done)
    lc_done.transition_to(Status.DONE)
    assert lc_done.awaiting_manual_ordinal is None
    store.update_lifecycle(lc_done, expected_version=5)
    reloaded_done = store.load_lifecycle("r-clear-done")
    assert reloaded_done is not None
    assert reloaded_done.status is Status.DONE
    assert reloaded_done.awaiting_manual_ordinal is None

    # --- -> FAILED_VALIDATION clears (via AWAITING_APPROVAL reject) ------
    lc_fv = Lifecycle(task_id="t", run_id="r-clear-fv")
    lc_fv.transition_to(Status.READY)
    lc_fv.transition_to(Status.RUNNING)
    lc_fv.transition_to(Status.VALIDATING)
    lc_fv.transition_to(Status.AWAITING_APPROVAL)
    lc_fv.awaiting_manual_ordinal = 5
    store.create_lifecycle(lc_fv)
    lc_fv.transition_to(Status.FAILED_VALIDATION, error="reviewer rejected")
    assert lc_fv.awaiting_manual_ordinal is None
    store.update_lifecycle(lc_fv, expected_version=5)
    reloaded_fv = store.load_lifecycle("r-clear-fv")
    assert reloaded_fv is not None
    assert reloaded_fv.status is Status.FAILED_VALIDATION
    assert reloaded_fv.awaiting_manual_ordinal is None


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


def test_attempt_aggregates_round_trip(store: object) -> None:
    """The rolled-up aggregate columns survive save/load/list on every
    backend (FR-6: the dashboard reads these relationally)."""
    assert isinstance(store, AttemptStore)
    _ensure_lifecycle(store, "r1")
    activity = datetime(2024, 1, 1, 0, 2, 30, tzinfo=timezone.utc)
    store.save_attempt(
        "r1",
        Attempt(
            number=1,
            started_at=datetime(2024, 1, 1, tzinfo=timezone.utc),
            run_id="r1",
            input_tokens=1000,
            output_tokens=200,
            cache_creation_input_tokens=30,
            cache_read_input_tokens=4,
            iterations_completed=3,
            turns=9,
            total_cost_usd=0.125,
            last_activity_at=activity,
        ),
    )
    for loaded in (store.load_attempt("r1", 1), store.list_attempts("r1")[0]):
        assert loaded is not None
        assert loaded.input_tokens == 1000
        assert loaded.output_tokens == 200
        assert loaded.cache_creation_input_tokens == 30
        assert loaded.cache_read_input_tokens == 4
        assert loaded.total_tokens == 1234
        assert loaded.iterations_completed == 3
        assert loaded.turns == 9
        assert loaded.total_cost_usd == 0.125
        assert loaded.last_activity_at == activity


def test_attempt_aggregates_default_to_zero(store: object) -> None:
    """An attempt saved without aggregates (zero completed iterations)
    reads back as zeroed counters and a None last-activity timestamp."""
    assert isinstance(store, AttemptStore)
    _ensure_lifecycle(store, "r1")
    store.save_attempt(
        "r1",
        Attempt(
            number=1,
            started_at=datetime(2024, 1, 1, tzinfo=timezone.utc),
            run_id="r1",
        ),
    )
    loaded = store.load_attempt("r1", 1)
    assert loaded is not None
    assert loaded.total_tokens == 0
    assert loaded.iterations_completed == 0
    assert loaded.turns == 0
    assert loaded.total_cost_usd == 0.0
    assert loaded.last_activity_at is None


def test_save_attempt_with_matching_expected_version_succeeds(
    store: object,
) -> None:
    assert isinstance(store, AttemptStore)
    assert isinstance(store, LifecycleStore)
    _ensure_lifecycle(store, "r1")
    lc = store.load_lifecycle("r1")
    assert lc is not None
    store.save_attempt(
        "r1",
        Attempt(
            number=1,
            started_at=datetime(2024, 1, 1, tzinfo=timezone.utc),
            run_id="r1",
            input_tokens=42,
        ),
        expected_version=lc.version,
    )
    loaded = store.load_attempt("r1", 1)
    assert loaded is not None
    assert loaded.input_tokens == 42


def test_save_attempt_with_stale_expected_version_raises(
    store: object,
) -> None:
    assert isinstance(store, AttemptStore)
    _ensure_lifecycle(store, "r1")
    with pytest.raises(OptimisticConcurrencyError):
        store.save_attempt(
            "r1",
            Attempt(
                number=1,
                started_at=datetime(2024, 1, 1, tzinfo=timezone.utc),
                run_id="r1",
            ),
            expected_version=999,
        )
    assert store.load_attempt("r1", 1) is None


def test_save_attempt_expected_version_against_missing_run_raises(
    store: object,
) -> None:
    assert isinstance(store, AttemptStore)
    with pytest.raises(LifecycleNotFoundError):
        store.save_attempt(
            "r-missing",
            Attempt(
                number=1,
                started_at=datetime(2024, 1, 1, tzinfo=timezone.utc),
                run_id="r-missing",
            ),
            expected_version=1,
        )


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


# --- Per-run monotonic domain-event sequence --------------------------------


def test_domain_event_sequences_are_dense_and_monotonic_per_run(
    store: object,
) -> None:
    """Without the retired shared cross-table counter, domain events
    number 1..N per run: dense, strictly ascending, assigned at append
    time (spec 00025 FR-5)."""
    assert isinstance(store, DomainEventStore)
    _seed(store)
    store.append_domain_event(
        TransitionedTo(run_id="r1", ts=_dts(1), target=Status.READY),
        expected_version=1,
    )
    store.append_domain_event(
        TransitionedTo(run_id="r1", ts=_dts(2), target=Status.RUNNING),
        expected_version=2,
    )
    events = store.list_domain_events("r1")
    assert [e.sequence for e in events] == [1, 2, 3]


def test_domain_event_sequences_are_independent_per_run_id(
    store: object,
) -> None:
    """The per-run sequence is scoped to ``run_id``: two runs each start
    at 1 and advance independently."""
    assert isinstance(store, DomainEventStore)
    _seed(store, "r1")
    _seed(store, "r2")
    store.append_domain_event(
        TransitionedTo(run_id="r1", ts=_dts(1), target=Status.READY),
        expected_version=1,
    )
    assert [e.sequence for e in store.list_domain_events("r1")] == [1, 2]
    assert [e.sequence for e in store.list_domain_events("r2")] == [1]


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


def test_attempt_finalized_preserves_boundary_rolled_aggregates(
    store: object,
) -> None:
    """Aggregates rolled up between AttemptStarted and AttemptFinalized
    survive finalization: the fold loads the attempt rows (with their
    counters) before applying the in-place finalize mutation, so the
    boundary rollups are not clobbered by the domain-event projection."""
    assert isinstance(store, DomainEventStore)
    assert isinstance(store, AttemptStore)
    assert isinstance(store, LifecycleStore)
    _seed(store)
    store.append_domain_event(
        TransitionedTo(run_id="r1", ts=_dts(1), target=Status.READY),
        expected_version=1,
    )
    store.append_domain_event(
        TransitionedTo(run_id="r1", ts=_dts(2), target=Status.RUNNING),
        expected_version=2,
    )
    store.append_domain_event(
        AttemptStarted(
            run_id="r1",
            ts=_dts(3),
            attempt_number=1,
            number=1,
            attempt_run_id="r1",
            started_at=_dts(3),
        ),
        expected_version=3,
    )
    rolled = store.load_attempt("r1", 1)
    assert rolled is not None
    rolled.input_tokens = 120
    rolled.output_tokens = 30
    rolled.cache_creation_input_tokens = 7
    rolled.cache_read_input_tokens = 3
    rolled.iterations_completed = 2
    rolled.turns = 5
    rolled.total_cost_usd = 0.5
    rolled.last_activity_at = _dts(4)
    store.save_attempt("r1", rolled, expected_version=4)
    store.append_domain_event(
        AttemptFinalized(
            run_id="r1",
            ts=_dts(5),
            attempt_number=1,
            number=1,
            outcome=Outcome.SUCCEEDED,
            ended_at=_dts(5),
            agent_output="done",
        ),
        expected_version=4,
    )
    loaded = store.load_attempt("r1", 1)
    assert loaded is not None
    assert loaded.outcome is Outcome.SUCCEEDED
    assert loaded.ended_at == _dts(5)
    assert loaded.total_tokens == 160
    assert loaded.iterations_completed == 2
    assert loaded.turns == 5
    assert loaded.total_cost_usd == 0.5
    assert loaded.last_activity_at == _dts(4)
    lc = store.load_lifecycle("r1")
    assert lc is not None
    assert lc.attempts[0].input_tokens == 120
    assert lc.attempts[0].outcome is Outcome.SUCCEEDED


def test_replay_of_listed_domain_events_matches_projection(
    store: object,
) -> None:
    """The events table is the domain ledger and nothing else (spec
    00025 FR-5): list_domain_events returns the typed stream in dense
    sequence order, and folding it reproduces exactly the lifecycle the
    projection row reports."""
    assert isinstance(store, DomainEventStore)
    assert isinstance(store, LifecycleStore)
    _seed(store)
    store.append_domain_event(
        TransitionedTo(run_id="r1", ts=_dts(2), target=Status.READY),
        expected_version=1,
    )

    events = store.list_domain_events("r1")
    domain_kinds = [type(e).__name__ for e in events]
    assert domain_kinds == ["LifecycleInitialized", "TransitionedTo"]
    assert [e.sequence for e in events] == [1, 2]

    folded = replay(events)
    loaded = store.load_lifecycle("r1")
    assert loaded is not None
    assert folded.status is loaded.status is Status.READY
    assert folded.version == loaded.version == 2


# --- Shared deterministic clock helper -------------------------------------


def _t(second: int) -> datetime:
    return datetime(2026, 5, 28, 12, 0, second, tzinfo=timezone.utc)




# --- TaskStore --------------------------------------------------------------


def _task(task_id: str = "t", goal: str = "Do the thing.") -> Task:
    return Task(id=task_id, goal=goal, graders=[CommandGrader(run="true")])


def test_save_task_returns_digest_and_is_idempotent(store: object) -> None:
    assert isinstance(store, TaskStore)
    task = _task()
    first = store.save_task(task, now=_t(0))
    second = store.save_task(task, now=_t(5))
    assert first == second
    loaded = store.load_task(task.id)
    assert loaded == task


def test_load_task_missing_returns_none(store: object) -> None:
    assert isinstance(store, TaskStore)
    assert store.load_task("absent") is None
    assert store.load_task("absent", "deadbeef") is None


def test_save_task_edit_creates_new_version(store: object) -> None:
    assert isinstance(store, TaskStore)
    original = _task(goal="Original goal.")
    edited = _task(goal="Edited goal.")
    h_original = store.save_task(original, now=_t(0))
    h_edited = store.save_task(edited, now=_t(10))
    assert h_original != h_edited
    # Latest (no hash) resolves to the most recently created version.
    assert store.load_task("t") == edited
    # Each exact version is still retrievable by its content hash.
    assert store.load_task("t", h_original) == original
    assert store.load_task("t", h_edited) == edited


def test_load_task_for_run_resolves_pinned_version(store: object) -> None:
    assert isinstance(store, TaskStore)
    assert isinstance(store, DomainEventStore)
    original = _task(goal="Original goal.")
    edited = _task(goal="Edited goal.")
    h_original = store.save_task(original, now=_t(0))
    store.save_task(edited, now=_t(10))
    # Seed a run pinned to the ORIGINAL version, then edit the catalog.
    store.append_domain_event(
        LifecycleInitialized(
            run_id="run-pin",
            ts=_t(0),
            task_id="t",
            task_content_hash=h_original,
        ),
        expected_version=0,
    )
    # The run resolves to exactly what it executed, not the latest edit.
    assert store.load_task_for_run("run-pin") == original


def test_load_task_for_run_missing_run_returns_none(store: object) -> None:
    assert isinstance(store, TaskStore)
    assert store.load_task_for_run("never-ran") is None


def test_save_task_round_trips_tags(store: object) -> None:
    assert isinstance(store, TaskStore)
    task = Task(
        id="t",
        goal="Do the thing.",
        graders=[CommandGrader(run="true")],
        tags=["http", "reliability"],
    )
    store.save_task(task, now=_t(0))
    loaded = store.load_task("t")
    assert loaded is not None
    # tags are part of the persisted definition and round-trip in order.
    assert loaded.tags == ["http", "reliability"]
    assert loaded == task


def test_editing_tags_forks_a_new_version(store: object) -> None:
    assert isinstance(store, TaskStore)
    base = Task(
        id="t",
        goal="Same goal.",
        graders=[CommandGrader(run="true")],
        tags=["a"],
    )
    retagged = Task(
        id="t",
        goal="Same goal.",
        graders=[CommandGrader(run="true")],
        tags=["b", "c"],
    )
    h1 = store.save_task(base, now=_t(0))
    h2 = store.save_task(retagged, now=_t(10))
    # tags are part of the content hash, so editing them mints a new version.
    assert h1 != h2
    # Latest resolves to the retagged version; each version stays addressable.
    latest = store.load_task("t")
    assert latest is not None and latest.tags == ["b", "c"]
    pinned = store.load_task("t", h1)
    assert pinned is not None and pinned.tags == ["a"]


# --- Control command channel (00013 store layer) ---------------------------


def test_enqueue_command_persists_pending_row(store: object) -> None:
    assert isinstance(store, ControlCommandStore)
    assert isinstance(store, LifecycleStore)
    _ensure_lifecycle(store, "r1")

    enqueued = store.enqueue_command(
        "r1",
        "say",
        {"text": "focus on the failing grader first"},
        now=_t(0),
    )
    assert enqueued.run_id == "r1"
    assert enqueued.kind == "say"
    assert dict(enqueued.payload) == {
        "text": "focus on the failing grader first"
    }
    assert enqueued.enqueued_at == _t(0)
    assert enqueued.claimed_at is None
    assert enqueued.id is not None


def test_enqueue_command_assigns_monotonic_id(store: object) -> None:
    assert isinstance(store, ControlCommandStore)
    assert isinstance(store, LifecycleStore)
    _ensure_lifecycle(store, "r1")

    first = store.enqueue_command("r1", "interrupt", {}, now=_t(0))
    second = store.enqueue_command(
        "r1", "say", {"text": "hello"}, now=_t(1)
    )
    assert first.id is not None and second.id is not None
    assert second.id > first.id


def test_claim_commands_returns_pending_rows_in_enqueue_order(
    store: object,
) -> None:
    assert isinstance(store, ControlCommandStore)
    assert isinstance(store, LifecycleStore)
    _ensure_lifecycle(store, "r1")

    store.enqueue_command("r1", "say", {"text": "first"}, now=_t(0))
    store.enqueue_command("r1", "interrupt", {}, now=_t(1))
    store.enqueue_command(
        "r1", "set_model", {"target": "claude-opus-4-8"}, now=_t(2)
    )

    claimed = store.claim_commands("r1", now=_t(3))
    assert [c.kind for c in claimed] == ["say", "interrupt", "set_model"]
    for record in claimed:
        assert record.claimed_at == _t(3)
        assert record.id is not None


def test_claim_commands_is_claim_once(store: object) -> None:
    """FR-2: enqueue two commands, claim once (both returned), claim
    again (none returned)."""
    assert isinstance(store, ControlCommandStore)
    assert isinstance(store, LifecycleStore)
    _ensure_lifecycle(store, "r1")

    store.enqueue_command("r1", "say", {"text": "one"}, now=_t(0))
    store.enqueue_command("r1", "interrupt", {}, now=_t(1))

    first_claim = store.claim_commands("r1", now=_t(2))
    assert len(first_claim) == 2

    second_claim = store.claim_commands("r1", now=_t(3))
    assert second_claim == []


def test_claim_commands_only_returns_unclaimed_rows_added_since(
    store: object,
) -> None:
    """A second batch enqueued after the first claim must be the only
    rows the next claim returns; earlier-claimed rows must not reappear."""
    assert isinstance(store, ControlCommandStore)
    assert isinstance(store, LifecycleStore)
    _ensure_lifecycle(store, "r1")

    store.enqueue_command("r1", "say", {"text": "one"}, now=_t(0))
    first_claim = store.claim_commands("r1", now=_t(1))
    assert [c.kind for c in first_claim] == ["say"]

    store.enqueue_command(
        "r1", "set_model", {"target": "claude-opus-4-8"}, now=_t(2)
    )
    second_claim = store.claim_commands("r1", now=_t(3))
    assert [c.kind for c in second_claim] == ["set_model"]
    assert second_claim[0].claimed_at == _t(3)


def test_claim_commands_with_empty_queue_returns_empty_list(
    store: object,
) -> None:
    assert isinstance(store, ControlCommandStore)
    assert isinstance(store, LifecycleStore)
    _ensure_lifecycle(store, "r1")
    assert store.claim_commands("r1", now=_t(0)) == []


def test_claim_commands_is_scoped_by_run_id(store: object) -> None:
    """A command enqueued for one run must not be claimed by another."""
    assert isinstance(store, ControlCommandStore)
    assert isinstance(store, LifecycleStore)
    _ensure_lifecycle(store, "r1")
    _ensure_lifecycle(store, "r2")

    store.enqueue_command("r1", "say", {"text": "to r1"}, now=_t(0))
    store.enqueue_command("r2", "say", {"text": "to r2"}, now=_t(1))

    r1_claim = store.claim_commands("r1", now=_t(2))
    assert len(r1_claim) == 1
    assert dict(r1_claim[0].payload) == {"text": "to r1"}

    r2_claim = store.claim_commands("r2", now=_t(2))
    assert len(r2_claim) == 1
    assert dict(r2_claim[0].payload) == {"text": "to r2"}


def test_control_command_payload_isolated_from_caller_mutations(
    store: object,
) -> None:
    """Mutating the payload after enqueue must not corrupt the stored row,
    matching the defensive-copy contract every other store record honors."""
    assert isinstance(store, ControlCommandStore)
    assert isinstance(store, LifecycleStore)
    _ensure_lifecycle(store, "r1")

    payload: dict[str, object] = {"text": "original"}
    store.enqueue_command("r1", "say", payload, now=_t(0))
    payload["text"] = "MUTATED"
    payload["new"] = "field"

    claimed = store.claim_commands("r1", now=_t(1))
    assert len(claimed) == 1
    assert dict(claimed[0].payload) == {"text": "original"}

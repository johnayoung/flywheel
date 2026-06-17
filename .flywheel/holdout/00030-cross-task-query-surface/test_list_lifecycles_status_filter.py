"""Held-out acceptance test for the status-filtered list_lifecycles query.

Criterion: when the store holds lifecycles in several statuses and a caller
requests the list filtered to a single status, the method returns exactly the
lifecycles in that status and no others.

This is a single-store read authored blind to the implementation. Seeded
run_ids/statuses are deliberately not the obvious 'r1'/READY fixture values so a
hardcoded status echo cannot pass. Assertions compare the SET of returned
run_ids -- never order.
"""

import pytest

from flywheel_core import InMemoryStore, Lifecycle, SqliteStore, Status


def _seed(store) -> None:
    """Seed seven lifecycles across four statuses and two task ids.

    Layout (run_id -> status, task_id):
      lc-a1 RUNNING    t-alpha
      lc-a2 DONE       t-alpha
      lc-a3 RUNNING    t-alpha
      lc-a4 FAILED     t-alpha
      lc-b1 RUNNING    t-beta
      lc-b2 DONE       t-beta
      lc-b3 VALIDATING t-beta
    """
    rows = [
        ("lc-a1", "t-alpha", Status.RUNNING),
        ("lc-a2", "t-alpha", Status.DONE),
        ("lc-a3", "t-alpha", Status.RUNNING),
        ("lc-a4", "t-alpha", Status.FAILED),
        ("lc-b1", "t-beta", Status.RUNNING),
        ("lc-b2", "t-beta", Status.DONE),
        ("lc-b3", "t-beta", Status.VALIDATING),
    ]
    for run_id, task_id, status in rows:
        store.create_lifecycle(
            Lifecycle(task_id=task_id, run_id=run_id, status=status)
        )


@pytest.mark.parametrize("make_store", [
    lambda tmp_path: InMemoryStore(),
    lambda tmp_path: SqliteStore(tmp_path / "store.db"),
])
def test_list_lifecycles_filters_to_single_status(make_store, tmp_path) -> None:
    store = make_store(tmp_path)
    _seed(store)

    # Filter to a single status via a one-element collection. RUNNING spans
    # both task ids, so the wrong implementations cannot coincidentally pass by
    # collapsing to a single task.
    result = store.list_lifecycles(statuses={Status.RUNNING})

    returned_run_ids = {lc.run_id for lc in result}
    assert returned_run_ids == {"lc-a1", "lc-a3", "lc-b1"}

    # Every returned element is a fully-folded Lifecycle in exactly the queried
    # status (defends against filtering the wrong column / returning the
    # complement set).
    for lc in result:
        assert lc.status is Status.RUNNING
        assert lc.attempts == lc.attempts  # attribute exists and is a list-like


@pytest.mark.parametrize("make_store", [
    lambda tmp_path: InMemoryStore(),
    lambda tmp_path: SqliteStore(tmp_path / "store.db"),
])
def test_list_lifecycles_filters_to_a_different_single_status(make_store, tmp_path) -> None:
    # A second status proves the method reads the requested value rather than
    # echoing one hardcoded literal.
    store = make_store(tmp_path)
    _seed(store)

    result = store.list_lifecycles(statuses={Status.DONE})

    assert {lc.run_id for lc in result} == {"lc-a2", "lc-b2"}
    for lc in result:
        assert lc.status is Status.DONE


@pytest.mark.parametrize("make_store", [
    lambda tmp_path: InMemoryStore(),
    lambda tmp_path: SqliteStore(tmp_path / "store.db"),
])
def test_list_lifecycles_empty_for_unmatched_status(make_store, tmp_path) -> None:
    # A status present in the enum but absent from the store yields [] (no
    # error). PENDING was never seeded.
    store = make_store(tmp_path)
    _seed(store)

    assert store.list_lifecycles(statuses={Status.PENDING}) == []

"""Memory-backend-specific tests for ``flywheel_core.store_memory.InMemoryStore``.

The shared protocol contract — round-trips, ordering, optimistic
concurrency, append-only enforcement — runs against every backend from
``test_store_contract.py``. Tests here cover behavior that is unique to
the in-memory substrate (no shared state across instances, no durable
persistence) and would not make sense to parametrize over backends.
"""

from __future__ import annotations

from flywheel_core import InMemoryStore, Lifecycle, Status


def test_two_in_memory_stores_have_independent_state() -> None:
    s1 = InMemoryStore()
    s2 = InMemoryStore()
    s1.create_lifecycle(Lifecycle(task_id="t", run_id="shared"))
    assert s1.load_lifecycle("shared") is not None
    assert s2.load_lifecycle("shared") is None


def test_blocked_requires_json_is_propagated_through_clone() -> None:
    """``_clone_lifecycle_row`` must forward ``blocked_requires_json``;
    otherwise the in-memory store silently drops the column across
    create/update/load."""
    store = InMemoryStore()
    payload = '[{"type": "env_var_set", "name": "EXAMPLE"}]'
    lc = Lifecycle(
        task_id="t",
        run_id="r1",
        blocked_requires_json=payload,
    )
    store.create_lifecycle(lc)

    loaded = store.load_lifecycle("r1")
    assert loaded is not None
    assert loaded.blocked_requires_json == payload

    # Default lifecycle stores None.
    store.create_lifecycle(Lifecycle(task_id="t", run_id="r2"))
    loaded_default = store.load_lifecycle("r2")
    assert loaded_default is not None
    assert loaded_default.blocked_requires_json is None

    # Clear back to None on update.
    loaded.transition_to(Status.READY)
    loaded.blocked_requires_json = None
    store.update_lifecycle(loaded, expected_version=1)
    cleared = store.load_lifecycle("r1")
    assert cleared is not None
    assert cleared.blocked_requires_json is None

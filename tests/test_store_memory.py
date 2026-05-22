"""Memory-backend-specific tests for ``flywheel.store_memory.InMemoryStore``.

The shared protocol contract — round-trips, ordering, optimistic
concurrency, append-only enforcement — runs against every backend from
``test_store_contract.py``. Tests here cover behavior that is unique to
the in-memory substrate (no shared state across instances, no durable
persistence) and would not make sense to parametrize over backends.
"""

from __future__ import annotations

from flywheel import InMemoryStore, Lifecycle


def test_two_in_memory_stores_have_independent_state() -> None:
    s1 = InMemoryStore()
    s2 = InMemoryStore()
    s1.create_lifecycle(Lifecycle(task_id="t", run_id="shared"))
    assert s1.load_lifecycle("shared") is not None
    assert s2.load_lifecycle("shared") is None

"""Held-out acceptance test: the task-id-filtered lifecycle query surface.

Criterion under test: ``store.list_lifecycles(task_id=<id>)`` returns exactly
that task's lifecycles in a stable, deterministic ``(updated_at DESC, run_id
DESC)`` order -- most-recently-updated first, ties broken by greater ``run_id``.

``updated_at`` is store-managed: it is NOT a field on ``Lifecycle`` and NOT a
parameter to any store method, so it cannot be injected, read, or forced through
the public API. The store stamps it from its own clock on each WRITE to a row
(``create_lifecycle`` stamps it; a later ``update_lifecycle`` re-stamps it
newer). The only lever over relative recency is therefore to perform a real
later write on the row that should sort first. Recency is asserted via the
RETURNED ORDER, never by reading a timestamp. The ``run_id`` DESC tiebreak is
treated opportunistically: it is asserted only for rows that read back with an
EQUAL store-assigned ``updated_at`` (discovered, never forced); if the store's
clock leaves them distinct, recency governs that pair instead and the test
demands nothing of the tiebreak. The test never fails on a tie the store's
clock refuses to produce.

Authored blind to the implementation, from the declared contract only.
"""

from __future__ import annotations

from flywheel_core import InMemoryStore, Lifecycle, Status

_TASK = "task-under-order"
_OTHER = "task-other"


def _run_ids(result: list[Lifecycle]) -> list[str]:
    return [lc.run_id for lc in result]


def test_list_lifecycles_task_id_filter_order_is_exact_recent_first_deterministic() -> None:
    store = InMemoryStore()

    # Seed three lifecycles for the target task. run_ids are caller-chosen and
    # arranged so the contractual order is distinguishable from accidental
    # orders:
    #   creation order  : a-002, a-009, a-005
    #   run_id ascending: a-002, a-005, a-009
    #   run_id descending: a-009, a-005, a-002
    # After we update a-005 last, the correct head is a-005, which matches NONE
    # of insertion-order, run_id-ascending, or run_id-descending head -- so a
    # plausible-wrong sort is caught.
    lc_002 = Lifecycle(task_id=_TASK, run_id="a-002")
    lc_009 = Lifecycle(task_id=_TASK, run_id="a-009")
    lc_005 = Lifecycle(task_id=_TASK, run_id="a-005")
    store.create_lifecycle(lc_002)
    store.create_lifecycle(lc_009)
    store.create_lifecycle(lc_005)

    # Seed a SECOND task. Its rows must never appear in the _TASK-filtered
    # result. We also give one of them the globally-newest write in the whole
    # store (below), proving the filter is applied independently of recency: a
    # globally-most-recent foreign row must neither leak in nor reorder targets.
    lc_b1 = Lifecycle(task_id=_OTHER, run_id="b-100")
    lc_b2 = Lifecycle(task_id=_OTHER, run_id="b-200")
    store.create_lifecycle(lc_b1)
    store.create_lifecycle(lc_b2)

    # Make a-005 the most-recently-updated TARGET row via a real later write.
    # transition_to mints version 2; update_lifecycle persists it under
    # optimistic concurrency and the store stamps a fresher updated_at. This is
    # the only lever over recency.
    lc_005.transition_to(Status.READY)  # version 1 -> 2
    store.update_lifecycle(lc_005, expected_version=1)

    # Update a FOREIGN row dead last, so the single globally-newest write in the
    # store belongs to task-other. If the filter were ignored or applied after
    # ordering, b-200 would surface (and likely lead) -- the exactness assertion
    # catches that.
    lc_b2.transition_to(Status.READY)  # version 1 -> 2
    store.update_lifecycle(lc_b2, expected_version=1)

    # --- (a) METAMORPHIC DETERMINISM -------------------------------------------
    first = store.list_lifecycles(task_id=_TASK)
    second = store.list_lifecycles(task_id=_TASK)
    first_ids = _run_ids(first)
    second_ids = _run_ids(second)

    assert first_ids == second_ids, (
        "identical task_id-filtered calls must return a byte-identical run_id "
        f"sequence (no run-to-run churn); got {first_ids!r} then {second_ids!r}"
    )
    # Distinguish a real (updated_at DESC, run_id DESC) order from an accidental
    # ascending/insertion order: a-005 was updated last and must lead, so the
    # sequence is neither the ascending nor descending pure run_id sort.
    assert first_ids != sorted(first_ids), (
        "returned order must not be a trivial run_id-ascending sort; the "
        f"most-recently-updated row must lead. got {first_ids!r}"
    )
    assert first_ids != sorted(first_ids, reverse=True), (
        "returned order must not be a trivial run_id-descending sort that "
        f"ignores recency. got {first_ids!r}"
    )

    # --- FILTER EXACTNESS ------------------------------------------------------
    assert all(lc.task_id == _TASK for lc in first), (
        "task_id filter must return only lifecycles for the requested task; "
        f"got task_ids {[lc.task_id for lc in first]!r}"
    )
    assert set(first_ids) == {"a-002", "a-005", "a-009"}, (
        "task_id filter must return exactly the requested task's seeded run_ids "
        f"(no foreign rows, none missing, no dupes); got {first_ids!r}"
    )
    assert len(first_ids) == 3, f"one entry per run_id, no duplicates; got {first_ids!r}"
    assert "b-100" not in first_ids and "b-200" not in first_ids, (
        "foreign-task lifecycles must be absent even when one is the globally "
        f"most-recent write; got {first_ids!r}"
    )

    # --- (b) RECENCY -----------------------------------------------------------
    # a-005 received the real later target write; under (updated_at DESC, ...) it
    # must be FIRST. Asserted via returned order, never via reading updated_at.
    assert first_ids[0] == "a-005", (
        "the most-recently-updated lifecycle for the task must sort first "
        f"(updated_at DESC); got {first_ids!r}"
    )

    # --- (c) run_id DESC TIEBREAK (clock-agnostic, no forced tie) --------------
    # a-002 and a-009 are the two rows that were NEVER separately updated, so
    # their relative recency is fixed entirely by their create writes. We
    # created a-002 STRICTLY BEFORE a-009 (and a-009 has the GREATER run_id), so
    # under the store's own clock a-009's create-stamp is newer-than-or-equal-to
    # a-002's. That pins a single lawful relation under (updated_at DESC, run_id
    # DESC) for EVERY clock resolution:
    #   * if the clock distinguished the two creates, a-009 is the more recent
    #     row and sorts first by RECENCY (relation b);
    #   * if the clock TIED them, the secondary key decides and a-009 -- the
    #     greater run_id -- sorts first by the run_id DESC TIEBREAK (relation c).
    # Either way a-009 precedes a-002. This NEVER fails a correct impl on any
    # clock and NEVER forces a tie; it simply exercises the tiebreak whenever the
    # store happens to tie the two creates, catching a run_id-agnostic sort that
    # would leave a tied pair in backend/insertion order (a-002 before a-009).
    rest = first_ids[1:]
    assert set(rest) == {"a-002", "a-009"}, (
        f"after the most-recent row, exactly the two un-updated rows remain; got {rest!r}"
    )
    assert rest.index("a-009") < rest.index("a-002"), (
        "the later-created, greater-run_id row (a-009) must precede the "
        "earlier-created, smaller-run_id row (a-002): a-009 wins by recency if "
        "the store's clock distinguished the creates, and by the run_id DESC "
        f"tiebreak if it tied them. got {rest!r}"
    )


def test_list_lifecycles_task_id_filter_unknown_task_is_empty_and_deterministic() -> None:
    store = InMemoryStore()
    store.create_lifecycle(Lifecycle(task_id="present", run_id="p-1"))

    first = store.list_lifecycles(task_id="absent")
    second = store.list_lifecycles(task_id="absent")

    assert _run_ids(first) == [], f"unknown task_id must yield empty list; got {first!r}"
    assert _run_ids(second) == [], "repeated unknown-task_id calls must agree (empty)"

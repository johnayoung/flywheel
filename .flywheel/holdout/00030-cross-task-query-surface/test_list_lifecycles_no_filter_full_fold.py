"""Held-out acceptance test for the unfiltered cross-task lifecycle query.

Criterion: ``store.list_lifecycles()`` (no status, no task_id filter) returns
EVERY lifecycle the store holds -- one entry per run_id -- and each entry is a
fully-folded ``Lifecycle`` whose ``.attempts`` matches what
``load_lifecycle(run_id)`` yields (ascending number order, populated, not a
stub shape).

This is authored blind to the implementation, from the declared contract only.
It defends against two narrow implementations:

  * one that returns only currently-active rows (e.g. RUNNING) and drops
    terminal ones -- killed by the run_id MEMBERSHIP assertion over a mix of
    active and terminal statuses;
  * one that returns lifecycle rows with empty/None ``.attempts`` (a stub
    shape) instead of the same fully-folded object ``load_lifecycle`` yields --
    killed by the MODEL-EQUIVALENCE assertion on the multi-attempt run.

No ordering is asserted (the contract does not state list order).
"""

from __future__ import annotations

from datetime import datetime, timezone

from flywheel_core import Attempt, InMemoryStore, Lifecycle, Status


def test_list_lifecycles_no_filter_returns_all_runs_fully_folded() -> None:
    store = InMemoryStore()
    base = datetime(2024, 1, 1, tzinfo=timezone.utc)

    # Seed N=4 runs in a MIX of statuses (active + terminal) with DIFFERING
    # attempt counts: one with 0 attempts, one with 1, two with >=2.
    #   r-running : RUNNING            (active), 2 attempts
    #   r-done    : DONE               (terminal), 0 attempts
    #   r-failed  : FAILED             (terminal), 3 attempts  <- multi-attempt oracle
    #   r-ready   : READY              (active), 1 attempt
    seeded = {
        "r-running": Status.RUNNING,
        "r-done": Status.DONE,
        "r-failed": Status.FAILED,
        "r-ready": Status.READY,
    }
    for run_id, status in seeded.items():
        store.create_lifecycle(
            Lifecycle(task_id="t", run_id=run_id, status=status)
        )

    # r-running: 2 attempts (insertion order deliberately not ascending).
    store.save_attempt(
        "r-running", Attempt(number=2, started_at=base, run_id="r-running")
    )
    store.save_attempt(
        "r-running", Attempt(number=1, started_at=base, run_id="r-running")
    )
    # r-failed: 3 attempts, inserted out of order; this is the multi-attempt
    # run used for the model-equivalence comparison below.
    store.save_attempt(
        "r-failed", Attempt(number=3, started_at=base, run_id="r-failed")
    )
    store.save_attempt(
        "r-failed", Attempt(number=1, started_at=base, run_id="r-failed")
    )
    store.save_attempt(
        "r-failed", Attempt(number=2, started_at=base, run_id="r-failed")
    )
    # r-ready: a single attempt.
    store.save_attempt(
        "r-ready", Attempt(number=1, started_at=base, run_id="r-ready")
    )
    # r-done: intentionally zero attempts.

    result = store.list_lifecycles()

    # (a) MEMBERSHIP: every seeded run_id is present, exactly once. Set
    # equality (no ordering assumed). This kills 'returns only active rows',
    # 'returns only the first row', and 'drops zero-attempt runs'.
    run_ids = [lc.run_id for lc in result]
    assert set(run_ids) == set(seeded)
    assert len(run_ids) == len(seeded), "one entry per run_id, no duplicates"

    # (b) MODEL EQUIVALENCE: the multi-attempt run's entry from the list must
    # carry the SAME folded attempts that load_lifecycle yields -- not an
    # empty/None stub. This is the model-equivalence relation against the
    # declared oracle.
    listed_failed = next(lc for lc in result if lc.run_id == "r-failed")
    oracle = store.load_lifecycle("r-failed")
    assert oracle is not None
    assert listed_failed.attempts == oracle.attempts
    # Belt-and-braces: the multi-attempt run is genuinely populated (a stub
    # that returned [] on BOTH sides would otherwise sneak past equality).
    assert [a.number for a in listed_failed.attempts] == [1, 2, 3]

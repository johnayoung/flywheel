"""Held-out acceptance test for the cross-task query surface (spec 00030).

CRITERION: holding only the database file -- no WorkSource, no task files on
disk -- a caller can enumerate every lifecycle and its status entirely through
the public protocol.

This test seeds several lifecycles directly into a file-backed ``SqliteStore``
in distinct statuses, closes and reopens the store on the SAME file, then calls
``list_lifecycles()`` and asserts the returned ``(run_id, status)`` pairs match
the seeded rows. The db lives under ``tmp_path`` -- an isolated, empty temp
directory -- so there is provably no ``.flywheel/tasks`` tree adjacent to it.
A solution that still needs the on-disk task source to enumerate (the audit-Q1
failure), or that reads task files behind the protocol, cannot pass: the answer
is read purely from store rows.

Authored BLIND to the implementation, from the contract alone.
"""

from __future__ import annotations

from flywheel_core import Lifecycle, SqliteStore, Status


def test_list_lifecycles_reads_every_run_and_status_from_store_file_only(
    tmp_path,
) -> None:
    db_path = tmp_path / "seeded.db"

    # Seed several lifecycles in distinct statuses, each as its own run_id.
    # ``status=`` seeds the row directly without driving transitions.
    seeded = {
        ("r-pending", Status.PENDING),
        ("r-ready", Status.READY),
        ("r-running", Status.RUNNING),
        ("r-validating", Status.VALIDATING),
        ("r-awaiting", Status.AWAITING_APPROVAL),
        ("r-failed-val", Status.FAILED_VALIDATION),
        ("r-internal", Status.INTERNAL_ERROR),
        ("r-done", Status.DONE),
        ("r-failed", Status.FAILED),
        ("r-interrupted", Status.INTERRUPTED),
    }

    store = SqliteStore(db_path)
    try:
        for run_id, status in seeded:
            store.create_lifecycle(
                Lifecycle(task_id="t", run_id=run_id, status=status)
            )
    finally:
        store.close()

    # Guard: only the db file is present. There is provably no on-disk task
    # tree beside it, so any enumeration that reaches for one cannot succeed.
    assert not (tmp_path / ".flywheel" / "tasks").exists()

    # Reopen a fresh store on the SAME file -- the caller holds only the db.
    reopened = SqliteStore(db_path)
    try:
        result = reopened.list_lifecycles()
    finally:
        reopened.close()

    # The load-bearing assertion: enumeration answers entirely from store
    # rows. Compare the SET of (run_id, status) pairs; ordering is unasserted.
    assert {(lc.run_id, lc.status) for lc in result} == seeded

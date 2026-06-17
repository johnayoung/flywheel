"""Held-out acceptance test for the cross-task query surface (spec 00030).

Criterion: a store opened on an existing schema-v12 database can answer the
lifecycle-list query WITHOUT requiring a schema re-create. This test defends
against smuggling a breaking schema migration (a new column or a bumped
CURRENT_SCHEMA_VERSION) under a feature that needs none -- such a migration
would force every existing adopter to re-create their store, and would make
reopening a store written by an earlier (matching) store raise
StoreSchemaError.

Written blind from the declared contract only. No raw SQL; the store is the
only surface under test.
"""

from flywheel_core import (
    CURRENT_SCHEMA_VERSION,
    Lifecycle,
    SqliteStore,
    Status,
)


def test_list_lifecycles_reopen_v12_no_recreate(tmp_path) -> None:
    # (a) The schema-version pin. This feature adds NO schema change, so the
    #     module-level constant must still equal its pre-feature value of 12.
    #     A smuggled migration that bumps this is caught here directly.
    assert CURRENT_SCHEMA_VERSION == 12

    db_path = tmp_path / "v12.db"
    run_id = "r-reopen-1"

    # Write a lifecycle through a store, then close it so the on-disk database
    # is the only state that survives to the reopen.
    s1 = SqliteStore(db_path)
    try:
        s1.create_lifecycle(
            Lifecycle(task_id="t-reopen", run_id=run_id, status=Status.PENDING)
        )
    finally:
        s1.close()

    # (b) The load-bearing discriminator: reopen a FRESH store on the same file.
    #     This MUST NOT raise StoreSchemaError. We deliberately do NOT wrap this
    #     in a try/except that swallows -- a smuggled schema bump (constant moved
    #     or on-disk schema changed) makes this line raise and fail the test.
    s2 = SqliteStore(db_path)
    try:
        rows = s2.list_lifecycles()

        # The prior lifecycle must come back. A migration that re-creates or
        # truncates the store on reopen, or that adds a NOT-NULL column so the
        # old row no longer reads back, loses this run_id and fails here.
        run_ids = {lc.run_id for lc in rows}
        assert run_id in run_ids

        # The returned object is a folded Lifecycle with the seeded status.
        seeded = next(lc for lc in rows if lc.run_id == run_id)
        assert seeded.status is Status.PENDING
    finally:
        s2.close()

"""Held-out acceptance test for spec 00032 (postgres read-path parity).

Criterion: when the postgres-backed store is built for a read verb, the
store-construction seam ``open_sqlite_bound_store`` RETURNS a usable
(un-closed) ``PostgresStore`` to the caller -- it does NOT close the store
and raise ``StoreConfigError``.

This test is BLIND to the implementation. It asserts only the declared
observable contract: with a reachable DSN supplied via ``PG_DSN_ENV`` and a
postgres ``WorkPolicy``, the seam returns a ``PostgresStore`` that is usable
for a read (``load_lifecycle(<missing-run-id>) is None`` without raising), and
raises no ``StoreConfigError``.

Discriminator (single call + use):
  - A refusal (raise ``StoreConfigError`` -- possibly with a widened message)
    fails the ``pytest.raises`` guard / the no-raise assertion.
  - A closed store fails when the read is attempted.
  - A ``None`` return / sqlite fallback fails the ``isinstance`` assertion.

Skips (does not fail) when no Postgres DB is reachable, via the
session-scoped ``require_postgres`` fixture.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from flywheel_core import PostgresStore
from flywheel_orchestrator._policy import WorkPolicy
from flywheel_orchestrator._store_factory import (
    PG_DSN_ENV,
    StoreConfigError,
    open_sqlite_bound_store,
)


def _postgres_policy() -> WorkPolicy:
    return WorkPolicy(
        source_kind="directory",
        tasks_dir=Path(".flywheel/tasks"),
        store_backend="postgres",
        store_schema=None,
    )


def test_seam_returns_usable_postgres_store(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    require_postgres: str,
) -> None:
    db_path = tmp_path / "x.sqlite"
    monkeypatch.setenv(PG_DSN_ENV, require_postgres)

    # The seam must NOT refuse a postgres policy: no StoreConfigError here.
    try:
        store = open_sqlite_bound_store(_postgres_policy(), db_path=db_path)
    except StoreConfigError as exc:  # pragma: no cover - failure path
        pytest.fail(
            "open_sqlite_bound_store refused a postgres policy with "
            f"StoreConfigError instead of returning a PostgresStore: {exc}"
        )

    try:
        # It returns an actual PostgresStore (not None, not a sqlite fallback).
        assert isinstance(store, PostgresStore)
        # The returned store is USABLE -- i.e. it was not closed before return.
        # A closed store would raise on this read; a working one returns None
        # for an unknown run id.
        assert store.load_lifecycle("run-00032-holdout-missing") is None
        # The sqlite db_path must not have been created for a postgres backend.
        assert not db_path.exists()
    finally:
        store.close()

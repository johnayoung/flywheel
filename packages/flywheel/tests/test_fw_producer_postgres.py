"""Under ``[store] backend = postgres`` the ``fw`` producer verbs persist
their control command into the configured Postgres database and print the
standard enqueue receipt -- instead of failing against a sqlite file (or
dual-writing one).

Every case drives the product CLI entrypoint (:func:`flywheel._cli.main`
with the operator's verb argv) so the whole store-routing seam is exercised
end to end: the router loads the postgres ``[store]`` policy, builds the
backend from the DSN environment, and injects it into core's enqueue path.
The command row is then read back over an independent connection -- never by
calling the store's ``enqueue_command`` directly.

Requires a reachable Postgres (Docker / testcontainers / the ``postgres``
extra); the root conftest's session-scoped container provides it, and each
case is skipped only when no database is reachable.
"""

from __future__ import annotations

from datetime import datetime, timezone
from pathlib import Path
from typing import Any
from uuid import uuid4

import pytest

from flywheel_core.lifecycle import Lifecycle, Status
from flywheel_orchestrator import load_effective_policy, resolve_db_path

from flywheel._cli import main


# --- helpers ---------------------------------------------------------------


def _open_store(dsn: str, schema: str) -> Any:
    """Open a PostgresStore bound to ``schema`` (bootstraps it if new).

    Imported lazily so a suite run without the ``postgres`` extra fails as
    a skip (via ``require_postgres``) rather than a collection error.
    """
    from flywheel_core import PostgresStore

    return PostgresStore(dsn, schema=schema, pool_min=1, pool_max=2)


def _write_policy(
    tmp_path: Path, schema: str, *, db_rel: str = ".flywheel/flywheel.sqlite"
) -> None:
    """Write a postgres ``[store]`` policy into ``tmp_path/flywheel.toml``."""
    (tmp_path / "flywheel.toml").write_text(
        '[source]\n'
        'kind = "directory"\n'
        '[store]\n'
        'backend = "postgres"\n'
        f'schema = "{schema}"\n'
        '[paths]\n'
        f'db = "{db_rel}"\n',
        encoding="utf-8",
    )


def _seed_running(store: Any, task_id: str) -> str:
    """Persist a RUNNING lifecycle; return its run_id."""
    now = datetime.now(timezone.utc)
    lc = Lifecycle(task_id=task_id, run_id=f"run-{task_id}-running")
    lc.transition_to(Status.READY, now=now)
    lc.transition_to(Status.RUNNING, now=now)
    store.create_lifecycle(lc)
    return lc.run_id


def _seed_awaiting(store: Any, task_id: str) -> str:
    """Persist an AWAITING_APPROVAL lifecycle; return its run_id."""
    now = datetime.now(timezone.utc)
    lc = Lifecycle(task_id=task_id, run_id=f"run-{task_id}-awaiting")
    lc.transition_to(Status.READY, now=now)
    lc.transition_to(Status.RUNNING, now=now)
    lc.transition_to(Status.VALIDATING, now=now)
    lc.transition_to(Status.AWAITING_APPROVAL, now=now)
    store.create_lifecycle(lc)
    return lc.run_id


def _claimed_kinds(dsn: str, schema: str, run_id: str) -> list[str]:
    """Kinds of the commands persisted for ``run_id`` (fresh connection)."""
    store = _open_store(dsn, schema)
    try:
        claimed = store.claim_commands(run_id, now=datetime.now(timezone.utc))
    finally:
        store.close()
    return [c.kind for c in claimed]


def _resolved_db(tmp_path: Path) -> Path:
    """The db path the router resolves from the on-disk policy."""
    policy = load_effective_policy(None)
    return resolve_db_path(None, policy=policy)


# --- postgres-backed producer verbs ----------------------------------------


def test_fw_approve_lands_in_postgres_and_touches_no_sqlite(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    require_postgres: str,
) -> None:
    """``fw approve RUN_ID`` against an AWAITING_APPROVAL run enqueues a
    ``kind=approve`` row into postgres, prints the receipt with no
    stale-pending warning, and writes no sqlite file."""
    dsn = require_postgres
    schema = f"flywheel_fwprod_{uuid4().hex[:12]}"
    monkeypatch.setenv("FLYWHEEL_PG_DSN", dsn)
    monkeypatch.delenv("DATABASE_URL", raising=False)
    monkeypatch.chdir(tmp_path)
    _write_policy(tmp_path, schema)

    store = _open_store(dsn, schema)
    try:
        run_id = _seed_awaiting(store, "task-a")
    finally:
        store.close()

    rc = main(["approve", run_id])
    assert rc == 0
    captured = capsys.readouterr()
    assert f"kind=approve run_id={run_id}" in captured.out
    # AWAITING_APPROVAL is the valid in-flight status for approve.
    assert "not in-flight" not in captured.err

    assert _claimed_kinds(dsn, schema, run_id) == ["approve"]
    assert not _resolved_db(tmp_path).exists()


def test_fw_interrupt_lands_in_postgres_and_touches_no_sqlite(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    require_postgres: str,
) -> None:
    """``fw interrupt RUN_ID`` against a RUNNING run enqueues a
    ``kind=interrupt`` row into postgres and writes no sqlite file."""
    dsn = require_postgres
    schema = f"flywheel_fwprod_{uuid4().hex[:12]}"
    monkeypatch.setenv("FLYWHEEL_PG_DSN", dsn)
    monkeypatch.delenv("DATABASE_URL", raising=False)
    monkeypatch.chdir(tmp_path)
    _write_policy(tmp_path, schema)

    store = _open_store(dsn, schema)
    try:
        run_id = _seed_running(store, "task-i")
    finally:
        store.close()

    rc = main(["interrupt", run_id])
    assert rc == 0
    captured = capsys.readouterr()
    assert f"kind=interrupt run_id={run_id}" in captured.out
    # RUNNING is in-flight for interrupt -- no stale-pending warning.
    assert "not in-flight" not in captured.err

    assert _claimed_kinds(dsn, schema, run_id) == ["interrupt"]
    assert not _resolved_db(tmp_path).exists()


def test_fw_say_lands_in_postgres_with_payload(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    require_postgres: str,
) -> None:
    """``fw say RUN_ID MSG`` (core's ``steer``) enqueues a ``kind=say``
    row carrying the message text into postgres, with no sqlite file."""
    dsn = require_postgres
    schema = f"flywheel_fwprod_{uuid4().hex[:12]}"
    monkeypatch.setenv("FLYWHEEL_PG_DSN", dsn)
    monkeypatch.delenv("DATABASE_URL", raising=False)
    monkeypatch.chdir(tmp_path)
    _write_policy(tmp_path, schema)

    store = _open_store(dsn, schema)
    try:
        run_id = _seed_running(store, "task-s")
    finally:
        store.close()

    rc = main(["say", run_id, "double-check the rubric finding"])
    assert rc == 0
    assert f"kind=say run_id={run_id}" in capsys.readouterr().out

    read = _open_store(dsn, schema)
    try:
        claimed = read.claim_commands(run_id, now=datetime.now(timezone.utc))
    finally:
        read.close()
    assert len(claimed) == 1
    assert claimed[0].kind == "say"
    assert claimed[0].payload == {"text": "double-check the rubric finding"}
    assert not _resolved_db(tmp_path).exists()


def test_fw_reject_not_in_flight_note_survives_under_postgres(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    require_postgres: str,
) -> None:
    """``fw reject`` treats only AWAITING_APPROVAL as in-flight, so a reject
    against a RUNNING run still enqueues (into postgres) but fires the
    stale-pending stderr note -- the note must survive the store port."""
    dsn = require_postgres
    schema = f"flywheel_fwprod_{uuid4().hex[:12]}"
    monkeypatch.setenv("FLYWHEEL_PG_DSN", dsn)
    monkeypatch.delenv("DATABASE_URL", raising=False)
    monkeypatch.chdir(tmp_path)
    _write_policy(tmp_path, schema)

    store = _open_store(dsn, schema)
    try:
        run_id = _seed_running(store, "task-r")
    finally:
        store.close()

    rc = main(["reject", run_id, "--feedback", "missing rollback"])
    assert rc == 0
    captured = capsys.readouterr()
    assert f"kind=reject run_id={run_id}" in captured.out
    # RUNNING is not in reject's in-flight set (AWAITING_APPROVAL only).
    assert "not in-flight" in captured.err
    assert "running" in captured.err

    read = _open_store(dsn, schema)
    try:
        claimed = read.claim_commands(run_id, now=datetime.now(timezone.utc))
    finally:
        read.close()
    assert len(claimed) == 1
    assert claimed[0].kind == "reject"
    assert claimed[0].payload == {"feedback": "missing rollback"}
    assert not _resolved_db(tmp_path).exists()


def test_fw_approve_unknown_run_exits_two_and_persists_nothing(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    require_postgres: str,
) -> None:
    """An unknown run under postgres is a producer-side error: exit 2 with
    the 'unknown to this store' message, nothing enqueued, no sqlite file."""
    dsn = require_postgres
    schema = f"flywheel_fwprod_{uuid4().hex[:12]}"
    monkeypatch.setenv("FLYWHEEL_PG_DSN", dsn)
    monkeypatch.delenv("DATABASE_URL", raising=False)
    monkeypatch.chdir(tmp_path)
    _write_policy(tmp_path, schema)

    # Bootstrap the schema so the store is reachable but the run is absent.
    _open_store(dsn, schema).close()

    rc = main(["approve", "run-ghost"])
    assert rc == 2
    err = capsys.readouterr().err
    assert "unknown to this store" in err
    assert "run-ghost" in err

    assert _claimed_kinds(dsn, schema, "run-ghost") == []
    assert not _resolved_db(tmp_path).exists()


def test_fw_producer_postgres_without_dsn_fails_loud(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    require_postgres: str,
) -> None:
    """``backend = postgres`` with neither DSN env var set fails fast
    (exit 2) via the factory's StoreConfigError -- never a silent sqlite
    fallback, so no sqlite file is created."""
    schema = f"flywheel_fwprod_{uuid4().hex[:12]}"
    monkeypatch.delenv("FLYWHEEL_PG_DSN", raising=False)
    monkeypatch.delenv("DATABASE_URL", raising=False)
    monkeypatch.chdir(tmp_path)
    _write_policy(tmp_path, schema)

    rc = main(["approve", "run-anything"])
    assert rc == 2
    err = capsys.readouterr().err
    assert "postgres" in err
    assert "FLYWHEEL_PG_DSN" in err
    assert not _resolved_db(tmp_path).exists()


def test_fw_producer_prefers_flywheel_pg_dsn_over_database_url(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    require_postgres: str,
) -> None:
    """``FLYWHEEL_PG_DSN`` wins over ``DATABASE_URL``: with the real DSN in
    the flywheel var and a bogus one in the fallback, the enqueue still
    lands in the reachable database."""
    dsn = require_postgres
    schema = f"flywheel_fwprod_{uuid4().hex[:12]}"
    monkeypatch.setenv("FLYWHEEL_PG_DSN", dsn)
    monkeypatch.setenv(
        "DATABASE_URL", "postgresql://nobody@127.0.0.1:1/does-not-exist"
    )
    monkeypatch.chdir(tmp_path)
    _write_policy(tmp_path, schema)

    store = _open_store(dsn, schema)
    try:
        run_id = _seed_running(store, "task-w")
    finally:
        store.close()

    rc = main(["interrupt", run_id])
    assert rc == 0
    assert f"kind=interrupt run_id={run_id}" in capsys.readouterr().out
    assert _claimed_kinds(dsn, schema, run_id) == ["interrupt"]
    assert not _resolved_db(tmp_path).exists()

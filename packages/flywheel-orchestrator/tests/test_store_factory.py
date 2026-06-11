"""Tests for the store factory (spec 00024 FR-6/FR-8).

Pure resolution logic runs against injected environment mappings; postgres
construction tests reuse the session-scoped test container from the root
``conftest.py`` and skip when no database is reachable.
"""

from __future__ import annotations

import sys
from pathlib import Path

import pytest

from flywheel_core.store_sqlite import SqliteStore
from flywheel_orchestrator._policy import PolicyError, WorkPolicy
from flywheel_orchestrator._store_factory import (
    PG_DSN_ENV,
    PG_DSN_FALLBACK_ENV,
    StoreConfigError,
    build_store,
    open_sqlite_bound_store,
    resolve_postgres_dsn,
)
from flywheel_orchestrator._workflow import main as orch_main


def _policy(
    backend: str = "sqlite", schema: str | None = None
) -> WorkPolicy:
    return WorkPolicy(
        source_kind="directory",
        tasks_dir=Path(".flywheel/tasks"),
        store_backend=backend,
        store_schema=schema,
    )


def _clear_dsn_env(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv(PG_DSN_ENV, raising=False)
    monkeypatch.delenv(PG_DSN_FALLBACK_ENV, raising=False)


# --- DSN resolution ----------------------------------------------------------


def test_resolve_dsn_primary_wins_silently() -> None:
    env = {PG_DSN_ENV: "postgresql://a/db", PG_DSN_FALLBACK_ENV: "postgresql://b/db"}
    assert resolve_postgres_dsn(env) == "postgresql://a/db"


def test_resolve_dsn_falls_back_to_database_url() -> None:
    env = {PG_DSN_FALLBACK_ENV: "postgresql://b/db"}
    assert resolve_postgres_dsn(env) == "postgresql://b/db"


def test_resolve_dsn_none_when_unset() -> None:
    assert resolve_postgres_dsn({}) is None


def test_resolve_dsn_treats_empty_values_as_unset() -> None:
    env = {PG_DSN_ENV: "", PG_DSN_FALLBACK_ENV: "  "}
    assert resolve_postgres_dsn(env) is None
    env = {PG_DSN_ENV: "", PG_DSN_FALLBACK_ENV: "postgresql://b/db"}
    assert resolve_postgres_dsn(env) == "postgresql://b/db"


# --- sqlite backend (and absent policy) --------------------------------------


def test_no_policy_builds_sqlite_at_path(tmp_path: Path) -> None:
    db_path = tmp_path / "state" / "flywheel.sqlite"
    db_path.parent.mkdir(parents=True)
    store = build_store(None, db_path=db_path)
    try:
        assert isinstance(store, SqliteStore)
        assert db_path.exists()
    finally:
        store.close()


def test_sqlite_backend_builds_sqlite_at_path(tmp_path: Path) -> None:
    db_path = tmp_path / "flywheel.sqlite"
    store = build_store(_policy("sqlite"), db_path=db_path)
    try:
        assert isinstance(store, SqliteStore)
        assert db_path.exists()
    finally:
        store.close()


def test_open_sqlite_bound_store_passes_sqlite_through(
    tmp_path: Path,
) -> None:
    db_path = tmp_path / "flywheel.sqlite"
    store = open_sqlite_bound_store(_policy("sqlite"), db_path=db_path)
    try:
        assert isinstance(store, SqliteStore)
    finally:
        store.close()


def test_sqlite_backend_ignores_dsn_env(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # A set DSN must not promote a sqlite policy to postgres.
    monkeypatch.setenv(PG_DSN_ENV, "postgresql://nope@127.0.0.1:1/nope")
    store = build_store(_policy("sqlite"), db_path=tmp_path / "x.sqlite")
    try:
        assert isinstance(store, SqliteStore)
    finally:
        store.close()


# --- postgres fail-fast (FR-8) -----------------------------------------------


def test_postgres_without_env_vars_fails_fast_naming_both(
    tmp_path: Path,
) -> None:
    with pytest.raises(StoreConfigError) as excinfo:
        build_store(_policy("postgres"), db_path=tmp_path / "x.sqlite", environ={})
    message = str(excinfo.value)
    assert PG_DSN_ENV in message
    assert PG_DSN_FALLBACK_ENV in message
    # The CLI's existing PolicyError handling must apply unchanged.
    assert isinstance(excinfo.value, PolicyError)


def test_postgres_missing_extra_names_install_command(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    dsn = "postgresql://flywheel:hunter2@dbhost:5432/flywheel"
    # Poisoning the module entry makes ``from flywheel_core.store_postgres
    # import PostgresStore`` raise ImportError even when the extra is
    # installed in this environment.
    monkeypatch.setitem(sys.modules, "flywheel_core.store_postgres", None)
    with pytest.raises(StoreConfigError) as excinfo:
        build_store(
            _policy("postgres"),
            db_path=tmp_path / "x.sqlite",
            environ={PG_DSN_ENV: dsn},
        )
    message = str(excinfo.value)
    assert "uv add 'flywheel[postgres]'" in message
    # The DSN (and especially its password) never appears in the error.
    assert dsn not in message
    assert "hunter2" not in message


def test_cli_status_postgres_without_env_vars_exits_2(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    """Any store-using verb fails fast with a message naming both vars."""
    _clear_dsn_env(monkeypatch)
    monkeypatch.chdir(tmp_path)
    (tmp_path / "flywheel.toml").write_text(
        '[source]\nkind = "directory"\n\n[store]\nbackend = "postgres"\n',
        encoding="utf-8",
    )
    assert orch_main(["status"]) == 2
    err = capsys.readouterr().err
    assert PG_DSN_ENV in err
    assert PG_DSN_FALLBACK_ENV in err


# --- postgres construction (test container) ----------------------------------


def test_postgres_backend_builds_postgres_store(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    require_postgres: str,
) -> None:
    from flywheel_core.store_postgres import PostgresStore

    monkeypatch.setenv(PG_DSN_ENV, require_postgres)
    store = build_store(_policy("postgres"), db_path=tmp_path / "x.sqlite")
    try:
        assert isinstance(store, PostgresStore)
        # Round-trip through a protocol method to prove the store works.
        assert store.load_lifecycle("run-store-factory-missing") is None
        # The sqlite path must not have been touched.
        assert not (tmp_path / "x.sqlite").exists()
    finally:
        store.close()


def test_postgres_primary_env_wins_over_garbage_fallback(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    require_postgres: str,
) -> None:
    from flywheel_core.store_postgres import PostgresStore

    monkeypatch.setenv(PG_DSN_ENV, require_postgres)
    monkeypatch.setenv(
        PG_DSN_FALLBACK_ENV, "postgresql://nope@127.0.0.1:1/nope"
    )
    store = build_store(_policy("postgres"), db_path=tmp_path / "x.sqlite")
    try:
        assert isinstance(store, PostgresStore)
    finally:
        store.close()


def test_postgres_schema_passes_through(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    require_postgres: str,
) -> None:
    from flywheel_core.store_postgres import PostgresStore

    monkeypatch.setenv(PG_DSN_ENV, require_postgres)
    store = build_store(
        _policy("postgres", schema="fw_store_factory"),
        db_path=tmp_path / "x.sqlite",
    )
    try:
        assert isinstance(store, PostgresStore)
        assert store._schema == "fw_store_factory"  # noqa: SLF001
    finally:
        store.close()


def test_open_sqlite_bound_store_refuses_postgres(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    require_postgres: str,
) -> None:
    monkeypatch.setenv(PG_DSN_ENV, require_postgres)
    with pytest.raises(StoreConfigError) as excinfo:
        open_sqlite_bound_store(
            _policy("postgres"), db_path=tmp_path / "x.sqlite"
        )
    assert "postgres store backend" in str(excinfo.value)


# --- connection failures propagate from the store ----------------------------


def test_postgres_unreachable_db_propagates_store_error(
    tmp_path: Path,
) -> None:
    psycopg = pytest.importorskip("psycopg")
    dsn = "postgresql://flywheel@127.0.0.1:1/flywheel?connect_timeout=1"
    with pytest.raises(psycopg.OperationalError):
        build_store(
            _policy("postgres"),
            db_path=tmp_path / "x.sqlite",
            environ={PG_DSN_ENV: dsn},
        )


def test_database_url_with_non_postgres_scheme_is_a_clear_failure(
    tmp_path: Path,
) -> None:
    """A mysql DATABASE_URL fails at connect -- never a silent sqlite fallback."""
    psycopg = pytest.importorskip("psycopg")
    db_path = tmp_path / "x.sqlite"
    with pytest.raises(psycopg.Error):
        build_store(
            _policy("postgres"),
            db_path=db_path,
            environ={PG_DSN_FALLBACK_ENV: "mysql://user@dbhost:3306/app"},
        )
    assert not db_path.exists()

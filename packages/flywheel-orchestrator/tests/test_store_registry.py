"""Tests for the store backend selection seam (the STORES registry).

These prove the registry-dispatch behavior and the uniform builder
contract that :mod:`test_store_factory` does not already cover: that the
two built-in names resolve to the real builder functions, that an unknown
name surfaces the shared :class:`UnknownPluginError`, that the sqlite path
never touches the postgres extra, and that the postgres builder passes the
policy schema through to the store constructor. The schema-passthrough and
missing-extra cases use a fake module injected into ``sys.modules`` (a test
double, restored automatically by monkeypatch) so no real database is
needed.
"""

from __future__ import annotations

import sys
from pathlib import Path
from types import ModuleType

import pytest

from flywheel_core._registry import UnknownPluginError
from flywheel_core.store_sqlite import SqliteStore
from flywheel_orchestrator._policy import PolicyError, WorkPolicy
from flywheel_orchestrator._store_factory import (
    StoreConfigError,
    build_postgres_store,
    build_sqlite_store,
    build_store,
    resolve_postgres_dsn,
)
from flywheel_orchestrator._store_factory import (
    PG_DSN_ENV,
    PG_DSN_FALLBACK_ENV,
)
from flywheel_orchestrator._store_registry import STORES


def _policy(backend: str = "sqlite", schema: str | None = None) -> WorkPolicy:
    return WorkPolicy(
        source_kind="directory",
        tasks_dir=Path(".flywheel/tasks"),
        store_backend=backend,
        store_schema=schema,
    )


# --- registry membership and dispatch ----------------------------------------


def test_registry_names_are_sqlite_then_postgres() -> None:
    assert STORES.names() == ("sqlite", "postgres")


def test_registry_resolves_sqlite_to_its_builder() -> None:
    assert STORES.resolve("sqlite") is build_sqlite_store


def test_registry_resolves_postgres_to_its_builder() -> None:
    # ``postgres`` carries extra="postgres"; resolving it imports the
    # builder's own module (always importable) -- the gated import is inside
    # the builder, not the registry target, so this resolves cleanly.
    assert STORES.resolve("postgres") is build_postgres_store


def test_registry_unknown_name_lists_known_backends() -> None:
    with pytest.raises(UnknownPluginError) as excinfo:
        STORES.resolve("nope")
    message = str(excinfo.value)
    assert "nope" in message
    assert "sqlite" in message
    assert "postgres" in message


# --- build_store sqlite path (no extra required) ------------------------------


def test_build_store_none_policy_yields_sqlite(tmp_path: Path) -> None:
    store = build_store(None, db_path=tmp_path / "x.sqlite")
    try:
        assert isinstance(store, SqliteStore)
    finally:
        store.close()


def test_build_store_sqlite_policy_yields_sqlite(tmp_path: Path) -> None:
    store = build_store(_policy("sqlite"), db_path=tmp_path / "x.sqlite")
    try:
        assert isinstance(store, SqliteStore)
    finally:
        store.close()


def test_sqlite_path_does_not_import_postgres_extra(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Poison the postgres module so any attempt to import it raises. The
    # sqlite path must never touch it.
    monkeypatch.setitem(sys.modules, "flywheel_core.store_postgres", None)
    store = build_store(_policy("sqlite"), db_path=tmp_path / "x.sqlite")
    try:
        assert isinstance(store, SqliteStore)
    finally:
        store.close()


# --- uniform builder contract -------------------------------------------------


def test_sqlite_builder_ignores_policy_and_environ(tmp_path: Path) -> None:
    # The builder must not re-dispatch on the policy: handed a postgres-ish
    # policy and a DSN environ, it still constructs a SqliteStore because the
    # registry already picked it. The signature args are accepted and ignored.
    store = build_sqlite_store(
        _policy("postgres", schema="should_be_ignored"),
        db_path=tmp_path / "x.sqlite",
        environ={PG_DSN_ENV: "postgresql://user:pw@host/db"},
    )
    try:
        assert isinstance(store, SqliteStore)
    finally:
        store.close()


def test_resolved_builders_share_call_signature(tmp_path: Path) -> None:
    # Structural check: both resolved builders accept (policy, *, db_path,
    # environ). sqlite constructs; postgres reaches its no-DSN fail-fast --
    # proving the call shape is identical without needing a database.
    sqlite_builder = STORES.resolve("sqlite")
    postgres_builder = STORES.resolve("postgres")

    sqlite_store = sqlite_builder(
        _policy("sqlite"), db_path=tmp_path / "x.sqlite", environ={}
    )
    try:
        assert isinstance(sqlite_store, SqliteStore)
    finally:
        sqlite_store.close()

    with pytest.raises(StoreConfigError):
        postgres_builder(
            _policy("postgres"), db_path=tmp_path / "x.sqlite", environ={}
        )


# --- postgres fail-fast (registry-routed) ------------------------------------


def test_postgres_no_dsn_raises_store_config_error_naming_both(
    tmp_path: Path,
) -> None:
    with pytest.raises(StoreConfigError) as excinfo:
        build_store(
            _policy("postgres"), db_path=tmp_path / "x.sqlite", environ={}
        )
    message = str(excinfo.value)
    assert PG_DSN_ENV in message
    assert PG_DSN_FALLBACK_ENV in message
    # CLI exit-2 contract: StoreConfigError is a PolicyError.
    assert isinstance(excinfo.value, PolicyError)


def test_postgres_missing_extra_hints_install_without_leaking_dsn(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    dsn = "postgresql://flywheel:hunter2@dbhost:5432/flywheel"
    # Setting the module to None makes importing it raise ImportError, which
    # import_extra brands as MissingExtraError -> StoreConfigError.
    monkeypatch.setitem(sys.modules, "flywheel_core.store_postgres", None)
    with pytest.raises(StoreConfigError) as excinfo:
        build_store(
            _policy("postgres"),
            db_path=tmp_path / "x.sqlite",
            environ={PG_DSN_ENV: dsn},
        )
    message = str(excinfo.value)
    assert "uv add 'flywheel[postgres]'" in message
    assert dsn not in message
    assert "hunter2" not in message


# --- DSN precedence (light, complements test_store_factory) -------------------


def test_resolve_dsn_primary_wins_and_whitespace_is_unset() -> None:
    assert (
        resolve_postgres_dsn(
            {PG_DSN_ENV: "postgresql://a/db", PG_DSN_FALLBACK_ENV: "postgresql://b/db"}
        )
        == "postgresql://a/db"
    )
    # Whitespace-only primary falls through to the fallback.
    assert (
        resolve_postgres_dsn(
            {PG_DSN_ENV: "   ", PG_DSN_FALLBACK_ENV: "postgresql://b/db"}
        )
        == "postgresql://b/db"
    )


# --- schema passthrough (fake module double, no real database) ---------------


def _install_fake_postgres_module(
    monkeypatch: pytest.MonkeyPatch,
) -> list[tuple[str, object]]:
    """Inject a fake ``flywheel_core.store_postgres`` recording construction.

    Returns a list the test reads after construction: one
    ``(dsn, schema_sentinel)`` tuple per ``PostgresStore(...)`` call, where
    ``schema_sentinel`` is the keyword value or a marker when omitted.
    """
    calls: list[tuple[str, object]] = []
    _OMITTED = "<omitted>"

    class _FakePostgresStore:
        def __init__(self, dsn: str, schema: str | None = _OMITTED) -> None:
            calls.append((dsn, schema))

    fake = ModuleType("flywheel_core.store_postgres")
    fake.PostgresStore = _FakePostgresStore  # type: ignore[attr-defined]
    monkeypatch.setitem(sys.modules, "flywheel_core.store_postgres", fake)
    return calls


def test_postgres_builder_passes_schema_through_when_set(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    calls = _install_fake_postgres_module(monkeypatch)
    dsn = "postgresql://flywheel@dbhost:5432/flywheel"
    build_postgres_store(
        _policy("postgres", schema="fw_ci"),
        db_path=tmp_path / "x.sqlite",
        environ={PG_DSN_ENV: dsn},
    )
    assert calls == [(dsn, "fw_ci")]


def test_postgres_builder_omits_schema_when_none(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    calls = _install_fake_postgres_module(monkeypatch)
    dsn = "postgresql://flywheel@dbhost:5432/flywheel"
    build_postgres_store(
        _policy("postgres", schema=None),
        db_path=tmp_path / "x.sqlite",
        environ={PG_DSN_ENV: dsn},
    )
    # schema kwarg not passed -> the default sentinel survives.
    assert len(calls) == 1
    recorded_dsn, recorded_schema = calls[0]
    assert recorded_dsn == dsn
    assert recorded_schema == "<omitted>"

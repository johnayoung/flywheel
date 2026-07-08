"""Startup fail-loud gate for a configured-but-unusable postgres store.

Spec 00075, criterion 5 / decision D-2: a process configured with
``[store] backend = "postgres"`` and no usable database must terminate with
a non-zero exit and an error naming the store misconfiguration, and must
never silently fall back to sqlite. These tests need no live postgres --
they exercise the unset-DSN path (no driver required) and a
connection-refused DSN against a closed local port -- and assert exit
status, message content, and that the repo sqlite path is neither created
nor modified.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from flywheel_orchestrator._policy import PolicyError, WorkPolicy
from flywheel_orchestrator._store_factory import (
    PG_DSN_ENV,
    PG_DSN_FALLBACK_ENV,
    StoreConfigError,
    preflight_store,
)
from flywheel_orchestrator._workflow import main as orch_main

# The built-in default store path, relative to the working directory (the
# worker resolves it when [paths] db is unset).
_DEFAULT_SQLITE = Path(".flywheel") / "flywheel.sqlite"

# A closed local port: connection is refused fast, no live server needed.
_UNREACHABLE_DSN = (
    "postgresql://flywheel:hunter2@127.0.0.1:1/flywheel?connect_timeout=1"
)


def _clear_dsn_env(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv(PG_DSN_ENV, raising=False)
    monkeypatch.delenv(PG_DSN_FALLBACK_ENV, raising=False)


def _write_postgres_policy(root: Path) -> None:
    (root / "flywheel.toml").write_text(
        '[source]\nkind = "directory"\n\n[store]\nbackend = "postgres"\n',
        encoding="utf-8",
    )


def _policy(backend: str = "postgres") -> WorkPolicy:
    return WorkPolicy(
        source_kind="directory",
        tasks_dir=Path(".flywheel/tasks"),
        store_backend=backend,
    )


# --- preflight_store unit behavior -------------------------------------------


def test_preflight_noop_for_sqlite_backend() -> None:
    # Even with a bogus DSN set, a sqlite policy must not be probed at all.
    assert preflight_store(_policy("sqlite"), environ={PG_DSN_ENV: "x"}) is None


def test_preflight_noop_for_absent_policy() -> None:
    assert preflight_store(None, environ={PG_DSN_ENV: "x"}) is None


def test_preflight_no_dsn_raises_naming_backend_and_both_sources() -> None:
    with pytest.raises(StoreConfigError) as excinfo:
        preflight_store(_policy("postgres"), environ={})
    message = str(excinfo.value)
    assert "postgres" in message
    assert PG_DSN_ENV in message
    assert PG_DSN_FALLBACK_ENV in message
    # The CLI's PolicyError handling (stderr + exit 2) must apply unchanged.
    assert isinstance(excinfo.value, PolicyError)


def test_preflight_unreachable_raises_naming_backend_without_dsn() -> None:
    pytest.importorskip("psycopg")
    with pytest.raises(StoreConfigError) as excinfo:
        preflight_store(_policy("postgres"), environ={PG_DSN_ENV: _UNREACHABLE_DSN})
    message = str(excinfo.value)
    assert "postgres" in message
    assert PG_DSN_ENV in message
    assert PG_DSN_FALLBACK_ENV in message
    # The DSN value (and especially its password) never appears in the error.
    assert _UNREACHABLE_DSN not in message
    assert "hunter2" not in message
    assert isinstance(excinfo.value, PolicyError)


# --- worker entry point: exit status, message, no sqlite side effect ---------


def test_worker_no_dsn_exits_nonzero_without_creating_sqlite(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    _clear_dsn_env(monkeypatch)
    monkeypatch.chdir(tmp_path)
    _write_postgres_policy(tmp_path)

    code = orch_main(["orchestrate"])

    assert code != 0
    err = capsys.readouterr().err
    assert "postgres" in err
    assert PG_DSN_ENV in err
    assert PG_DSN_FALLBACK_ENV in err
    # No sqlite store (nor its parent dir) was created by the failed start.
    assert not (tmp_path / _DEFAULT_SQLITE).exists()


def test_worker_unreachable_exits_nonzero_without_creating_sqlite(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    pytest.importorskip("psycopg")
    _clear_dsn_env(monkeypatch)
    monkeypatch.setenv(PG_DSN_ENV, _UNREACHABLE_DSN)
    monkeypatch.chdir(tmp_path)
    _write_postgres_policy(tmp_path)

    code = orch_main(["orchestrate"])

    assert code != 0
    err = capsys.readouterr().err
    assert "postgres" in err
    assert PG_DSN_ENV in err
    assert PG_DSN_FALLBACK_ENV in err
    # The DSN (and its password) is never echoed back to the operator.
    assert _UNREACHABLE_DSN not in err
    assert "hunter2" not in err
    assert not (tmp_path / _DEFAULT_SQLITE).exists()


def test_worker_failure_leaves_existing_sqlite_byte_identical(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A failed postgres start never touches a pre-existing sqlite store."""
    _clear_dsn_env(monkeypatch)
    monkeypatch.chdir(tmp_path)
    _write_postgres_policy(tmp_path)

    # Pre-existing sqlite state a warn-and-continue fallback would clobber.
    sqlite_path = tmp_path / _DEFAULT_SQLITE
    sqlite_path.parent.mkdir(parents=True)
    sentinel = b"pre-existing sqlite bytes -- must survive a failed pg start"
    sqlite_path.write_bytes(sentinel)

    code = orch_main(["orchestrate"])

    assert code != 0
    assert sqlite_path.read_bytes() == sentinel

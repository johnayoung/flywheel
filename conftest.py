"""Workspace-wide pytest fixtures shared across all package test suites.

Lives at the repo root so every ``packages/*/tests`` suite sees it without a
cross-package import. Its sole job today is the Postgres test container: a
single session-scoped instance backs every postgres-parametrized test across
the core, orchestrator, and worktree suites.
"""

from __future__ import annotations

import importlib.util

import pytest

_PG_STATE: dict[str, object] = {"checked": False}


def _get_postgres_dsn() -> str | None:
    """Return a DSN backed by a session-scoped testcontainer, or ``None`` when
    Docker / testcontainers / the ``postgres`` extra is unavailable.

    Caches both success and failure so the container starts exactly once per
    run and probe failures don't bog down later cases.
    """
    if _PG_STATE["checked"]:
        return _PG_STATE.get("dsn")  # type: ignore[return-value]
    _PG_STATE["checked"] = True
    if importlib.util.find_spec("psycopg") is None:
        _PG_STATE["reason"] = (
            "flywheel[postgres] extra not installed (psycopg missing)"
        )
        return None
    try:
        from testcontainers.postgres import PostgresContainer
    except ImportError:
        _PG_STATE["reason"] = "testcontainers not installed"
        return None
    try:
        container = PostgresContainer("postgres:16-alpine")
        container.start()
    except Exception as exc:  # docker missing, image pull failure, etc.
        _PG_STATE["reason"] = f"Docker unavailable: {exc}"
        return None
    dsn: str = container.get_connection_url(driver=None)
    _PG_STATE["container"] = container
    _PG_STATE["dsn"] = dsn

    import atexit

    atexit.register(container.stop)
    return dsn


def _postgres_skip_reason() -> str:
    return str(_PG_STATE.get("reason", "unknown"))


@pytest.fixture(scope="session")
def postgres_dsn() -> str | None:
    """Session-scoped Postgres DSN, or ``None`` when no database is reachable.

    Tests that require Postgres should ``pytest.skip`` when this is ``None``
    (see the ``require_postgres`` fixture for the skip-or-return shortcut).
    """
    return _get_postgres_dsn()


@pytest.fixture(scope="session")
def require_postgres(postgres_dsn: str | None) -> str:
    """Like :func:`postgres_dsn` but skips the test when no DB is reachable."""
    if postgres_dsn is None:
        pytest.skip(f"Postgres backend skipped: {_postgres_skip_reason()}")
    return postgres_dsn

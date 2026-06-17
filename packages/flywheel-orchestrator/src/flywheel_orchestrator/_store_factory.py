"""Single construction point for core stores in this package (spec 00024).

:func:`build_store` maps a :class:`~flywheel_orchestrator._policy.WorkPolicy`
to the store backend it names: sqlite (the default, and the behavior of
every policy that predates the ``[store]`` section) or postgres. The
postgres DSN is never part of the policy file -- it is resolved from the
environment (``FLYWHEEL_PG_DSN``, falling back to ``DATABASE_URL``) at
construction time, and never echoed back in error messages.

After the factory's introduction, only this module constructs core stores
inside ``flywheel-orchestrator``; the command paths in ``_workflow`` and
``_orchestrate`` route through it.
"""

from __future__ import annotations

import os
from collections.abc import Mapping
from pathlib import Path
from typing import TYPE_CHECKING

from flywheel_core._registry import MissingExtraError, import_extra, install_hint
from flywheel_core.store_sqlite import SqliteStore

from flywheel_orchestrator._policy import PolicyError, WorkPolicy
from flywheel_orchestrator._store_registry import STORES

if TYPE_CHECKING:
    from flywheel_core.store_postgres import PostgresStore

# DSN environment contract (spec FR-4/FR-6): the flywheel-specific variable
# wins over the 12-factor fallback, silently.
PG_DSN_ENV = "FLYWHEEL_PG_DSN"

PG_DSN_FALLBACK_ENV = "DATABASE_URL"


class StoreConfigError(PolicyError):
    """The configured store backend cannot be constructed.

    Raised for the fail-fast cases of spec FR-8: backend ``postgres`` with
    neither DSN env var set, and backend ``postgres`` without the optional
    extra installed. Subclasses :class:`PolicyError` so the CLI's existing
    error handling (message to stderr, exit 2) applies unchanged. Messages
    never contain a DSN value.
    """


def resolve_postgres_dsn(
    environ: Mapping[str, str] | None = None,
) -> str | None:
    """Resolve the postgres DSN from the environment, or ``None``.

    ``FLYWHEEL_PG_DSN`` wins over ``DATABASE_URL`` silently (spec edge
    case). Empty or whitespace-only values count as unset so an
    ``export FLYWHEEL_PG_DSN=`` typo cannot select an empty DSN -- the
    fallback (or the fail-fast error) applies instead. ``environ``
    defaults to ``os.environ``; tests inject a plain mapping.
    """
    env: Mapping[str, str] = os.environ if environ is None else environ
    for name in (PG_DSN_ENV, PG_DSN_FALLBACK_ENV):
        value = env.get(name)
        if value is not None and value.strip():
            return value
    return None


def build_sqlite_store(
    policy: WorkPolicy | None,
    *,
    db_path: Path,
    environ: Mapping[str, str] | None = None,
) -> SqliteStore:
    """Build the sqlite backend (the registry's ``sqlite`` target).

    Yields ``SqliteStore(db_path)`` exactly as the command paths
    constructed it before the factory existed; ``db_path`` must already be
    resolved by the caller (flag > policy > default, see ``resolve_db_path``).
    ``policy``/``environ`` are unused but kept for the shared builder
    signature the registry dispatches on.
    """
    return SqliteStore(db_path)


def build_postgres_store(
    policy: WorkPolicy | None,
    *,
    db_path: Path,
    environ: Mapping[str, str] | None = None,
) -> PostgresStore:
    """Build the postgres backend (the registry's ``postgres`` target).

    Resolves the DSN via :func:`resolve_postgres_dsn`, imports the store
    lazily through :func:`~flywheel_core._registry.import_extra` so the
    ``postgres`` extra stays optional, and passes the policy's
    ``store_schema`` through when set; pool sizing stays at the store's code
    defaults. An unreachable database, or a ``DATABASE_URL`` carrying a
    non-postgres scheme, surfaces as the store's own connection error --
    never a silent sqlite fallback.

    Raises :class:`StoreConfigError` when neither ``FLYWHEEL_PG_DSN`` nor
    ``DATABASE_URL`` is set, or when the postgres extra is not installed.
    """
    dsn = resolve_postgres_dsn(environ)
    if dsn is None:
        raise StoreConfigError(
            f"store backend is postgres but neither {PG_DSN_ENV} nor "
            f"{PG_DSN_FALLBACK_ENV} is set; export one with a postgres "
            f"connection string"
        )
    try:
        module = import_extra("flywheel_core.store_postgres", "postgres")
    except MissingExtraError as exc:
        raise StoreConfigError(
            f"store backend is postgres but the postgres extra is not "
            f"installed; {install_hint('postgres')}"
        ) from exc
    if policy is not None and policy.store_schema is not None:
        return module.PostgresStore(dsn, schema=policy.store_schema)
    return module.PostgresStore(dsn)


def build_store(
    policy: WorkPolicy | None,
    *,
    db_path: Path,
    environ: Mapping[str, str] | None = None,
) -> SqliteStore | PostgresStore:
    """Construct the core store ``policy`` selects.

    Routes the policy's ``store_backend`` (``sqlite`` when no policy is
    given) through the :data:`~flywheel_orchestrator._store_registry.STORES`
    registry, which owns the unknown-backend and missing-extra failures, then
    calls the resolved builder. The per-backend construction lives in
    :func:`build_sqlite_store` / :func:`build_postgres_store`.
    """
    backend = policy.store_backend if policy is not None else "sqlite"
    builder = STORES.resolve(backend)
    return builder(policy, db_path=db_path, environ=environ)


def open_sqlite_bound_store(
    policy: WorkPolicy | None, *, db_path: Path
) -> SqliteStore | PostgresStore:
    """Build a verb's store through the factory.

    All construction goes through :func:`build_store`, so the postgres
    fail-fast paths -- no DSN env var, missing extra -- surface with the
    factory's messages (spec FR-8), and a connection failure against an
    unreachable database propagates as the store's own error.

    A successfully constructed postgres store is now returned un-closed to
    the caller: the orchestrator's verb read paths route every cross-task
    lifecycle, per-run attempts, and aggregate read through the
    backend-agnostic store protocol (``list_lifecycles``, ``list_attempts``,
    ``summarize_spend``, ...), so a ``PostgresStore`` answers them
    identically to a ``SqliteStore`` (spec 00032 read-path port). The name
    is retained for call-site stability; the seam is no longer
    sqlite-pinned. The no-DSN and missing-extra fail-fast paths are
    unchanged -- those raise in :func:`build_postgres_store` before any
    store is constructed.
    """
    return build_store(policy, db_path=db_path)


__all__ = [
    "PG_DSN_ENV",
    "PG_DSN_FALLBACK_ENV",
    "StoreConfigError",
    "build_postgres_store",
    "build_sqlite_store",
    "build_store",
    "open_sqlite_bound_store",
    "resolve_postgres_dsn",
]

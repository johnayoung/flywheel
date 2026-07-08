"""The registries of core and claim store backends.

Names the two store backends an operator can select in ``[store] backend``
and binds each to a builder in :mod:`flywheel_orchestrator._store_factory`.
The builders share one signature so :func:`_store_factory.build_store` (core
store) and :func:`_store_factory.build_claim_store` (orchestrator claim store)
can dispatch by name with no per-backend branch; each registry owns the
unknown-name and missing-extra failures for its family.

The ``postgres`` spec carries ``extra="postgres"`` so that resolving it
without ``psycopg`` installed surfaces as the shared
:class:`~flywheel_core._registry.MissingExtraError` (which the factory maps
to its operator-facing ``StoreConfigError``). A third-party store backend is
discovered and selectable by advertising a ``flywheel.stores`` (core) or
``flywheel.claim_stores`` (claim) entry point, with no fork (built-ins win a
name collision).
"""

from __future__ import annotations

from flywheel_core._registry import PluginSpec, Registry

STORES = Registry("store", "flywheel.stores")

STORES.register(
    PluginSpec(
        name="sqlite",
        target="flywheel_orchestrator._store_factory:build_sqlite_store",
        summary="local SQLite file (default)",
    )
)
STORES.register(
    PluginSpec(
        name="postgres",
        target="flywheel_orchestrator._store_factory:build_postgres_store",
        extra="postgres",
        summary="durable Postgres backend",
    )
)

# The claim/lease store family. Selected by the same ``[store] backend`` knob
# as the core store, so a postgres deployment routes both onto postgres and no
# orchestrator coordination state leaks to sqlite (spec 00075).
CLAIM_STORES = Registry("claim store", "flywheel.claim_stores")

CLAIM_STORES.register(
    PluginSpec(
        name="sqlite",
        target="flywheel_orchestrator._store_factory:build_sqlite_claim_store",
        summary="local SQLite file (default)",
    )
)
CLAIM_STORES.register(
    PluginSpec(
        name="postgres",
        target="flywheel_orchestrator._store_factory:build_postgres_claim_store",
        extra="postgres",
        summary="durable Postgres backend",
    )
)

__all__ = ["CLAIM_STORES", "STORES"]

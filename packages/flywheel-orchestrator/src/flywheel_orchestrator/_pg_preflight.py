"""Guided, failsafe Postgres bring-up for ``flywheel init``.

The store's runtime ``_bootstrap`` is idempotent and correct, but on a
managed Postgres (Supabase, RDS, a PgBouncer/Supavisor pool) the *first
worker run* is a terrible place to discover that the DSN points at a
transaction-mode pooler, that the role cannot create the schema, or that
an existing schema is at a version the store will refuse. Those failures
are silent-until-runtime; this module surfaces them at init time.

The engine runs an ordered list of checks over one connection and
returns structured :class:`PreflightCheck` results. ``_workflow`` prints
them and, when any check is ``"block"`` and the operator did not pass
``--allow-unverified``, aborts init without writing ``flywheel.toml`` --
so a misconfigured target never yields a config that looks ready.

Every check is read-only except the optional provision step
(``--provision``), which constructs a :class:`PostgresStore` so its
bootstrap DDL runs during init rather than at the first worker run.

psycopg is imported locally (never at module import) so importing this
module is free without the ``postgres`` extra; the engine reports the
missing extra as its first blocking check instead.
"""

from __future__ import annotations

from collections.abc import Sequence
from dataclasses import dataclass, field
from typing import Literal

from flywheel_core.store_protocols import CURRENT_SCHEMA_VERSION

# The one schema version the store migrates forward in place (v11 -> v12,
# an additive nullable column). Mirrors store_postgres._bootstrap; kept in
# sync here so the version check can say "will auto-migrate" instead of
# "will be refused".
_IN_PLACE_MIGRATABLE_FROM: frozenset[int] = frozenset({11})

# Ports that unambiguously mean a transaction-mode pooler. Supabase's
# Supavisor splits session (5432) and transaction (6543) onto distinct
# ports, so 6543 is a reliable block. A generic PgBouncer port (6432) is
# NOT listed: it carries no mode information (it can be session or
# transaction mode), so blocking it would false-positive on a valid
# session pool. The behavioral probe is the only other signal -- and only
# in its failing direction (see _check_pooler_mode).
_TXN_POOLER_PORTS: dict[int, str] = {
    6543: "the Supabase Supavisor transaction pooler",
}

CheckStatus = Literal["ok", "warn", "block"]


@dataclass(frozen=True)
class PreflightCheck:
    """One named readiness check and its verdict.

    ``remedy`` is a copy-pasteable fix (a shell command or SQL) shown
    beneath a ``warn``/``block`` line; ``None`` when nothing to do.
    """

    name: str
    status: CheckStatus
    detail: str
    remedy: str | None = None


@dataclass(frozen=True)
class PreflightOutcome:
    checks: list[PreflightCheck] = field(default_factory=list)
    provisioned: bool = False

    @property
    def blocked(self) -> bool:
        return any(c.status == "block" for c in self.checks)


def _sanitize(message: str, dsn: str, password: str | None) -> str:
    """Strip the DSN and its password from a driver error string."""
    out = message.replace(dsn, "<dsn>")
    if password:
        out = out.replace(password, "<password>")
    return " ".join(out.split())


def run_postgres_preflight(
    dsn: str,
    schema: str,
    *,
    provision: bool = False,
    allow_unverified: bool = False,
) -> PreflightOutcome:
    """Run the ordered Postgres readiness checks against ``dsn``.

    The connection-dependent checks (pooler mode, privileges, schema
    version) share one short-lived connection. A failure that prevents
    later checks (missing extra, unreachable server) short-circuits with
    a single blocking result. Provisioning runs last and only when
    requested and not blocked (unless ``allow_unverified`` overrode the
    block).
    """
    checks: list[PreflightCheck] = []

    try:
        import psycopg
        import psycopg.conninfo
    except ImportError:
        checks.append(
            PreflightCheck(
                "postgres extra",
                "block",
                "the 'postgres' extra is not installed",
                remedy="uv add 'flywheel[postgres]'",
            )
        )
        return PreflightOutcome(checks=checks)

    checks.append(
        PreflightCheck("postgres extra", "ok", "psycopg is importable")
    )

    try:
        password = psycopg.conninfo.conninfo_to_dict(dsn).get("password")
    except Exception:
        password = None
    password = password if isinstance(password, str) else None

    try:
        conn = psycopg.connect(dsn, connect_timeout=5, autocommit=True)
    except Exception as exc:  # OperationalError and friends
        checks.append(
            PreflightCheck(
                "connection",
                "block",
                f"could not connect: {_sanitize(str(exc), dsn, password)}",
                remedy=(
                    "verify the host, port, and credentials; for Supabase "
                    "use the Session pooler (port 5432) or the direct "
                    "connection, never the Transaction pooler (6543)"
                ),
            )
        )
        return PreflightOutcome(checks=checks)

    checks.append(
        PreflightCheck("connection", "ok", "reached the server")
    )

    try:
        checks.append(_check_pooler_mode(conn, dsn, password))
        checks.append(_check_privileges(conn, schema))
        checks.append(_check_schema_version(conn, schema))
    finally:
        conn.close()

    outcome = PreflightOutcome(checks=checks)

    if provision and (not outcome.blocked or allow_unverified):
        prov = _provision(dsn, schema, password)
        checks.append(prov)
        return PreflightOutcome(checks=checks, provisioned=prov.status == "ok")

    return outcome


def _dsn_port(dsn: str) -> int | None:
    try:
        import psycopg
        import psycopg.conninfo

        raw = psycopg.conninfo.conninfo_to_dict(dsn).get("port")
    except Exception:
        return None
    try:
        return int(raw) if raw is not None else None
    except (TypeError, ValueError):
        return None


_SESSION_REMEDY = (
    "use a session connection: for Supabase the Session pooler (port "
    "5432) or the direct connection; never the Transaction pooler (6543)"
)


def _check_pooler_mode(conn: object, dsn: str, password: str | None) -> PreflightCheck:
    """Flag connections that cannot hold session state.

    flywheel needs a session connection: psycopg issues server-side
    prepared statements and the store's reactive layer uses
    ``LISTEN/NOTIFY`` -- both break on a transaction-mode pooler that
    rebinds backends per transaction. Detecting that from one client
    connection is only partly possible, and this check is deliberately
    honest about the limit:

    * The behavioral probe (set a session GUC, read it back in the next
      autocommit statement) is trusted **only when it fails** -- a value
      that does not survive proves the pool is transaction-scoped. A lone
      idle client often reuses one backend, so a *passing* probe does NOT
      prove session mode and is not treated as proof.
    * Supabase's Supavisor puts the transaction pooler on a dedicated
      port (6543), which is an unambiguous, reliable block.

    Anything else is reported OK with a detail that says exactly what was
    verified; the next-steps text reminds the operator to use a session
    connection, since a single-connection check cannot rule out a
    transaction pooler that only misbehaves under concurrency.
    """
    port = _dsn_port(dsn)
    port_hint = _TXN_POOLER_PORTS.get(port) if port is not None else None

    probe_error: str | None = None
    state_lost: bool | None = None
    try:
        cur = conn.cursor()  # type: ignore[attr-defined]
        cur.execute(
            "SELECT set_config('flywheel.preflight_probe', 'session-ok', false)"
        )
        cur.execute("SELECT current_setting('flywheel.preflight_probe', true)")
        row = cur.fetchone()
        state_lost = not (bool(row) and row[0] == "session-ok")
    except Exception as exc:
        probe_error = _sanitize(str(exc), dsn, password)

    # Strongest signal: state demonstrably did not persist.
    if state_lost is True:
        return PreflightCheck(
            "pooler mode",
            "block",
            "session state did NOT persist across statements -- a "
            "transaction-mode pooler; prepared statements and "
            "LISTEN/NOTIFY will fail at runtime",
            remedy=_SESSION_REMEDY,
        )

    # Reliable for the realistic dangerous case (Supabase 6543).
    if port_hint is not None:
        return PreflightCheck(
            "pooler mode",
            "block",
            f"port {port} is {port_hint}; flywheel needs a session "
            "connection (prepared statements and LISTEN/NOTIFY will fail)",
            remedy=_SESSION_REMEDY,
        )

    if probe_error is not None:
        return PreflightCheck(
            "pooler mode",
            "warn",
            f"could not verify session mode: {probe_error}; ensure this is "
            "a session or direct connection, not a transaction-mode pooler",
        )

    return PreflightCheck(
        "pooler mode",
        "ok",
        "session state persisted and the port is not a known "
        "transaction-pooler port",
    )


def provisioning_sql(schema: str, role: str) -> str:
    """The exact SQL to make ``role`` able to own objects in ``schema``.

    Run as a Postgres superuser / privileged role (e.g. ``postgres`` in
    Supabase). ``CREATE SCHEMA`` is idempotent, and the grant targets the
    least-privilege role from the operator's DSN. Identifiers are double-
    quoted; the engine only ever passes regex-validated schema names and a
    role read back from the live connection, so neither can inject.
    """
    return (
        f'CREATE SCHEMA IF NOT EXISTS "{schema}";\n'
        f'GRANT ALL ON SCHEMA "{schema}" TO "{role}";'
    )


def _check_privileges(conn: object, schema: str) -> PreflightCheck:
    """Verify the connecting role can create/own the target schema.

    If the schema exists, the role needs ``CREATE``+``USAGE`` on it; if it
    does not, the role needs ``CREATE`` on the database to ``CREATE
    SCHEMA``. The remedy names the exact ``GRANT``.
    """
    cur = conn.cursor()  # type: ignore[attr-defined]
    cur.execute("SELECT current_user, current_database()")
    role, database = cur.fetchone()

    cur.execute(
        "SELECT EXISTS (SELECT 1 FROM information_schema.schemata "
        "WHERE schema_name = %s)",
        (schema,),
    )
    schema_exists = bool(cur.fetchone()[0])

    if schema_exists:
        cur.execute(
            "SELECT has_schema_privilege(%s, 'CREATE'), "
            "has_schema_privilege(%s, 'USAGE')",
            (schema, schema),
        )
        can_create, can_use = cur.fetchone()
        if can_create and can_use:
            return PreflightCheck(
                "privileges",
                "ok",
                f"role {role!r} can create objects in schema {schema!r}",
            )
        return PreflightCheck(
            "privileges",
            "block",
            f"role {role!r} lacks CREATE/USAGE on existing schema "
            f"{schema!r}",
            remedy="run as a privileged role:\n"
            + provisioning_sql(schema, role),
        )

    cur.execute(
        "SELECT has_database_privilege(%s, 'CREATE')", (database,)
    )
    can_create_schema = bool(cur.fetchone()[0])
    if can_create_schema:
        return PreflightCheck(
            "privileges",
            "ok",
            f"role {role!r} can CREATE SCHEMA {schema!r} in {database!r}",
        )
    return PreflightCheck(
        "privileges",
        "block",
        f"role {role!r} cannot CREATE SCHEMA in database {database!r} "
        f"and schema {schema!r} does not exist",
        remedy="run as a privileged role (e.g. Supabase 'postgres'):\n"
        + provisioning_sql(schema, role),
    )


def _check_schema_version(conn: object, schema: str) -> PreflightCheck:
    """Preview whether the store would accept an existing schema.

    A fresh target (no schema, or no ``schema_version`` row) is fine -- the
    first worker (or ``--provision``) bootstraps it. An existing schema at
    the current version is fine; at a migratable version it warns; at any
    other version it blocks, because the store refuses such a schema at
    open time and the operator should see that before queuing work.
    """
    cur = conn.cursor()  # type: ignore[attr-defined]
    cur.execute(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables "
        "WHERE table_schema = %s AND table_name = 'schema_version')",
        (schema,),
    )
    if not bool(cur.fetchone()[0]):
        return PreflightCheck(
            "schema version",
            "ok",
            f"schema {schema!r} is not yet initialized; it will be "
            f"created at version {CURRENT_SCHEMA_VERSION} on first use",
        )

    from psycopg import sql

    cur.execute(
        sql.SQL("SELECT version FROM {}.schema_version WHERE id = 1").format(
            sql.Identifier(schema)
        )
    )
    row = cur.fetchone()
    observed = int(row[0]) if row is not None else None

    if observed == CURRENT_SCHEMA_VERSION:
        return PreflightCheck(
            "schema version",
            "ok",
            f"schema {schema!r} is at version {observed} (current)",
        )
    if observed in _IN_PLACE_MIGRATABLE_FROM:
        return PreflightCheck(
            "schema version",
            "warn",
            f"schema {schema!r} is at version {observed}; it will be "
            f"migrated in place to {CURRENT_SCHEMA_VERSION} on first use",
        )
    return PreflightCheck(
        "schema version",
        "block",
        f"schema {schema!r} is at version {observed}, which the store "
        f"(version {CURRENT_SCHEMA_VERSION}) will refuse to open",
        remedy=(
            f'DROP SCHEMA "{schema}" CASCADE;  -- destroys existing '
            f"flywheel run history in this schema, then re-init"
        ),
    )


def _provision(dsn: str, schema: str, password: str | None) -> PreflightCheck:
    """Run the store's bootstrap DDL now (create schema + tables).

    Constructs a :class:`PostgresStore`, which runs the idempotent
    bootstrap, then closes it. Any failure becomes a blocking result so
    init does not report success on a half-provisioned target.
    """
    try:
        from flywheel_core.store_postgres import PostgresStore

        store = PostgresStore(dsn, schema=schema)
        store.close()
    except Exception as exc:
        return PreflightCheck(
            "provision",
            "block",
            f"bootstrap failed: {_sanitize(str(exc), dsn, password)}",
        )
    return PreflightCheck(
        "provision",
        "ok",
        f"schema {schema!r} provisioned at version "
        f"{CURRENT_SCHEMA_VERSION}",
    )


def format_report(checks: Sequence[PreflightCheck]) -> str:
    """Render the checks as an aligned, human-readable block."""
    glyph = {"ok": "OK  ", "warn": "WARN", "block": "BLOCK"}
    lines = ["Postgres preflight:"]
    for c in checks:
        lines.append(f"  [{glyph[c.status]}] {c.name}: {c.detail}")
        if c.remedy is not None:
            remedy_lines = c.remedy.splitlines() or [""]
            lines.append(f"         fix: {remedy_lines[0]}")
            # Continuation lines (e.g. a multi-statement SQL block) align
            # under the first so the block is copy-pasteable.
            for extra in remedy_lines[1:]:
                lines.append(f"              {extra}")
    return "\n".join(lines)

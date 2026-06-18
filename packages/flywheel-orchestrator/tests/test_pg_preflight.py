"""Tests for the guided Postgres preflight (:mod:`._pg_preflight`).

The integration tests run against the session-scoped Postgres container
(``require_postgres``); they skip cleanly when Docker / the postgres extra
is unavailable. The pooler-mode decision and the report formatter are
unit-tested without a database.
"""

from __future__ import annotations

import pytest

from flywheel_orchestrator._pg_preflight import (
    PreflightCheck,
    PreflightOutcome,
    _check_pooler_mode,
    _dsn_port,
    format_report,
    run_postgres_preflight,
)


def _status(outcome: PreflightOutcome, name: str) -> str:
    for check in outcome.checks:
        if check.name == name:
            return check.status
    raise AssertionError(f"no check named {name!r} in {outcome.checks!r}")


# --- pooler-mode decision (no database) --------------------------------------


class _FakeCursor:
    def __init__(self, probe_value: str | None) -> None:
        self._probe_value = probe_value
        self._last = ""

    def execute(self, query: str, *args: object) -> None:
        self._last = query

    def fetchone(self):
        if "current_setting" in self._last:
            return (self._probe_value,)
        return ("",)


class _FakeConn:
    def __init__(self, probe_value: str | None) -> None:
        self._probe_value = probe_value

    def cursor(self) -> _FakeCursor:
        return _FakeCursor(self._probe_value)


def test_pooler_mode_ok_when_session_state_survives_on_session_port() -> None:
    check = _check_pooler_mode(
        _FakeConn("session-ok"), "postgresql://h:5432/db", None
    )
    assert check.status == "ok"


def test_pooler_mode_blocks_when_session_state_lost() -> None:
    # A failed probe is the one trustworthy behavioral signal -- it blocks
    # regardless of port.
    check = _check_pooler_mode(
        _FakeConn(""), "postgresql://h:5432/db", None
    )
    assert check.status == "block"
    assert "transaction-mode pooler" in check.detail
    assert check.remedy is not None and "5432" in check.remedy


def test_pooler_mode_blocks_on_supabase_transaction_port_even_if_probe_passes() -> None:
    # A lone idle client can make a transaction pooler pass the GUC probe;
    # the unambiguous 6543 port still blocks.
    check = _check_pooler_mode(
        _FakeConn("session-ok"), "postgresql://h:6543/db", None
    )
    assert check.status == "block"
    assert "6543" in check.detail
    assert check.remedy is not None and "5432" in check.remedy


def test_dsn_port_parses_and_tolerates_missing() -> None:
    pytest.importorskip("psycopg")
    assert _dsn_port("postgresql://h:6543/db") == 6543
    assert _dsn_port("not a dsn at all") is None


# --- report formatter --------------------------------------------------------


def test_format_report_renders_status_and_remedy() -> None:
    report = format_report(
        [
            PreflightCheck("connection", "ok", "reached the server"),
            PreflightCheck(
                "privileges", "block", "role lacks CREATE", remedy="GRANT ..."
            ),
        ]
    )
    assert "[OK  ] connection: reached the server" in report
    assert "[BLOCK] privileges: role lacks CREATE" in report
    assert "fix: GRANT ..." in report


# --- integration against the real container ----------------------------------


@pytest.fixture
def fresh_schema(require_postgres: str):
    """Yield an unused schema name, dropping it before and after the test."""
    import psycopg

    name = "fw_preflight_test"
    with psycopg.connect(require_postgres, autocommit=True) as conn:
        conn.execute(f'DROP SCHEMA IF EXISTS "{name}" CASCADE')
    yield name
    with psycopg.connect(require_postgres, autocommit=True) as conn:
        conn.execute(f'DROP SCHEMA IF EXISTS "{name}" CASCADE')


def test_preflight_fresh_schema_all_ok(
    require_postgres: str, fresh_schema: str
) -> None:
    outcome = run_postgres_preflight(require_postgres, fresh_schema)
    assert not outcome.blocked
    assert _status(outcome, "postgres extra") == "ok"
    assert _status(outcome, "connection") == "ok"
    assert _status(outcome, "pooler mode") == "ok"
    assert _status(outcome, "privileges") == "ok"
    # Not yet initialized -> the version check is informational, not a block.
    assert _status(outcome, "schema version") == "ok"


def test_preflight_provision_creates_schema_at_current_version(
    require_postgres: str, fresh_schema: str
) -> None:
    from flywheel_core.store_protocols import CURRENT_SCHEMA_VERSION

    outcome = run_postgres_preflight(
        require_postgres, fresh_schema, provision=True
    )
    assert outcome.provisioned
    assert _status(outcome, "provision") == "ok"

    # A second preflight now sees an initialized schema at the current version.
    again = run_postgres_preflight(require_postgres, fresh_schema)
    version_check = next(
        c for c in again.checks if c.name == "schema version"
    )
    assert version_check.status == "ok"
    assert str(CURRENT_SCHEMA_VERSION) in version_check.detail


def test_preflight_incompatible_schema_version_blocks(
    require_postgres: str, fresh_schema: str
) -> None:
    import psycopg
    from psycopg import sql

    # Stand up just enough of a foreign schema to look initialized at a
    # version the store would refuse.
    with psycopg.connect(require_postgres, autocommit=True) as conn:
        conn.execute(
            sql.SQL("CREATE SCHEMA {}").format(sql.Identifier(fresh_schema))
        )
        conn.execute(
            sql.SQL(
                "CREATE TABLE {}.schema_version (id INT PRIMARY KEY, "
                "version INT NOT NULL)"
            ).format(sql.Identifier(fresh_schema))
        )
        conn.execute(
            sql.SQL("INSERT INTO {}.schema_version (id, version) VALUES (1, 99)")
            .format(sql.Identifier(fresh_schema))
        )

    outcome = run_postgres_preflight(require_postgres, fresh_schema)
    assert outcome.blocked
    version_check = next(
        c for c in outcome.checks if c.name == "schema version"
    )
    assert version_check.status == "block"
    assert "99" in version_check.detail
    assert version_check.remedy is not None
    assert "DROP SCHEMA" in version_check.remedy


def test_preflight_privileges_block_for_unprivileged_role(
    require_postgres: str,
) -> None:
    import psycopg

    role = "fw_preflight_lowpriv"
    schema = "fw_preflight_lowpriv_schema"
    with psycopg.connect(require_postgres, autocommit=True) as conn:
        conn.execute(f'DROP SCHEMA IF EXISTS "{schema}" CASCADE')
        conn.execute(f'DROP ROLE IF EXISTS "{role}"')
        # A fresh role has CONNECT (via PUBLIC) but no CREATE on the
        # database, so it cannot create the absent target schema.
        conn.execute(f"CREATE ROLE \"{role}\" LOGIN PASSWORD 'probe-pw'")
    try:
        lowpriv_dsn = psycopg.conninfo.make_conninfo(
            require_postgres, user=role, password="probe-pw"
        )
        outcome = run_postgres_preflight(lowpriv_dsn, schema)
        assert outcome.blocked
        priv = next(c for c in outcome.checks if c.name == "privileges")
        assert priv.status == "block"
        assert priv.remedy is not None
    finally:
        with psycopg.connect(require_postgres, autocommit=True) as conn:
            conn.execute(f'DROP SCHEMA IF EXISTS "{schema}" CASCADE')
            conn.execute(f'DROP ROLE IF EXISTS "{role}"')

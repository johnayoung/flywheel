"""SQLite-specific tests for ``flywheel.store_sqlite.SqliteStore``.

The shared protocol contract lives in ``test_store_contract.py`` and
runs against every backend. Tests here pin down properties that only
make sense for the SQLite backend: pragma application on every
connection, WAL persistence across opens, FK enforcement, raw
append-only enforcement on ``grader_results``, durability across opens,
and behavior under concurrent writers.
"""

from __future__ import annotations

import json
import sqlite3
from datetime import datetime, timezone
from pathlib import Path

import pytest

from flywheel import (
    CURRENT_SCHEMA_VERSION,
    Attempt,
    GraderResultRecord,
    Lifecycle,
    OptimisticConcurrencyError,
    SqliteStore,
    Status,
    StoreSchemaError,
)
from flywheel.store_sqlite import _SCHEMA_PATH


# --- Schema bootstrap ------------------------------------------------------


def test_schema_path_points_at_canonical_package_file() -> None:
    """The bootstrap path is the canonical schema file bundled in the package."""
    assert _SCHEMA_PATH.name == "persistence-schema.sql"
    assert _SCHEMA_PATH.is_file()
    text = _SCHEMA_PATH.read_text(encoding="utf-8")
    # Two non-negotiable pragmas from the constraint list.
    assert "PRAGMA journal_mode = WAL" in text
    assert "PRAGMA foreign_keys = ON" in text


def test_bootstrap_creates_every_schema_table(tmp_path: Path) -> None:
    db = tmp_path / "boot.db"
    store = SqliteStore(db)
    try:
        rows = store._connection.execute(
            "SELECT name FROM sqlite_master "
            "WHERE type='table' AND name NOT LIKE 'sqlite_%' "
            "ORDER BY name"
        ).fetchall()
        names = {r["name"] for r in rows}
        assert names == {
            "tasks",
            "lifecycles",
            "attempts",
            "events",
            "grader_results",
            "claude_session_store",
            "sdk_messages",
            "run_sequence",
            "schema_version",
            "task_claims",
            "control_commands",
        }
    finally:
        store.close()


def test_bootstrap_is_idempotent_across_reopens(tmp_path: Path) -> None:
    db = tmp_path / "boot.db"
    SqliteStore(db).close()
    # Reopen: bootstrap should not fail or duplicate objects.
    store = SqliteStore(db)
    try:
        trigger_rows = store._connection.execute(
            "SELECT name FROM sqlite_master WHERE type='trigger' "
            "AND tbl_name='grader_results'"
        ).fetchall()
        names = {r["name"] for r in trigger_rows}
        assert names == {"grader_results_no_update", "grader_results_no_delete"}
    finally:
        store.close()


# --- Per-connection pragmas ------------------------------------------------


def test_foreign_keys_enforced_on_every_new_connection(
    tmp_path: Path,
) -> None:
    db = tmp_path / "fk.db"
    s1 = SqliteStore(db)
    s2 = SqliteStore(db)
    try:
        for s in (s1, s2):
            (fk_on,) = s._connection.execute(
                "PRAGMA foreign_keys"
            ).fetchone()
            assert fk_on == 1, (
                "foreign_keys must be ON for every connection"
            )
    finally:
        s2.close()
        s1.close()


def test_wal_mode_is_set_on_file_backed_db_and_persists_across_opens(
    tmp_path: Path,
) -> None:
    db = tmp_path / "wal.db"
    s1 = SqliteStore(db)
    try:
        (mode,) = s1._connection.execute(
            "PRAGMA journal_mode"
        ).fetchone()
        assert mode == "wal"
    finally:
        s1.close()

    # WAL is sticky at the DB level: a brand-new connection should
    # report the same journal mode without re-applying the pragma.
    raw = sqlite3.connect(str(db))
    try:
        (mode,) = raw.execute("PRAGMA journal_mode").fetchone()
        assert mode == "wal", (
            f"expected WAL to persist across opens, got {mode!r}"
        )
    finally:
        raw.close()


def test_foreign_keys_rejects_orphan_event_insert(tmp_path: Path) -> None:
    """FK enforcement is observable: orphan inserts raise."""
    db = tmp_path / "orphan.db"
    store = SqliteStore(db)
    try:
        with pytest.raises(sqlite3.IntegrityError):
            store._connection.execute(
                "INSERT INTO events "
                "(run_id, ts, kind, payload_json, sequence) "
                "VALUES (?, ?, ?, ?, ?)",
                ("missing-run", "2024-01-01T00:00:00+00:00", "x", "{}", 1),
            )
    finally:
        store.close()


# --- Optimistic concurrency at the SQL level -------------------------------


def test_optimistic_concurrency_conflict_via_two_open_stores(
    tmp_path: Path,
) -> None:
    db = tmp_path / "concurrency.db"
    s1 = SqliteStore(db)
    s2 = SqliteStore(db)
    try:
        s1.create_lifecycle(Lifecycle(task_id="t", run_id="r1"))

        # s1 transitions and writes successfully at expected_version=1.
        loaded = s1.load_lifecycle("r1")
        assert loaded is not None
        loaded.transition_to(Status.READY)
        s1.update_lifecycle(loaded, expected_version=1)

        # s2's snapshot of the row is stale at version=1; trying to
        # write a different transition surfaces OptimisticConcurrencyError.
        stale = Lifecycle(
            task_id="t", run_id="r1", status=Status.READY, version=2
        )
        with pytest.raises(OptimisticConcurrencyError) as exc:
            s2.update_lifecycle(stale, expected_version=1)
        assert exc.value.expected_version == 1
        assert exc.value.actual_version == 2

        # Stored row is unchanged from s1's successful write.
        final = s2.load_lifecycle("r1")
        assert final is not None
        assert final.status is Status.READY
        assert final.version == 2
    finally:
        s2.close()
        s1.close()


# --- Append-only enforcement at the DB level -------------------------------


def _seed_grader_result(store: SqliteStore) -> None:
    store.create_lifecycle(Lifecycle(task_id="t", run_id="r1"))
    store.save_attempt(
        "r1",
        Attempt(
            number=1,
            started_at=datetime(2024, 1, 1, tzinfo=timezone.utc),
            run_id="r1",
        ),
    )
    store.append_grader_result(
        GraderResultRecord(
            run_id="r1",
            attempt_number=1,
            ordinal=0,
            grader_type="command",
            grader_spec={"type": "command", "run": "true"},
            passed=True,
            duration_ms=1,
            payload={"exit_code": 0},
            ts=datetime(2024, 1, 1, tzinfo=timezone.utc),
        )
    )


def test_raw_update_on_grader_results_is_rejected(tmp_path: Path) -> None:
    store = SqliteStore(tmp_path / "append.db")
    try:
        _seed_grader_result(store)
        with pytest.raises(sqlite3.IntegrityError) as exc:
            store._connection.execute(
                "UPDATE grader_results SET passed = 0 WHERE id = 1"
            )
        assert "append-only" in str(exc.value)
    finally:
        store.close()


def test_raw_delete_on_grader_results_is_rejected(tmp_path: Path) -> None:
    store = SqliteStore(tmp_path / "append.db")
    try:
        _seed_grader_result(store)
        with pytest.raises(sqlite3.IntegrityError) as exc:
            store._connection.execute(
                "DELETE FROM grader_results WHERE id = 1"
            )
        assert "append-only" in str(exc.value)
        # Row still present.
        listed = store.list_grader_results("r1", 1)
        assert len(listed) == 1
    finally:
        store.close()


# --- Audit-log round-trip --------------------------------------------------


def test_grader_spec_and_payload_round_trip_exactly(tmp_path: Path) -> None:
    """``grader_spec_json`` / ``payload_json`` are the audit log; the
    schema constraint ``payload_json must round-trip exactly`` is
    asserted at the JSON-normalized level."""
    store = SqliteStore(tmp_path / "rt.db")
    try:
        store.create_lifecycle(Lifecycle(task_id="t", run_id="r1"))
        store.save_attempt(
            "r1",
            Attempt(
                number=1,
                started_at=datetime(2024, 1, 1, tzinfo=timezone.utc),
                run_id="r1",
            ),
        )
        spec = {
            "type": "rubric",
            "assertions": ["a one", "b two"],
            "name": "x",
        }
        payload = {
            "judge_model": "claude-opus-4-7",
            "per_assertion": [
                {"text": "a one", "verdict": "pass", "rationale": "ok"}
            ],
            "usage": {"input_tokens": 12, "output_tokens": 34},
        }
        store.append_grader_result(
            GraderResultRecord(
                run_id="r1",
                attempt_number=1,
                ordinal=0,
                grader_type="rubric",
                grader_spec=spec,
                passed=True,
                duration_ms=42,
                payload=payload,
                ts=datetime(2024, 1, 1, tzinfo=timezone.utc),
                grader_name="x",
            )
        )
        listed = store.list_grader_results("r1", 1)
        assert len(listed) == 1
        rec = listed[0]
        assert dict(rec.grader_spec) == spec
        assert dict(rec.payload) == payload
        assert rec.grader_name == "x"
        # The raw stored JSON normalizes identically to the input.
        raw = store._connection.execute(
            "SELECT grader_spec_json, payload_json FROM grader_results "
            "WHERE id = ?",
            (rec.id,),
        ).fetchone()
        assert json.loads(raw["grader_spec_json"]) == spec
        assert json.loads(raw["payload_json"]) == payload
    finally:
        store.close()


# --- Durability across opens -----------------------------------------------


def test_lifecycle_state_survives_close_and_reopen(tmp_path: Path) -> None:
    db = tmp_path / "durable.db"
    s1 = SqliteStore(db)
    lc = Lifecycle(task_id="t", run_id="r1")
    lc.transition_to(Status.READY)
    s1.create_lifecycle(lc)
    s1.close()

    s2 = SqliteStore(db)
    try:
        loaded = s2.load_lifecycle("r1")
        assert loaded is not None
        assert loaded.status is Status.READY
        assert loaded.version == 2
    finally:
        s2.close()


# --- blocked_requires_json column round-trip -------------------------------


def test_blocked_requires_json_persists_verbatim_in_sqlite(
    tmp_path: Path,
) -> None:
    """The hand-written SqliteStore SELECT/INSERT/UPDATE statements must
    name ``blocked_requires_json`` so the column round-trips. NULL must
    survive as ``None`` (not coerced to empty string)."""
    store = SqliteStore(tmp_path / "blocked.db")
    try:
        # 1. Persist with the column set to a non-empty JSON string.
        payload = (
            '[{"type": "command_grader", "name": "full-suite"}]'
        )
        lc = Lifecycle(
            task_id="t",
            run_id="r-set",
            blocked_requires_json=payload,
        )
        store.create_lifecycle(lc)
        loaded = store.load_lifecycle("r-set")
        assert loaded is not None
        assert loaded.blocked_requires_json == payload

        # The raw stored value is the same string verbatim, not JSON-encoded.
        raw = store._connection.execute(
            "SELECT blocked_requires_json FROM lifecycles WHERE run_id = ?",
            ("r-set",),
        ).fetchone()
        assert raw["blocked_requires_json"] == payload

        # 2. Persist with the column omitted -> NULL -> None.
        lc_null = Lifecycle(task_id="t", run_id="r-null")
        store.create_lifecycle(lc_null)
        loaded_null = store.load_lifecycle("r-null")
        assert loaded_null is not None
        assert loaded_null.blocked_requires_json is None
        raw_null = store._connection.execute(
            "SELECT blocked_requires_json FROM lifecycles WHERE run_id = ?",
            ("r-null",),
        ).fetchone()
        assert raw_null["blocked_requires_json"] is None

        # 3. Update can clear the column back to NULL.
        loaded.transition_to(Status.READY)
        loaded.blocked_requires_json = None
        store.update_lifecycle(loaded, expected_version=1)
        cleared = store.load_lifecycle("r-set")
        assert cleared is not None
        assert cleared.blocked_requires_json is None
        raw_cleared = store._connection.execute(
            "SELECT blocked_requires_json FROM lifecycles WHERE run_id = ?",
            ("r-set",),
        ).fetchone()
        assert raw_cleared["blocked_requires_json"] is None
    finally:
        store.close()


def test_blocked_requires_json_survives_close_and_reopen(
    tmp_path: Path,
) -> None:
    """The column must survive a process boundary, not just a single
    connection. Durability of the persisted JSON string is the whole
    point of moving it onto the lifecycle row."""
    db = tmp_path / "blocked-durable.db"
    payload = '[{"type": "file_exists", "path": "/tmp/x", "present": true}]'
    s1 = SqliteStore(db)
    s1.create_lifecycle(
        Lifecycle(
            task_id="t",
            run_id="r1",
            blocked_requires_json=payload,
        )
    )
    s1.close()

    s2 = SqliteStore(db)
    try:
        loaded = s2.load_lifecycle("r1")
        assert loaded is not None
        assert loaded.blocked_requires_json == payload
    finally:
        s2.close()


# --- Session ordering ------------------------------------------------------


def test_session_entries_listed_in_seq_order_for_tuple_keying(
    tmp_path: Path,
) -> None:
    """``claude_session_store`` rows must respect the
    ``(project_key, session_id, subpath, seq)`` ordering when listed."""
    store = SqliteStore(tmp_path / "sess.db")
    try:
        from flywheel import ClaudeSessionEntry

        for i in range(5):
            store.append_session_entry(
                ClaudeSessionEntry(
                    project_key="p",
                    session_id="s",
                    entry=f"e{i}",
                    mtime=i,
                )
            )
        listed = store.list_session_entries("p", "s")
        seqs: list[int] = []
        for e in listed:
            assert e.seq is not None
            seqs.append(e.seq)
        assert seqs == sorted(seqs)
        assert [e.entry for e in listed] == ["e0", "e1", "e2", "e3", "e4"]
    finally:
        store.close()


# --- Schema-version pin: refuse pre-feature stores -------------------------


def _make_legacy_sqlite_db(path: Path) -> None:
    """Bootstrap a database with the pre-audit schema (no ``sequence``
    column on ``events``, no ``sdk_messages``/``run_sequence``/
    ``schema_version`` tables). Mirrors the shape SqliteStore expected
    before this feature."""
    conn = sqlite3.connect(str(path), isolation_level=None)
    try:
        conn.executescript(
            """
            PRAGMA journal_mode = WAL;
            PRAGMA foreign_keys = ON;
            CREATE TABLE lifecycles (
                run_id           TEXT PRIMARY KEY,
                task_id          TEXT NOT NULL,
                status           TEXT NOT NULL,
                version          INTEGER NOT NULL,
                retries          INTEGER NOT NULL,
                error            TEXT,
                agent_output     TEXT,
                session_id       TEXT,
                artifacts_dir    TEXT,
                worker_id        TEXT,
                timestamps_json  TEXT NOT NULL,
                updated_at       DATETIME NOT NULL
            );
            CREATE TABLE events (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id          TEXT NOT NULL,
                attempt_number  INTEGER,
                ts              DATETIME NOT NULL,
                kind            TEXT NOT NULL,
                payload_json    TEXT NOT NULL,
                FOREIGN KEY (run_id) REFERENCES lifecycles(run_id)
            );
            """
        )
    finally:
        conn.close()


def test_opening_legacy_store_raises_store_schema_error(
    tmp_path: Path,
) -> None:
    db = tmp_path / "legacy.db"
    _make_legacy_sqlite_db(db)
    with pytest.raises(StoreSchemaError) as exc:
        SqliteStore(db)
    assert "store must be re-created" in str(exc.value)
    assert exc.value.expected_version == CURRENT_SCHEMA_VERSION
    assert exc.value.observed_version is None


def test_opening_store_with_mismatched_schema_version_raises(
    tmp_path: Path,
) -> None:
    db = tmp_path / "mismatch.db"
    store = SqliteStore(db)
    store.close()
    # Corrupt the version row to simulate a future-schema store.
    conn = sqlite3.connect(str(db), isolation_level=None)
    try:
        conn.execute(
            "UPDATE schema_version SET version = ? WHERE id = 1",
            (CURRENT_SCHEMA_VERSION + 99,),
        )
    finally:
        conn.close()
    with pytest.raises(StoreSchemaError) as exc:
        SqliteStore(db)
    assert "store must be re-created" in str(exc.value)
    assert exc.value.observed_version == CURRENT_SCHEMA_VERSION + 99
    assert exc.value.expected_version == CURRENT_SCHEMA_VERSION


def test_fresh_store_records_current_schema_version(tmp_path: Path) -> None:
    db = tmp_path / "fresh.db"
    store = SqliteStore(db)
    try:
        row = store._connection.execute(
            "SELECT version FROM schema_version WHERE id = 1"
        ).fetchone()
        assert row is not None
        assert int(row["version"]) == CURRENT_SCHEMA_VERSION
    finally:
        store.close()


def _downgrade_to_v3_without_control_commands(db: Path) -> None:
    """Strip the v4 additions from a bootstrapped store so it looks like a
    pre-feature v3 database: drop ``control_commands`` (table + index) and
    pin ``schema_version`` back to 3. The store's bootstrap then exercises
    the forward migration path on reopen."""
    conn = sqlite3.connect(str(db), isolation_level=None)
    try:
        conn.execute("DROP INDEX IF EXISTS idx_control_commands_pending")
        conn.execute("DROP TABLE IF EXISTS control_commands")
        conn.execute(
            "UPDATE schema_version SET version = 3 WHERE id = 1"
        )
    finally:
        conn.close()


def test_v3_store_upgrades_to_v4_with_control_commands_table(
    tmp_path: Path,
) -> None:
    """Existing v3 stores must forward-migrate cleanly on reopen: the
    ``control_commands`` table appears, schema_version bumps to v4, and
    pre-existing lifecycle data survives untouched."""
    db = tmp_path / "v3-upgrade.db"
    s_initial = SqliteStore(db)
    payload = '[{"type": "command_grader", "name": "ok"}]'
    s_initial.create_lifecycle(
        Lifecycle(
            task_id="t",
            run_id="run-pre-migration",
            blocked_requires_json=payload,
        )
    )
    s_initial.close()

    _downgrade_to_v3_without_control_commands(db)

    # Sanity-check the downgrade: control_commands really is gone and the
    # version row is back at 3.
    conn = sqlite3.connect(str(db), isolation_level=None)
    try:
        tables = {
            r[0]
            for r in conn.execute(
                "SELECT name FROM sqlite_master WHERE type='table'"
            ).fetchall()
        }
        assert "control_commands" not in tables
        (version,) = conn.execute(
            "SELECT version FROM schema_version WHERE id = 1"
        ).fetchone()
        assert version == 3
    finally:
        conn.close()

    s_migrated = SqliteStore(db)
    try:
        rows = s_migrated._connection.execute(
            "SELECT name FROM sqlite_master "
            "WHERE type='table' AND name='control_commands'"
        ).fetchall()
        assert rows, "v3 -> v4 migration must create control_commands"
        (version,) = s_migrated._connection.execute(
            "SELECT version FROM schema_version WHERE id = 1"
        ).fetchone()
        assert version == CURRENT_SCHEMA_VERSION
        # Pre-migration data survives untouched.
        loaded = s_migrated.load_lifecycle("run-pre-migration")
        assert loaded is not None
        assert loaded.blocked_requires_json == payload
        # New verbs are wired in and work end-to-end against the upgraded
        # store; the migration is not just DDL-only.
        enqueued = s_migrated.enqueue_command(
            "run-pre-migration",
            "interrupt",
            {},
            now=datetime(2026, 1, 1, tzinfo=timezone.utc),
        )
        assert enqueued.id is not None
        claimed = s_migrated.claim_commands(
            "run-pre-migration",
            now=datetime(2026, 1, 1, 0, 0, 1, tzinfo=timezone.utc),
        )
        assert [c.kind for c in claimed] == ["interrupt"]
    finally:
        s_migrated.close()


def _downgrade_to_v4_without_awaiting_manual_ordinal(db: Path) -> None:
    """Strip the v5 additions from a bootstrapped store so it looks like a
    pre-feature v4 database: drop ``lifecycles.awaiting_manual_ordinal``
    and pin ``schema_version`` back to 4. The store's bootstrap then
    exercises the forward migration path on reopen."""
    conn = sqlite3.connect(str(db), isolation_level=None)
    try:
        conn.execute(
            "ALTER TABLE lifecycles DROP COLUMN awaiting_manual_ordinal"
        )
        conn.execute(
            "UPDATE schema_version SET version = 4 WHERE id = 1"
        )
    finally:
        conn.close()


def test_v4_store_upgrades_to_v5_with_awaiting_manual_ordinal_column(
    tmp_path: Path,
) -> None:
    """Existing v4 stores must forward-migrate cleanly on reopen: the
    ``awaiting_manual_ordinal`` column appears (as nullable INTEGER),
    schema_version bumps to v5, and pre-existing lifecycle data survives
    untouched. A pre-migration row reads ``None`` for the new column."""
    db = tmp_path / "v4-upgrade.db"
    s_initial = SqliteStore(db)
    s_initial.create_lifecycle(
        Lifecycle(
            task_id="t",
            run_id="run-pre-migration",
            blocked_requires_json='[{"type": "command_grader", "name": "ok"}]',
        )
    )
    s_initial.close()

    _downgrade_to_v4_without_awaiting_manual_ordinal(db)

    # Sanity-check the downgrade: the new column really is gone and the
    # version row is back at 4.
    conn = sqlite3.connect(str(db), isolation_level=None)
    try:
        cols = {
            r[1]
            for r in conn.execute("PRAGMA table_info(lifecycles)").fetchall()
        }
        assert "awaiting_manual_ordinal" not in cols
        (version,) = conn.execute(
            "SELECT version FROM schema_version WHERE id = 1"
        ).fetchone()
        assert version == 4
    finally:
        conn.close()

    s_migrated = SqliteStore(db)
    try:
        cols = {
            r["name"]
            for r in s_migrated._connection.execute(
                "PRAGMA table_info(lifecycles)"
            ).fetchall()
        }
        assert "awaiting_manual_ordinal" in cols, (
            "v4 -> v5 migration must add awaiting_manual_ordinal"
        )
        (version,) = s_migrated._connection.execute(
            "SELECT version FROM schema_version WHERE id = 1"
        ).fetchone()
        assert version == CURRENT_SCHEMA_VERSION
        # Pre-migration data survives untouched; the new column reads NULL.
        loaded = s_migrated.load_lifecycle("run-pre-migration")
        assert loaded is not None
        assert loaded.blocked_requires_json == (
            '[{"type": "command_grader", "name": "ok"}]'
        )
        assert loaded.awaiting_manual_ordinal is None
        # New writes can set + clear the column end-to-end against the
        # upgraded store.
        loaded.transition_to(Status.READY)
        loaded.awaiting_manual_ordinal = 2
        s_migrated.update_lifecycle(loaded, expected_version=1)
        reloaded = s_migrated.load_lifecycle("run-pre-migration")
        assert reloaded is not None
        assert reloaded.awaiting_manual_ordinal == 2
    finally:
        s_migrated.close()


def test_opening_v2_store_without_blocked_requires_json_adds_column(
    tmp_path: Path,
) -> None:
    # Simulates a database created before cf45b58: schema_version is 2
    # and every other v2 table is present, but lifecycles.blocked_requires_json
    # is missing. The bootstrap must ALTER the column back in instead of
    # leaving SELECTs to crash with "no such column".
    db = tmp_path / "pre_blocked.db"
    SqliteStore(db).close()
    conn = sqlite3.connect(str(db), isolation_level=None)
    try:
        conn.execute(
            "ALTER TABLE lifecycles DROP COLUMN blocked_requires_json"
        )
        rows = conn.execute("PRAGMA table_info(lifecycles)").fetchall()
        assert not any(r[1] == "blocked_requires_json" for r in rows)
    finally:
        conn.close()

    store = SqliteStore(db)
    try:
        rows = store._connection.execute(
            "PRAGMA table_info(lifecycles)"
        ).fetchall()
        assert any(r["name"] == "blocked_requires_json" for r in rows)
        lifecycle = Lifecycle(
            run_id="run-recovered",
            task_id="t",
            status=Status.PENDING,
            timestamps={
                Status.PENDING: datetime(2026, 1, 1, tzinfo=timezone.utc)
            },
            blocked_requires_json='[{"type": "file_exists", "path": "x"}]',
        )
        store.create_lifecycle(lifecycle)
        loaded = store.load_lifecycle("run-recovered")
        assert loaded is not None
        assert (
            loaded.blocked_requires_json
            == '[{"type": "file_exists", "path": "x"}]'
        )
    finally:
        store.close()

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
    Attempt,
    GraderResultRecord,
    Lifecycle,
    OptimisticConcurrencyError,
    SqliteStore,
    Status,
)
from flywheel.store_sqlite import _SCHEMA_PATH


# --- Schema bootstrap ------------------------------------------------------


def test_schema_path_points_at_canonical_docs_file() -> None:
    """The bootstrap path is the canonical schema file under docs/."""
    assert _SCHEMA_PATH.name == "persistence-schema.sql"
    assert _SCHEMA_PATH.exists()
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
            "lifecycles",
            "attempts",
            "events",
            "grader_results",
            "claude_session_store",
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
                "INSERT INTO events (run_id, ts, kind, payload_json) "
                "VALUES (?, ?, ?, ?)",
                ("missing-run", "2024-01-01T00:00:00+00:00", "x", "{}"),
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
        seqs = [e.seq for e in listed]
        assert seqs == sorted(seqs)
        assert [e.entry for e in listed] == ["e0", "e1", "e2", "e3", "e4"]
    finally:
        store.close()

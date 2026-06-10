"""Tests for the ``flywheel-tui`` CLI surface.

Drives :func:`flywheel_tui.main` directly so the JSON snapshot, the
non-TTY auto-detect, and the missing-store error path are all exercised
without spawning the Textual app. The interactive TUI path is covered
by :mod:`test_dashboard`.
"""

from __future__ import annotations

import json
from datetime import datetime, timezone
from pathlib import Path

import pytest

from flywheel.lifecycle import Lifecycle, Status
from flywheel.store_sqlite import SqliteStore

from flywheel_tui import main


def _seed_running(store: SqliteStore, task_id: str) -> Lifecycle:
    now = datetime.now(timezone.utc)
    lc = Lifecycle(task_id=task_id, run_id=f"run-{task_id}-running")
    lc.transition_to(Status.READY, now=now)
    lc.transition_to(Status.RUNNING, now=now)
    store.create_lifecycle(lc)
    return lc


def test_main_json_mode_emits_parseable_snapshot(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_running(store, "alpha")
    finally:
        store.close()
    rc = main(["--db", str(db), "--json"])
    assert rc == 0
    out = capsys.readouterr().out
    payload = json.loads(out)
    assert payload["summary"]["active_workers"] == 1
    assert payload["rows"][0]["task_id"] == "alpha"
    # No ANSI escape sequences in --json mode (FR-1).
    assert "\x1b" not in out


def test_main_non_tty_stdout_auto_engages_snapshot_mode(
    tmp_path: Path,
    capsys: pytest.CaptureFixture[str],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A piped stdout (``sys.stdout.isatty()`` returns False) forces
    snapshot mode without ``--json`` — Claude Code's print-mode pattern."""
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_running(store, "piped")
    finally:
        store.close()
    # capsys's captured stdout is already a non-TTY; assert by passing
    # neither --json. To make the intent explicit, force isatty=False
    # explicitly even when capsys already does so.
    import sys

    monkeypatch.setattr(sys.stdout, "isatty", lambda: False, raising=False)
    rc = main(["--db", str(db)])
    assert rc == 0
    payload = json.loads(capsys.readouterr().out)
    assert payload["rows"][0]["task_id"] == "piped"


def test_main_missing_store_exits_with_init_remedy(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """No store at the resolved path -> exit 2 + an actionable message
    naming the path and the ``flywheel-orchestrate init`` remedy."""
    missing = tmp_path / "does-not-exist.sqlite"
    rc = main(["--db", str(missing), "--json"])
    assert rc == 2
    captured = capsys.readouterr()
    assert str(missing) in captured.err
    assert "flywheel-orchestrate init" in captured.err


def test_main_json_mode_with_empty_store(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """Zero active runs: snapshot still emits valid JSON with an empty
    rows list (no crash on the empty path)."""
    db = tmp_path / "db.sqlite"
    SqliteStore(db).close()
    rc = main(["--db", str(db), "--json"])
    assert rc == 0
    payload = json.loads(capsys.readouterr().out)
    assert payload["rows"] == []
    assert payload["summary"]["active_workers"] == 0

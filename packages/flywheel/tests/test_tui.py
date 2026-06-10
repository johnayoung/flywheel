"""Tests for the operator-console entry point inside ``flywheel``.

Drives :func:`flywheel.tui_main` directly so the JSON snapshot, the
non-TTY auto-detect, and the missing-store error path are all exercised
without spawning the Textual app. The interactive TUI path is covered
by :mod:`test_dashboard` and :mod:`test_session_screen`.
"""

from __future__ import annotations

import json
from datetime import datetime, timezone
from pathlib import Path

import pytest

from flywheel_core.lifecycle import Lifecycle, Status
from flywheel_core.store_sqlite import SqliteStore
from flywheel_orchestrator import WorkPolicy

from flywheel import tui_main
from flywheel._tui import _resolve_model_for_worker


def _seed_running(store: SqliteStore, task_id: str) -> Lifecycle:
    now = datetime.now(timezone.utc)
    lc = Lifecycle(task_id=task_id, run_id=f"run-{task_id}-running")
    lc.transition_to(Status.READY, now=now)
    lc.transition_to(Status.RUNNING, now=now)
    store.create_lifecycle(lc)
    return lc


def test_tui_main_json_mode_emits_parseable_snapshot(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_running(store, "alpha")
    finally:
        store.close()
    rc = tui_main(["--db", str(db), "--json"])
    assert rc == 0
    out = capsys.readouterr().out
    payload = json.loads(out)
    assert payload["summary"]["active_workers"] == 1
    assert payload["rows"][0]["task_id"] == "alpha"
    # No ANSI escape sequences in --json mode (FR-1).
    assert "\x1b" not in out


def test_tui_main_non_tty_stdout_auto_engages_snapshot_mode(
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
    import sys

    monkeypatch.setattr(sys.stdout, "isatty", lambda: False, raising=False)
    rc = tui_main(["--db", str(db)])
    assert rc == 0
    payload = json.loads(capsys.readouterr().out)
    assert payload["rows"][0]["task_id"] == "piped"


def test_tui_main_missing_store_exits_with_init_remedy(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """No store at the resolved path -> exit 2 + an actionable message
    naming the path and the ``fw init`` remedy."""
    missing = tmp_path / "does-not-exist.sqlite"
    rc = tui_main(["--db", str(missing), "--json"])
    assert rc == 2
    captured = capsys.readouterr()
    assert str(missing) in captured.err
    assert "fw init" in captured.err


def test_tui_main_json_mode_with_empty_store(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """Zero active runs: snapshot still emits valid JSON with an empty
    rows list (no crash on the empty path)."""
    db = tmp_path / "db.sqlite"
    SqliteStore(db).close()
    rc = tui_main(["--db", str(db), "--json"])
    assert rc == 0
    payload = json.loads(capsys.readouterr().out)
    assert payload["rows"] == []
    assert payload["summary"]["active_workers"] == 0


# --- model resolution helper -----------------------------------------------


def _directory_policy(
    tmp_path: Path, *, model: str | None = None
) -> WorkPolicy:
    """Minimal directory-kind policy for resolver tests."""

    return WorkPolicy(
        source_kind="directory",
        tasks_dir=tmp_path / "tasks",
        model=model,
    )


def test_resolve_model_prefers_explicit_flag_over_policy(
    tmp_path: Path,
) -> None:
    """CLI ``--model`` wins, even when the policy pins a different id."""

    policy = _directory_policy(tmp_path, model="claude-sonnet-4-5")
    assert (
        _resolve_model_for_worker("claude-opus-4-8", policy)
        == "claude-opus-4-8"
    )


def test_resolve_model_falls_back_to_policy_when_flag_absent(
    tmp_path: Path,
) -> None:
    """No ``--model`` flag: the policy's ``[agent] model`` is used."""

    policy = _directory_policy(tmp_path, model="claude-sonnet-4-5")
    assert _resolve_model_for_worker(None, policy) == "claude-sonnet-4-5"


def test_resolve_model_returns_none_for_no_flag_and_no_policy(
    tmp_path: Path,
) -> None:
    """Bare CLI + no policy + no model: ``None`` so the SDK uses its
    own default (the current pre-feature behaviour)."""

    assert _resolve_model_for_worker(None, None) is None
    # And a policy without ``[agent] model`` (``.model is None``)
    # behaves the same way.
    assert _resolve_model_for_worker(None, _directory_policy(tmp_path)) is None


def test_resolve_model_flag_wins_when_policy_is_unset(tmp_path: Path) -> None:
    """No policy loaded but ``--model`` given: the flag still wins."""

    assert _resolve_model_for_worker("claude-haiku", None) == "claude-haiku"

"""Tests for the ``fw`` router.

Drives :func:`flywheel_cli.main` directly so each delegation target is
exercised through the seam the console script will take. The router
must forward output and exit codes from the underlying implementation
unchanged (FR-4 of spec 00021).
"""

from __future__ import annotations

import json
import sys
from datetime import datetime, timezone
from pathlib import Path

import pytest

from flywheel.lifecycle import Lifecycle, Status
from flywheel.store_sqlite import SqliteStore

from flywheel_cli import main


# --- helpers ---------------------------------------------------------------


def _seed_running(store: SqliteStore, task_id: str) -> Lifecycle:
    """Persist a RUNNING lifecycle so producer verbs see a valid target."""
    now = datetime.now(timezone.utc)
    lc = Lifecycle(task_id=task_id, run_id=f"run-{task_id}-running")
    lc.transition_to(Status.READY, now=now)
    lc.transition_to(Status.RUNNING, now=now)
    store.create_lifecycle(lc)
    return lc


def _seed_awaiting(store: SqliteStore, task_id: str) -> Lifecycle:
    """Persist an AWAITING_APPROVAL lifecycle for approve/reject targets."""
    now = datetime.now(timezone.utc)
    lc = Lifecycle(task_id=task_id, run_id=f"run-{task_id}-awaiting")
    lc.transition_to(Status.READY, now=now)
    lc.transition_to(Status.RUNNING, now=now)
    lc.transition_to(Status.VALIDATING, now=now)
    lc.transition_to(Status.AWAITING_APPROVAL, now=now)
    store.create_lifecycle(lc)
    return lc


# --- top-level surface -----------------------------------------------------


def test_fw_help_exits_zero_and_prints_verbs(
    capsys: pytest.CaptureFixture[str],
) -> None:
    """``fw --help`` (and ``-h``) prints the verb list and exits 0."""
    assert main(["--help"]) == 0
    out = capsys.readouterr().out
    # Spot-check that every FR-4 verb shows up in the help text.
    for verb in (
        "init",
        "worker",
        "status",
        "live",
        "say",
        "interrupt",
        "approve",
        "reject",
        "archive",
        "recover",
        "recheck-blocked",
        "audit",
    ):
        assert verb in out, f"verb {verb!r} missing from fw --help"

    assert main(["-h"]) == 0


def test_fw_unknown_verb_exits_two(
    capsys: pytest.CaptureFixture[str],
) -> None:
    """Unknown verbs are a usage error -- exit 2, point at ``fw --help``."""
    rc = main(["does-not-exist"])
    assert rc == 2
    err = capsys.readouterr().err
    assert "unknown command" in err
    assert "fw --help" in err


# --- bare fw delegates to the TUI fork -------------------------------------


def test_fw_bare_emits_snapshot_when_stdout_not_tty(
    tmp_path: Path,
    capsys: pytest.CaptureFixture[str],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Bare ``fw`` with a non-TTY stdout prints the JSON snapshot the
    operator console produces under ``--json`` (FR-1)."""
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_running(store, "alpha")
    finally:
        store.close()

    monkeypatch.setattr(sys.stdout, "isatty", lambda: False, raising=False)
    rc = main(["--db", str(db)])
    assert rc == 0
    payload = json.loads(capsys.readouterr().out)
    assert payload["rows"][0]["task_id"] == "alpha"


def test_fw_json_flag_emits_snapshot(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """``fw --json`` always emits the JSON snapshot regardless of TTY."""
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_running(store, "beta")
    finally:
        store.close()

    rc = main(["--json", "--db", str(db)])
    assert rc == 0
    payload = json.loads(capsys.readouterr().out)
    assert payload["rows"][0]["task_id"] == "beta"
    # No ANSI escapes in JSON mode (FR-1: snapshot is parseable, no
    # terminal control bytes leak through).
    assert "\x1b" not in json.dumps(payload)


# --- orchestrator-routed verbs ---------------------------------------------


def test_fw_init_scaffolds_flywheel_dir(
    tmp_path: Path,
    capsys: pytest.CaptureFixture[str],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """``fw init`` delegates to the orchestrator's init, which scaffolds
    ``.flywheel/`` and ``flywheel.toml`` in the working directory."""
    monkeypatch.chdir(tmp_path)
    rc = main(["init"])
    assert rc == 0
    assert (tmp_path / ".flywheel" / "tasks" / "active" / ".gitkeep").is_file()
    assert (tmp_path / "flywheel.toml").is_file()
    out = capsys.readouterr().out
    assert "created: flywheel.toml" in out


def test_fw_status_json_round_trips_against_store(
    tmp_path: Path,
    capsys: pytest.CaptureFixture[str],
) -> None:
    """``fw status --json`` honors ``--db`` / ``--tasks-dir`` and returns
    parseable JSON (round-trips a fresh store with no work)."""
    db = tmp_path / "db.sqlite"
    tasks_dir = tmp_path / "tasks"
    (tasks_dir / "active").mkdir(parents=True)
    SqliteStore(db).close()
    rc = main(
        [
            "status",
            "--json",
            "--db",
            str(db),
            "--tasks-dir",
            str(tasks_dir),
        ]
    )
    assert rc == 0
    assert json.loads(capsys.readouterr().out) == []


def test_fw_live_with_no_runs_prints_marker(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """``fw live`` falls through the orchestrator's no-runs branch."""
    db = tmp_path / "db.sqlite"
    SqliteStore(db).close()
    rc = main(["live", "--db", str(db)])
    assert rc == 0
    assert "(no in-flight runs)" in capsys.readouterr().out


# --- core-routed producer verbs --------------------------------------------


def test_fw_interrupt_enqueues_kind_interrupt(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """``fw interrupt RUN_ID`` persists a ``kind=interrupt`` row."""
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_running(store, "task-i")
    finally:
        store.close()

    rc = main(["interrupt", "run-task-i-running", "--db", str(db)])
    assert rc == 0
    assert "kind=interrupt" in capsys.readouterr().out

    store = SqliteStore(db)
    try:
        claimed = store.claim_commands(
            "run-task-i-running", now=datetime.now(timezone.utc)
        )
    finally:
        store.close()
    assert [c.kind for c in claimed] == ["interrupt"]


def test_fw_say_aliases_to_steer_and_persists_text(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """``fw say RUN_ID MSG`` enqueues a ``kind=say`` row via core's
    ``steer`` verb (the surface name is renamed, the wire kind is not)."""
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_running(store, "task-s")
    finally:
        store.close()

    rc = main(
        [
            "say",
            "run-task-s-running",
            "double-check the rubric finding",
            "--db",
            str(db),
        ]
    )
    assert rc == 0
    assert "kind=say" in capsys.readouterr().out

    store = SqliteStore(db)
    try:
        claimed = store.claim_commands(
            "run-task-s-running", now=datetime.now(timezone.utc)
        )
    finally:
        store.close()
    assert len(claimed) == 1
    assert claimed[0].kind == "say"
    assert claimed[0].payload == {"text": "double-check the rubric finding"}


def test_fw_approve_enqueues_kind_approve(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """``fw approve RUN_ID`` persists a ``kind=approve`` row when the
    lifecycle is parked at AWAITING_APPROVAL."""
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_awaiting(store, "task-a")
    finally:
        store.close()

    rc = main(["approve", "run-task-a-awaiting", "--db", str(db)])
    assert rc == 0
    captured = capsys.readouterr()
    assert "kind=approve" in captured.out
    # AWAITING_APPROVAL is the valid in-flight status for approve, so no
    # stale-pending warning fires.
    assert "not in-flight" not in captured.err


def test_fw_reject_forwards_feedback_payload(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """``fw reject RUN_ID --feedback X`` persists the feedback in the
    payload, exactly as the underlying core verb does."""
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_awaiting(store, "task-r")
    finally:
        store.close()

    feedback = "rollback strategy is missing"
    rc = main(
        [
            "reject",
            "run-task-r-awaiting",
            "--feedback",
            feedback,
            "--db",
            str(db),
        ]
    )
    assert rc == 0
    assert "kind=reject" in capsys.readouterr().out

    store = SqliteStore(db)
    try:
        claimed = store.claim_commands(
            "run-task-r-awaiting", now=datetime.now(timezone.utc)
        )
    finally:
        store.close()
    assert len(claimed) == 1
    assert claimed[0].kind == "reject"
    assert claimed[0].payload == {"feedback": feedback}


# --- audit / worker --------------------------------------------------------


def test_fw_audit_unknown_run_exits_zero_with_stderr_notice(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """``fw audit RUN_ID`` for an unknown run id mirrors the underlying
    ``python -m flywheel.audit`` behavior: exit 0 with a stderr notice."""
    db = tmp_path / "db.sqlite"
    SqliteStore(db).close()
    rc = main(["audit", "run-ghost", "--db", str(db)])
    assert rc == 0
    err = capsys.readouterr().err
    assert "no records for run_id run-ghost" in err


def test_fw_worker_help_exits_zero(
    capsys: pytest.CaptureFixture[str],
) -> None:
    """``fw worker --help`` forwards through to the worker's parser,
    which handles ``--help`` by exiting 0 (argparse convention)."""
    with pytest.raises(SystemExit) as excinfo:
        main(["worker", "--help"])
    assert excinfo.value.code == 0
    out = capsys.readouterr().out
    # Worker's own description mentions the git-worktree role.
    assert "worktree" in out


def test_fw_status_help_exits_zero(
    capsys: pytest.CaptureFixture[str],
) -> None:
    """``fw status --help`` reaches the orchestrator parser's help."""
    with pytest.raises(SystemExit) as excinfo:
        main(["status", "--help"])
    assert excinfo.value.code == 0
    out = capsys.readouterr().out
    assert "--json" in out


def test_fw_approve_help_exits_zero(
    capsys: pytest.CaptureFixture[str],
) -> None:
    """``fw approve --help`` reaches the core parser's help text."""
    with pytest.raises(SystemExit) as excinfo:
        main(["approve", "--help"])
    assert excinfo.value.code == 0
    out = capsys.readouterr().out
    assert "RUN_ID" in out

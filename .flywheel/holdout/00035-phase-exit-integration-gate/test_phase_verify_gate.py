"""Held-out acceptance test (spec 00035): the phase-exit integration gate.

criterion 1: a configured phase-verify command that exits non-zero leaves the
  phase active (not archived).
criterion 2: a configured phase-verify command that exits 0 archives the phase.
criterion 3: the command runs against the merged base in repo_root (it observes a
  base-only fact), not inside a task sandbox.
criterion 4: phase_verify=None archives exactly as today.

Authored blind from the contract (WorkPolicy.phase_verify; the gate runs the
command in repo_root and gates archival on its exit code). Drives the real
archive_phases entrypoint and reads archived-vs-active phase state from the
filesystem, never stderr. Outside the four pytest testpaths; collected
explicitly by the grader.

NOTE for the implementing session's fw-verify: confirm this fails before the
feature and passes after, and that it discriminates the gate from a no-op, given
the exact seam (archive_phases vs archive_completed_phases) chosen.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

from flywheel_core import Lifecycle, SqliteStore, Status
from flywheel_orchestrator import WorkPolicy
from flywheel_worktree import worker


def _git(cwd: Path, *args: str) -> str:
    return subprocess.run(
        ["git", "-C", str(cwd), *args], text=True, capture_output=True, check=True
    ).stdout.strip()


def _repo(path: Path) -> Path:
    path.mkdir(parents=True, exist_ok=True)
    _git(path, "init", "-b", "main")
    _git(path, "config", "user.email", "op@example.invalid")
    _git(path, "config", "user.name", "op")
    _git(path, "config", "commit.gpgsign", "false")
    (path / "MERGED_BASE_MARKER").write_text("present only on the merged base\n")
    _git(path, "add", "-A")
    _git(path, "commit", "-m", "init")
    return path


def _write_done_phase(tasks_dir: Path, db_path: Path) -> None:
    phase = tasks_dir / "active" / "01-phase"
    phase.mkdir(parents=True, exist_ok=True)
    (phase / "a.json").write_text(
        json.dumps({"id": "a", "goal": "g.", "graders": [{"type": "command", "run": "true"}]})
    )
    store = SqliteStore(db_path)
    try:
        store.create_lifecycle(
            Lifecycle(task_id="a", run_id="run-a", status=Status.DONE)
        )
    finally:
        store.close()


def _policy(phase_verify: str | None) -> WorkPolicy:
    return WorkPolicy(source_kind="directory", phase_verify=phase_verify)


def _active_dir(tasks_dir: Path) -> Path:
    return tasks_dir / "active" / "01-phase"


def _archive_dir(tasks_dir: Path) -> Path:
    return tasks_dir / "archive" / "01-phase"


def test_failing_phase_verify_blocks_archival(tmp_path: Path) -> None:
    repo = _repo(tmp_path / "repo")
    tasks_dir = repo / ".flywheel" / "tasks"
    db_path = repo / ".flywheel" / "flywheel.sqlite"
    db_path.parent.mkdir(parents=True, exist_ok=True)
    _write_done_phase(tasks_dir, db_path)

    worker.archive_phases(
        tasks_dir, db_path, lambda _m: None, repo_root=repo, policy=_policy("false")
    )

    assert _active_dir(tasks_dir).is_dir(), "a failing gate must leave the phase active"
    assert not _archive_dir(tasks_dir).exists(), "a failing gate must not archive"


def test_passing_phase_verify_archives(tmp_path: Path) -> None:
    repo = _repo(tmp_path / "repo")
    tasks_dir = repo / ".flywheel" / "tasks"
    db_path = repo / ".flywheel" / "flywheel.sqlite"
    db_path.parent.mkdir(parents=True, exist_ok=True)
    _write_done_phase(tasks_dir, db_path)

    worker.archive_phases(
        tasks_dir, db_path, lambda _m: None, repo_root=repo, policy=_policy("true")
    )

    assert _archive_dir(tasks_dir).is_dir(), "a passing gate must archive the phase"
    assert not _active_dir(tasks_dir).exists()


def test_phase_verify_runs_against_merged_base_in_repo_root(tmp_path: Path) -> None:
    repo = _repo(tmp_path / "repo")
    tasks_dir = repo / ".flywheel" / "tasks"
    db_path = repo / ".flywheel" / "flywheel.sqlite"
    db_path.parent.mkdir(parents=True, exist_ok=True)
    _write_done_phase(tasks_dir, db_path)

    # The gate command succeeds only if it observes a fact that exists on the
    # merged base in repo_root (the MERGED_BASE_MARKER file), proving it ran
    # there and not in a task sandbox.
    worker.archive_phases(
        tasks_dir,
        db_path,
        lambda _m: None,
        repo_root=repo,
        policy=_policy("test -f MERGED_BASE_MARKER"),
    )
    assert _archive_dir(tasks_dir).is_dir(), (
        "the gate command must run against the merged base in repo_root (where "
        "MERGED_BASE_MARKER exists), so the phase archives"
    )


def test_no_phase_verify_archives_as_today(tmp_path: Path) -> None:
    repo = _repo(tmp_path / "repo")
    tasks_dir = repo / ".flywheel" / "tasks"
    db_path = repo / ".flywheel" / "flywheel.sqlite"
    db_path.parent.mkdir(parents=True, exist_ok=True)
    _write_done_phase(tasks_dir, db_path)

    worker.archive_phases(
        tasks_dir, db_path, lambda _m: None, repo_root=repo, policy=_policy(None)
    )
    assert _archive_dir(tasks_dir).is_dir(), "with no gate, a done phase archives as today"

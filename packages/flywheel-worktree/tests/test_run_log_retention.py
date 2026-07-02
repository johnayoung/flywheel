"""Behavior oracle for the worker's active run-log retention.

The harness's telemetry sink writes one ``<run_id>.jsonl`` per run under
``.flywheel/logs/runs/`` and never rotates them. ``worker.sweep_run_logs``
holds that directory at or under a configured bound, reclaiming the OLDEST run
files (by mtime) while the most-recent N -- including the in-flight run --
survive. These tests pin WHICH files survive, not merely a count. Do not weaken
or delete assertions.
"""

from __future__ import annotations

import os
from pathlib import Path

from flywheel_worktree import worker


def _make_run_file(runs_dir: Path, run_id: str, mtime: float) -> Path:
    """Write a run JSONL file and stamp it with a distinct ``mtime`` so the
    surviving set is deterministic (newest-by-mtime)."""
    runs_dir.mkdir(parents=True, exist_ok=True)
    path = runs_dir / f"{run_id}.jsonl"
    path.write_text('{"kind": "x"}\n')
    os.utime(path, (mtime, mtime))
    return path


def test_reclaims_oldest_and_pins_survivors(tmp_path: Path) -> None:
    runs = tmp_path / "logs" / "runs"
    # Six runs, strictly increasing mtimes: run-0 oldest ... run-5 newest.
    files = {i: _make_run_file(runs, f"run-{i}", 1000.0 + i) for i in range(6)}
    worker.sweep_run_logs(runs, 2, log=lambda _m: None)
    # Exactly the two most-recent survive; the four oldest are reclaimed.
    survivors = sorted(p.name for p in runs.iterdir())
    assert survivors == ["run-4.jsonl", "run-5.jsonl"]
    for i in range(4):
        assert not files[i].exists()
    assert files[4].exists() and files[5].exists()


def test_fewer_files_than_bound_reclaims_nothing(tmp_path: Path) -> None:
    runs = tmp_path / "logs" / "runs"
    kept = [_make_run_file(runs, f"run-{i}", 1000.0 + i) for i in range(3)]
    worker.sweep_run_logs(runs, 10, log=lambda _m: None)
    assert sorted(p.name for p in runs.iterdir()) == [
        "run-0.jsonl",
        "run-1.jsonl",
        "run-2.jsonl",
    ]
    assert all(p.exists() for p in kept)


def test_at_bound_reclaims_nothing(tmp_path: Path) -> None:
    runs = tmp_path / "logs" / "runs"
    kept = [_make_run_file(runs, f"run-{i}", 1000.0 + i) for i in range(4)]
    worker.sweep_run_logs(runs, 4, log=lambda _m: None)
    assert all(p.exists() for p in kept)


def test_newest_run_is_never_reclaimed(tmp_path: Path) -> None:
    runs = tmp_path / "logs" / "runs"
    # The in-flight run is the newest; even at bound=1 it must survive and the
    # older run must be the one deleted -- deleting the newest is the defect.
    _make_run_file(runs, "run-old", 1000.0)
    newest = _make_run_file(runs, "run-inflight", 2000.0)
    worker.sweep_run_logs(runs, 1, log=lambda _m: None)
    assert newest.exists()
    assert sorted(p.name for p in runs.iterdir()) == ["run-inflight.jsonl"]


def test_missing_runs_dir_is_a_noop(tmp_path: Path) -> None:
    # Worker started before any run wrote a file: absent directory, no raise.
    worker.sweep_run_logs(tmp_path / "logs" / "runs", 5, log=lambda _m: None)


def test_non_positive_bound_disables_retention(tmp_path: Path) -> None:
    runs = tmp_path / "logs" / "runs"
    kept = [_make_run_file(runs, f"run-{i}", 1000.0 + i) for i in range(5)]
    worker.sweep_run_logs(runs, 0, log=lambda _m: None)
    assert all(p.exists() for p in kept)  # 0 == disabled, nothing reclaimed


def test_only_jsonl_files_are_reclaimed(tmp_path: Path) -> None:
    runs = tmp_path / "logs" / "runs"
    _make_run_file(runs, "run-0", 1000.0)
    _make_run_file(runs, "run-1", 1001.0)
    _make_run_file(runs, "run-2", 1002.0)
    # An older non-JSONL sibling (an operator note) is left untouched even
    # though it predates the reclaimed run files.
    other = runs / "NOTES.txt"
    other.write_text("keep me\n")
    os.utime(other, (500.0, 500.0))
    worker.sweep_run_logs(runs, 1, log=lambda _m: None)
    assert other.exists()
    assert sorted(p.name for p in runs.iterdir() if p.suffix == ".jsonl") == [
        "run-2.jsonl"
    ]


def test_reclaim_is_logged(tmp_path: Path) -> None:
    runs = tmp_path / "logs" / "runs"
    for i in range(4):
        _make_run_file(runs, f"run-{i}", 1000.0 + i)
    messages: list[str] = []
    worker.sweep_run_logs(runs, 1, log=messages.append)
    assert any("reclaimed" in m for m in messages)


def test_retention_is_default_on(tmp_path: Path) -> None:
    # The knob is default-on: a positive module bound wired as the CLI default.
    assert worker.DEFAULT_RUN_LOG_RETENTION > 0
    args = worker._build_parser().parse_args([])
    assert args.run_log_retention == worker.DEFAULT_RUN_LOG_RETENTION

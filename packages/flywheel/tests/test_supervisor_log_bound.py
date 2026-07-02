"""Tests for the bounded supervisor-log directory (spec 00071 criterion #1).

Decision D-1: across repeated supervisor starts the log directory's footprint is
held at or under a configured ceiling by reclaiming the OLDEST logs, while the
most-recent log content survives intact. The two obligations are graded
together so neither a delete-everything "rotation" (footprint zero, no forensics)
nor a never-reclaim no-op (one new unbounded file per start) can pass.

The reclaim seam is exercised directly on a seeded over-bound directory and
through the supervisor's per-start ``_open_log`` hook, so the bound is proven as
an observable end-state, not a function that merely exists.
"""

from __future__ import annotations

import os
from pathlib import Path

from flywheel._autopilot_supervisor import (
    DEFAULT_MAX_LOG_BYTES,
    LOG_FILENAME_PREFIX,
    AutopilotSupervisor,
    reclaim_supervisor_logs,
)


def _write_log(directory: Path, seq: int, size: int, *, mtime: float) -> Path:
    """Seed one supervisor log of ``size`` bytes with an explicit ``mtime``.

    The filename carries the real prefix plus an increasing sequence so the
    reclaim's name tiebreaker orders them chronologically; the widely-spaced
    ``mtime`` pins age unambiguously regardless of filesystem timestamp
    resolution.
    """
    path = directory / f"{LOG_FILENAME_PREFIX}20260101T000000-{seq:04d}.log"
    path.write_bytes(b"x" * size)
    os.utime(path, (mtime, mtime))
    return path


def _footprint(directory: Path) -> int:
    return sum(
        p.stat().st_size for p in directory.glob(f"{LOG_FILENAME_PREFIX}*.log")
    )


def _survivors(directory: Path) -> list[Path]:
    return sorted(directory.glob(f"{LOG_FILENAME_PREFIX}*.log"))


def _log_at(directory: Path, seq: int) -> Path:
    return directory / f"{LOG_FILENAME_PREFIX}20260101T000000-{seq:04d}.log"


# --- the reclaim seam on a seeded over-bound directory ----------------------


def test_reclaim_holds_footprint_at_or_under_ceiling(tmp_path: Path) -> None:
    """Oldest logs are reclaimed until the footprint is at or under the bound."""
    for seq in range(5):
        _write_log(tmp_path, seq, 100, mtime=1_000_000.0 + seq * 100)

    reclaim_supervisor_logs(tmp_path, max_bytes=250)

    assert _footprint(tmp_path) <= 250
    # The oldest were reclaimed; the most-recent log survives with its content.
    assert not _log_at(tmp_path, 0).exists()
    assert not _log_at(tmp_path, 1).exists()
    newest = _log_at(tmp_path, 4)
    assert newest.exists()
    assert newest.read_bytes() == b"x" * 100


def test_reclaim_leaves_a_small_directory_untouched(tmp_path: Path) -> None:
    """Fewer bytes than the ceiling -> nothing is reclaimed (no over-eager delete)."""
    first = _write_log(tmp_path, 0, 50, mtime=1_000_000.0)
    second = _write_log(tmp_path, 1, 50, mtime=1_000_100.0)

    reclaim_supervisor_logs(tmp_path, max_bytes=1000)

    assert first.exists() and second.exists()
    assert _footprint(tmp_path) == 100


def test_reclaim_never_empties_a_single_oversized_log(tmp_path: Path) -> None:
    """A lone log larger than the ceiling survives intact -- not truncated to zero.

    The D-1 defect a blunt cap would introduce: a "rotation" that empties the
    directory. Preserving recent content wins over hitting the ceiling in this
    degenerate case.
    """
    big = _write_log(tmp_path, 0, 10_000, mtime=1_000_000.0)

    reclaim_supervisor_logs(tmp_path, max_bytes=100)

    assert big.exists()
    assert big.read_bytes() == b"x" * 10_000  # content survives, not emptied
    assert _survivors(tmp_path), "reclaim must never empty the directory"


def test_reclaim_keeps_newest_when_every_log_exceeds_ceiling(
    tmp_path: Path,
) -> None:
    """Even when each log alone exceeds the bound, the newest is preserved."""
    for seq in range(3):
        _write_log(tmp_path, seq, 500, mtime=1_000_000.0 + seq * 100)

    reclaim_supervisor_logs(tmp_path, max_bytes=100)

    newest = _log_at(tmp_path, 2)
    assert _survivors(tmp_path) == [newest]
    assert newest.read_bytes() == b"x" * 500  # recent content survives intact


def test_most_recent_log_readable_at_exactly_the_ceiling(tmp_path: Path) -> None:
    """Landing exactly at the ceiling still leaves the most-recent log readable."""
    for seq in range(3):
        _write_log(tmp_path, seq, 100, mtime=1_000_000.0 + seq * 100)

    reclaim_supervisor_logs(tmp_path, max_bytes=200)

    assert _footprint(tmp_path) == 200  # exactly at the ceiling
    newest = _log_at(tmp_path, 2)
    assert newest.exists()
    assert newest.read_bytes() == b"x" * 100  # still readable


def test_reclaim_ignores_non_supervisor_files(tmp_path: Path) -> None:
    """The reclaim only touches supervisor logs, never siblings like activity.json."""
    activity = tmp_path / "activity.json"
    activity.write_text("{}")
    for seq in range(3):
        _write_log(tmp_path, seq, 500, mtime=1_000_000.0 + seq * 100)

    reclaim_supervisor_logs(tmp_path, max_bytes=100)

    assert activity.exists() and activity.read_text() == "{}"


# --- the bound through the supervisor's per-start log-open hook --------------


def test_repeated_starts_hold_directory_under_ceiling(tmp_path: Path) -> None:
    """Across repeated starts the directory stays at or under the ceiling.

    ``_open_log`` is the per-start hook: it reclaims the oldest logs before
    opening the next one. With each start's output smaller than the ceiling, the
    directory measured right after a start (old logs already reclaimed, the new
    log still empty) is always at or under the bound -- proving the footprint
    does not grow one unbounded file per start.
    """
    ceiling = 400
    payload = b"y" * 300  # one start's output; < ceiling so a lone log fits
    sup = AutopilotSupervisor(
        log_dir=tmp_path, spawn_argv=["true"], max_log_bytes=ceiling
    )

    for _ in range(6):
        handle = sup._open_log()
        # Post-reclaim, pre-write: the footprint is bounded by the ceiling.
        assert _footprint(tmp_path) <= ceiling
        handle.write(payload)
        handle.close()

    survivors = _survivors(tmp_path)
    assert survivors, "reclaim must never empty the directory"
    # The most-recent log content survived every reclaim (not delete-all).
    assert any(p.stat().st_size > 0 for p in survivors)
    assert survivors[-1].read_bytes() == payload


def test_default_ceiling_is_on(tmp_path: Path) -> None:
    """The ceiling is default-on: a positive finite bound, not disabled."""
    sup = AutopilotSupervisor(log_dir=tmp_path, spawn_argv=["true"])
    assert sup._max_log_bytes == DEFAULT_MAX_LOG_BYTES
    assert isinstance(DEFAULT_MAX_LOG_BYTES, int)
    assert DEFAULT_MAX_LOG_BYTES > 0

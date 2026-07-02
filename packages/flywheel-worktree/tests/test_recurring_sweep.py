"""Recurring worktree-retention cadence.

The retention sweep must run on a bounded recurring cadence while the daemon
loop runs -- not once at boot -- so a parked worktree that ages past the
retention window is reclaimed mid-run without a restart, while a within-window
worktree survives every tick. These oracles drive a real git repo + worktrees
(no live model): the ``behavior`` tests exercise one tick, and the ``cadence``
test drives the real daemon loop (``run_daemon_loop``) across several cycles
with an injected clock so the mid-run reclaim is asserted without a restart and
without wall-clock time. Do not weaken or delete assertions.
"""

from __future__ import annotations

import os
import subprocess
from pathlib import Path

from flywheel_worktree import worker

DAY = 86400
RETENTION_DAYS = 7
WINDOW = RETENTION_DAYS * DAY


def _git(cwd: Path, *args: str) -> str:
    return subprocess.run(
        ["git", "-C", str(cwd), *args],
        text=True,
        capture_output=True,
        check=True,
    ).stdout.strip()


def _init_repo(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True)
    _git(path, "init", "-b", "main")
    _git(path, "config", "user.email", "t@example.com")
    _git(path, "config", "user.name", "t")
    _git(path, "config", "commit.gpgsign", "false")
    (path / "README.md").write_text("base\n")
    _git(path, "add", "-A")
    _git(path, "commit", "-m", "init")


def _worktrees_dir(repo: Path) -> Path:
    d = repo / ".flywheel" / "worktrees"
    d.mkdir(parents=True, exist_ok=True)
    return d


def _add_worktree(repo: Path, worktrees_dir: Path, task_id: str) -> Path:
    """Register a real git worktree at ``worktrees_dir/<task_id>`` on the
    ``flywheel/_root/<task_id>`` branch retention_sweep matches and deletes."""
    wt = worktrees_dir / task_id
    _git(
        repo,
        "worktree",
        "add",
        "-b",
        f"flywheel/_root/{task_id}",
        str(wt),
        "main",
    )
    return wt


def _set_mtime(path: Path, when: float) -> None:
    os.utime(path, (when, when))


def _branch_exists(repo: Path, branch: str) -> bool:
    return bool(
        _git(
            repo,
            "for-each-ref",
            "--format=%(refname:short)",
            f"refs/heads/{branch}",
        )
    )


def test_sweep_tick_reclaims_aged(tmp_path: Path) -> None:
    """One tick removes a parked worktree older than the window and its
    ``flywheel/_root/<task-id>`` branch -- exactly one sweep's semantics."""
    repo = tmp_path / "repo"
    _init_repo(repo)
    worktrees_dir = _worktrees_dir(repo)
    wt = _add_worktree(repo, worktrees_dir, "aged")

    now = 1_000_000_000.0
    _set_mtime(wt, now - (WINDOW + DAY))  # parked well past the window

    worker.retention_cadence_tick(
        repo, worktrees_dir, RETENTION_DAYS, lambda _m: None, now=lambda: now
    )

    assert not wt.exists()  # aged worktree reclaimed
    assert not _branch_exists(repo, "flywheel/_root/aged")  # ... and its branch


def test_within_window_survives(tmp_path: Path) -> None:
    """A worktree younger than the window is left untouched by a tick: the
    cadence must not over-reclaim."""
    repo = tmp_path / "repo"
    _init_repo(repo)
    worktrees_dir = _worktrees_dir(repo)
    wt = _add_worktree(repo, worktrees_dir, "fresh")

    now = 1_000_000_000.0
    _set_mtime(wt, now - DAY)  # one day old, inside the 7-day window

    worker.retention_cadence_tick(
        repo, worktrees_dir, RETENTION_DAYS, lambda _m: None, now=lambda: now
    )

    assert wt.exists()  # within-window worktree survives the sweep
    assert _branch_exists(repo, "flywheel/_root/fresh")  # ... branch intact


def test_cadence_recurs(tmp_path: Path) -> None:
    """The sweep recurs across daemon cycles, not once at boot.

    ``aging`` is within the window at boot but crosses it as the injected clock
    advances mid-run; ``survivor`` is kept in-window every cycle. Driving the
    real ``run_daemon_loop`` shows ``aging`` reclaimed by a later tick while
    ``survivor`` outlives every tick. A boot-only sweep -- exactly one tick --
    leaves ``aging`` parked (asserted as the control below), so reclaim here is
    a property of the recurring CADENCE, which a single boot sweep cannot have.
    """
    repo = tmp_path / "repo"
    _init_repo(repo)
    worktrees_dir = _worktrees_dir(repo)

    aging = _add_worktree(repo, worktrees_dir, "aging")
    survivor = _add_worktree(repo, worktrees_dir, "survivor")

    start = 1_000_000_000.0
    # Both fresh at boot (age 0): a single boot sweep reclaims neither.
    _set_mtime(aging, start)
    _set_mtime(survivor, start)

    clock = {"now": start}
    ticks = {"count": 0}

    def _now() -> float:
        return clock["now"]

    def _cycle() -> None:
        # Mirror the daemon: one retention tick per cycle, reading the clock
        # fresh. ``survivor`` is touched each cycle to model an in-window
        # worktree; ``aging`` is never touched again, so it ages out.
        _set_mtime(survivor, clock["now"])
        worker.retention_cadence_tick(
            repo, worktrees_dir, RETENTION_DAYS, lambda _m: None, now=_now
        )
        ticks["count"] += 1
        clock["now"] += 3 * DAY  # advance the clock 3 days between ticks

    # Control: exactly one tick (the boot-only cadence) leaves ``aging`` parked.
    _cycle()
    assert aging.exists()  # a single sweep cannot reclaim a within-window tree

    # Now run the recurring cadence over several cycles from the same start.
    clock["now"] = start
    ticks["count"] = 0
    code = worker.run_daemon_loop(
        run_cycle=_cycle,
        once=False,
        poll_interval=0,
        should_stop=lambda: False,
        sleep=lambda _s, _stop: None,
        max_cycles=4,
    )

    assert code == 0
    assert ticks["count"] == 4  # the sweep ran every cycle, not once at boot
    assert not aging.exists()  # reclaimed mid-run once it aged past the window
    assert not _branch_exists(repo, "flywheel/_root/aging")
    assert survivor.exists()  # within-window worktree survived every tick
    assert _branch_exists(repo, "flywheel/_root/survivor")

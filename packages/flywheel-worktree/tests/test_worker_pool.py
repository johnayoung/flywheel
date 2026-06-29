"""Tests for the worker concurrency pool supervisor (spec 00060, D-5).

A single ``flywheel worker`` invocation with concurrency > 1 supervises N
single-task worker subprocesses. These tests drive :class:`worker.WorkerPool`
with trivial synthetic members (plain ``python -c`` children) so the supervision
logic -- spawn N distinct members, retire cleanly-drained ``--once`` members,
restart-and-reclaim crashed ones (bounded), and group-kill every member's whole
subtree on stop with no orphan left behind -- is exercised deterministically,
without standing up real worktrees or stores.

The orphan-free-shutdown test mirrors
``packages/flywheel/tests/test_supervisor_group_shutdown.py``: a member that
ignores SIGTERM and spawns a SIGTERM-ignoring grandchild can only be ended by
the escalation to SIGKILL, proving the pool kills the whole process group.
"""

from __future__ import annotations

import argparse
import contextlib
import os
import signal
import sys
import time
from pathlib import Path

from flywheel_worktree import worker


# --- helpers ----------------------------------------------------------------


def _alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def _reap_group(pid: int) -> None:
    with contextlib.suppress(ProcessLookupError, OSError):
        os.killpg(os.getpgid(pid), signal.SIGKILL)


def _sleep_argv(_worker_id: str) -> list[str]:
    return [sys.executable, "-c", "import time; time.sleep(300)"]


def _exit_zero_argv(_worker_id: str) -> list[str]:
    return [sys.executable, "-c", "import sys; sys.exit(0)"]


def _crash_argv(_worker_id: str) -> list[str]:
    return [sys.executable, "-c", "import sys; sys.exit(7)"]


def _ignore_term_argv(pidfile: Path) -> list[str]:
    """A child that ignores SIGTERM and spawns a SIGTERM-ignoring grandchild.

    The grandchild's pid is written to ``pidfile``; both ignore SIGTERM, so only
    the escalation to SIGKILL can end them -- a true group-escalation probe.
    """
    grandchild = (
        "import signal,time;"
        "signal.signal(signal.SIGTERM, signal.SIG_IGN);"
        "time.sleep(300)"
    )
    script = (
        "import signal, subprocess, sys, time\n"
        "signal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
        f"g = subprocess.Popen([sys.executable, '-c', {grandchild!r}])\n"
        f"open({str(pidfile)!r}, 'w').write(str(g.pid))\n"
        "time.sleep(300)\n"
    )
    return [sys.executable, "-c", script]


def _read_pid(pidfile: Path, *, timeout: float = 5.0) -> int:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if pidfile.exists():
            text = pidfile.read_text().strip()
            if text:
                return int(text)
        time.sleep(0.02)
    raise AssertionError("grandchild pid file never appeared")


def _wait_exited(proc: object, *, timeout: float = 5.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if proc.poll() is not None:  # type: ignore[attr-defined]
            return
        time.sleep(0.02)
    raise AssertionError("member did not exit in time")


# --- spawn / identity -------------------------------------------------------


def test_pool_spawns_size_distinct_members() -> None:
    pool = worker.WorkerPool(
        size=3,
        spawn_member=_sleep_argv,
        log=lambda _msg: None,
        once=False,
        prefix="t",
    )
    try:
        pool.start()
        pids = pool.live_member_pids()
        assert len(pids) == 3
        assert len(set(pids)) == 3  # distinct OS processes
    finally:
        pool.stop(timeout=0.5)
    assert pool.live_member_pids() == []


def test_pool_member_worker_ids_are_distinct_and_prefixed() -> None:
    seen: list[str] = []

    def spawn(worker_id: str) -> list[str]:
        seen.append(worker_id)
        return _sleep_argv(worker_id)

    pool = worker.WorkerPool(
        size=3,
        spawn_member=spawn,
        log=lambda _msg: None,
        once=False,
        prefix="fleet",
    )
    try:
        pool.start()
        assert seen == ["fleet-0", "fleet-1", "fleet-2"]
        assert len(set(seen)) == 3
    finally:
        pool.stop(timeout=0.5)


# --- --once drain / retire --------------------------------------------------


def test_pool_once_retires_cleanly_drained_members() -> None:
    pool = worker.WorkerPool(
        size=2,
        spawn_member=_exit_zero_argv,
        log=lambda _msg: None,
        once=True,
        prefix="once",
        poll_interval=0.02,
    )
    code = pool.run_supervised()
    assert code == 0
    assert pool.is_done()
    assert pool.live_member_pids() == []


def test_pool_once_does_not_respawn_clean_members() -> None:
    spawns: list[str] = []

    def spawn(worker_id: str) -> list[str]:
        spawns.append(worker_id)
        return _exit_zero_argv(worker_id)

    pool = worker.WorkerPool(
        size=2,
        spawn_member=spawn,
        log=lambda _msg: None,
        once=True,
        prefix="once",
        poll_interval=0.02,
    )
    assert pool.run_supervised() == 0
    # Each slot spawned exactly once -- a clean exit-0 drain must not respawn,
    # else --once would never terminate.
    assert sorted(spawns) == ["once-0", "once-1"]


# --- crash restart-and-reclaim (bounded) ------------------------------------


def test_pool_restarts_crashed_member_same_slot() -> None:
    pool = worker.WorkerPool(
        size=1,
        spawn_member=_crash_argv,
        log=lambda _msg: None,
        once=False,
        prefix="r",
        max_restarts_per_slot=3,
    )
    try:
        pool.start()
        first = pool._members[0]
        first_pid = first.proc.pid
        _wait_exited(first.proc)
        pool._supervise_tick()
        # The slot was respawned with a fresh process to restore the pool.
        restarted = pool._members[0]
        assert restarted.proc.pid != first_pid
    finally:
        pool.stop(timeout=0.5)


def test_pool_gives_up_after_restart_budget() -> None:
    pool = worker.WorkerPool(
        size=1,
        spawn_member=_crash_argv,
        log=lambda _msg: None,
        once=False,
        prefix="g",
        poll_interval=0.02,
        max_restarts_per_slot=2,
    )
    code = pool.run_supervised()
    # A member that crashes on every (re)start exhausts the budget and the pool
    # exits non-zero rather than spinning forever.
    assert code == 1
    assert pool.live_member_pids() == []


# --- orphan-free shutdown (#9) ----------------------------------------------


def test_pool_stop_force_kills_sigterm_ignoring_groups(tmp_path) -> None:
    pidfiles = {
        "k-0": tmp_path / "gc-0.pid",
        "k-1": tmp_path / "gc-1.pid",
    }

    def spawn(worker_id: str) -> list[str]:
        return _ignore_term_argv(pidfiles[worker_id])

    pool = worker.WorkerPool(
        size=2,
        spawn_member=spawn,
        log=lambda _msg: None,
        once=False,
        prefix="k",
    )
    pool.start()
    member_pids = list(pool.live_member_pids())
    grandchild_pids = [_read_pid(pidfiles[wid]) for wid in ("k-0", "k-1")]
    try:
        assert len(member_pids) == 2
        assert all(_alive(p) for p in member_pids + grandchild_pids)
        # SIGTERM is ignored, so stop must escalate to SIGKILL within the window
        # and take down every member's whole process group, grandchildren too.
        pool.stop(timeout=0.5)
        assert all(not _alive(p) for p in member_pids)
        assert all(not _alive(p) for p in grandchild_pids)
    finally:
        for p in member_pids:
            _reap_group(p)
        for p in grandchild_pids:
            with contextlib.suppress(ProcessLookupError, OSError):
                os.kill(p, signal.SIGKILL)


def test_pool_stop_is_idempotent() -> None:
    pool = worker.WorkerPool(
        size=1,
        spawn_member=_sleep_argv,
        log=lambda _msg: None,
        once=False,
        prefix="i",
    )
    pool.start()
    pool.stop(timeout=0.5)
    # A second stop after everything is already down must not raise.
    pool.stop(timeout=0.5)
    assert pool.live_member_pids() == []


# --- member argv plumbing (#5) ----------------------------------------------


def _full_args(**overrides: object) -> argparse.Namespace:
    """A Namespace shaped like ``_build_parser``'s output, as a pool parent sees
    it (the fields :func:`worker._pool_member_argv` forwards)."""
    base = {
        "max_turns": 12,
        "max_retries": 4,
        "worktree_retention_days": 7,
        "heartbeat": 30,
        "poll_interval": 5,
        "lease_seconds": 300.0,
        "reconcile_seconds": 15.0,
        "tasks_dir": None,
        "db": None,
        "once": False,
    }
    base.update(overrides)
    return argparse.Namespace(**base)


def test_pool_member_argv_pins_concurrency_one_and_distinct_id() -> None:
    argv = worker._pool_member_argv(_full_args(), "fleet-2", model=None)
    assert argv[:3] == [sys.executable, "-m", "flywheel_worktree.worker"]
    # Concurrency is pinned to 1 so a member runs one task and never recurses
    # into another pool, whatever the config says.
    assert "--concurrency" in argv
    assert argv[argv.index("--concurrency") + 1] == "1"
    assert "--worker-id" in argv
    assert argv[argv.index("--worker-id") + 1] == "fleet-2"


def test_pool_member_argv_forwards_run_knobs() -> None:
    args = _full_args(
        max_turns=9,
        max_retries=2,
        lease_seconds=120.0,
        tasks_dir="/tmp/tasks",
        db="/tmp/db.sqlite",
        once=True,
    )
    argv = worker._pool_member_argv(args, "fleet-0", model="sonnet")
    assert argv[argv.index("--max-turns") + 1] == "9"
    assert argv[argv.index("--max-retries") + 1] == "2"
    assert argv[argv.index("--lease-seconds") + 1] == "120.0"
    assert argv[argv.index("--tasks-dir") + 1] == "/tmp/tasks"
    assert argv[argv.index("--db") + 1] == "/tmp/db.sqlite"
    assert argv[argv.index("--model") + 1] == "sonnet"
    assert "--once" in argv


def test_pool_member_argv_omits_optional_flags_when_unset() -> None:
    argv = worker._pool_member_argv(_full_args(), "fleet-0", model=None)
    assert "--tasks-dir" not in argv
    assert "--db" not in argv
    assert "--model" not in argv
    assert "--once" not in argv


def test_pool_member_argv_distinct_per_member() -> None:
    a = worker._pool_member_argv(_full_args(), "fleet-0", model=None)
    b = worker._pool_member_argv(_full_args(), "fleet-1", model=None)
    assert a[a.index("--worker-id") + 1] == "fleet-0"
    assert b[b.index("--worker-id") + 1] == "fleet-1"

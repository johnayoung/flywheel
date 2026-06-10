"""Tests for :mod:`flywheel._worker_supervisor`.

Drives :class:`WorkerSupervisor` with a trivial child process (a
Python one-liner that sleeps) so the supervisor's ownership /
quit-path / DEAD-after-exit behaviour is exercised end-to-end
without launching the real git-worktree worker daemon (the spec
explicitly approves this substitution for ownership / quit-path
tests). Liveness detection is covered against a real SQLite store
that mirrors what ``SqliteClaimStore`` would persist.
"""

from __future__ import annotations

import os
import signal
import subprocess
import sys
import time
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest

from flywheel_orchestrator import SqliteClaimStore

from flywheel._worker_supervisor import (
    DEFAULT_STOP_TIMEOUT_SECONDS,
    WorkerState,
    WorkerStatus,
    WorkerSupervisor,
    build_default_spawn_argv,
    has_live_lease,
)


# A trivial child process used by every ownership / quit-path test.
# Sleeps long enough that no test races against its self-exit, but the
# tests always send SIGTERM or kill before letting the process linger.
def _sleep_argv(seconds: int = 60) -> list[str]:
    return [sys.executable, "-c", f"import time; time.sleep({seconds})"]


def _wait_for_exit(pid: int, *, timeout: float = 5.0) -> bool:
    """Spin-wait until ``pid`` is no longer running (or ``timeout``).

    Used to verify ``detach()`` keeps the child alive (we wait *for it
    NOT to exit*) and ``stop()`` actually reaped the process.
    """

    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return True
        time.sleep(0.05)
    return False


def _process_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    return True


# --- has_live_lease ---------------------------------------------------------


def test_has_live_lease_returns_false_for_missing_db(tmp_path: Path) -> None:
    """A path that does not exist must not raise -- a fresh project
    cannot have a live lease."""

    db = tmp_path / "absent.sqlite"
    assert has_live_lease(db) is False


def test_has_live_lease_returns_false_for_db_without_claims_table(
    tmp_path: Path,
) -> None:
    """A SQLite file lacking the orchestrator's ``task_claims`` table
    (only ``SqliteStore`` was bootstrapped) reports no live worker."""

    db = tmp_path / "core-only.sqlite"
    from flywheel_core.store_sqlite import SqliteStore

    SqliteStore(db).close()
    assert has_live_lease(db) is False


def test_has_live_lease_true_when_unexpired_lease_present(tmp_path: Path) -> None:
    """A row with ``lease_expires_at`` in the future counts as live."""

    db = tmp_path / "store.sqlite"
    claims = SqliteClaimStore(db)
    try:
        now = datetime.now(timezone.utc)
        claims.acquire_claim(
            "task-1", "worker-A", now=now, lease_seconds=60.0
        )
    finally:
        claims.close()
    assert has_live_lease(db) is True


def test_has_live_lease_false_when_lease_expired(tmp_path: Path) -> None:
    """A lapsed lease is treated as no live worker (the same
    lease-lapse semantics the orchestrator already honours when
    deciding whether to steal a claim)."""

    db = tmp_path / "store.sqlite"
    claims = SqliteClaimStore(db)
    try:
        past = datetime.now(timezone.utc) - timedelta(hours=1)
        claims.acquire_claim(
            "task-1", "worker-A", now=past, lease_seconds=60.0
        )
    finally:
        claims.close()
    assert has_live_lease(db) is False


# --- build_default_spawn_argv ----------------------------------------------


def test_default_spawn_argv_targets_flywheel_worktree_module(
    tmp_path: Path,
) -> None:
    """The default spawn argv runs the worker module under the active
    interpreter and forwards ``--db`` (so the child reads the same
    store the console resolved)."""

    db = tmp_path / "db.sqlite"
    argv = build_default_spawn_argv(db, tasks_dir=None)
    assert argv[0] == sys.executable
    assert argv[1] == "-m"
    assert argv[2] == "flywheel_worktree.worker"
    assert "--db" in argv
    assert str(db) in argv
    assert "--tasks-dir" not in argv  # only added when provided
    # ``--model`` is omitted entirely when the console resolved
    # ``None`` so the worker's own default (also ``None``) lets the
    # SDK fall through to the Claude Code default.
    assert "--model" not in argv


def test_default_spawn_argv_forwards_tasks_dir(tmp_path: Path) -> None:
    db = tmp_path / "db.sqlite"
    tasks = tmp_path / "tasks"
    argv = build_default_spawn_argv(db, tasks_dir=tasks)
    assert "--tasks-dir" in argv
    idx = argv.index("--tasks-dir")
    assert argv[idx + 1] == str(tasks)


def test_default_spawn_argv_forwards_model_when_set(tmp_path: Path) -> None:
    """An effective model id is appended as ``--model <value>``."""

    db = tmp_path / "db.sqlite"
    argv = build_default_spawn_argv(
        db, tasks_dir=None, model="claude-sonnet-4-5"
    )
    assert "--model" in argv
    idx = argv.index("--model")
    assert argv[idx + 1] == "claude-sonnet-4-5"


def test_default_spawn_argv_omits_model_when_none(tmp_path: Path) -> None:
    """Explicit ``model=None`` matches the keyword's default: no flag."""

    db = tmp_path / "db.sqlite"
    argv = build_default_spawn_argv(db, tasks_dir=None, model=None)
    assert "--model" not in argv


# --- WorkerSupervisor lifecycle --------------------------------------------


def test_supervisor_status_is_none_for_fresh_store(tmp_path: Path) -> None:
    """A supervisor wrapping a never-touched store reports ``NONE``."""

    sup = WorkerSupervisor(
        db_path=tmp_path / "absent.sqlite",
        log_dir=tmp_path / "logs",
        spawn_argv=_sleep_argv(),
    )
    assert sup.status().state == WorkerState.NONE
    assert sup.owns_supervised_child() is False


def test_supervisor_start_spawns_supervised_child(tmp_path: Path) -> None:
    """``start()`` on a NONE supervisor flips state to SUPERVISED with a pid."""

    sup = WorkerSupervisor(
        db_path=tmp_path / "absent.sqlite",
        log_dir=tmp_path / "logs",
        spawn_argv=_sleep_argv(),
    )
    try:
        status = sup.start()
        assert status.state == WorkerState.SUPERVISED
        assert status.pid is not None
        assert _process_alive(status.pid)
        # Idempotent: a second start returns the same supervised state
        # and never spawns a peer.
        again = sup.start()
        assert again.state == WorkerState.SUPERVISED
        assert again.pid == status.pid
        # A supervisor log file lands in the configured log_dir.
        logs = list((tmp_path / "logs").glob("supervisor-*.log"))
        assert len(logs) == 1
    finally:
        sup.stop(timeout=2.0)


def test_supervisor_does_not_spawn_when_lease_is_live(tmp_path: Path) -> None:
    """A live lease in the store -> ``start()`` returns DETACHED with no spawn."""

    db = tmp_path / "store.sqlite"
    claims = SqliteClaimStore(db)
    try:
        now = datetime.now(timezone.utc)
        claims.acquire_claim(
            "task-detached", "worker-external", now=now, lease_seconds=60.0
        )
    finally:
        claims.close()
    sup = WorkerSupervisor(
        db_path=db,
        log_dir=tmp_path / "logs",
        spawn_argv=_sleep_argv(),
    )
    status = sup.start()
    assert status.state == WorkerState.DETACHED
    assert sup.owns_supervised_child() is False
    # No supervisor log was written because no spawn happened.
    assert not (tmp_path / "logs").exists() or not list(
        (tmp_path / "logs").glob("supervisor-*.log")
    )


def test_supervisor_detach_keeps_child_alive(tmp_path: Path) -> None:
    """``detach()`` forgets the child without signaling it; the
    process keeps running so a second console can take over."""

    sup = WorkerSupervisor(
        db_path=tmp_path / "absent.sqlite",
        log_dir=tmp_path / "logs",
        spawn_argv=_sleep_argv(),
    )
    status = sup.start()
    pid = status.pid
    assert pid is not None
    try:
        sup.detach()
        # Supervisor no longer owns a child.
        assert sup.status().state == WorkerState.NONE
        assert sup.owns_supervised_child() is False
        # Detached child is still running.
        assert _process_alive(pid)
    finally:
        # Clean up the detached process so the test does not leak.
        try:
            os.kill(pid, signal.SIGTERM)
            _wait_for_exit(pid, timeout=2.0)
        except ProcessLookupError:
            pass


def test_supervisor_stop_terminates_and_reaps(tmp_path: Path) -> None:
    """``stop()`` SIGTERMs the supervised child and waits for it to exit.

    After ``stop()`` the supervisor reports ``DEAD`` (a stopped child
    is a dead child until the operator respawns); the underlying
    process is fully reaped, so ``os.kill(pid, 0)`` raises.
    """

    sup = WorkerSupervisor(
        db_path=tmp_path / "absent.sqlite",
        log_dir=tmp_path / "logs",
        spawn_argv=_sleep_argv(),
    )
    status = sup.start()
    pid = status.pid
    assert pid is not None
    assert sup.stop(timeout=5.0) is True
    assert _wait_for_exit(pid, timeout=2.0)
    status_after = sup.status()
    assert status_after.state == WorkerState.DEAD
    assert status_after.pid == pid


def test_supervisor_stop_without_child_is_noop(tmp_path: Path) -> None:
    """``stop()`` on a supervisor that never spawned returns False."""

    sup = WorkerSupervisor(
        db_path=tmp_path / "absent.sqlite",
        log_dir=tmp_path / "logs",
        spawn_argv=_sleep_argv(),
    )
    assert sup.stop(timeout=1.0) is False


def test_supervisor_status_reports_dead_after_unexpected_exit(
    tmp_path: Path,
) -> None:
    """A supervised child that exits on its own surfaces as DEAD."""

    sup = WorkerSupervisor(
        db_path=tmp_path / "absent.sqlite",
        log_dir=tmp_path / "logs",
        # The child exits immediately with a non-zero code.
        spawn_argv=[sys.executable, "-c", "import sys; sys.exit(7)"],
    )
    status = sup.start()
    assert status.state == WorkerState.SUPERVISED
    pid = status.pid
    assert pid is not None
    # Wait for the child to actually exit before polling status.
    deadline = time.monotonic() + 5.0
    while time.monotonic() < deadline:
        result = sup.status()
        if result.state == WorkerState.DEAD:
            assert result.pid == pid
            assert result.message is not None
            assert "exit=7" in result.message
            return
        time.sleep(0.05)
    pytest.fail("supervisor never reported DEAD for an exited child")


def test_supervisor_restart_after_dead_clears_state(tmp_path: Path) -> None:
    """``start()`` after a DEAD child spawns a fresh supervised child.

    The new worker's startup recovery sweep handles any stranded
    lifecycles (existing semantics); the supervisor just respawns and
    transitions out of DEAD on the next ``status()`` query.
    """

    quick_exit = [sys.executable, "-c", "import sys; sys.exit(1)"]
    sup = WorkerSupervisor(
        db_path=tmp_path / "absent.sqlite",
        log_dir=tmp_path / "logs",
        spawn_argv=quick_exit,
    )
    sup.start()
    # Wait for DEAD to settle.
    deadline = time.monotonic() + 5.0
    while time.monotonic() < deadline:
        if sup.status().state == WorkerState.DEAD:
            break
        time.sleep(0.05)
    # Substitute a long-lived argv and respawn.
    sup._spawn_argv = _sleep_argv()  # test-only patch of the spawn argv
    try:
        status = sup.start()
        assert status.state == WorkerState.SUPERVISED
        assert sup.owns_supervised_child() is True
    finally:
        sup.stop(timeout=2.0)


def test_supervisor_start_failure_surfaces_as_error_state(
    tmp_path: Path,
) -> None:
    """A ``Popen`` failure (bad executable) flips state to ``ERROR``."""

    sup = WorkerSupervisor(
        db_path=tmp_path / "absent.sqlite",
        log_dir=tmp_path / "logs",
        spawn_argv=[str(tmp_path / "does-not-exist")],
    )
    status = sup.start()
    assert status.state == WorkerState.ERROR
    assert status.message is not None
    # Subsequent ``status()`` calls keep reporting ERROR until a
    # successful ``start()`` clears it.
    assert sup.status().state == WorkerState.ERROR


def test_default_stop_timeout_is_documented_constant() -> None:
    """The documented default is the public constant; guards against
    accidental drift between the constant and the method signature."""

    assert DEFAULT_STOP_TIMEOUT_SECONDS == 10.0


def test_close_is_a_detach_alias(tmp_path: Path) -> None:
    """``close()`` performs a detach so the TUI ``finally`` block never
    silently kills the child (spec: SIGINT path == detach path)."""

    sup = WorkerSupervisor(
        db_path=tmp_path / "absent.sqlite",
        log_dir=tmp_path / "logs",
        spawn_argv=_sleep_argv(),
    )
    pid = sup.start().pid
    assert pid is not None
    try:
        sup.close()
        assert _process_alive(pid)
    finally:
        try:
            os.kill(pid, signal.SIGTERM)
            _wait_for_exit(pid, timeout=2.0)
        except ProcessLookupError:
            pass


def test_status_dataclass_is_frozen() -> None:
    """:class:`WorkerStatus` is frozen so a dashboard cannot mutate
    the supervisor's cached state through it."""

    status = WorkerStatus(state=WorkerState.NONE)
    with pytest.raises(Exception):  # noqa: PT011 - FrozenInstanceError or AttributeError
        status.state = WorkerState.SUPERVISED  # type: ignore[misc]


# --- subprocess: real worker module (smoke test) ---------------------------


def test_default_argv_runs_worker_module_help(tmp_path: Path) -> None:
    """Smoke-check the default argv: ``python -m flywheel_worktree.worker
    --help`` exits 0 with the worker's description in stdout.

    Confirms the module path is invocable (so a real spawn would
    reach the worker's argparse, not a ``ModuleNotFoundError``).
    """

    argv = build_default_spawn_argv(tmp_path / "db.sqlite", tasks_dir=None)
    proc = subprocess.run(
        [*argv[:3], "--help"],
        capture_output=True,
        text=True,
        check=False,
    )
    assert proc.returncode == 0
    assert "worktree" in proc.stdout.lower()

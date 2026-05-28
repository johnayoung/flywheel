"""Tests for the spawn-failure circuit breaker in ``.workflow/task-worker.sh``.

The audit at ``.workflow/audits/08-recoverable-blocked-lifecycles.md``
documented the loop spinning 76 times against a threshold of 3 because
``spawn_eligible`` reset ``SPAWN_FAILURES[$task_id]`` on every successful
``create_worktree``, while the no-lifecycle-row crash path could only
ever push the counter to 1 before the next reset.

This test drives the real bash worker against a stub ``uv`` shim that
always crashes ``flywheel.workflow run`` before touching the DB and
asserts the worker stops spawning the same task after
``SPAWN_FAILURE_THRESHOLD`` attempts. No claude-agent-sdk involved.
"""

from __future__ import annotations

import json
import os
import select
import signal
import stat
import subprocess
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
WORKER_SH = REPO_ROOT / ".workflow" / "task-worker.sh"


def _write_uv_shim(shim_path: Path, task_file: Path) -> None:
    """Drop an executable ``uv`` shim that mimics ``flywheel.workflow``.

    The worker invokes its ``run_workflow`` helper as
    ``uv run python -m flywheel.workflow <subcommand> ...``. The shim
    intercepts that exact shape:

    * ``next`` always echoes the same task file path (forever picking
      the broken task)
    * ``run`` exits non-zero with no DB writes (simulates a crash before
      ``store.create_lifecycle`` -- the precise failure mode the audit
      caught)
    * ``archive`` and ``live`` produce no output
    """
    shim_path.write_text(
        "#!/usr/bin/env bash\n"
        "# Stub `uv` for the circuit-breaker test.\n"
        "# Layout when the worker calls us:\n"
        "#   $1=run $2=python $3=-m $4=flywheel.workflow $5=<subcommand>\n"
        'if [[ "${1:-}" == "run" && "${4:-}" == "flywheel.workflow" ]]; then\n'
        '  case "${5:-}" in\n'
        f'    next)    echo {task_file}; exit 0 ;;\n'
        "    run)     exit 1 ;;\n"
        "    archive) exit 0 ;;\n"
        "    live)    exit 0 ;;\n"
        "    *)       exit 0 ;;\n"
        "  esac\n"
        "fi\n"
        "exit 0\n"
    )
    mode = shim_path.stat().st_mode
    shim_path.chmod(mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


def _init_sandbox_repo(sandbox: Path) -> None:
    """Create a minimal git repo on `main` with one commit so the worker can run."""
    sandbox.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        ["git", "init", "-q", "--initial-branch=main", str(sandbox)],
        check=True,
    )
    subprocess.run(
        ["git", "-C", str(sandbox), "config", "user.email", "test@example.com"],
        check=True,
    )
    subprocess.run(
        ["git", "-C", str(sandbox), "config", "user.name", "Test"],
        check=True,
    )
    subprocess.run(
        [
            "git",
            "-C",
            str(sandbox),
            "commit",
            "--allow-empty",
            "-q",
            "-m",
            "init",
        ],
        check=True,
    )


def _drain_until(
    proc: subprocess.Popen[bytes],
    needle: str,
    deadline: float,
) -> tuple[bool, str]:
    """Read stderr until `needle` appears or `deadline` elapses.

    Uses ``select`` so the test can give up on a deadline instead of
    blocking forever in ``readline()``. Returns (matched, captured_text).
    """
    assert proc.stderr is not None
    fd = proc.stderr.fileno()
    chunks: list[str] = []
    while time.time() < deadline:
        remaining = max(0.0, deadline - time.time())
        ready, _, _ = select.select([fd], [], [], min(0.5, remaining))
        if fd in ready:
            chunk = os.read(fd, 4096)
            if not chunk:
                break
            chunks.append(chunk.decode("utf-8", errors="replace"))
            captured = "".join(chunks)
            if needle in captured:
                return True, captured
        if proc.poll() is not None:
            # Drain anything left, then bail.
            try:
                tail = os.read(fd, 65536).decode("utf-8", errors="replace")
                if tail:
                    chunks.append(tail)
            except OSError:
                pass
            break
    return False, "".join(chunks)


def _terminate(proc: subprocess.Popen[bytes]) -> None:
    """Best-effort shutdown of the still-running worker."""
    if proc.poll() is not None:
        return
    # Escalating SIGINT matches the worker's own shutdown handler.
    for _ in range(3):
        if proc.poll() is not None:
            return
        try:
            proc.send_signal(signal.SIGINT)
        except ProcessLookupError:
            return
        try:
            proc.wait(timeout=3)
            return
        except subprocess.TimeoutExpired:
            continue
    if proc.poll() is None:
        proc.kill()
        try:
            proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            pass


def test_circuit_breaker_trips_on_repeated_no_lifecycle_row_crashes(
    tmp_path: Path,
) -> None:
    """A stubbed workflow that always exits non-zero must trip the breaker.

    Prior to the fix, ``spawn_eligible`` reset ``SPAWN_FAILURES[$task_id]``
    on every successful ``create_worktree``, so the no-row crash branch in
    ``remove_finished`` ping-ponged 0/1 forever. After the fix, the reset
    is gated on observing a non-empty lifecycle status, so three
    consecutive no-row crashes accumulate to ``SPAWN_FAILURE_THRESHOLD``
    and the worker stops spawning the task.
    """
    sandbox = tmp_path / "repo"
    _init_sandbox_repo(sandbox)

    # Worker reads .workflow/task-worker.sh relative to wherever the operator
    # invoked it; we point it at the real script but execute inside the
    # sandbox so `git rev-parse --show-toplevel` resolves to the sandbox.
    shim_dir = tmp_path / "bin"
    shim_dir.mkdir()

    tasks_dir = sandbox / ".workflow" / "tasks" / "active" / "test-phase"
    tasks_dir.mkdir(parents=True)
    task_file = tasks_dir / "broken-task.json"
    task_file.write_text(
        json.dumps(
            {
                "id": "broken-task",
                "goal": "Always crash before writing a lifecycle row.",
                "graders": [{"type": "command", "run": "true"}],
            }
        )
    )

    _write_uv_shim(shim_dir / "uv", task_file)

    log_dir = tmp_path / "logs"
    db_path = tmp_path / "flywheel.sqlite"

    env = os.environ.copy()
    env["PATH"] = f"{shim_dir}{os.pathsep}{env['PATH']}"
    # Strip any inherited harness vars that could leak into the worker run.
    for key in ("CLAUDE_AGENT_TOKEN", "ANTHROPIC_API_KEY"):
        env.pop(key, None)

    proc = subprocess.Popen(
        [
            "bash",
            str(WORKER_SH),
            "--tasks-dir",
            str(sandbox / ".workflow" / "tasks"),
            "--db",
            str(db_path),
            "--log-dir",
            str(log_dir),
            "--heartbeat",
            "0",
            "--max-parallel",
            "1",
        ],
        cwd=sandbox,
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        bufsize=0,
    )

    try:
        deadline = time.time() + 30
        tripped, captured = _drain_until(proc, "giving up", deadline)
    finally:
        _terminate(proc)

    assert tripped, (
        "Expected the spawn-failure circuit breaker to trip with a "
        "'giving up' message within 30s. Captured worker stderr:\n" + captured
    )
    # Same lifecycle/no-row message text expected at .workflow/task-worker.sh:697.
    assert "broken-task finished with no lifecycle row" in captured, (
        "Expected the 'no lifecycle row' diagnostic in stderr. Captured:\n"
        + captured
    )

"""Command grader runner.

Executes the ``command`` graders declared on a :class:`flywheel.task.Task` in
list order, persisting one row per execution to a :class:`GraderResultStore`
in the documented append-only shape.

Contract surface (see ``docs/task-schema.md`` and ``docs/persistence-schema.sql``):

* Graders are run in the order they appear on ``task.graders``. On the first
  failure, later command graders are skipped (matches the cost-order
  contract). Non-``command`` graders are never executed; their ordinals are
  still preserved so audits can reconstruct the original list from
  ``grader_results.ordinal`` alone.
* Pass is strictly ``exit_code == 0``. Non-zero exit, signal termination,
  and timeout are all failures and are distinguishable in ``payload`` via a
  ``termination`` discriminator (``exited`` | ``signal`` | ``timeout``).
* ``grader_spec`` snapshots the input grader verbatim at run time so later
  edits to the task definition do not rewrite historical truth.
* stdout/stderr capture is bounded (tail-only) with a deterministic byte
  boundary. Full streams may optionally be written to ``artifacts_dir``;
  when written, their paths are surfaced via ``stdout_path`` / ``stderr_path``
  in the payload.

The runner has no opinion about retry policy, lifecycle transitions, or
which graders come after ``command`` — those are the harness's concern.
"""

from __future__ import annotations

import os
import subprocess
import time
from collections.abc import Callable, Mapping
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from flywheel.store_protocols import GraderResultRecord, GraderResultStore
from flywheel.task import CommandGrader, Task

DEFAULT_TAIL_BYTES: int = 8192


def _utcnow() -> datetime:
    return datetime.now(timezone.utc)


def _bounded_tail(data: bytes, max_bytes: int) -> tuple[str, bool]:
    """Return the last ``max_bytes`` of ``data`` decoded as UTF-8 (with
    replacement) plus a truncation flag.

    Boundary is byte-deterministic: identical input bytes produce an
    identical tail string regardless of locale or process state. Multi-byte
    characters cut by the boundary are replaced via ``errors="replace"``.
    """

    if max_bytes <= 0 or len(data) == 0:
        return "", len(data) > 0
    if len(data) <= max_bytes:
        return data.decode("utf-8", errors="replace"), False
    tail = data[-max_bytes:]
    return tail.decode("utf-8", errors="replace"), True


def _grader_spec_snapshot(grader: CommandGrader) -> dict[str, Any]:
    """Snapshot the grader object as it appeared in the task at run time."""
    spec: dict[str, Any] = {"type": "command", "run": grader.run}
    if grader.name is not None:
        spec["name"] = grader.name
    return spec


def _safe_label(label: str) -> str:
    return "".join(
        ch if ch.isalnum() or ch in ("-", "_", ".") else "_" for ch in label
    )


def _write_artifact(path: Path, content: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(content)


def run_command_graders(
    task: Task,
    store: GraderResultStore,
    *,
    run_id: str,
    attempt_number: int,
    cwd: str | os.PathLike[str] | None = None,
    env: Mapping[str, str] | None = None,
    per_grader_timeout_seconds: float | None = None,
    stdout_tail_bytes: int = DEFAULT_TAIL_BYTES,
    stderr_tail_bytes: int = DEFAULT_TAIL_BYTES,
    artifacts_dir: str | os.PathLike[str] | None = None,
    now: Callable[[], datetime] | None = None,
) -> list[GraderResultRecord]:
    """Run every ``command`` grader on ``task.graders`` in list order.

    For each command grader:

    1. Spawn the subprocess (``shell=True``) with the supplied ``cwd`` / ``env``.
    2. Wait for completion, enforcing ``per_grader_timeout_seconds`` if set.
    3. Capture bounded tails of stdout/stderr. If ``artifacts_dir`` is set,
       also persist the full streams alongside their tail in the payload.
    4. Append one :class:`GraderResultRecord` to ``store`` whose
       ``grader_spec`` snapshots the grader, ``payload`` matches the
       documented command shape, and ``ordinal`` is the grader's index in
       ``task.graders`` (so non-command graders preserve numbering).

    On the first failing command grader, later command graders are not
    executed. Non-command graders are skipped without persisting any row;
    transcript / rubric / manual graders belong to other runners.

    Returns the persisted records in execution order (in store-assigned form,
    i.e. with ``id`` populated).
    """

    clock = now or _utcnow
    artifacts_path = Path(artifacts_dir) if artifacts_dir is not None else None
    persisted: list[GraderResultRecord] = []
    aborted = False

    for ordinal, grader in enumerate(task.graders):
        if not isinstance(grader, CommandGrader):
            continue
        if aborted:
            break

        spec = _grader_spec_snapshot(grader)
        ts_start = clock()
        start_ns = time.monotonic_ns()

        proc = subprocess.Popen(
            grader.run,
            shell=True,
            cwd=cwd,
            env=dict(env) if env is not None else None,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )

        timed_out = False
        try:
            stdout, stderr = proc.communicate(
                timeout=per_grader_timeout_seconds
            )
        except subprocess.TimeoutExpired:
            timed_out = True
            proc.kill()
            stdout, stderr = proc.communicate()

        end_ns = time.monotonic_ns()
        duration_ms = (end_ns - start_ns) // 1_000_000

        exit_code = proc.returncode

        stdout_tail, stdout_truncated = _bounded_tail(stdout, stdout_tail_bytes)
        stderr_tail, stderr_truncated = _bounded_tail(stderr, stderr_tail_bytes)

        payload: dict[str, Any] = {
            "run": grader.run,
            "exit_code": exit_code,
            "stdout_tail": stdout_tail,
            "stderr_tail": stderr_tail,
            "stdout_truncated": stdout_truncated,
            "stderr_truncated": stderr_truncated,
        }

        if timed_out:
            payload["termination"] = "timeout"
            if per_grader_timeout_seconds is not None:
                payload["timeout_seconds"] = per_grader_timeout_seconds
        elif exit_code is not None and exit_code < 0:
            payload["termination"] = "signal"
            payload["signal"] = -exit_code
        else:
            payload["termination"] = "exited"

        if artifacts_path is not None:
            label = _safe_label(grader.name or "command")
            stem = f"grader-{ordinal:03d}-{label}"
            stdout_path = artifacts_path / f"{stem}.stdout"
            stderr_path = artifacts_path / f"{stem}.stderr"
            _write_artifact(stdout_path, stdout)
            _write_artifact(stderr_path, stderr)
            payload["stdout_path"] = str(stdout_path)
            payload["stderr_path"] = str(stderr_path)

        passed = (not timed_out) and exit_code == 0

        record = GraderResultRecord(
            run_id=run_id,
            attempt_number=attempt_number,
            ordinal=ordinal,
            grader_type="command",
            grader_spec=spec,
            grader_name=grader.name,
            passed=passed,
            duration_ms=int(duration_ms),
            payload=payload,
            ts=ts_start,
        )
        persisted.append(store.append_grader_result(record))

        if not passed:
            aborted = True

    return persisted


__all__ = [
    "DEFAULT_TAIL_BYTES",
    "run_command_graders",
]

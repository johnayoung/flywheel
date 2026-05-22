"""Contract tests for :mod:`flywheel.examples.hello`.

Exercises the documented entry point — both the importable
:func:`run_hello_example` and the ``python -m flywheel.examples.hello``
subprocess invocation — and asserts the smoke contract:

* Lifecycle reaches ``done`` against a real SQLite store.
* Harness events are streamed to stdout in their structured form (one
  JSON object per line).
* Re-running against the same SQLite file does not corrupt prior runs'
  audit data — new ``run_id`` per invocation, prior rows still load.
* The CLI exits ``0`` on success.
"""

from __future__ import annotations

import asyncio
import json
import subprocess
import sys
from pathlib import Path

from flywheel import SqliteStore, Status
from flywheel.examples.hello import build_task, run_hello_example


class TestHelloExample:
    def test_runs_to_done_against_sqlite(self, tmp_path: Path) -> None:
        db = tmp_path / "hello.sqlite"
        outcome = asyncio.run(run_hello_example(db))

        assert outcome.lifecycle.status == Status.DONE
        assert db.exists()
        assert len(outcome.attempts) == 1
        assert outcome.attempts[0].outcome is not None
        assert outcome.attempts[0].outcome.value == "succeeded"

    def test_persisted_state_reloads_from_sqlite(self, tmp_path: Path) -> None:
        db = tmp_path / "hello.sqlite"
        outcome = asyncio.run(run_hello_example(db))

        store = SqliteStore(db)
        try:
            reloaded = store.load_lifecycle(outcome.lifecycle.run_id)
            assert reloaded is not None
            assert reloaded.status == Status.DONE
            attempts = store.list_attempts(outcome.lifecycle.run_id)
            assert len(attempts) == 1
            grader_rows = store.list_grader_results(
                outcome.lifecycle.run_id, 1
            )
            assert [r.grader_type for r in grader_rows] == ["command"]
            assert all(r.passed for r in grader_rows)
        finally:
            store.close()

    def test_repeated_runs_preserve_prior_audit_trail(
        self, tmp_path: Path
    ) -> None:
        db = tmp_path / "hello.sqlite"
        first = asyncio.run(run_hello_example(db))
        second = asyncio.run(run_hello_example(db))

        assert first.lifecycle.run_id != second.lifecycle.run_id
        assert first.lifecycle.status == Status.DONE
        assert second.lifecycle.status == Status.DONE

        store = SqliteStore(db)
        try:
            assert store.load_lifecycle(first.lifecycle.run_id) is not None
            assert store.load_lifecycle(second.lifecycle.run_id) is not None
            first_attempts = store.list_attempts(first.lifecycle.run_id)
            second_attempts = store.list_attempts(second.lifecycle.run_id)
            assert len(first_attempts) == 1
            assert len(second_attempts) == 1
        finally:
            store.close()

    def test_entry_point_streams_structured_events_to_stdout(
        self, tmp_path: Path
    ) -> None:
        db = tmp_path / "hello.sqlite"
        completed = subprocess.run(
            [sys.executable, "-m", "flywheel.examples.hello", str(db)],
            capture_output=True,
            text=True,
            check=False,
        )

        assert completed.returncode == 0, completed.stderr
        lines = [
            line for line in completed.stdout.splitlines() if line.strip()
        ]
        assert lines, "expected at least one event on stdout"

        events: list[dict[str, object]] = []
        for line in lines:
            parsed = json.loads(line)
            assert isinstance(parsed, dict)
            for key in ("id", "run_id", "ts", "kind", "payload"):
                assert key in parsed, f"event missing {key!r}: {parsed!r}"
            events.append(parsed)

        kinds = {event["kind"] for event in events}
        assert "harness.attempt_started" in kinds
        assert "harness.iteration_completed" in kinds
        assert "harness.attempt_finalized" in kinds

    def test_command_grader_runs_real_shell_invocation(self) -> None:
        """The trivial task's grader must be a real, deterministic shell
        command that exits 0 — not a fictional invocation."""
        task = build_task()
        assert len(task.graders) == 1
        grader = task.graders[0]
        assert grader.type == "command"
        completed = subprocess.run(
            grader.run,  # type: ignore[attr-defined]
            shell=True,
            capture_output=True,
            text=True,
            check=False,
        )
        assert completed.returncode == 0, completed.stderr

"""Contract tests for :mod:`flywheel_core.examples.hello`.

The example's production agent is the real Claude Code CLI driven via
``claude_agent_sdk.query``. Tests inject a fake :class:`InvokeFunc`
through the same hot-swap seam — they exercise the wiring (task shape,
SQLite store dump, structured event streaming) without spawning a live
agent subprocess.

Two contracts the example must keep no matter how the agent is wired:

* The :class:`CommandGrader` runs a real shell command — when the agent
  has written the expected file, the grader returns exit ``0``; when
  the agent has not, the grader fails. Both branches are checked here
  with subprocess.
* The example's :func:`dump_store_state` produces output that lets a
  human reconstruct the entire run from one read of stdout — lifecycle
  row, every attempt, every event, every grader result.
"""

from __future__ import annotations

import asyncio
import io
import json
import subprocess
from pathlib import Path

from claude_agent_sdk import AssistantMessage, ResultMessage, TextBlock

from flywheel_core import (
    InvocationRequest,
    InvocationSignals,
    IterationResult,
    SqliteStore,
    Status,
    ValidEnvelope,
)
from flywheel_core.envelope import CLOSING_FENCE, Intent, OPENING_FENCE
from flywheel_core.examples.hello import (
    TARGET_CONTENT,
    TARGET_FILENAME,
    build_task,
    dump_store_state,
    run_hello_example,
)


def _make_writing_invoke(target_path: Path):
    """Build a fake invoker that simulates an agent writing the target file.

    Stand-in for what the real Claude Code agent does when invoked: write
    ``TARGET_CONTENT`` into ``target_path``, then emit an
    ``intent=verify`` envelope. The wiring under test is the harness ->
    store -> grader path, not the agent itself.
    """
    transcript = (
        "Wrote the file.\n"
        f"{OPENING_FENCE}\n"
        '{"intent": "verify", "reason": "file written"}\n'
        f"{CLOSING_FENCE}\n"
    )

    async def _invoke(_request: InvocationRequest) -> IterationResult:
        target_path.parent.mkdir(parents=True, exist_ok=True)
        target_path.write_text(TARGET_CONTENT + "\n", encoding="utf-8")
        usage = {
            "input_tokens": 50,
            "output_tokens": 12,
            "cache_read_input_tokens": 0,
            "cache_creation_input_tokens": 0,
        }
        assistant = AssistantMessage(
            content=[TextBlock(text=transcript)],
            model="hello-test",
            stop_reason="end_turn",
            session_id="hello-test-session",
            usage=usage,
        )
        result = ResultMessage(
            subtype="success",
            duration_ms=1,
            duration_api_ms=1,
            is_error=False,
            num_turns=1,
            session_id="hello-test-session",
            stop_reason="end_turn",
            total_cost_usd=0.0,
            usage=usage,
        )
        signals = InvocationSignals(
            stop_reason="end_turn",
            num_turns=1,
            total_cost_usd=0.0,
            result_is_error=False,
            result_subtype="success",
            api_error_status=None,
            session_id="hello-test-session",
        )
        return IterationResult(
            transcript=transcript,
            messages=(assistant, result),
            envelope=ValidEnvelope(
                intent=Intent.VERIFY, reason="file written"
            ),
            signals=signals,
        )

    return _invoke


class TestRunHelloExample:
    def test_runs_to_done_when_agent_writes_expected_file(
        self, tmp_path: Path
    ) -> None:
        db = tmp_path / "hello.sqlite"
        sandbox = tmp_path / "sandbox"
        invoke = _make_writing_invoke(sandbox / TARGET_FILENAME)

        outcome = asyncio.run(
            run_hello_example(
                db_path=db, sandbox=sandbox, invoke=invoke
            )
        )

        assert outcome.lifecycle.status == Status.DONE
        assert (sandbox / TARGET_FILENAME).read_text() == (
            TARGET_CONTENT + "\n"
        )
        assert len(outcome.attempts) == 1
        attempt = outcome.attempts[0]
        assert attempt.outcome is not None
        assert attempt.outcome.value == "succeeded"

    def test_run_id_unique_across_repeated_runs(self, tmp_path: Path) -> None:
        db = tmp_path / "hello.sqlite"
        sandbox = tmp_path / "sandbox"
        invoke = _make_writing_invoke(sandbox / TARGET_FILENAME)

        first = asyncio.run(
            run_hello_example(db_path=db, sandbox=sandbox, invoke=invoke)
        )
        second = asyncio.run(
            run_hello_example(db_path=db, sandbox=sandbox, invoke=invoke)
        )
        assert first.lifecycle.run_id != second.lifecycle.run_id

        store = SqliteStore(db)
        try:
            assert store.load_lifecycle(first.lifecycle.run_id) is not None
            assert store.load_lifecycle(second.lifecycle.run_id) is not None
        finally:
            store.close()

    def test_sqlite_persists_full_audit_trail(self, tmp_path: Path) -> None:
        db = tmp_path / "hello.sqlite"
        sandbox = tmp_path / "sandbox"
        invoke = _make_writing_invoke(sandbox / TARGET_FILENAME)

        outcome = asyncio.run(
            run_hello_example(db_path=db, sandbox=sandbox, invoke=invoke)
        )

        store = SqliteStore(db)
        try:
            lifecycle = store.load_lifecycle(outcome.lifecycle.run_id)
            assert lifecycle is not None
            assert lifecycle.status == Status.DONE

            attempts = store.list_attempts(outcome.lifecycle.run_id)
            assert len(attempts) == 1

            grader_rows = store.list_grader_results(
                outcome.lifecycle.run_id, 1
            )
            grader_types = [r.grader_type for r in grader_rows]
            assert "command" in grader_types
            assert all(r.passed for r in grader_rows)

            # Telemetry lives in the per-run JSONL file, not the store
            # (spec 00025): the store no longer exposes a telemetry read
            # at all, and every events row is a domain event.
            assert not hasattr(store, "list_events")
            assert store.list_domain_events(outcome.lifecycle.run_id)
        finally:
            store.close()

        run_file = (
            db.parent / "logs" / "runs" / f"{outcome.lifecycle.run_id}.jsonl"
        )
        assert run_file.exists()
        kinds = {
            json.loads(line)["kind"]
            for line in run_file.read_text(encoding="utf-8").splitlines()
            if line.strip()
        }
        assert "harness.attempt_started" in kinds
        assert "harness.iteration_completed" in kinds
        assert "harness.attempt_finalized" in kinds


class TestDumpStoreState:
    def test_dump_emits_lifecycle_attempts_events_and_graders(
        self, tmp_path: Path
    ) -> None:
        db = tmp_path / "hello.sqlite"
        sandbox = tmp_path / "sandbox"
        invoke = _make_writing_invoke(sandbox / TARGET_FILENAME)

        outcome = asyncio.run(
            run_hello_example(db_path=db, sandbox=sandbox, invoke=invoke)
        )

        store = SqliteStore(db)
        buffer = io.StringIO()
        try:
            dump_store_state(
                store,
                outcome.lifecycle.run_id,
                out=buffer,
                logs_root=db.parent / "logs",
            )
        finally:
            store.close()

        text = buffer.getvalue()
        assert "=== lifecycle" in text
        assert outcome.lifecycle.run_id in text
        assert "status       : done" in text
        assert "=== attempts" in text
        assert "succeeded" in text
        assert "=== events" in text
        assert "harness.attempt_started" in text
        assert "harness.attempt_finalized" in text
        assert "hello-file-exact-match" in text
        assert "PASS" in text


class TestCommandGraderIsReal:
    def test_grader_passes_when_file_matches(self, tmp_path: Path) -> None:
        sandbox = tmp_path / "sandbox"
        sandbox.mkdir()
        (sandbox / TARGET_FILENAME).write_text(
            TARGET_CONTENT + "\n", encoding="utf-8"
        )

        task = build_task(sandbox)
        command_grader = task.graders[0]
        assert command_grader.type == "command"

        completed = subprocess.run(
            command_grader.run,  # type: ignore[attr-defined]
            shell=True,
            capture_output=True,
            text=True,
            check=False,
        )
        assert completed.returncode == 0, completed.stderr

    def test_grader_fails_when_file_missing(self, tmp_path: Path) -> None:
        sandbox = tmp_path / "sandbox"
        sandbox.mkdir()

        task = build_task(sandbox)
        command_grader = task.graders[0]

        completed = subprocess.run(
            command_grader.run,  # type: ignore[attr-defined]
            shell=True,
            capture_output=True,
            text=True,
            check=False,
        )
        assert completed.returncode != 0

    def test_grader_fails_when_file_content_differs(
        self, tmp_path: Path
    ) -> None:
        sandbox = tmp_path / "sandbox"
        sandbox.mkdir()
        (sandbox / TARGET_FILENAME).write_text(
            "something else\n", encoding="utf-8"
        )

        task = build_task(sandbox)
        command_grader = task.graders[0]

        completed = subprocess.run(
            command_grader.run,  # type: ignore[attr-defined]
            shell=True,
            capture_output=True,
            text=True,
            check=False,
        )
        assert completed.returncode != 0


class TestStreamedEvents:
    def test_event_lines_are_emitted_during_run(
        self, tmp_path: Path, capsys: object
    ) -> None:
        """The example forwards every persisted event to stdout in a
        structurally consumable form: one ``event {json}`` line per
        harness event. The final ``dump_store_state`` block is also
        written to stdout. Capture both and verify."""
        db = tmp_path / "hello.sqlite"
        sandbox = tmp_path / "sandbox"
        invoke = _make_writing_invoke(sandbox / TARGET_FILENAME)

        asyncio.run(
            run_hello_example(db_path=db, sandbox=sandbox, invoke=invoke)
        )

        captured = capsys.readouterr()  # type: ignore[attr-defined]
        stdout = captured.out

        event_lines = [
            line[len("event "):]
            for line in stdout.splitlines()
            if line.startswith("event ")
        ]
        assert event_lines, "expected at least one streamed event"

        for line in event_lines:
            parsed = json.loads(line)
            assert isinstance(parsed, dict)
            for key in ("run_id", "ts", "kind", "payload"):
                assert key in parsed, f"event missing {key!r}: {parsed!r}"

        kinds = {json.loads(line)["kind"] for line in event_lines}
        assert "harness.attempt_started" in kinds
        assert "harness.iteration_completed" in kinds
        assert "harness.attempt_finalized" in kinds

        # The dump_store_state block is also written to stdout.
        assert "=== lifecycle" in stdout

"""Behavioral tests for ``flywheel_core.grader_command.run_command_graders``.

Each test asserts a property of the documented contract: ordering by
``Task.graders`` index, abort-after-failure, distinguishable failure
records for non-zero exit / signal / timeout, bounded stdout/stderr
tails, and ``grader_spec_json`` snapshotting that survives later edits.
"""

from __future__ import annotations

from datetime import datetime, timezone
from pathlib import Path

import pytest

from flywheel_core import (
    CommandGrader,
    Context,
    GraderResultRecord,
    InMemoryStore,
    ManualGrader,
    RubricGrader,
    Task,
    TranscriptGrader,
    run_command_graders,
)


def _attempt_run_id(store: InMemoryStore, run_id: str = "r1") -> None:
    """Bootstrap a lifecycle + attempt so FK-aware stores accept results.

    ``InMemoryStore`` does not enforce FKs, but using the same shape as
    ``test_store_contract`` keeps these tests honest about the audit path.
    """

    from flywheel_core import Attempt, Lifecycle

    if store.load_lifecycle(run_id) is None:
        store.create_lifecycle(Lifecycle(task_id="t", run_id=run_id))
    if store.load_attempt(run_id, 1) is None:
        store.save_attempt(
            run_id,
            Attempt(
                number=1,
                started_at=datetime(2024, 1, 1, tzinfo=timezone.utc),
                run_id=run_id,
            ),
        )


def _task(*graders: object) -> Task:
    return Task(goal="g", graders=list(graders))  # type: ignore[arg-type]


def _run(
    task: Task,
    store: InMemoryStore,
    **kwargs: object,
) -> list[GraderResultRecord]:
    return run_command_graders(
        task,
        store,
        run_id="r1",
        attempt_number=1,
        **kwargs,  # type: ignore[arg-type]
    )


# --- Pass / order ----------------------------------------------------------


def test_passing_grader_records_exit_zero_and_exited_termination() -> None:
    store = InMemoryStore()
    _attempt_run_id(store)
    task = _task(CommandGrader(run="true", name="ok"))

    results = _run(task, store)

    assert len(results) == 1
    row = results[0]
    assert row.passed is True
    assert row.grader_type == "command"
    assert row.grader_name == "ok"
    assert row.ordinal == 0
    assert row.payload["run"] == "true"
    assert row.payload["exit_code"] == 0
    assert row.payload["termination"] == "exited"
    assert "signal" not in row.payload
    assert "timeout_seconds" not in row.payload


def test_runs_command_graders_in_list_order() -> None:
    store = InMemoryStore()
    _attempt_run_id(store)
    task = _task(
        CommandGrader(run="echo first", name="a"),
        CommandGrader(run="echo second", name="b"),
        CommandGrader(run="echo third", name="c"),
    )

    results = _run(task, store)
    assert [r.grader_name for r in results] == ["a", "b", "c"]
    assert [r.ordinal for r in results] == [0, 1, 2]
    assert [r.payload["stdout_tail"].strip() for r in results] == [
        "first",
        "second",
        "third",
    ]


# --- Abort after first failure --------------------------------------------


def test_first_failure_aborts_later_command_graders() -> None:
    store = InMemoryStore()
    _attempt_run_id(store)
    task = _task(
        CommandGrader(run="true", name="ok"),
        CommandGrader(run="exit 7", name="boom"),
        CommandGrader(run="echo never", name="skipped"),
    )

    results = _run(task, store)

    assert [r.grader_name for r in results] == ["ok", "boom"]
    assert [r.passed for r in results] == [True, False]
    assert results[1].payload["exit_code"] == 7
    assert results[1].payload["termination"] == "exited"

    # The skipped grader was not persisted.
    rows = store.list_grader_results("r1", 1)
    assert [r.grader_name for r in rows] == ["ok", "boom"]


# --- Non-command graders are skipped, ordinals are preserved --------------


def test_skips_non_command_graders_and_preserves_ordinals() -> None:
    store = InMemoryStore()
    _attempt_run_id(store)
    task = _task(
        RubricGrader(assertions=["x"]),
        CommandGrader(run="true", name="cmd0"),
        TranscriptGrader(max_turns=5),
        CommandGrader(run="true", name="cmd1"),
        ManualGrader(instruction="approve"),
    )

    results = _run(task, store)

    # Only the two command graders ran.
    assert [r.grader_name for r in results] == ["cmd0", "cmd1"]
    # Ordinals reflect index in task.graders, not execution sequence.
    assert [r.ordinal for r in results] == [1, 3]
    # Every persisted row is a command type — non-command graders never run.
    assert all(r.grader_type == "command" for r in results)


def test_only_non_command_graders_yields_no_executions() -> None:
    store = InMemoryStore()
    _attempt_run_id(store)
    task = _task(
        RubricGrader(assertions=["x"]),
        ManualGrader(instruction="approve"),
    )

    results = _run(task, store)
    assert results == []
    assert store.list_grader_results("r1", 1) == []


# --- Failure modes are distinguishable ------------------------------------


def test_non_zero_exit_recorded_as_failure_with_exit_code() -> None:
    store = InMemoryStore()
    _attempt_run_id(store)
    task = _task(CommandGrader(run="exit 42"))

    [row] = _run(task, store)
    assert row.passed is False
    assert row.payload["exit_code"] == 42
    assert row.payload["termination"] == "exited"
    assert "signal" not in row.payload
    assert "timeout_seconds" not in row.payload


def test_signal_termination_recorded_distinguishably() -> None:
    store = InMemoryStore()
    _attempt_run_id(store)
    # Shell sends SIGKILL (9) to its own PID — the spawned subprocess
    # terminates via signal. Python sets returncode to -9 in this case.
    task = _task(CommandGrader(run="kill -KILL $$"))

    [row] = _run(task, store)
    assert row.passed is False
    assert row.payload["termination"] == "signal"
    assert row.payload["signal"] == 9
    assert row.payload["exit_code"] == -9
    assert "timeout_seconds" not in row.payload


def test_timeout_recorded_distinguishably() -> None:
    store = InMemoryStore()
    _attempt_run_id(store)
    task = _task(CommandGrader(run="sleep 30", name="slow"))

    [row] = _run(task, store, per_grader_timeout_seconds=0.2)
    assert row.passed is False
    assert row.payload["termination"] == "timeout"
    assert row.payload["timeout_seconds"] == 0.2
    # exit_code is whatever the kill produced — non-zero, but the
    # discriminator is `termination`, not exit_code.
    assert row.payload["exit_code"] != 0


def test_failure_modes_are_pairwise_distinguishable() -> None:
    """Audits must be able to tell exit / signal / timeout apart from
    payload alone — without consulting timing or external context."""

    store = InMemoryStore()
    _attempt_run_id(store)
    task = _task(
        CommandGrader(run="exit 1", name="nonzero"),
    )
    [nonzero] = _run(task, store)

    store2 = InMemoryStore()
    _attempt_run_id(store2)
    [signaled] = _run(
        _task(CommandGrader(run="kill -KILL $$", name="signaled")),
        store2,
    )

    store3 = InMemoryStore()
    _attempt_run_id(store3)
    [timed_out] = _run(
        _task(CommandGrader(run="sleep 30", name="timed_out")),
        store3,
        per_grader_timeout_seconds=0.2,
    )

    terminations = {
        nonzero.payload["termination"],
        signaled.payload["termination"],
        timed_out.payload["termination"],
    }
    assert terminations == {"exited", "signal", "timeout"}
    assert all(not r.passed for r in (nonzero, signaled, timed_out))


# --- Bounded stdout/stderr tail -------------------------------------------


def test_stdout_tail_is_bounded_by_configured_byte_cap() -> None:
    store = InMemoryStore()
    _attempt_run_id(store)
    # Generate ~4 KiB of stdout, then cap the tail at 256 bytes.
    task = _task(
        CommandGrader(
            run="python -c \"import sys; sys.stdout.write('x' * 4096)\"",
            name="loud",
        )
    )

    [row] = _run(task, store, stdout_tail_bytes=256)
    tail = row.payload["stdout_tail"]
    assert isinstance(tail, str)
    assert len(tail) == 256
    assert row.payload["stdout_truncated"] is True
    assert tail.endswith("x")


def test_stdout_tail_returns_full_when_under_cap() -> None:
    store = InMemoryStore()
    _attempt_run_id(store)
    task = _task(CommandGrader(run="echo small"))

    [row] = _run(task, store, stdout_tail_bytes=8192)
    assert row.payload["stdout_truncated"] is False
    assert row.payload["stdout_tail"].strip() == "small"


def test_stderr_is_captured_independently_from_stdout() -> None:
    store = InMemoryStore()
    _attempt_run_id(store)
    task = _task(
        CommandGrader(
            run="echo on-out; echo on-err 1>&2; exit 3",
            name="mixed",
        )
    )

    [row] = _run(task, store)
    assert row.payload["exit_code"] == 3
    assert "on-out" in row.payload["stdout_tail"]
    assert "on-err" in row.payload["stderr_tail"]
    assert "on-out" not in row.payload["stderr_tail"]
    assert "on-err" not in row.payload["stdout_tail"]


# --- grader_spec snapshotting ---------------------------------------------


def test_grader_spec_snapshots_input_verbatim() -> None:
    store = InMemoryStore()
    _attempt_run_id(store)
    grader = CommandGrader(run="uv run pytest tests/http", name="tests")
    task = _task(grader)

    [row] = _run(task, store)
    assert dict(row.grader_spec) == {
        "type": "command",
        "run": "uv run pytest tests/http",
        "name": "tests",
    }


def test_later_grader_edits_do_not_rewrite_history() -> None:
    """`grader_spec_json` is the historical truth — mutating the original
    grader after the run must not change what was persisted."""

    store = InMemoryStore()
    _attempt_run_id(store)
    grader = CommandGrader(run="true", name="snapshot")
    task = _task(grader)

    _run(task, store)

    grader.run = "MUTATED"
    grader.name = "RENAMED"
    rows = store.list_grader_results("r1", 1)
    assert dict(rows[0].grader_spec) == {
        "type": "command",
        "run": "true",
        "name": "snapshot",
    }


# --- Persistence shape -----------------------------------------------------


def test_payload_contains_documented_command_shape() -> None:
    store = InMemoryStore()
    _attempt_run_id(store)
    task = _task(CommandGrader(run="echo hello"))

    [row] = _run(task, store)
    for key in ("run", "exit_code", "stdout_tail", "stderr_tail"):
        assert key in row.payload, f"missing documented key: {key}"


def test_artifacts_dir_persists_full_streams_and_records_paths(
    tmp_path: Path,
) -> None:
    store = InMemoryStore()
    _attempt_run_id(store)
    task = _task(
        CommandGrader(
            run=(
                "python -c \"import sys; "
                "sys.stdout.write('x' * 4096); "
                "sys.stderr.write('y' * 4096)\""
            ),
            name="loud",
        )
    )

    [row] = _run(
        task,
        store,
        stdout_tail_bytes=128,
        stderr_tail_bytes=128,
        artifacts_dir=tmp_path,
    )
    stdout_path = Path(row.payload["stdout_path"])
    stderr_path = Path(row.payload["stderr_path"])
    assert stdout_path.parent == tmp_path
    assert stderr_path.parent == tmp_path
    assert stdout_path.read_bytes() == b"x" * 4096
    assert stderr_path.read_bytes() == b"y" * 4096
    # The bounded tail still bounds the payload row even though the full
    # stream is on disk.
    assert len(row.payload["stdout_tail"]) == 128


def test_each_row_keys_by_run_id_attempt_number_and_ordinal() -> None:
    store = InMemoryStore()
    _attempt_run_id(store)
    task = _task(
        CommandGrader(run="true", name="a"),
        RubricGrader(assertions=["x"]),
        CommandGrader(run="true", name="c"),
    )

    _run(task, store)
    rows = store.list_grader_results("r1", 1)
    assert [(r.run_id, r.attempt_number, r.ordinal) for r in rows] == [
        ("r1", 1, 0),
        ("r1", 1, 2),
    ]


def test_records_are_appended_not_updated_on_rerun() -> None:
    """Re-running the same task appends fresh rows; previous rows persist."""

    store = InMemoryStore()
    _attempt_run_id(store)
    task = _task(CommandGrader(run="true", name="x"))

    _run(task, store)
    # Second attempt — different attempt_number, fresh ordinal=0.
    from flywheel_core import Attempt

    store.save_attempt(
        "r1",
        Attempt(
            number=2,
            started_at=datetime(2024, 1, 2, tzinfo=timezone.utc),
            run_id="r1",
        ),
    )
    run_command_graders(
        task, store, run_id="r1", attempt_number=2
    )

    a1 = store.list_grader_results("r1", 1)
    a2 = store.list_grader_results("r1", 2)
    assert len(a1) == 1 and len(a2) == 1
    assert a1[0].id != a2[0].id


# --- Sanity: empty / minimal task graders ---------------------------------


def test_task_with_only_failing_first_command_skips_all_later_graders() -> None:
    store = InMemoryStore()
    _attempt_run_id(store)
    task = _task(
        CommandGrader(run="false", name="initial"),
        CommandGrader(run="true", name="second"),
        RubricGrader(assertions=["x"]),
    )

    results = _run(task, store)
    assert [r.grader_name for r in results] == ["initial"]
    assert results[0].passed is False


def test_unused_imports_smoke() -> None:
    """Sanity: catch accidental removal of the Context export (used elsewhere)."""

    assert Context  # noqa: B018 — keep the import live
    with pytest.raises(TypeError):
        CommandGrader()  # type: ignore[call-arg]

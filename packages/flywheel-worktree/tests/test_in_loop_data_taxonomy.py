"""In-loop verification fixture for spec 00025 (data taxonomy alignment).

Drives a real ``worker.run_once`` cycle — real orchestrate scheduling, real
``SqliteStore``, real harness, real git worktree submit — with only the
agent's text scripted through the injectable invoker seam, and asserts the
taxonomy realignment end to end (spec 00017 FR-3, both loop-produced ends):

* telemetry (SDK messages, harness.* events, domain mirrors) lands in the
  per-run JSONL the harness's sink writes — produced by the harness,
  consumed here by parsing the file the way the audit readers do;
* token/iteration/activity aggregates land on the relational attempt row
  through the versioned rollup path;
* a steering command round-trips end to end: enqueued through the real
  store producer path (the CLI's shape, on its own connection), claimed
  via the real claim-once primitive, fed through the harness's
  ``on_command_applied`` seam, ledgered as a ``CommandApplied`` domain
  event, mirrored into the run JSONL, and its queue row deleted;
* the attempt carries the world-state pin (base commit SHA + effective
  model id) and the pin survives domain-event replay.

The schema-version half (spec 00017 FR-4, adapted to 00025's
refuse-not-migrate decision) seeds a store stamped at the previous schema
version and asserts the real ``SqliteStore`` refuses it with
``StoreSchemaError`` — a fresh current-schema store does not satisfy it.

Fixture/temp stores only; the project's own ``.flywheel/flywheel.sqlite``
is never touched.
"""

from __future__ import annotations

import json
import sqlite3
import subprocess
from datetime import datetime, timezone
from pathlib import Path

import pytest
from claude_agent_sdk import AssistantMessage, ResultMessage, TextBlock

from flywheel_core import (
    CommandApplied,
    Intent,
    InvocationRequest,
    InvocationSignals,
    IterationResult,
    Status,
    ValidEnvelope,
)
from flywheel_core.store_protocols import (
    CURRENT_SCHEMA_VERSION,
    StoreSchemaError,
)
from flywheel_core.store_sqlite import SqliteStore
from flywheel_worktree import worker


def _git(cwd: Path, *args: str) -> str:
    proc = subprocess.run(
        ["git", "-C", str(cwd), *args],
        text=True,
        capture_output=True,
        check=True,
    )
    return proc.stdout.strip()


def _init_repo(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True)
    _git(path, "init", "-b", "main")
    _git(path, "config", "user.email", "worker-test@example.com")
    _git(path, "config", "user.name", "worker test")
    _git(path, "config", "commit.gpgsign", "false")
    (path / "README.md").write_text("base\n")
    _git(path, "add", "-A")
    _git(path, "commit", "-m", "init")


def _submitter(repo: Path) -> "worker.GitWorktreeSubmitter":
    worktrees = repo / ".flywheel" / "worktrees"
    worktrees.mkdir(parents=True, exist_ok=True)
    return worker.GitWorktreeSubmitter(
        repo_root=repo,
        tasks_dir=repo / ".flywheel" / "tasks",
        worktrees_dir=worktrees,
        phase_base="main",
        lock_path=repo / ".flywheel" / ".merge.lock",
        log=lambda _m: None,
    )


def _task_file(repo: Path, phase: str, task_id: str) -> Path:
    tf = repo / ".flywheel" / "tasks" / "active" / phase / f"{task_id}.json"
    tf.parent.mkdir(parents=True, exist_ok=True)
    tf.write_text(
        json.dumps(
            {
                "id": task_id,
                "goal": f"Goal for {task_id}.",
                "graders": [{"type": "command", "run": "true"}],
            }
        )
    )
    return tf


_USAGE = {
    "input_tokens": 120,
    "output_tokens": 45,
    "cache_creation_input_tokens": 10,
    "cache_read_input_tokens": 30,
}


def _messages() -> tuple[object, ...]:
    return (
        AssistantMessage(
            content=[TextBlock(text="done")],
            model="claude-test",
            stop_reason="end_turn",
            session_id="sess",
            usage=dict(_USAGE),
        ),
        ResultMessage(
            subtype="success",
            duration_ms=1,
            duration_api_ms=1,
            is_error=False,
            num_turns=2,
            session_id="sess",
            stop_reason="end_turn",
            total_cost_usd=0.05,
            usage=dict(_USAGE),
        ),
    )


def _signals() -> InvocationSignals:
    return InvocationSignals(
        stop_reason="end_turn",
        num_turns=2,
        total_cost_usd=0.05,
        result_is_error=False,
        result_subtype="success",
        api_error_status=None,
        session_id="sess",
    )


def test_real_loop_routes_telemetry_aggregates_and_steering(
    tmp_path: Path,
) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _submitter(repo)
    _task_file(repo, "01-phase", "t1")
    worktree = repo / ".flywheel" / "worktrees" / "t1"
    db_path = repo / ".flywheel" / "flywheel.sqlite"

    async def _invoke(request: InvocationRequest) -> IterationResult:
        # Play the agent: commit work in the prepared worktree.
        (worktree / "work.txt").write_text("agent output")
        _git(worktree, "add", "-A")
        _git(worktree, "commit", "-m", "agent work")

        # Play the operator mid-iteration: enqueue a steering command
        # through the real store producer path (a separate connection,
        # exactly the CLI's shape), then play the live watcher: claim it
        # via the real claim-once primitive and feed it through the
        # harness's on_command_applied seam. Only the SDK dispatch is
        # scripted; the ledger append, queue-row deletion, and JSONL
        # mirror below are the real harness paths.
        producer = SqliteStore(db_path)
        try:
            row = producer._connection.execute(  # noqa: SLF001
                "SELECT run_id FROM lifecycles WHERE task_id = 't1'"
            ).fetchone()
            run_id = row["run_id"]
            producer.enqueue_command(
                run_id,
                "say",
                {"text": "focus on graders"},
                now=datetime.now(timezone.utc),
            )
            claimed = producer.claim_commands(
                run_id, now=datetime.now(timezone.utc)
            )
        finally:
            producer.close()
        assert len(claimed) == 1
        assert request.on_command_applied is not None
        request.on_command_applied(claimed[0])

        result = IterationResult(
            transcript="ok",
            messages=_messages(),  # type: ignore[arg-type]
            envelope=ValidEnvelope(intent=Intent.VERIFY),
            signals=_signals(),
            failure=None,
        )
        if request.on_message is not None:
            for msg in result.messages:
                request.on_message(msg)  # type: ignore[arg-type]
        return result

    report = worker.run_once(
        s,
        tasks_dir=repo / ".flywheel" / "tasks",
        db_path=db_path,
        worktrees_dir=repo / ".flywheel" / "worktrees",
        model="claude-opus-4-8",
        max_turns=4,
        max_retries=0,
        invoke=_invoke,
    )

    assert [r.status for r in report.runs] == [Status.DONE]
    run_id = report.runs[0].run_id

    # --- Telemetry lands in the per-run JSONL, not the store ------------
    run_file = db_path.parent / "logs" / "runs" / f"{run_id}.jsonl"
    assert run_file.is_file()
    lines = [
        json.loads(line)
        for line in run_file.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    kinds = [line["kind"] for line in lines]
    assert "AssistantMessage" in kinds  # SDK message stream
    assert "harness.iteration_completed" in kinds  # harness telemetry
    assert "domain.attempt_finalized" in kinds  # ledger mirror
    assert "domain.command_applied" in kinds  # steering mirror

    store = SqliteStore(db_path)
    try:
        # --- Aggregates land on the relational attempt row --------------
        attempts = store.list_attempts(run_id)
        assert len(attempts) == 1
        attempt = attempts[0]
        assert attempt.input_tokens == _USAGE["input_tokens"]
        assert attempt.output_tokens == _USAGE["output_tokens"]
        assert (
            attempt.cache_read_input_tokens
            == _USAGE["cache_read_input_tokens"]
        )
        assert attempt.iterations_completed == 1
        assert attempt.turns == 2
        assert attempt.total_cost_usd == pytest.approx(0.05)
        assert attempt.last_activity_at is not None

        # --- Steering round-tripped into the ledger ----------------------
        steering = [
            e
            for e in store.list_domain_events(run_id)
            if isinstance(e, CommandApplied)
        ]
        assert len(steering) == 1
        assert steering[0].command_kind == "say"
        assert dict(steering[0].command_payload) == {
            "text": "focus on graders"
        }
        # The applied queue row was deleted after the event committed.
        pending = store._connection.execute(  # noqa: SLF001
            "SELECT COUNT(*) AS n FROM control_commands"
        ).fetchone()
        assert pending["n"] == 0

        # --- World-state pin recorded and replay-stable ------------------
        ctx = attempt.agent_context
        assert ctx["model_id"] == "claude-opus-4-8"
        assert len(ctx["base_commit_sha"]) == 40
    finally:
        store.close()


def test_unmigratable_schema_version_store_is_refused(
    tmp_path: Path,
) -> None:
    """Spec 00017 FR-4 under 00025's refuse-not-migrate decision: a store
    whose schema_version has no supported forward migration is refused by
    the real SqliteStore with StoreSchemaError, not silently accepted.
    (v11 is the one exception — it forward-migrates additively to v12;
    that path is pinned in flywheel-core's test_store_sqlite.)"""
    db_path = tmp_path / "old.sqlite"
    SqliteStore(db_path).close()

    # Stamp the on-disk store two versions back — older than the one
    # supported forward migration, the shape a long-stale deployment
    # would present on upgrade.
    conn = sqlite3.connect(db_path)
    try:
        conn.execute(
            "UPDATE schema_version SET version = ? WHERE id = 1",
            (CURRENT_SCHEMA_VERSION - 2,),
        )
        conn.commit()
    finally:
        conn.close()

    with pytest.raises(StoreSchemaError) as exc_info:
        SqliteStore(db_path)
    assert exc_info.value.observed_version == CURRENT_SCHEMA_VERSION - 2
    assert exc_info.value.expected_version == CURRENT_SCHEMA_VERSION
    assert "store must be re-created" in str(exc_info.value)

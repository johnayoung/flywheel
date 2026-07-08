"""Under ``[store] backend = postgres`` a worker-driven run lands its whole
record in the configured Postgres database and writes nothing to sqlite (spec
00075).

Drives a single fresh task to ``DONE`` through :func:`orchestrate` with a
postgres policy, then proves:

* the run's whole record landed in postgres -- ``task_versions``,
  ``lifecycles``, ``attempts``, ``grader_results`` and ``events`` each carry
  rows, and the claim was written to ``task_claims`` while the lease was held
  (captured mid-run, since a released claim is a deleted row);
* nothing leaked to sqlite -- the policy's ``db_path`` file is absent both
  while the lease is held and after the run, so the dual-write this task closes
  (which would have created it) cannot have happened.

Requires a reachable Postgres (Docker / testcontainers / the ``postgres``
extra); the root conftest's session-scoped container provides it, and the run
is skipped only when no database is reachable.
"""

from __future__ import annotations

import asyncio
import io
import json
from pathlib import Path
from uuid import uuid4

from claude_agent_sdk import AssistantMessage, ResultMessage, TextBlock

from flywheel_core import (
    Intent,
    InvocationRequest,
    InvocationSignals,
    IterationResult,
    Status,
    ValidEnvelope,
)
from flywheel_orchestrator import orchestrate
from flywheel_orchestrator._policy import WorkPolicy
from flywheel_orchestrator._store_factory import PG_DSN_ENV

# The core store's run-record tables. The claim table (``task_claims``) is
# checked mid-run instead, since its row is deleted on lease release.
_CORE_TABLES = (
    "task_versions",
    "lifecycles",
    "attempts",
    "grader_results",
    "events",
)


def _write_task(phase: Path, task_id: str) -> None:
    phase.mkdir(parents=True, exist_ok=True)
    payload: dict[str, object] = {
        "id": task_id,
        "goal": f"Goal for {task_id}.",
        "graders": [{"type": "command", "run": "true"}],
    }
    (phase / f"{task_id}.json").write_text(json.dumps(payload))


def _signals() -> InvocationSignals:
    return InvocationSignals(
        stop_reason="end_turn",
        num_turns=1,
        total_cost_usd=0.0,
        result_is_error=False,
        result_subtype="success",
        api_error_status=None,
        session_id="sess",
    )


def _messages() -> tuple[object, ...]:
    return (
        AssistantMessage(
            content=[TextBlock(text="done")],
            model="claude-test",
            stop_reason="end_turn",
            session_id="sess",
            usage=None,
        ),
        ResultMessage(
            subtype="success",
            duration_ms=1,
            duration_api_ms=1,
            is_error=False,
            num_turns=1,
            session_id="sess",
            stop_reason="end_turn",
            total_cost_usd=0.0,
            usage=None,
        ),
    )


def _verify_result() -> IterationResult:
    return IterationResult(
        transcript="ok",
        messages=_messages(),  # type: ignore[arg-type]
        envelope=ValidEnvelope(intent=Intent.VERIFY),
        signals=_signals(),
        failure=None,
    )


def _table_count(dsn: str, schema: str, table: str) -> int:
    """Row count of ``schema.table`` over a fresh, independent connection."""
    import psycopg
    from psycopg import sql

    with psycopg.connect(dsn) as conn:
        with conn.cursor() as cur:
            cur.execute(
                sql.SQL("SELECT count(*) FROM {}.{}").format(
                    sql.Identifier(schema), sql.Identifier(table)
                )
            )
            row = cur.fetchone()
    assert row is not None
    return int(row[0])


def test_postgres_backed_run_lands_full_record_and_touches_no_sqlite(
    tmp_path: Path,
    monkeypatch,
    require_postgres: str,
) -> None:
    dsn = require_postgres
    monkeypatch.setenv(PG_DSN_ENV, dsn)
    schema = f"flywheel_dataplane_{uuid4().hex[:12]}"
    policy = WorkPolicy(
        source_kind="directory",
        tasks_dir=tmp_path / "tasks",
        store_backend="postgres",
        store_schema=schema,
    )

    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "solo")
    db_path = tmp_path / "flywheel.sqlite"

    mid: dict[str, object] = {}

    async def _invoke(request: InvocationRequest) -> IterationResult:
        # The lease is held here: the claim must already be committed to
        # postgres (a separate connection sees it) and no sqlite file exists.
        mid["claims"] = _table_count(dsn, schema, "task_claims")
        mid["sqlite_exists"] = db_path.exists()
        return _verify_result()

    report = asyncio.run(
        orchestrate(
            tasks_dir=tmp_path / "tasks",
            db_path=db_path,
            sandbox_root=tmp_path / "sandboxes",
            invoke=_invoke,
            policy=policy,
            max_retries=0,
            max_turns=4,
            stream=io.StringIO(),
        )
    )

    # The task was driven to DONE on the postgres backend.
    assert [r.task_id for r in report.runs] == ["solo"]
    assert report.runs[0].status is Status.DONE

    # The claim landed in postgres -- and NOT in sqlite -- while held.
    assert mid["claims"] == 1, "the held claim was not written to postgres"
    assert mid["sqlite_exists"] is False, (
        "a sqlite file existed while the postgres-backed run held its lease"
    )

    # The whole run record landed in postgres.
    for table in _CORE_TABLES:
        count = _table_count(dsn, schema, table)
        assert count >= 1, f"postgres table {table!r} carries no row for the run"

    # Nothing leaked to sqlite: the dual-write this task closes would have
    # created the db_path file (absent == byte-identical to its pre-run state).
    assert not db_path.exists(), (
        "a postgres-backed run wrote a sqlite database at db_path"
    )

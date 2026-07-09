"""End-to-end control-plane composition under ``[store] backend = postgres``.

An ``fw approve`` issued through the product CLI against a parked
``AWAITING_APPROVAL`` run is consumed by the SAME still-running
``orchestrate`` session -- driving the lifecycle to ``DONE`` with no session
recycling and no sqlite artifact.

This is the phase's composition holdout. It threads the two prerequisites --
the postgres producer (``fw approve`` enqueues the control command into
postgres, ``test_fw_producer_postgres``) and the in-session approval re-sweep
(an approve enqueued *after* the empty sweep is consumed by the same session
once the sweep mark expires, ``test_orchestrator_approval_expiry``) -- through
ONE policy surface: a ``flywheel.toml`` in the working directory selects
``backend = postgres`` with a unique schema, and BOTH the orchestrate call
(via :func:`load_effective_policy`) and the CLI invocation
(:func:`flywheel._cli.main`, which loads the same file) resolve it.

The scenario is exercised inside a *single* ``orchestrate`` call:

1. A gated task (higher priority) is driven to ``AWAITING_APPROVAL`` and parks.
2. The reactive sweep (section 1b) passes it empty and marks it with a TTL.
3. A second task keeps the session alive for another pass; its stubbed agent
   issues ``fw approve RUN_ID`` through the product CLI entrypoint (only now,
   strictly after the empty sweep) and advances the injected clock past the
   mark's TTL.
4. A later pass of the *same* call re-sweeps the now-expired mark, claims the
   pending approve out of postgres, resolves it in place, and the lifecycle
   reaches ``DONE`` before ``orchestrate`` returns.

Time is controlled exclusively through ``orchestrate``'s injected ``now``
clock -- no real sleeps, no polling threads. The store is postgres throughout:
the enqueue lands in postgres (the CLI enqueues, orchestrate claims from the
same schema) and the resolved sqlite db path never materializes, asserted both
mid-session and after the session returns. Against the pre-expiry
session-permanent sweep mark the mark never expires, step 4 skips the parked
run, and the final ``DONE`` assertion fails -- so the enqueue-after-empty-sweep
timing is a genuine discriminator, not decoration.

Requires a reachable Postgres (Docker / testcontainers / the ``postgres``
extra); the root conftest's session-scoped container provides it, and the case
is skipped only when no database is reachable.
"""

from __future__ import annotations

import asyncio
import io
import json
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any
from uuid import uuid4

import pytest
from claude_agent_sdk import AssistantMessage, ResultMessage, TextBlock

from flywheel_core import (
    Intent,
    InvocationRequest,
    InvocationSignals,
    IterationResult,
    Status,
    ValidEnvelope,
)
from flywheel_orchestrator import (
    load_effective_policy,
    orchestrate,
    resolve_db_path,
)
from flywheel_orchestrator._orchestrate import APPROVAL_SWEEP_MARK_TTL_SECONDS

from flywheel._cli import main


# --- fake agent (mirrors the sqlite twin's always-verify stub) -------------


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


# --- task / policy writers -------------------------------------------------


def _write_gated_task(phase: Path, task_id: str, *, priority: int) -> None:
    """A task whose command grader passes then parks on a single manual gate,
    so a verifying agent drives the lifecycle straight to
    ``AWAITING_APPROVAL``."""
    phase.mkdir(parents=True, exist_ok=True)
    payload: dict[str, object] = {
        "id": task_id,
        "goal": f"Goal for {task_id}.",
        "priority": priority,
        "graders": [
            {"type": "command", "run": "true"},
            {
                "type": "manual",
                "instruction": "Confirm the rollout.",
                "name": "operator-confirm",
            },
        ],
    }
    (phase / f"{task_id}.json").write_text(json.dumps(payload))


def _write_plain_task(phase: Path, task_id: str, *, priority: int) -> None:
    """A task with a single always-passing command grader (drives to DONE)."""
    phase.mkdir(parents=True, exist_ok=True)
    payload: dict[str, object] = {
        "id": task_id,
        "goal": f"Goal for {task_id}.",
        "priority": priority,
        "graders": [{"type": "command", "run": "true"}],
    }
    (phase / f"{task_id}.json").write_text(json.dumps(payload))


def _write_policy(
    tmp_path: Path, schema: str, *, db_rel: str = ".flywheel/flywheel.sqlite"
) -> None:
    """Write a postgres ``[store]`` policy into ``tmp_path/flywheel.toml``.

    The one policy surface both control-plane sides resolve: the CLI loads it
    internally (``flywheel._cli.main`` -> ``load_effective_policy``) and the
    orchestrate call is handed the same loaded policy below.
    """
    (tmp_path / "flywheel.toml").write_text(
        '[source]\n'
        'kind = "directory"\n'
        '[store]\n'
        'backend = "postgres"\n'
        f'schema = "{schema}"\n'
        '[paths]\n'
        f'db = "{db_rel}"\n',
        encoding="utf-8",
    )


def _open_store(dsn: str, schema: str) -> Any:
    """Open a PostgresStore bound to ``schema`` (bootstraps it if new).

    Imported lazily so a suite run without the ``postgres`` extra fails as a
    skip (via ``require_postgres``) rather than a collection error.
    """
    from flywheel_core import PostgresStore

    return PostgresStore(dsn, schema=schema, pool_min=1, pool_max=2)


# --- test ------------------------------------------------------------------


def test_fw_approve_consumed_in_session_reaches_done_under_postgres(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    require_postgres: str,
) -> None:
    dsn = require_postgres
    schema = f"flywheel_ctrlplane_{uuid4().hex[:12]}"
    monkeypatch.setenv("FLYWHEEL_PG_DSN", dsn)
    monkeypatch.delenv("DATABASE_URL", raising=False)
    monkeypatch.chdir(tmp_path)
    _write_policy(tmp_path, schema)

    # BOTH sides resolve the postgres backend + schema through this one on-disk
    # policy: the orchestrate call is handed the loaded policy, and the CLI
    # loads the identical file from the working directory.
    policy = load_effective_policy(None)
    assert policy is not None
    assert policy.store_backend == "postgres"
    assert policy.store_schema == schema
    db_path = resolve_db_path(None, policy=policy)

    phase = tmp_path / "tasks" / "active" / "01-phase"
    # ``gated`` (priority 1) is selected before ``keepalive`` (priority 0), so
    # it parks AWAITING_APPROVAL and is swept-empty on an EARLIER pass than the
    # one that drives the session-keepalive task.
    _write_gated_task(phase, "gated", priority=1)
    _write_plain_task(phase, "keepalive", priority=0)

    # A fake clock the test advances by hand -- the only time source in play.
    now_state = {"t": datetime(2026, 1, 1, tzinfo=timezone.utc)}

    def _clock() -> datetime:
        return now_state["t"]

    # Issue the approve through the product CLI exactly once, and only after
    # ``gated`` is observed parked in AWAITING_APPROVAL. The gated task's OWN
    # drive sees its lifecycle still RUNNING (not parked), so the enqueue fires
    # on a later invoke -- the keepalive task's -- which the loop reaches only
    # AFTER section 1b has swept the parked run empty and marked it. Advancing
    # the clock past the mark's TTL in the same step lets the next pass's sweep
    # treat the mark as expired and re-resolve.
    state: dict[str, Any] = {
        "approve_enqueued": False,
        "approve_rc": None,
        "gated_run_id": None,
        "sqlite_mid_session": None,
    }

    async def _invoke(request: InvocationRequest) -> IterationResult:
        if not state["approve_enqueued"]:
            store = _open_store(dsn, schema)
            try:
                parked = store.list_lifecycles(
                    statuses=[Status.AWAITING_APPROVAL], task_id="gated"
                )
            finally:
                store.close()
            if parked:
                run_id = parked[0].run_id
                state["gated_run_id"] = run_id
                # The control-plane producer: the operator's ``fw approve``,
                # driven through the exact product entrypoint. It loads the same
                # flywheel.toml, builds the postgres store from the DSN env, and
                # enqueues the approve into ``schema`` -- the same schema this
                # orchestrate session claims from.
                state["approve_rc"] = main(["approve", run_id])
                # The postgres-backed enqueue writes no sqlite file mid-session
                # (captured while the session is still running its keepalive).
                state["sqlite_mid_session"] = db_path.exists()
                now_state["t"] += timedelta(
                    seconds=APPROVAL_SWEEP_MARK_TTL_SECONDS + 5
                )
                state["approve_enqueued"] = True
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
            now=_clock,
        )
    )

    # Guard against a vacuous pass: the approve really was issued mid-session,
    # through the CLI, and it succeeded (exit 0).
    assert state["approve_enqueued"] is True
    assert state["approve_rc"] == 0
    assert state["gated_run_id"] is not None

    # ``gated`` was driven once (to AWAITING_APPROVAL); the in-place resolve
    # advances the lifecycle without minting a second RunRecord.
    gated_runs = [r for r in report.runs if r.task_id == "gated"]
    assert len(gated_runs) == 1
    assert gated_runs[0].status is Status.AWAITING_APPROVAL
    run_id = gated_runs[0].run_id
    assert run_id == state["gated_run_id"]

    # No sqlite leaked while the postgres-backed session held its keepalive.
    assert state["sqlite_mid_session"] is False, (
        "a sqlite file existed mid-session under a postgres [store] policy"
    )

    # The single orchestrate call consumed the CLI-issued approve out of
    # postgres: the mark expired, the run was re-swept, and the lifecycle
    # reached DONE -- proven by reading it back over an independent connection.
    store = _open_store(dsn, schema)
    try:
        final = store.load_lifecycle(run_id)
        assert final is not None
        assert final.status is Status.DONE
        # The -> DONE edge clears the awaiting ordinal.
        assert final.awaiting_manual_ordinal is None
    finally:
        store.close()

    # Nothing leaked to sqlite: the resolved db path is absent after the run,
    # so the postgres-backed control plane never dual-wrote a sqlite database.
    assert not db_path.exists(), (
        "a postgres-backed control-plane run wrote a sqlite database at db_path"
    )

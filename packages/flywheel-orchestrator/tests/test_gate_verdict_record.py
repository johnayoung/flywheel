"""Held-out gate verdict record on the ledger (spec 00073, #1/#2/#11, D-1/D-2).

The landing tests (``test_held_out_gate_landing.py``) pin the gate *decision*:
what lands, what blocks, what fails closed. These pin the durable *record* of
that decision -- every held-out gate evaluation (pass, fail, or no-gate) appends
one :class:`HeldOutGateEvaluated` to the run's domain-event ledger, carrying each
executed grader's name, outcome, and a bounded raw output tail, so a gate-decided
park is diagnosable from the store alone rather than only from the in-process
``RunRecord``.

Each test drives a real lifecycle through ``orchestrate`` with the agent stubbed,
then reads the persisted domain events back out of the file-backed store and
asserts on the verdict record and its receipts -- including that a receipt for a
grader that produced output carries that real output (never an empty excerpt).
"""

from __future__ import annotations

import asyncio
import io
import json
from pathlib import Path

from claude_agent_sdk import AssistantMessage, ResultMessage, TextBlock

from flywheel_core import (
    GATE_EXCERPT_MAX_BYTES,
    HeldOutGateEvaluated,
    Intent,
    InvocationRequest,
    InvocationSignals,
    IterationResult,
    LandingParked,
    ValidEnvelope,
)
from flywheel_core.store_sqlite import SqliteStore
from flywheel_orchestrator import (
    FilesystemHeldOutGraderSource,
    SandboxRequest,
    SubmitRequest,
    orchestrate,
)


# --- helpers ----------------------------------------------------------------


def _write_task(phase: Path, task_id: str, *, grader_run: str = "true") -> None:
    phase.mkdir(parents=True, exist_ok=True)
    payload: dict[str, object] = {
        "id": task_id,
        "goal": f"Goal for {task_id}.",
        "graders": [{"type": "command", "run": grader_run}],
    }
    (phase / f"{task_id}.json").write_text(json.dumps(payload))


def _register_held_out(root: Path, task_id: str, entries: object) -> None:
    root.mkdir(parents=True, exist_ok=True)
    (root / f"{task_id}.json").write_text(json.dumps(entries))


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


def _always_verify():
    async def _invoke(request: InvocationRequest) -> IterationResult:
        return IterationResult(
            transcript="ok",
            messages=_messages(),  # type: ignore[arg-type]
            envelope=ValidEnvelope(intent=Intent.VERIFY),
            signals=_signals(),
            failure=None,
        )

    return _invoke


def _drive(
    tmp_path: Path,
    *,
    held_out_root: Path | None,
    submit_calls: list[SubmitRequest],
):
    def prepare(req: SandboxRequest) -> Path:
        sandbox = tmp_path / "prepared" / req.task_id
        sandbox.mkdir(parents=True, exist_ok=True)
        return sandbox

    source = (
        FilesystemHeldOutGraderSource(root=held_out_root)
        if held_out_root is not None
        else None
    )
    return asyncio.run(
        orchestrate(
            tasks_dir=tmp_path / "tasks",
            db_path=tmp_path / "flywheel.sqlite",
            sandbox_root=tmp_path / "sandboxes",
            invoke=_always_verify(),
            max_retries=0,
            max_turns=4,
            stream=io.StringIO(),
            prepare_sandbox=prepare,
            submit=submit_calls.append,
            held_out_source=source,
        )
    )


def _verdict_records(tmp_path: Path, run_id: str) -> list[HeldOutGateEvaluated]:
    """Read the run's persisted held-out gate verdict records from the store."""
    store = SqliteStore(str(tmp_path / "flywheel.sqlite"))
    try:
        events = store.list_domain_events(run_id)
    finally:
        store.close()
    return [e for e in events if isinstance(e, HeldOutGateEvaluated)]


def _park_records(tmp_path: Path, run_id: str) -> list[LandingParked]:
    store = SqliteStore(str(tmp_path / "flywheel.sqlite"))
    try:
        events = store.list_domain_events(run_id)
    finally:
        store.close()
    return [e for e in events if isinstance(e, LandingParked)]


# --- #1: a passing gate persists a verdict record with real oracle output ----


def test_passing_gate_appends_verdict_record_with_grader_output(
    tmp_path: Path,
) -> None:
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "alpha", grader_run="true")
    held_out = tmp_path / "held_out"
    # The held-out oracle prints a recognizable marker to stdout and exits 0.
    _register_held_out(
        held_out,
        "alpha",
        [{"type": "command", "run": "echo GATEPASS_MARKER", "name": "oracle-a"}],
    )

    submit_calls: list[SubmitRequest] = []
    report = _drive(tmp_path, held_out_root=held_out, submit_calls=submit_calls)
    run_id = report.runs[0].run_id

    records = _verdict_records(tmp_path, run_id)
    assert len(records) == 1
    record = records[0]
    assert record.outcome == "pass"
    assert len(record.receipts) == 1
    receipt = record.receipts[0]
    assert receipt.grader_name == "oracle-a"
    assert receipt.passed is True
    # The receipt carries the grader's REAL captured output, not an empty
    # excerpt: assert on the oracle's actual content (spec 00073 edge case).
    assert "GATEPASS_MARKER" in receipt.output_excerpt


# --- #2: a failing gate persists the verdict record AND the park witness -----


def test_failing_gate_appends_verdict_record_and_park(tmp_path: Path) -> None:
    phase = tmp_path / "tasks" / "active" / "01-phase"
    # The visible grader passes (the run reports DONE), but the held-out oracle
    # writes to stderr and exits non-zero, so the gate blocks the land.
    _write_task(phase, "alpha", grader_run="true")
    held_out = tmp_path / "held_out"
    _register_held_out(
        held_out,
        "alpha",
        [
            {
                "type": "command",
                "run": "echo GATEFAIL_DIAG 1>&2; exit 1",
                "name": "oracle-b",
            }
        ],
    )

    submit_calls: list[SubmitRequest] = []
    report = _drive(tmp_path, held_out_root=held_out, submit_calls=submit_calls)
    run_id = report.runs[0].run_id

    # Nothing landed (decision unchanged), but the verdict is now on the ledger.
    assert submit_calls == []

    records = _verdict_records(tmp_path, run_id)
    assert len(records) == 1
    record = records[0]
    assert record.outcome == "fail"
    assert len(record.receipts) == 1
    receipt = record.receipts[0]
    assert receipt.grader_name == "oracle-b"
    assert receipt.passed is False
    # The failing oracle's stderr diagnostic is retained in the excerpt so the
    # block is diagnosable from the store alone.
    assert "GATEFAIL_DIAG" in receipt.output_excerpt

    # The verdict record is additive to (not a replacement for) the existing
    # held-out-gate landing-parked witness.
    parked = _park_records(tmp_path, run_id)
    assert [p.park_kind for p in parked] == ["held-out-gate"]


# --- D-1: a no-gate evaluation leaves a record distinct from no record -------


def test_no_gate_evaluation_appends_distinguishable_record(
    tmp_path: Path,
) -> None:
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "alpha")
    # Source wired, but the task has no held-out registration -> NO_GATE.
    held_out = tmp_path / "held_out"
    held_out.mkdir()

    submit_calls: list[SubmitRequest] = []
    report = _drive(tmp_path, held_out_root=held_out, submit_calls=submit_calls)
    run_id = report.runs[0].run_id

    records = _verdict_records(tmp_path, run_id)
    # The gate ran and found nothing to gate: exactly one no-gate record with no
    # per-grader receipts -- present, and thus distinguishable from no record.
    assert len(records) == 1
    assert records[0].outcome == "no_gate"
    assert records[0].receipts == ()


def test_absent_source_leaves_no_verdict_record(tmp_path: Path) -> None:
    """The distinguishing counterpart: when no held-out source is wired the gate
    never runs, so there is NO verdict record -- the no-gate record above is a
    positive fact, not the absence of one."""
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "alpha")

    submit_calls: list[SubmitRequest] = []
    report = _drive(tmp_path, held_out_root=None, submit_calls=submit_calls)
    run_id = report.runs[0].run_id

    assert _verdict_records(tmp_path, run_id) == []


# --- #11: an oracle emitting past the bound persists a truncated final tail ---


def test_oracle_output_exceeding_bound_is_tail_truncated(
    tmp_path: Path,
) -> None:
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "alpha", grader_run="true")
    held_out = tmp_path / "held_out"
    # Emit a START marker, ~9KB of filler, then a FINAL marker (well over the
    # 8192-byte bound). The persisted excerpt must be a tail: it keeps the FINAL
    # content and drops the START.
    big = (
        "printf 'STARTEDGE'; "
        "head -c 9000 /dev/zero | tr '\\0' 'A'; "
        "printf 'FINALEDGE'"
    )
    _register_held_out(
        held_out,
        "alpha",
        [{"type": "command", "run": big, "name": "oracle-big"}],
    )

    submit_calls: list[SubmitRequest] = []
    report = _drive(tmp_path, held_out_root=held_out, submit_calls=submit_calls)
    run_id = report.runs[0].run_id

    records = _verdict_records(tmp_path, run_id)
    assert len(records) == 1
    excerpt = records[0].receipts[0].output_excerpt
    # Bounded (criterion 1) and a tail retaining the final content (criterion
    # 11): the ending survives, the beginning is dropped.
    assert len(excerpt.encode("utf-8")) <= GATE_EXCERPT_MAX_BYTES
    assert "FINALEDGE" in excerpt
    assert "STARTEDGE" not in excerpt

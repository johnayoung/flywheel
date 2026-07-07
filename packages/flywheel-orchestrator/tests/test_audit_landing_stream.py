"""flywheel audit surfaces landing-stage ledger records (spec 00073, #9, D-5).

A run whose landing was decided by the held-out gate appends its verdict -- and,
when the gate blocks, a landing-parked witness -- to the domain-event ledger
*after* the attempt finalized. Those decisions live only in the store, so the
harness's in-run mirror never wrote them to the run file (a ``domain.*`` line is
skipped by the file reader anyway). These tests pin that the audit stream
nonetheless projects them at the tail, sequenced after every in-run record, so
``flywheel audit`` shows the gate decision in one view -- while a run with no
landing decision streams exactly as before.

Each test drives a real lifecycle through ``orchestrate`` with the agent stubbed,
then reads the run's audit stream back and asserts on the projected records.
"""

from __future__ import annotations

import asyncio
import io
import json
from pathlib import Path

from claude_agent_sdk import AssistantMessage, ResultMessage, TextBlock

from flywheel_core import (
    EventRecord,
    Intent,
    InvocationRequest,
    InvocationSignals,
    IterationResult,
    ValidEnvelope,
    stream,
)
from flywheel_core.store_protocols import AuditRecord
from flywheel_core.store_sqlite import SqliteStore
from flywheel_orchestrator import (
    FilesystemHeldOutGraderSource,
    SandboxRequest,
    SubmitRequest,
    orchestrate,
)


_GATE_VERDICT_KIND = "domain.held_out_gate_evaluated"
_LANDING_PARKED_KIND = "domain.landing_parked"
_LANDING_KINDS = frozenset({_GATE_VERDICT_KIND, _LANDING_PARKED_KIND})


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


def _audit_records(tmp_path: Path, run_id: str) -> list[AuditRecord]:
    """Read the run's full audit stream (replay), the way ``flywheel audit`` does."""
    store = SqliteStore(str(tmp_path / "flywheel.sqlite"))
    try:
        return list(stream(run_id, store=store, logs_root=tmp_path / "logs"))
    finally:
        store.close()


def _event_kind(record: AuditRecord) -> str | None:
    return record.kind if isinstance(record, EventRecord) else None


# --- #9: a gate-blocked run projects verdict + park after finalization -------


def test_audit_stream_appends_gate_verdict_and_park_after_finalization(
    tmp_path: Path,
) -> None:
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
    # Nothing landed -- this is the gate-decided-park path.
    assert submit_calls == []

    records = _audit_records(tmp_path, run_id)
    kinds = [_event_kind(r) for r in records]
    # Both landing-stage records reach the stream as ``domain.<kind>`` events.
    assert _GATE_VERDICT_KIND in kinds
    assert _LANDING_PARKED_KIND in kinds

    landing_positions = [
        i for i, r in enumerate(records) if _event_kind(r) in _LANDING_KINDS
    ]
    in_run_positions = [
        i for i, r in enumerate(records) if _event_kind(r) not in _LANDING_KINDS
    ]
    # There is in-run telemetry, and every landing-stage record sorts strictly
    # after all of it -- i.e. after attempt finalization.
    assert in_run_positions, "expected in-run telemetry before the landing records"
    assert min(landing_positions) > max(in_run_positions)

    # Ledger order is preserved at the tail: the verdict record precedes the
    # park witness.
    verdict_i = kinds.index(_GATE_VERDICT_KIND)
    park_i = kinds.index(_LANDING_PARKED_KIND)
    assert verdict_i < park_i

    # The stream's ascending-sequence contract holds across the file->ledger
    # seam (the projected records continue past the last file sequence).
    seqs = [r.sequence for r in records if r.sequence is not None]
    assert seqs == sorted(seqs)
    assert len(seqs) == len(set(seqs))

    # The verdict payload carries the real failing-oracle diagnostic, verbatim
    # (no redactor was supplied), so the block is diagnosable from the stream.
    verdict = records[verdict_i]
    assert isinstance(verdict, EventRecord)
    assert verdict.payload["outcome"] == "fail"
    assert any(
        "GATEFAIL_DIAG" in receipt.get("output_excerpt", "")
        for receipt in verdict.payload["receipts"]
    )


# --- a passing gate projects the verdict, but no park ------------------------


def test_audit_stream_appends_passing_gate_verdict_without_park(
    tmp_path: Path,
) -> None:
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "alpha", grader_run="true")
    held_out = tmp_path / "held_out"
    _register_held_out(
        held_out,
        "alpha",
        [{"type": "command", "run": "echo GATEPASS_MARKER", "name": "oracle-a"}],
    )

    submit_calls: list[SubmitRequest] = []
    report = _drive(tmp_path, held_out_root=held_out, submit_calls=submit_calls)
    run_id = report.runs[0].run_id

    records = _audit_records(tmp_path, run_id)
    kinds = [_event_kind(r) for r in records]
    # A passing gate leaves a verdict record but no park witness.
    assert _GATE_VERDICT_KIND in kinds
    assert _LANDING_PARKED_KIND not in kinds

    verdict = records[kinds.index(_GATE_VERDICT_KIND)]
    assert isinstance(verdict, EventRecord)
    assert verdict.payload["outcome"] == "pass"
    # The verdict is the final record: it sorts after every in-run record.
    assert verdict.sequence == max(
        r.sequence for r in records if r.sequence is not None
    )


# --- edge case: a run with no landing decision streams as before -------------


def test_audit_stream_without_landing_decision_is_unchanged(
    tmp_path: Path,
) -> None:
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "alpha")

    submit_calls: list[SubmitRequest] = []
    report = _drive(tmp_path, held_out_root=None, submit_calls=submit_calls)
    run_id = report.runs[0].run_id

    records = _audit_records(tmp_path, run_id)
    # The run produced telemetry, and none of it is a projected landing record:
    # with no held-out source the gate never ran, so there is no decision to
    # surface and the stream is identical to the pre-projection stream.
    assert records
    assert not any(
        isinstance(r, EventRecord) and r.kind.startswith("domain.")
        for r in records
    )

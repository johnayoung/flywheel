"""End-to-end wiring of the landable-change gate through ``orchestrate``
(spec 00061, the orchestrator-gate layer).

The seam tests in ``test_landability_seam.py`` cover the predicate helper in
isolation; these drive a real lifecycle through a file-backed store with the
agent stubbed and a bundled strategy that supplies ``is_landable``, asserting
the orchestrator routes a non-landable finished run back through bounded retry
(re-driven against the same base) instead of landing it as ``DONE`` -- and that
a committed (landable) change lands through the unchanged submit path. The
orchestrator and core stay git-unaware: the only landability knowledge lives in
the strategy's predicate (criterion #6).
"""

from __future__ import annotations

import asyncio
import io
import json
from pathlib import Path

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
    LandabilityVerdict,
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


def _always_verify():
    async def _invoke(request: InvocationRequest) -> IterationResult:
        return _verify_result()

    return _invoke


class _RecordingStrategy:
    """A bundled strategy that supplies the optional ``is_landable`` predicate.

    ``verdicts`` is consumed one entry per finished run; once exhausted the
    last verdict repeats. Records every probe and submit call so a test can
    assert the gate re-drove (or did not land) the run.
    """

    def __init__(
        self, root: Path, verdicts: list[LandabilityVerdict]
    ) -> None:
        self._root = root
        self._verdicts = verdicts
        self.probe_calls: list[SubmitRequest] = []
        self.submit_calls: list[SubmitRequest] = []

    def prepare_sandbox(self, request: SandboxRequest) -> Path:
        sandbox = self._root / request.task_id
        sandbox.mkdir(parents=True, exist_ok=True)
        return sandbox

    def submit(self, request: SubmitRequest) -> None:
        self.submit_calls.append(request)

    def is_landable(self, request: SubmitRequest) -> LandabilityVerdict:
        self.probe_calls.append(request)
        idx = min(len(self.probe_calls) - 1, len(self._verdicts) - 1)
        return self._verdicts[idx]


# --- tests ------------------------------------------------------------------


def test_non_landable_run_is_redriven_then_lands(tmp_path: Path) -> None:
    """Criterion #1: a finished run whose change is not landable is not
    recorded as a landed DONE -- it is re-driven by bounded retry. The second
    (now landable) run lands DONE through the unchanged submit path, and submit
    is invoked exactly once, for the landed run."""
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "alpha")

    strategy = _RecordingStrategy(
        tmp_path / "prepared",
        verdicts=[
            LandabilityVerdict(landable=False, reason="uncommitted tree"),
            LandabilityVerdict(landable=True),
        ],
    )

    report = asyncio.run(
        orchestrate(
            tasks_dir=tmp_path / "tasks",
            db_path=tmp_path / "flywheel.sqlite",
            sandbox_root=tmp_path / "sandboxes",
            invoke=_always_verify(),
            max_retries=1,
            max_turns=4,
            stream=io.StringIO(),
            strategy=strategy,
        )
    )

    assert [r.status for r in report.runs] == [Status.DONE]
    # The predicate was consulted on both finished runs (attempt 1 + retry).
    assert len(strategy.probe_calls) == 2
    # submit landed exactly once, for the landable run.
    assert len(strategy.submit_calls) == 1
    assert strategy.submit_calls[0].status is Status.DONE
    assert strategy.submit_calls[0].task_id == "alpha"


def test_never_landable_run_ends_failed_not_done(tmp_path: Path) -> None:
    """Criterion #2/#4: a run that never produces a landable change is never
    recorded as DONE; it exhausts the retry budget and ends FAILED, and submit
    sees the FAILED (park) status rather than a landing."""
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "beta")

    strategy = _RecordingStrategy(
        tmp_path / "prepared",
        verdicts=[LandabilityVerdict(landable=False, reason="no commits")],
    )

    report = asyncio.run(
        orchestrate(
            tasks_dir=tmp_path / "tasks",
            db_path=tmp_path / "flywheel.sqlite",
            sandbox_root=tmp_path / "sandboxes",
            invoke=_always_verify(),
            max_retries=1,
            max_turns=4,
            stream=io.StringIO(),
            strategy=strategy,
        )
    )

    assert [r.status for r in report.runs] == [Status.FAILED]
    # Both the first run and its single retry were probed and found unlandable.
    assert len(strategy.probe_calls) == 2
    # submit is still invoked (to park), but never with a DONE landing.
    assert len(strategy.submit_calls) == 1
    assert strategy.submit_calls[0].status is Status.FAILED


def test_landable_run_lands_on_first_attempt(tmp_path: Path) -> None:
    """Criterion #5: a landable verdict lands DONE on the first attempt with no
    spurious retry -- the post-run path is unchanged."""
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "gamma")

    strategy = _RecordingStrategy(
        tmp_path / "prepared",
        verdicts=[LandabilityVerdict(landable=True)],
    )

    report = asyncio.run(
        orchestrate(
            tasks_dir=tmp_path / "tasks",
            db_path=tmp_path / "flywheel.sqlite",
            sandbox_root=tmp_path / "sandboxes",
            invoke=_always_verify(),
            max_retries=1,
            max_turns=4,
            stream=io.StringIO(),
            strategy=strategy,
        )
    )

    assert [r.status for r in report.runs] == [Status.DONE]
    assert len(strategy.probe_calls) == 1
    assert len(strategy.submit_calls) == 1
    assert strategy.submit_calls[0].status is Status.DONE


def test_no_predicate_strategy_lands_unaffected(tmp_path: Path) -> None:
    """Criterion #3: a strategy with no ``is_landable`` (a non-git task class)
    is treated as always-landable and lands DONE on the first attempt, exactly
    as before the gate existed."""
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "delta")

    submit_calls: list[SubmitRequest] = []

    class _NoProbeStrategy:
        def prepare_sandbox(self, request: SandboxRequest) -> Path:
            sandbox = tmp_path / "prepared" / request.task_id
            sandbox.mkdir(parents=True, exist_ok=True)
            return sandbox

        def submit(self, request: SubmitRequest) -> None:
            submit_calls.append(request)

    report = asyncio.run(
        orchestrate(
            tasks_dir=tmp_path / "tasks",
            db_path=tmp_path / "flywheel.sqlite",
            sandbox_root=tmp_path / "sandboxes",
            invoke=_always_verify(),
            max_retries=1,
            max_turns=4,
            stream=io.StringIO(),
            strategy=_NoProbeStrategy(),
        )
    )

    assert [r.status for r in report.runs] == [Status.DONE]
    assert len(submit_calls) == 1
    assert submit_calls[0].status is Status.DONE

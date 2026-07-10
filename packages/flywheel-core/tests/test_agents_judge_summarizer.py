"""The rubric-judge and recovery-summarizer invokes on the agents runtime.

Two layers under test. The factory layer
(``make_agents_judge_invoke`` / ``make_agents_summarizer_invoke``) is driven
against the real stack -- AgentRuntime + LocalHost + the claude-code CLI
transport -- via a scripted stream-json executable, plus a recording runtime
double for the per-call ``AgentConfiguration`` (model resolution, turn cap,
auto permissions, ``skills='all'``). The workflow layer asserts
``run_task_object``'s multi-agent opt-in builds both invokes with the knobs
the harness default path would resolve (judge from HarnessConfig sources,
summarizer from ``run_recovery_summarizer``'s own defaults) and that the
legacy path (``agent_id=None``) never touches the factories.
"""

from __future__ import annotations

import asyncio
import json
import sys
import textwrap
from collections.abc import Awaitable
from pathlib import Path
from typing import Any, cast

import flywheel_core.agents_invoke as agents_invoke_module
from flywheel_agents import (
    AgentRuntime,
    CompletedRun,
    PermissionPolicy,
    RunRequest,
    StopInfo,
    StopReason,
)
from flywheel_core import InvocationSignals, IterationResult, Task
from flywheel_core.agents_invoke import (
    make_agents_judge_invoke,
    make_agents_summarizer_invoke,
)
from flywheel_core.envelope import parse_envelope
from flywheel_core.grader_rubric import ValidVerdict, parse_verdict
from flywheel_core.harness import InvocationRequest
from flywheel_core.task import RubricGrader
from flywheel_core.workflow import run_task_object

def _run[T](awaitable: Awaitable[T]) -> T:
    """Drive an invoke's Awaitable on a fresh loop (sync tests, no plugins)."""

    async def _main() -> T:
        return await awaitable

    return asyncio.run(_main())


# --- Factory layer -----------------------------------------------------------


_VERDICT_TEXT = (
    "checked the worktree\n<!-- RUBRIC_VERDICT -->\n"
    + json.dumps({"passed": True, "summary": "verified", "unknown": False})
    + "\n<!-- /RUBRIC_VERDICT -->"
)

# Scripted claude-code stream-json stand-in (the test_agents_invoke.py
# pattern): drains stdin, records its argv for flag assertions, then emits
# one assistant turn whose text is a RUBRIC_VERDICT fenced block.
_JUDGE_SCRIPT = textwrap.dedent(
    """
    import json
    import sys

    sys.stdin.read()
    with open("argv.json", "w") as fh:
        json.dump(sys.argv, fh)

    def emit(obj):
        print(json.dumps(obj), flush=True)

    emit({"type": "system", "subtype": "init", "session_id": "sess-judge"})
    verdict = (
        "checked the worktree\\n<!-- RUBRIC_VERDICT -->\\n"
        + json.dumps({"passed": True, "summary": "verified", "unknown": False})
        + "\\n<!-- /RUBRIC_VERDICT -->"
    )
    emit(
        {
            "type": "assistant",
            "message": {
                "content": [{"type": "text", "text": verdict}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 3, "output_tokens": 2},
            },
        }
    )
    emit(
        {
            "type": "result",
            "subtype": "success",
            "is_error": False,
            "num_turns": 1,
            "total_cost_usd": 0.0,
            "usage": {"input_tokens": 3, "output_tokens": 2},
        }
    )
    """
)

_HANDOFF_TEXT = (
    "<!-- RECOVERY_HANDOFF -->\n"
    + json.dumps(
        {
            "work_done": "half",
            "work_remaining": "half",
            "key_decisions": "none",
            "suggested_next_step": "finish",
        }
    )
    + "\n<!-- /RECOVERY_HANDOFF -->"
)

_SUMMARIZER_SCRIPT = textwrap.dedent(
    """
    import json
    import sys

    sys.stdin.read()

    def emit(obj):
        print(json.dumps(obj), flush=True)

    emit({"type": "system", "subtype": "init", "session_id": "sess-summ"})
    handoff = (
        "<!-- RECOVERY_HANDOFF -->\\n"
        + json.dumps(
            {
                "work_done": "half",
                "work_remaining": "half",
                "key_decisions": "none",
                "suggested_next_step": "finish",
            }
        )
        + "\\n<!-- /RECOVERY_HANDOFF -->"
    )
    emit(
        {
            "type": "assistant",
            "message": {
                "content": [{"type": "text", "text": handoff}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 3, "output_tokens": 2},
            },
        }
    )
    emit(
        {
            "type": "result",
            "subtype": "success",
            "is_error": False,
            "num_turns": 1,
            "total_cost_usd": 0.0,
            "usage": {"input_tokens": 3, "output_tokens": 2},
        }
    )
    """
)


class _RecordingRuntime:
    """Test double matching ``AgentRuntime.run(request, *, sink, host)``."""

    def __init__(self, final_text: str) -> None:
        self.requests: list[RunRequest] = []
        self._final_text = final_text

    async def run(
        self,
        request: RunRequest,
        *,
        sink: object | None = None,
        host: object | None = None,
    ) -> CompletedRun:
        self.requests.append(request)
        return CompletedRun(
            final_text=self._final_text,
            stop=StopInfo(reason=StopReason.COMPLETED, finished=True),
        )


def test_judge_invoke_real_run_returns_fenced_verdict(tmp_path: Path) -> None:
    script = tmp_path / "scripted_judge.py"
    script.write_text(_JUDGE_SCRIPT)
    judge = make_agents_judge_invoke(
        agent_id="claude-code",
        judge_max_turns=7,
        judge_model=None,
        command_override=(sys.executable, str(script)),
    )
    grader = RubricGrader(assertions=["it works"], judge_model="judge-model-x")

    response = _run(judge("judge this", grader, tmp_path))

    assert "<!-- RUBRIC_VERDICT -->" in response
    verdict = parse_verdict(response)
    assert isinstance(verdict, ValidVerdict)
    assert verdict.passed is True
    # The knobs reached the real command line: per-grader model override,
    # turn cap, and the auto (bypass-equivalent) permission flag.
    argv = json.loads((tmp_path / "argv.json").read_text())
    assert argv[argv.index("--model") + 1] == "judge-model-x"
    assert argv[argv.index("--max-turns") + 1] == "7"
    assert "--dangerously-skip-permissions" in argv


def test_summarizer_invoke_real_run_returns_handoff(tmp_path: Path) -> None:
    script = tmp_path / "scripted_summarizer.py"
    script.write_text(_SUMMARIZER_SCRIPT)
    summarize = make_agents_summarizer_invoke(
        agent_id="claude-code",
        summarizer_max_turns=8,
        summarizer_model=None,
        command_override=(sys.executable, str(script)),
    )

    response = _run(summarize("summarize this", tmp_path))

    assert response == _HANDOFF_TEXT


def test_judge_configuration_grader_model_overrides_default(
    tmp_path: Path,
) -> None:
    recorder = _RecordingRuntime(_VERDICT_TEXT)
    judge = make_agents_judge_invoke(
        agent_id="claude-code",
        transport="cli",
        judge_max_turns=13,
        judge_model="default-judge-model",
        runtime=cast(AgentRuntime, recorder),
    )
    with_override = RubricGrader(
        assertions=["a"], judge_model="per-grader-model"
    )
    without_override = RubricGrader(assertions=["a"])

    first = _run(judge("p1", with_override, tmp_path))
    second = _run(judge("p2", without_override, str(tmp_path)))

    assert first == _VERDICT_TEXT
    assert second == _VERDICT_TEXT
    assert len(recorder.requests) == 2
    overridden, defaulted = recorder.requests
    assert overridden.configuration.model_id == "per-grader-model"
    assert defaulted.configuration.model_id == "default-judge-model"
    for request, prompt in zip(recorder.requests, ("p1", "p2")):
        assert request.prompt == prompt
        assert request.working_directory == tmp_path
        config = request.configuration
        assert config.agent_id == "claude-code"
        assert config.max_turns == 13
        assert config.permission_policy is PermissionPolicy.AUTO
        assert config.adapter_options["skills"] == "all"
        assert config.adapter_options["transport"] == "cli"


def test_summarizer_configuration_carries_knobs(tmp_path: Path) -> None:
    recorder = _RecordingRuntime(_HANDOFF_TEXT)
    summarize = make_agents_summarizer_invoke(
        agent_id="claude-code",
        summarizer_max_turns=8,
        summarizer_model="summ-model",
        runtime=cast(AgentRuntime, recorder),
    )

    response = _run(summarize("prompt", str(tmp_path)))

    assert response == _HANDOFF_TEXT
    (request,) = recorder.requests
    assert request.prompt == "prompt"
    assert request.working_directory == tmp_path
    config = request.configuration
    assert config.agent_id == "claude-code"
    assert config.model_id == "summ-model"
    assert config.max_turns == 8
    assert config.permission_policy is PermissionPolicy.AUTO
    assert config.adapter_options == {"skills": "all"}


# --- Workflow wiring ---------------------------------------------------------


_ENVELOPE = (
    "done\n<!-- LOOP_STATUS -->\n"
    + json.dumps({"intent": "verify", "reason": "complete"})
    + "\n<!-- /LOOP_STATUS -->"
)


def _scripted_result() -> IterationResult:
    return IterationResult(
        transcript=_ENVELOPE,
        messages=(),
        envelope=parse_envelope(_ENVELOPE),
        signals=InvocationSignals(
            stop_reason="end_turn",
            num_turns=1,
            total_cost_usd=0.0,
            result_is_error=False,
            result_subtype="success",
            api_error_status=None,
            session_id="sess-judge-wiring",
        ),
        usage={"input_tokens": 1, "output_tokens": 1},
    )


def _patch_factories(
    monkeypatch: Any, captured: dict[str, dict[str, Any]], judge_calls: list[str]
) -> None:
    """Replace all three agents_invoke factories with capturing stand-ins."""

    def _main_factory(**kwargs: Any):
        async def _invoke(request: InvocationRequest) -> IterationResult:
            return _scripted_result()

        return _invoke

    def _judge_factory(**kwargs: Any):
        captured["judge"] = kwargs

        async def _judge(
            prompt: str, grader: RubricGrader, worktree: Path | str
        ) -> str:
            judge_calls.append(prompt)
            return _VERDICT_TEXT

        return _judge

    def _summarizer_factory(**kwargs: Any):
        captured["summarizer"] = kwargs

        async def _summarize(prompt: str, worktree: Path | str) -> str:
            return _HANDOFF_TEXT

        return _summarize

    monkeypatch.setattr(agents_invoke_module, "make_agents_invoke", _main_factory)
    monkeypatch.setattr(
        agents_invoke_module, "make_agents_judge_invoke", _judge_factory
    )
    monkeypatch.setattr(
        agents_invoke_module, "make_agents_summarizer_invoke", _summarizer_factory
    )


def test_agent_id_threads_judge_and_summarizer_invokes(
    tmp_path: Path, monkeypatch: Any
) -> None:
    captured: dict[str, dict[str, Any]] = {}
    judge_calls: list[str] = []
    _patch_factories(monkeypatch, captured, judge_calls)

    outcome = asyncio.run(
        run_task_object(
            Task(goal="g", graders=[RubricGrader(assertions=["it works"])]),
            db_path=tmp_path / "db.sqlite",
            sandbox=tmp_path / "sandbox",
            agent_id="claude-code",
            agent_transport="cli",
            rubric_judge_max_turns=11,
        )
    )

    assert outcome.lifecycle.status.value == "done"
    # The repo knob feeds the judge cap exactly as it feeds
    # HarnessConfig.rubric_judge_max_turns; judge_model mirrors
    # HarnessConfig.rubric_judge_model (never set on this path).
    assert captured["judge"] == {
        "agent_id": "claude-code",
        "transport": "cli",
        "judge_max_turns": 11,
        "judge_model": None,
    }
    # The harness passes neither summarizer knob, so the factory mirrors
    # run_recovery_summarizer's own defaults.
    assert captured["summarizer"] == {
        "agent_id": "claude-code",
        "transport": "cli",
        "summarizer_max_turns": 8,
        "summarizer_model": None,
    }
    # The built judge invoke reached HarnessConfig.rubric_judge_invoke: the
    # rubric grader was verified through it, not the legacy SDK default.
    assert judge_calls, "judge invoke never reached the harness"


def test_agent_id_default_knobs_mirror_harness_defaults(
    tmp_path: Path, monkeypatch: Any
) -> None:
    captured: dict[str, dict[str, Any]] = {}
    _patch_factories(monkeypatch, captured, judge_calls=[])

    outcome = asyncio.run(
        run_task_object(
            Task(goal="g", graders=[]),
            db_path=tmp_path / "db.sqlite",
            sandbox=tmp_path / "sandbox",
            agent_id="claude-code",
        )
    )

    assert outcome.lifecycle.status.value == "done"
    assert captured["judge"] == {
        "agent_id": "claude-code",
        "transport": None,
        "judge_max_turns": 32,
        "judge_model": None,
    }
    assert captured["summarizer"] == {
        "agent_id": "claude-code",
        "transport": None,
        "summarizer_max_turns": 8,
        "summarizer_model": None,
    }


def test_legacy_path_never_builds_agents_invokes(
    tmp_path: Path, monkeypatch: Any
) -> None:
    captured: dict[str, dict[str, Any]] = {}
    _patch_factories(monkeypatch, captured, judge_calls=[])

    async def _injected(request: InvocationRequest) -> IterationResult:
        return _scripted_result()

    outcome = asyncio.run(
        run_task_object(
            Task(goal="g", graders=[]),
            db_path=tmp_path / "db.sqlite",
            sandbox=tmp_path / "sandbox",
            invoke=_injected,
        )
    )

    assert outcome.lifecycle.status.value == "done"
    assert captured == {}

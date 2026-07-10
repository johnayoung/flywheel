"""The multi-agent opt-in branch of ``run_task_object`` (``agent_id=``).

When ``agent_id`` is set and no ``invoke`` is injected, the workflow must
build its invoker through ``flywheel_core.agents_invoke.make_agents_invoke``
with the Claude permission vocabulary mapped to the generic policy — and the
legacy path must stay byte-identical when ``agent_id`` is None.
"""

from __future__ import annotations

import asyncio
import json
from pathlib import Path
from typing import Any

import flywheel_core.agents_invoke as agents_invoke_module
from flywheel_core import IterationResult, InvocationSignals, Task
from flywheel_core.envelope import parse_envelope
from flywheel_core.harness import InvocationRequest
from flywheel_core.workflow import _PERMISSION_POLICIES, run_task_object

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
            session_id="sess-optin",
        ),
        usage={"input_tokens": 1, "output_tokens": 1},
    )


def test_agent_id_routes_through_make_agents_invoke(
    tmp_path: Path, monkeypatch: Any
) -> None:
    captured: dict[str, Any] = {}

    def _factory(**kwargs: Any):
        captured.update(kwargs)

        async def _invoke(request: InvocationRequest) -> IterationResult:
            return _scripted_result()

        return _invoke

    monkeypatch.setattr(agents_invoke_module, "make_agents_invoke", _factory)
    outcome = asyncio.run(
        run_task_object(
            Task(goal="g", graders=[]),
            db_path=tmp_path / "db.sqlite",
            sandbox=tmp_path / "sandbox",
            agent_id="claude-code",
            agent_transport="cli",
            model="claude-sonnet-4-5",
            skills="all",
            allowed_tools=("Bash", "Edit"),
            mcp_servers=("serena",),
            mcp_strict=True,
            exec_enabled=True,
            exec_auto_allow=False,
        )
    )
    assert outcome.lifecycle.status.value == "done"
    assert captured["agent_id"] == "claude-code"
    assert captured["model"] == "claude-sonnet-4-5"
    assert captured["permission_policy"] == "auto"
    assert captured["working_directory"] == tmp_path / "sandbox"
    options = captured["adapter_options"]
    assert options["transport"] == "cli"
    assert options["skills"] == "all"
    assert options["allowed_tools"] == ("Bash", "Edit")
    assert options["mcp_servers"] == ("serena",)
    assert options["mcp_strict"] is True
    assert options["sandbox_exec"] == {
        "enabled": True,
        "autoAllowBashIfSandboxed": False,
    }


def test_permission_vocabulary_mapping_never_defaults_to_bypass() -> None:
    assert _PERMISSION_POLICIES["bypassPermissions"] == "auto"
    assert _PERMISSION_POLICIES["plan"] == "plan"
    assert _PERMISSION_POLICIES["default"] == "supervised"
    assert _PERMISSION_POLICIES["acceptEdits"] == "supervised"
    assert _PERMISSION_POLICIES.get("anythingElse", "supervised") == "supervised"

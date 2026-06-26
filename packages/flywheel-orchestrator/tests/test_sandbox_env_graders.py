"""[sandbox.env] reaches command graders, not just the agent (spec: build-cache
parity). An agent build and the command grader that verifies it must share the
same environment (e.g. a Rust CARGO_TARGET_DIR / RUSTC_WRAPPER), so the grader
subprocess runs under the resolved [sandbox.env], identical to the agent's.
"""

from __future__ import annotations

import os

from flywheel_core.grader_command import run_command_graders
from flywheel_core.store_protocols import GraderResultRecord
from flywheel_core.task import CommandGrader, Task
from flywheel_orchestrator._orchestrate import _sandbox_agent_primitives
from flywheel_orchestrator._policy import (
    SandboxEnv,
    SandboxPolicy,
    WorkPolicy,
    resolve_grader_env,
)


# --- resolve_grader_env -----------------------------------------------------


def test_no_config_returns_none_so_graders_inherit_unchanged() -> None:
    # Default [sandbox.env] (inherit_home, no pass/set) -> None -> graders
    # inherit the worker env exactly as before (byte-identical).
    assert resolve_grader_env(SandboxEnv()) is None


def test_set_value_overlays_the_ambient_environment() -> None:
    env = resolve_grader_env(
        SandboxEnv(set_values={"FW_PROBE": "on"}),
        base_environ={"PATH": "/usr/bin", "HOME": "/home/x"},
    )
    assert env is not None
    assert env["FW_PROBE"] == "on"  # the override is present
    assert env["PATH"] == "/usr/bin"  # ambient env preserved (graders need it)
    assert env["HOME"] == "/home/x"


def test_passthrough_forwards_present_names_only() -> None:
    env = resolve_grader_env(
        SandboxEnv(passthrough=("PRESENT", "ABSENT")),
        base_environ={"PRESENT": "yes", "PATH": "/usr/bin"},
    )
    assert env is not None
    assert env["PRESENT"] == "yes"
    assert "ABSENT" not in env  # absent names are dropped, never blank-filled


def test_inherit_home_false_scopes_to_resolved_values_only() -> None:
    env = resolve_grader_env(
        SandboxEnv(set_values={"FW_PROBE": "on"}, inherit_home=False),
        base_environ={"PATH": "/usr/bin", "HOME": "/home/x"},
    )
    assert env == {"FW_PROBE": "on"}  # no ambient inheritance when scoped


def test_set_wins_over_passthrough_on_collision() -> None:
    env = resolve_grader_env(
        SandboxEnv(passthrough=("FW_PROBE",), set_values={"FW_PROBE": "set"}),
        base_environ={"FW_PROBE": "passed", "PATH": "/usr/bin"},
    )
    assert env is not None and env["FW_PROBE"] == "set"


# --- the orchestrator decomposes grader_env alongside agent_env -------------


def test_primitives_carry_grader_env_from_policy() -> None:
    policy = WorkPolicy(
        source_kind="directory",
        sandbox=SandboxPolicy(env=SandboxEnv(set_values={"FW_PROBE": "on"})),
    )
    primitives = _sandbox_agent_primitives(policy)
    assert "grader_env" in primitives
    assert primitives["grader_env"]["FW_PROBE"] == "on"
    # A None policy / no [sandbox.env] -> grader_env None (inherit), byte-identical.
    assert _sandbox_agent_primitives(None)["grader_env"] is None


# --- end-to-end: the resolved env actually reaches the grader subprocess ----


class _Recorder:
    def __init__(self) -> None:
        self.records: list[GraderResultRecord] = []

    def append_grader_result(
        self, result: GraderResultRecord
    ) -> GraderResultRecord:
        self.records.append(result)
        return result

    def list_grader_results(
        self, run_id: str, attempt_number: int
    ) -> list[GraderResultRecord]:
        return list(self.records)


def _probe_task() -> Task:
    # The grader passes only when FW_PROBE=on is visible in its environment.
    return Task(
        goal="probe that the grader sees the sandbox env.",
        graders=[CommandGrader(run='test "$FW_PROBE" = on', name="probe")],
    )


def test_set_value_reaches_the_grader_subprocess() -> None:
    grader_env = resolve_grader_env(
        SandboxEnv(set_values={"FW_PROBE": "on"}),
        base_environ=dict(os.environ),
    )
    records = run_command_graders(
        _probe_task(), _Recorder(), run_id="r1", attempt_number=1, env=grader_env
    )
    assert len(records) == 1 and records[0].passed  # grader saw FW_PROBE=on


def test_without_sandbox_env_the_probe_grader_fails() -> None:
    # No [sandbox.env] -> env None -> the probe var is absent -> grader fails,
    # proving the pass above is caused by the injected env, not ambient state.
    assert "FW_PROBE" not in os.environ
    records = run_command_graders(
        _probe_task(), _Recorder(), run_id="r1", attempt_number=1, env=None
    )
    assert len(records) == 1 and not records[0].passed

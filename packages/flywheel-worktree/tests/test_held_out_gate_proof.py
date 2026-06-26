"""End-to-end held-out gate proof through the real construction path (00057 #3/#5).

The activation-layer tests in ``test_worker.py`` prove ``build_held_out_source``
resolves a configured ``[held_out] root`` under the repo root. These tests close
the remaining gap (spec 00057 D-2): drive a real lifecycle through
``orchestrate`` with the source built *from a parsed policy* via
``build_held_out_source`` -- not a hand-built ``FilesystemHeldOutGraderSource`` --
and assert the *landing* consequences a config-to-source wiring regression would
break:

* #3 -- a task whose ``<root>/<task_id>.json`` registration's command grader
  PASSes against the committed tree lands identically to a no-held-out baseline
  (``submit`` invoked); a task whose registration FAILs does not land
  (``submit`` never invoked) and its worktree is parked, with the gate-failed
  end-state distinguishable on the recorded run.
* #5 -- while the gate is active with a registration and its oracle on disk at
  the configured root, neither file is present anywhere under the agent's
  worktree during the run (the committed-pointer / git-ignored-payload property).

The discrimination is reproduced THROUGH the gate: the registration (written by
the shipped ``write_oracle_registration``) invokes an oracle that grades the
committed content, so a correct committed tree PASSes and a plausible-wrong one
FAILs. The oracle is referenced by an absolute path OUTSIDE the committed tree
and run with ``cwd`` = the committed tree, so it observes the committed changes
while its own file never enters the worktree.
"""

from __future__ import annotations

import asyncio
import io
import json
import sys
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
    FilesystemHeldOutGraderSource,
    GateOutcome,
    HeldOutGraderSource,
    SandboxRequest,
    SubmitRequest,
    WorkPolicy,
    load_policy,
    orchestrate,
    write_oracle_registration,
)
from flywheel_worktree import worker

# An oracle authored blind: run with the committed tree as cwd, it imports the
# committed module and asserts a discriminating relation. It loads the agent's
# committed code by putting cwd (the committed tree) on the import path -- the
# oracle file itself lives outside that tree, so an absolute path is the only way
# the gate can reach it (D-2).
_ORACLE_SOURCE = """\
import os
import sys

sys.path.insert(0, os.getcwd())

from solution import normalize

# Discriminating input: a correct `normalize` sorts ascending; a plausible-wrong
# identity / no-op implementation returns the input unchanged and dies here.
assert normalize([3, 1, 2]) == [1, 2, 3], "normalize must sort ascending"
"""

_CORRECT_SOLUTION = """\
def normalize(values):
    return sorted(values)
"""

# Plausible-wrong reference: returns the input order unchanged (the off-by-design
# the oracle's discriminating input was built to kill).
_WRONG_SOLUTION = """\
def normalize(values):
    return list(values)
"""


# --- helpers ----------------------------------------------------------------


def _write_task(phase: Path, task_id: str) -> None:
    """A task whose visible grader passes, so the lifecycle reaches DONE and the
    held-out gate -- not the agent's own grader -- decides whether it lands."""
    phase.mkdir(parents=True, exist_ok=True)
    payload = {
        "id": task_id,
        "goal": f"Goal for {task_id}.",
        "graders": [{"type": "command", "run": "true"}],
    }
    (phase / f"{task_id}.json").write_text(json.dumps(payload))


def _write_oracle(root: Path) -> Path:
    """Author the oracle at the git-ignored held-out root, OUTSIDE every committed
    tree the gate grades (D-1 payload directory)."""
    root.mkdir(parents=True, exist_ok=True)
    oracle = root / "normalize_oracle.py"
    oracle.write_text(_ORACLE_SOURCE, encoding="utf-8")
    return oracle


def _policy_with_held_out(repo_root: Path, root_name: str) -> WorkPolicy:
    """Parse a real ``flywheel.toml`` with ``[held_out] root`` set, so the source
    is built from the same config-to-policy path the worker uses (D-2)."""
    repo_root.mkdir(parents=True, exist_ok=True)
    toml = repo_root / "flywheel.toml"
    toml.write_text(
        '[source]\nkind = "directory"\n\n'
        f'[held_out]\nroot = "{root_name}"\n',
        encoding="utf-8",
    )
    return load_policy(toml)


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
    root: Path,
    *,
    source: HeldOutGraderSource | None,
    submit_calls: list[SubmitRequest],
    solutions: dict[str, str],
):
    """Drive ``orchestrate`` over the tasks under ``root``. The prepared sandbox
    is the committed tree the gate grades: each task's ``solution.py`` is written
    into it from ``solutions`` so the oracle observes that committed content."""
    prepared_root = root / "prepared"

    def prepare(req: SandboxRequest) -> Path:
        sandbox = prepared_root / req.task_id
        sandbox.mkdir(parents=True, exist_ok=True)
        solution = solutions.get(req.task_id)
        if solution is not None:
            (sandbox / "solution.py").write_text(solution, encoding="utf-8")
        return sandbox

    report = asyncio.run(
        orchestrate(
            tasks_dir=root / "tasks",
            db_path=root / "flywheel.sqlite",
            sandbox_root=root / "sandboxes",
            invoke=_always_verify(),
            max_retries=0,
            max_turns=4,
            stream=io.StringIO(),
            prepare_sandbox=prepare,
            submit=submit_calls.append,
            held_out_source=source,
        )
    )
    return report, prepared_root


# --- #3: built-from-config gate blocks a fail, lands a pass like baseline -----


def test_config_built_gate_blocks_fail_and_lands_pass_like_baseline(
    tmp_path: Path,
) -> None:
    """The gate built from a parsed ``[held_out] root`` policy via
    ``build_held_out_source`` blocks a task whose registration's command grader
    fails the committed tree and lands one whose registration passes -- the pass
    reaching the same landed end-state as a no-held-out baseline (#3).

    The registration is produced by the shipped ``write_oracle_registration`` and
    the discrimination is reproduced THROUGH the gate: a correct committed
    ``solution.py`` PASSes, a plausible-wrong one FAILs, because the oracle grades
    the committed content, not the agent's self-report.
    """
    # --- baseline: the pass-task driven with NO held-out source at all ---------
    baseline_root = tmp_path / "baseline"
    _write_task(baseline_root / "tasks" / "active" / "01-phase", "pass-task")
    baseline_submits: list[SubmitRequest] = []
    baseline, _ = _drive(
        baseline_root,
        source=None,
        submit_calls=baseline_submits,
        solutions={"pass-task": _CORRECT_SOLUTION},
    )

    # --- active: gate built from the real construction path --------------------
    active_root = tmp_path / "active"
    phase = active_root / "tasks" / "active" / "01-phase"
    _write_task(phase, "pass-task")
    _write_task(phase, "fail-task")

    held_out = active_root / "held_out"
    oracle = _write_oracle(held_out)
    # Same oracle for both tasks: only the committed tree differs, so the gate's
    # verdict can only come from grading the committed content.
    write_oracle_registration(
        held_out, "pass-task", oracle, interpreter=sys.executable
    )
    write_oracle_registration(
        held_out, "fail-task", oracle, interpreter=sys.executable
    )

    policy = _policy_with_held_out(active_root, "held_out")
    source = worker.build_held_out_source(policy, active_root)
    # The config actually wired a source, rooted under the repo root (D-2).
    assert isinstance(source, FilesystemHeldOutGraderSource)
    assert source.root == held_out

    active_submits: list[SubmitRequest] = []
    report, prepared_root = _drive(
        active_root,
        source=source,
        submit_calls=active_submits,
        solutions={
            "pass-task": _CORRECT_SOLUTION,
            "fail-task": _WRONG_SOLUTION,
        },
    )
    by_id = {r.task_id: r for r in report.runs}
    pass_run = by_id["pass-task"]
    fail_run = by_id["fail-task"]

    # The pass-task lands exactly like the no-held-out baseline: DONE, gate PASS,
    # submit invoked with the DONE landing status.
    assert [r.status for r in baseline.runs] == [Status.DONE]
    assert [c.task_id for c in baseline_submits] == ["pass-task"]
    assert baseline.runs[0].gate is None
    assert (pass_run.status, pass_run.gate) == (Status.DONE, GateOutcome.PASS)
    pass_submits = [c for c in active_submits if c.task_id == "pass-task"]
    assert [c.status for c in pass_submits] == [Status.DONE]
    assert baseline_submits[0].status == pass_submits[0].status == Status.DONE

    # The fail-task does not land: the lifecycle is still DONE (the gate is a
    # landing decision, not a re-judgement of the attempt), but the gate FAILed,
    # submit was never invoked for it, and its worktree is parked.
    assert (fail_run.status, fail_run.gate) == (Status.DONE, GateOutcome.FAIL)
    assert "normalize_oracle.py" in fail_run.gate_reason
    assert {c.task_id for c in active_submits} == {"pass-task"}
    assert (prepared_root / "fail-task").is_dir()

    # The gate-failed end-state is distinguishable from the landed one on the
    # recorded runs (criterion #3 / 00050 #6).
    assert (pass_run.status, pass_run.gate) != (fail_run.status, fail_run.gate)


def test_plausible_wrong_committed_tree_fails_the_gate(tmp_path: Path) -> None:
    """A green-on-everything registration that grades nothing is a defect: the
    SAME registration + oracle that PASSes a correct tree must FAIL a
    plausible-wrong one, proving the oracle grades the committed content (#3
    edge case). Driven through ``orchestrate`` with the source built from config.
    """
    repo_root = tmp_path / "repo"
    held_out = repo_root / "held_out"
    oracle = _write_oracle(held_out)

    # Correct tree -> PASS.
    phase = repo_root / "tasks" / "active" / "01-phase"
    _write_task(phase, "task")
    write_oracle_registration(
        held_out, "task", oracle, interpreter=sys.executable
    )
    policy = _policy_with_held_out(repo_root, "held_out")
    source = worker.build_held_out_source(policy, repo_root)

    correct_submits: list[SubmitRequest] = []
    correct_report, _ = _drive(
        repo_root,
        source=source,
        submit_calls=correct_submits,
        solutions={"task": _CORRECT_SOLUTION},
    )
    assert correct_report.runs[0].gate is GateOutcome.PASS
    assert {c.task_id for c in correct_submits} == {"task"}

    # Same registration + oracle, a plausible-wrong committed tree -> FAIL.
    repo_root2 = tmp_path / "repo2"
    held_out2 = repo_root2 / "held_out"
    oracle2 = _write_oracle(held_out2)
    phase2 = repo_root2 / "tasks" / "active" / "01-phase"
    _write_task(phase2, "task")
    write_oracle_registration(
        held_out2, "task", oracle2, interpreter=sys.executable
    )
    policy2 = _policy_with_held_out(repo_root2, "held_out")
    source2 = worker.build_held_out_source(policy2, repo_root2)

    wrong_submits: list[SubmitRequest] = []
    wrong_report, _ = _drive(
        repo_root2,
        source=source2,
        submit_calls=wrong_submits,
        solutions={"task": _WRONG_SOLUTION},
    )
    assert wrong_report.runs[0].gate is GateOutcome.FAIL
    assert wrong_submits == []


# --- #5: the held-out payload never enters the agent's worktree ---------------


def test_registration_and_oracle_absent_from_worktree(tmp_path: Path) -> None:
    """While the gate is active with a registration and its oracle on disk at the
    configured root, neither file is present anywhere under the agent's worktree
    during the run (#5). The orchestrator reads the payload out of band; it never
    materializes it into the committed tree.
    """
    repo_root = tmp_path / "repo"
    phase = repo_root / "tasks" / "active" / "01-phase"
    _write_task(phase, "task")

    held_out = repo_root / "held_out"
    oracle = _write_oracle(held_out)
    registration = write_oracle_registration(
        held_out, "task", oracle, interpreter=sys.executable
    )

    policy = _policy_with_held_out(repo_root, "held_out")
    source = worker.build_held_out_source(policy, repo_root)

    submit_calls: list[SubmitRequest] = []
    report, prepared_root = _drive(
        repo_root,
        source=source,
        submit_calls=submit_calls,
        solutions={"task": _CORRECT_SOLUTION},
    )
    # The gate actually ran (otherwise worktree-absence is vacuous).
    assert report.runs[0].gate is GateOutcome.PASS
    assert {c.task_id for c in submit_calls} == {"task"}

    # The worktree (committed tree) the run used: only the agent's own files.
    sandbox = prepared_root / "task"
    files = [p for p in sandbox.rglob("*") if p.is_file()]
    names = {p.name for p in files}
    assert oracle.name not in names
    assert registration.name not in names

    # Neither payload file lives anywhere under the worktree subtree, and the
    # configured root is outside it (committed-pointer / git-ignored-payload).
    sandbox_prefix = str(sandbox.resolve()) + "/"
    assert not str(oracle.resolve()).startswith(sandbox_prefix)
    assert not str(registration.resolve()).startswith(sandbox_prefix)
    assert not str(held_out.resolve()).startswith(sandbox_prefix)

    # The oracle's body never leaked into any worktree file either.
    oracle_body = oracle.read_text(encoding="utf-8")
    for path in files:
        assert oracle_body not in path.read_text(encoding="utf-8", errors="ignore")

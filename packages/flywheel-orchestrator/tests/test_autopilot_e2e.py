"""End-to-end in-loop verification for autopilot (spec 00058, autopilot-e2e).

Grades acceptance criterion #7 (and ties it to #8): autopilot authors a task
whose authoritative grader is a fixture repo's *pre-existing* check, the REAL
orchestrate loop drives it from red to green with a scripted executing invoker,
and the verified branch FF-merges into the base. This is the phase's
in-loop-verification proof -- it drives the same orchestrate/submit path
production uses, not a unit of it.
"""

from __future__ import annotations

import asyncio
import json
import re
import subprocess
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
from flywheel_orchestrator import OrchestratorReport
from flywheel_orchestrator._autopilot import (
    GRADER_SOURCE_REPO_COMMAND,
    run_refill_pass,
)
from flywheel_worktree import worker


# --- git helpers (mirror flywheel-worktree's worker test harness) -----------


def _git(cwd: Path, *args: str) -> str:
    proc = subprocess.run(
        ["git", "-C", str(cwd), *args],
        text=True,
        capture_output=True,
        check=True,
    )
    return proc.stdout.strip()


def _rev(repo: Path, ref: str) -> str:
    return _git(repo, "rev-parse", ref)


#: A pre-existing, committed check that is RED until ``feature.py`` exists.
_VERIFY_SH = "test -f feature.py\n"


def _init_repo_with_failing_check(repo: Path) -> None:
    repo.mkdir(parents=True, exist_ok=True)
    _git(repo, "init", "-b", "main")
    _git(repo, "config", "user.email", "e2e-test@example.com")
    _git(repo, "config", "user.name", "e2e test")
    _git(repo, "config", "commit.gpgsign", "false")
    (repo / "verify.sh").write_text(_VERIFY_SH)
    (repo / "README.md").write_text("base\n")
    _git(repo, "add", "-A")
    _git(repo, "commit", "-m", "init with a failing check")


def _check_passes(repo: Path) -> bool:
    return subprocess.run(
        ["sh", "verify.sh"], cwd=repo, capture_output=True
    ).returncode == 0


# --- scripted invokers ------------------------------------------------------


def _authoring_response() -> str:
    body = {
        "tasks": [
            {
                "task": {
                    "id": "add-feature",
                    "goal": "The feature module exists so the repo check passes.",
                    "graders": [{"type": "command", "run": "sh verify.sh"}],
                    "tags": ["autopilot"],
                    "context": {},
                },
                "authoritative_grader": "sh verify.sh",
                "grader_source": GRADER_SOURCE_REPO_COMMAND,
                # The grader spines on the pre-existing committed check, not a
                # check this task's own diff creates (ties #7 to #8).
                "grader_target": "verify.sh",
                "creates_files": ["feature.py"],
                "assumptions": ["assume feature.py satisfies the check"],
            }
        ],
        "dropped": "",
    }
    return f"```json\n{json.dumps(body)}\n```"


def _make_autopilot_invoker():
    async def _invoke(prompt: str) -> str:
        tier_match = re.search(r"TIER: (\d+)", prompt)
        if tier_match is not None:
            tier_value = int(tier_match.group(1))
            if tier_value == 5:
                body = {
                    "relevant": True,
                    "reason": "the repo check is red; a feature is missing",
                    "findings": [
                        {
                            "id": "feature",
                            "title": "make the repo check pass",
                            "detail": "verify.sh is red",
                            "evidence": ["verify.sh:1"],
                            "urgency": 6,
                            "importance": 7,
                            "blocks": 0,
                            "effort": 1,
                            "ready": True,
                        }
                    ],
                }
            else:
                body = {"relevant": False, "reason": "n/a", "findings": []}
            return f"```json\n{json.dumps(body)}\n```"
        return _authoring_response()

    return _invoke


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


def _submitter(repo: Path) -> worker.GitWorktreeSubmitter:
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


def _run_autopilot_then_loop(
    repo: Path, *, fix: bool
) -> tuple[Path, OrchestratorReport]:
    """Author via autopilot, then drive the real loop with the executing agent."""
    tasks_dir = repo / ".flywheel" / "tasks"
    invoker = _make_autopilot_invoker()
    pass_result = asyncio.run(
        run_refill_pass(
            tasks_dir=tasks_dir,
            repo_root=repo,
            target_depth=3,
            discovery_invoker=invoker,
            authoring_invoker=invoker,
        )
    )
    assert pass_result.emitted_count == 1
    emitted = pass_result.emitted[0]
    # #8 tie: the authoritative grader is the fixture's pre-existing check.
    assert emitted.authoritative_grader == "sh verify.sh"
    assert (repo / emitted.grader_target).exists()

    worktrees_dir = repo / ".flywheel" / "worktrees"
    worktree = worktrees_dir / "add-feature"

    async def _exec(request: InvocationRequest) -> IterationResult:
        if fix:
            # Play the agent: create the missing feature and commit it, turning
            # the pre-existing check from red to green.
            (worktree / "feature.py").write_text("VALUE = 1\n")
            _git(worktree, "add", "-A")
            _git(worktree, "commit", "-m", "add feature")
        return IterationResult(
            transcript="ok",
            messages=_messages(),  # type: ignore[arg-type]
            envelope=ValidEnvelope(intent=Intent.VERIFY),
            signals=_signals(),
            failure=None,
        )

    report = worker.run_once(
        _submitter(repo),
        tasks_dir=tasks_dir,
        db_path=repo / ".flywheel" / "flywheel.sqlite",
        worktrees_dir=worktrees_dir,
        model=None,
        max_turns=4,
        max_retries=0,
        invoke=_exec,
    )
    return tasks_dir, report


def test_autopilot_task_lands_via_real_loop(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo_with_failing_check(repo)
    assert not _check_passes(repo)  # RED before the run

    base_before = _rev(repo, "main")
    _, report = _run_autopilot_then_loop(repo, fix=True)

    # The real loop drove the authored task to a green grade.
    assert [r.status for r in report.runs] == [Status.DONE]
    # Red -> green: the fixture's own pre-existing check now passes on the base.
    assert _check_passes(repo)
    # The verified branch FF-merged into the base (the base ref advanced).
    assert _rev(repo, "main") != base_before
    assert (repo / "feature.py").exists()


def test_unfixed_defect_does_not_land(tmp_path: Path) -> None:
    """The negative case: a scripted agent that does NOT fix the defect leaves
    the check red and the work must NOT FF-merge -- the grade gates the land."""
    repo = tmp_path / "repo"
    _init_repo_with_failing_check(repo)
    base_before = _rev(repo, "main")

    _, report = _run_autopilot_then_loop(repo, fix=False)

    assert report.runs  # the task ran
    assert report.runs[0].status != Status.DONE
    # The check stays red and nothing FF-merged into the base.
    assert not _check_passes(repo)
    assert _rev(repo, "main") == base_before
    assert not (repo / "feature.py").exists()

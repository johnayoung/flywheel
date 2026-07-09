"""Policy -> ``run_task_object`` threading for the attempt budgets.

The deadline/turn-budget mirror of ``test_sandbox_limit_threading.py``.
``_attempt_budget_primitives`` decomposes ``policy.deadlines`` (the top-level
``[deadlines]`` table, spec 00066) and
``policy.sandbox.limits.rubric_judge_max_turns`` into the plain kwargs the
orchestrator spreads into ``run_task_object`` -- exactly as
``_sandbox_limit_primitives`` does for the ceiling primitives. An absent
section, or a ``None`` policy (library callers), decomposes to the harness
defaults so a fast run stays byte-identical: a default :class:`DeadlineConfig`
and a ``None`` turn budget.
"""

from __future__ import annotations

import textwrap
from pathlib import Path

from flywheel_core.deadline_config import DeadlineConfig
from flywheel_orchestrator._orchestrate import _attempt_budget_primitives
from flywheel_orchestrator._policy import load_policy


def _policy(tmp_path: Path, body: str):
    p = tmp_path / "flywheel.toml"
    p.write_text(
        '[source]\nkind = "directory"\n' + textwrap.dedent(body),
        encoding="utf-8",
    )
    return load_policy(p)


def test_deadlines_decompose_from_policy(tmp_path: Path) -> None:
    pol = _policy(
        tmp_path,
        "[deadlines]\nagent_iteration_seconds = 120\nrubric_judge_seconds = 45\n",
    )
    deadlines = _attempt_budget_primitives(pol)["deadlines"]
    assert deadlines.agent_iteration_seconds == 120.0
    assert deadlines.rubric_judge_seconds == 45.0
    # A partial table keeps the finite defaults for the classes it omits.
    assert deadlines.command_grader_seconds == 900.0


def test_deadline_zero_decomposes_to_unbounded(tmp_path: Path) -> None:
    # ``0`` is the on-disk unbounded opt-out: the class resolves to None, not
    # 0.0 and not the class default.
    pol = _policy(tmp_path, "[deadlines]\nagent_iteration_seconds = 0\n")
    deadlines = _attempt_budget_primitives(pol)["deadlines"]
    assert deadlines.agent_iteration_seconds is None
    assert deadlines.command_grader_seconds == 900.0


def test_rubric_judge_max_turns_decomposes_from_policy(tmp_path: Path) -> None:
    pol = _policy(tmp_path, "[sandbox.limits]\nrubric_judge_max_turns = 8\n")
    assert _attempt_budget_primitives(pol)["rubric_judge_max_turns"] == 8


def test_absent_budgets_default_to_harness_defaults(tmp_path: Path) -> None:
    pol = _policy(tmp_path, "")
    prims = _attempt_budget_primitives(pol)
    assert prims["deadlines"] == DeadlineConfig()
    assert prims["rubric_judge_max_turns"] is None


def test_none_policy_decomposes_to_harness_defaults() -> None:
    # Library callers pass no policy; the fast default is the harness's own
    # finite default-on ceilings and unset (None) turn budget.
    prims = _attempt_budget_primitives(None)
    assert prims["deadlines"] == DeadlineConfig()
    assert prims["rubric_judge_max_turns"] is None

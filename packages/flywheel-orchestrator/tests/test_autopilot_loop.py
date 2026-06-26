"""Tests for the autopilot single refill pass (spec 00058, autopilot-loop).

Grades acceptance criterion #6: a single pass fills the work source up to the
target depth from actionable findings; when nothing is actionable it writes zero
tasks and returns cleanly. All agent calls go through the injectable invoker
seam, so the tests are deterministic and offline.
"""

from __future__ import annotations

import asyncio
import json
import re
from pathlib import Path

from flywheel_core.loaders import load_task_file
from flywheel_orchestrator._autopilot import (
    AUTOPILOT_PHASE,
    GRADER_SOURCE_REPO_COMMAND,
    recompute_final,
    run_refill_pass,
    ScoreBreakdown,
    Tier,
)
from flywheel_orchestrator._sources import DirectoryWorkSource


def _seed_repo(tmp_path: Path) -> Path:
    repo = tmp_path / "repo"
    (repo / "tests").mkdir(parents=True)
    (repo / "tests" / "test_existing.py").write_text("def test_ok():\n    pass\n")
    return repo


def _discovery_json(tier_value: int, *, relevant: bool, n_findings: int) -> str:
    if not relevant:
        body = {
            "relevant": False,
            "reason": f"tier {tier_value} does not apply",
            "findings": [],
        }
    else:
        body = {
            "relevant": True,
            "reason": f"tier {tier_value} applies",
            "findings": [
                {
                    "id": f"f{i}",
                    "title": f"cover-t{tier_value}-{i}",
                    "detail": "needs work",
                    "evidence": ["src/x.py:1"],
                    "urgency": 3,
                    "importance": 4,
                    "blocks": 0,
                    "effort": 1,
                    "ready": True,
                }
                for i in range(n_findings)
            ],
        }
    return f"```json\n{json.dumps(body)}\n```"


def _authoring_json(prompt: str) -> str:
    # Derive a unique task id from the finding title carried in the prompt.
    match = re.search(r"Finding \(Tier \d+, [^)]*\): (.+)", prompt)
    title = match.group(1).strip() if match else "autopilot-task"
    task_id = re.sub(r"[^a-z0-9-]+", "-", title.lower())
    body = {
        "tasks": [
            {
                "task": {
                    "id": task_id,
                    "goal": f"{title} is covered by an existing repo check.",
                    "graders": [
                        {"type": "command", "run": "pytest tests/test_existing.py"}
                    ],
                    "tags": ["autopilot"],
                    "context": {},
                },
                "authoritative_grader": "pytest tests/test_existing.py",
                "grader_source": GRADER_SOURCE_REPO_COMMAND,
                "grader_target": "tests/test_existing.py",
                "creates_files": [],
                "assumptions": ["assume pytest"],
            }
        ],
        "dropped": "",
    }
    return f"```json\n{json.dumps(body)}\n```"


def _make_invoker(relevant: dict[int, int]):
    """relevant maps tier value -> number of findings; absent tiers not-relevant."""

    async def _invoke(prompt: str) -> str:
        tier_match = re.search(r"TIER: (\d+)", prompt)
        if tier_match is not None:
            tier_value = int(tier_match.group(1))
            n = relevant.get(tier_value, 0)
            return _discovery_json(
                tier_value, relevant=tier_value in relevant, n_findings=n
            )
        return _authoring_json(prompt)

    return _invoke


def _active_count(tasks_dir: Path) -> int:
    return len(DirectoryWorkSource(tasks_dir).list_work())


# --- Criterion #6(a): refill an empty queue up to the target ----------------


def test_pass_fills_empty_queue_to_target_depth(tmp_path: Path) -> None:
    repo = _seed_repo(tmp_path)
    tasks_dir = tmp_path / "tasks"
    invoker = _make_invoker({5: 2, 8: 2})  # 4 actionable findings
    result = asyncio.run(
        run_refill_pass(
            tasks_dir=tasks_dir,
            repo_root=repo,
            target_depth=3,
            discovery_invoker=invoker,
            authoring_invoker=invoker,
        )
    )
    # Empty queue (0) + target 3 -> exactly 3 tasks emitted (the 4th finding is
    # left for the next run).
    assert result.emitted_count == 3
    assert _active_count(tasks_dir) == 3
    # Every emitted file loads through the authoritative validator with a grader.
    for path in result.emitted_paths:
        assert path.parent.name == AUTOPILOT_PHASE
        task = load_task_file(path)
        assert task.graders


def test_pass_caps_at_available_findings_when_fewer_than_target(
    tmp_path: Path,
) -> None:
    repo = _seed_repo(tmp_path)
    tasks_dir = tmp_path / "tasks"
    invoker = _make_invoker({5: 2})  # only 2 findings, target 10
    result = asyncio.run(
        run_refill_pass(
            tasks_dir=tasks_dir,
            repo_root=repo,
            target_depth=10,
            discovery_invoker=invoker,
            authoring_invoker=invoker,
        )
    )
    assert result.emitted_count == 2
    assert _active_count(tasks_dir) == 2


# --- Criterion #6(b): clean repo emits nothing and returns cleanly ----------


def test_clean_repo_emits_zero_tasks_and_returns_clean(tmp_path: Path) -> None:
    repo = _seed_repo(tmp_path)
    tasks_dir = tmp_path / "tasks"
    invoker = _make_invoker({})  # every tier not-relevant / no findings
    result = asyncio.run(
        run_refill_pass(
            tasks_dir=tasks_dir,
            repo_root=repo,
            target_depth=5,
            discovery_invoker=invoker,
            authoring_invoker=invoker,
        )
    )
    assert result.emitted_count == 0
    assert result.dropped == ()
    assert _active_count(tasks_dir) == 0
    # All 11 tiers recorded a not-relevant verdict.
    assert len(result.not_relevant_tiers) == 11


# --- Edge: queue already at/above target ------------------------------------


def test_queue_at_target_emits_nothing(tmp_path: Path) -> None:
    repo = _seed_repo(tmp_path)
    tasks_dir = tmp_path / "tasks"
    # Pre-seed the queue to the target depth.
    phase = tasks_dir / "active" / "existing"
    phase.mkdir(parents=True)
    for i in range(3):
        (phase / f"pre{i}.json").write_text(
            json.dumps(
                {
                    "id": f"pre{i}",
                    "goal": f"Pre-existing task {i}.",
                    "graders": [{"type": "command", "run": "true"}],
                }
            )
        )
    invoker = _make_invoker({5: 5})
    result = asyncio.run(
        run_refill_pass(
            tasks_dir=tasks_dir,
            repo_root=repo,
            target_depth=3,
            discovery_invoker=invoker,
            authoring_invoker=invoker,
        )
    )
    assert result.emitted_count == 0
    assert "at or above target" in result.reason


# --- Criterion #3 persisted: recorded breakdown recomputes ------------------


def test_emitted_file_records_a_recomputable_score_breakdown(
    tmp_path: Path,
) -> None:
    repo = _seed_repo(tmp_path)
    tasks_dir = tmp_path / "tasks"
    invoker = _make_invoker({8: 1})
    result = asyncio.run(
        run_refill_pass(
            tasks_dir=tasks_dir,
            repo_root=repo,
            target_depth=5,
            discovery_invoker=invoker,
            authoring_invoker=invoker,
        )
    )
    assert result.emitted_count == 1
    data = json.loads(result.emitted_paths[0].read_text())
    ap = data["autopilot"]
    # The recorded components recompute to the recorded final under the formula.
    bd = ScoreBreakdown(
        tier=Tier(ap["tier"]),
        tier_weight=ap["tier_weight"],
        urgency=ap["urgency"],
        importance=ap["importance"],
        blocks=ap["blocks"],
        effort=ap["effort"],
        final=ap["final"],
        preemptive=ap["preemptive"],
    )
    assert recompute_final(bd) == ap["final"]
    # Priority derives from the final score so the scheduler orders the work.
    assert data["priority"] == round(ap["final"])

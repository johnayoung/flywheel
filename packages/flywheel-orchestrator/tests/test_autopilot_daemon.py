"""Tests for the neverending autopilot daemon loop (spec 00058, autopilot-daemon).

Grades acceptance criterion #9 and decision D-5: bare ``flywheel autopilot``
runs repeated refill passes on its interval, idles (does not exit) on an empty
cycle, resumes authoring on a later cycle, and stops only on an explicit stop
signal. The loop's collaborators are injected, so the test runs with no real
wall-clock waits and no live model.
"""

from __future__ import annotations

import asyncio
import json
import re
from pathlib import Path

from flywheel_orchestrator._autopilot import AutopilotPassResult, run_refill_pass
from flywheel_orchestrator._autopilot_run import run_daemon_loop


def _seed_repo(tmp_path: Path) -> Path:
    repo = tmp_path / "repo"
    (repo / "tests").mkdir(parents=True)
    (repo / "tests" / "test_existing.py").write_text("def test_ok():\n    pass\n")
    return repo


def _discovery_json(tier_value: int, *, relevant: bool, n: int) -> str:
    if not relevant:
        body = {"relevant": False, "reason": "n/a", "findings": []}
    else:
        body = {
            "relevant": True,
            "reason": "applies",
            "findings": [
                {
                    "id": f"f{i}",
                    "title": f"cover-t{tier_value}-{i}",
                    "urgency": 3,
                    "importance": 4,
                    "effort": 1,
                    "ready": True,
                }
                for i in range(n)
            ],
        }
    return f"```json\n{json.dumps(body)}\n```"


def _authoring_json(prompt: str) -> str:
    match = re.search(r"Finding \(Tier \d+, [^)]*\): (.+)", prompt)
    title = match.group(1).strip() if match else "autopilot-task"
    task_id = re.sub(r"[^a-z0-9-]+", "-", title.lower())
    body = {
        "tasks": [
            {
                "task": {
                    "id": task_id,
                    "goal": f"{title} covered.",
                    "graders": [
                        {"type": "command", "run": "pytest tests/test_existing.py"}
                    ],
                },
                "authoritative_grader": "pytest tests/test_existing.py",
                "grader_source": "repo_command",
                "grader_target": "tests/test_existing.py",
                "creates_files": [],
            }
        ],
        "dropped": "",
    }
    return f"```json\n{json.dumps(body)}\n```"


def test_idle_cycle_does_not_exit_and_later_cycle_authors(tmp_path: Path) -> None:
    """Cycle 1 finds nothing (idle); cycle 2 finds work; stop after cycle 2."""
    repo = _seed_repo(tmp_path)
    tasks_dir = tmp_path / "tasks"
    cycle_state = {"productive": False}

    async def invoker(prompt: str) -> str:
        tier_match = re.search(r"TIER: (\d+)", prompt)
        if tier_match is not None:
            tier_value = int(tier_match.group(1))
            if cycle_state["productive"] and tier_value == 5:
                return _discovery_json(5, relevant=True, n=1)
            return _discovery_json(tier_value, relevant=False, n=0)
        return _authoring_json(prompt)

    results: list[AutopilotPassResult] = []

    def run_cycle() -> AutopilotPassResult:
        res = asyncio.run(
            run_refill_pass(
                tasks_dir=tasks_dir,
                repo_root=repo,
                target_depth=5,
                discovery_invoker=invoker,
                authoring_invoker=invoker,
            )
        )
        results.append(res)
        # After the first (idle) cycle, the repo gains actionable work.
        cycle_state["productive"] = True
        return res

    stop_state = {"stop": False}
    counter = {"n": 0}

    def should_stop() -> bool:
        return stop_state["stop"]

    def fake_sleep(seconds: float, cb) -> None:
        counter["n"] += 1
        if counter["n"] >= 2:
            stop_state["stop"] = True

    cycles = run_daemon_loop(
        run_cycle=run_cycle,
        interval_seconds=0.0,
        should_stop=should_stop,
        sleep=fake_sleep,
        max_cycles=10,
    )

    assert cycles >= 2
    assert results[0].emitted_count == 0  # idle cycle wrote nothing
    assert any(r.emitted_count >= 1 for r in results[1:])  # later cycle authored


def test_loop_runs_until_stop_signal_not_until_empty() -> None:
    """An always-empty pass keeps looping; only the stop signal ends it."""
    empty_calls = {"n": 0}

    def run_cycle() -> AutopilotPassResult:
        empty_calls["n"] += 1
        return AutopilotPassResult(reason="idle")

    stop_state = {"stop": False}

    def should_stop() -> bool:
        return stop_state["stop"]

    def fake_sleep(seconds: float, cb) -> None:
        if empty_calls["n"] >= 3:
            stop_state["stop"] = True

    cycles = run_daemon_loop(
        run_cycle=run_cycle,
        interval_seconds=0.0,
        should_stop=should_stop,
        sleep=fake_sleep,
        max_cycles=50,
    )
    assert cycles >= 3
    assert empty_calls["n"] == cycles


def test_stop_before_first_cycle_runs_nothing() -> None:
    def run_cycle() -> AutopilotPassResult:  # pragma: no cover - must not run
        raise AssertionError("no cycle should run when stop is set up front")

    cycles = run_daemon_loop(
        run_cycle=run_cycle,
        interval_seconds=0.0,
        should_stop=lambda: True,
        sleep=lambda s, cb: None,
    )
    assert cycles == 0

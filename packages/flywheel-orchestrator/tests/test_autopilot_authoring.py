"""Tests for headless autopilot authoring (spec 00058, autopilot-authoring).

Grades acceptance criteria #1 (every emitted task loads as a valid core Task
with at least one grader) and #8 (the authoritative grader is a pre-existing
repo check or a held-out oracle, never a check the same task's own diff
creates). The authoring agent is driven through the injectable invoker seam, so
a scripted invoker makes the test deterministic and offline.
"""

from __future__ import annotations

import asyncio
import json
from pathlib import Path

from flywheel_core.loaders import load_task_file, serialize_task
from flywheel_orchestrator._autopilot import (
    GRADER_SOURCE_HELD_OUT,
    GRADER_SOURCE_REPO_COMMAND,
    Finding,
    Tier,
    author_finding,
)


def _finding(fid: str = "f1", tier: Tier = Tier.TEST_COVERAGE) -> Finding:
    return Finding(
        id=fid,
        tier=tier,
        title="add coverage for the parser",
        detail="the parser module has no tests",
        evidence=("src/parser.py:1",),
        urgency=3,
        importance=5,
        effort=2,
    )


def _scripted(response: str):
    async def _invoke(prompt: str) -> str:
        return response
    return _invoke


def _fenced(body: dict) -> str:
    return f"```json\n{json.dumps(body)}\n```"


def _seed_repo(tmp_path: Path) -> Path:
    """A fixture repo with a pre-existing committed check the grader can point at."""
    repo = tmp_path / "repo"
    (repo / "tests").mkdir(parents=True)
    (repo / "tests" / "test_existing.py").write_text("def test_ok():\n    pass\n")
    return repo


# --- Criterion #1: emitted tasks load and carry a grader --------------------


def test_emitted_task_loads_and_carries_grader(tmp_path: Path) -> None:
    repo = _seed_repo(tmp_path)
    response = _fenced(
        {
            "tasks": [
                {
                    "task": {
                        "id": "test-parser",
                        "goal": "The parser module is covered by tests.",
                        "graders": [
                            {"type": "command", "run": "pytest tests/test_existing.py"}
                        ],
                        "tags": ["autopilot"],
                        "context": {},
                    },
                    "authoritative_grader": "pytest tests/test_existing.py",
                    "grader_source": GRADER_SOURCE_REPO_COMMAND,
                    "grader_target": "tests/test_existing.py",
                    "creates_files": ["src/parser_test_helpers.py"],
                    "assumptions": ["assume pytest is the runner"],
                }
            ],
            "dropped": "",
        }
    )
    result = asyncio.run(
        author_finding(_finding(), repo_root=repo, invoker=_scripted(response))
    )
    assert len(result.emitted) == 1
    assert result.dropped == ()
    emitted = result.emitted[0]
    assert emitted.task.graders  # non-empty
    assert emitted.assumptions == ("assume pytest is the runner",)

    # Round-trip through the authoritative validator: serialize and re-load via
    # load_task_file (criterion #1's grader), proving it is a real Task file.
    out = tmp_path / "emitted.json"
    out.write_text(json.dumps(serialize_task(emitted.task)))
    reloaded = load_task_file(out)
    assert reloaded.graders


# --- Criterion #8: authoritative grader is out-of-band, not self-authored ---


def test_grader_target_must_pre_exist_in_repo(tmp_path: Path) -> None:
    repo = _seed_repo(tmp_path)
    response = _fenced(
        {
            "tasks": [
                {
                    "task": {
                        "id": "test-parser",
                        "goal": "The parser is covered.",
                        "graders": [
                            {"type": "command", "run": "pytest tests/test_new.py"}
                        ],
                    },
                    "authoritative_grader": "pytest tests/test_new.py",
                    "grader_source": GRADER_SOURCE_REPO_COMMAND,
                    # This target does NOT exist before the run -> drop.
                    "grader_target": "tests/test_new.py",
                    "creates_files": [],
                }
            ],
            "dropped": "",
        }
    )
    result = asyncio.run(
        author_finding(_finding(), repo_root=repo, invoker=_scripted(response))
    )
    assert result.emitted == ()
    assert len(result.dropped) == 1
    assert "pre-exist" in result.dropped[0].reason


def test_grader_naming_a_diff_created_file_is_rejected(tmp_path: Path) -> None:
    repo = _seed_repo(tmp_path)
    # The grader runs a test file the same task's diff creates -> self-attestation.
    (repo / "tests" / "test_self.py").write_text("def test_x():\n    pass\n")
    response = _fenced(
        {
            "tasks": [
                {
                    "task": {
                        "id": "test-parser",
                        "goal": "The parser is covered.",
                        "graders": [
                            {"type": "command", "run": "pytest tests/test_self.py"}
                        ],
                    },
                    "authoritative_grader": "pytest tests/test_self.py",
                    "grader_source": GRADER_SOURCE_REPO_COMMAND,
                    "grader_target": "tests/test_self.py",
                    # The task's own diff creates the very file it grades against.
                    "creates_files": ["tests/test_self.py"],
                }
            ],
            "dropped": "",
        }
    )
    result = asyncio.run(
        author_finding(_finding(), repo_root=repo, invoker=_scripted(response))
    )
    assert result.emitted == ()
    assert len(result.dropped) == 1
    assert "self-attestation" in result.dropped[0].reason


def test_held_out_oracle_is_an_acceptable_authoritative_source(
    tmp_path: Path,
) -> None:
    repo = _seed_repo(tmp_path)
    oracle = tmp_path / "held-out" / "oracle.py"
    oracle.parent.mkdir(parents=True)
    oracle.write_text("import sys\nsys.exit(0)\n")
    response = _fenced(
        {
            "tasks": [
                {
                    "task": {
                        "id": "test-parser",
                        "goal": "The parser is covered.",
                        "graders": [
                            {"type": "command", "run": f"python3 {oracle}"}
                        ],
                    },
                    "authoritative_grader": f"python3 {oracle}",
                    "grader_source": GRADER_SOURCE_HELD_OUT,
                    "grader_target": str(oracle),
                    "creates_files": [],
                }
            ],
            "dropped": "",
        }
    )
    result = asyncio.run(
        author_finding(_finding(), repo_root=repo, invoker=_scripted(response))
    )
    assert len(result.emitted) == 1
    assert result.emitted[0].grader_source == GRADER_SOURCE_HELD_OUT
    assert result.emitted[0].held_out_oracle_path == str(oracle)


# --- Drops ------------------------------------------------------------------


def test_grader_less_task_is_dropped_not_emitted(tmp_path: Path) -> None:
    repo = _seed_repo(tmp_path)
    response = _fenced(
        {
            "tasks": [
                {
                    "task": {
                        "id": "noop",
                        "goal": "Do a thing.",
                        "graders": [],
                    },
                    "authoritative_grader": "pytest tests/test_existing.py",
                    "grader_source": GRADER_SOURCE_REPO_COMMAND,
                    "grader_target": "tests/test_existing.py",
                }
            ],
            "dropped": "",
        }
    )
    result = asyncio.run(
        author_finding(_finding(), repo_root=repo, invoker=_scripted(response))
    )
    assert result.emitted == ()
    assert len(result.dropped) == 1


def test_finding_with_no_expressible_grader_is_dropped_with_reason(
    tmp_path: Path,
) -> None:
    repo = _seed_repo(tmp_path)
    response = _fenced(
        {
            "tasks": [],
            "dropped": "no out-of-band check exists for this cosmetic finding",
        }
    )
    result = asyncio.run(
        author_finding(_finding(), repo_root=repo, invoker=_scripted(response))
    )
    assert result.emitted == ()
    assert len(result.dropped) == 1
    assert "cosmetic" in result.dropped[0].reason


def test_authoritative_grader_must_be_a_grader_on_the_task(tmp_path: Path) -> None:
    repo = _seed_repo(tmp_path)
    response = _fenced(
        {
            "tasks": [
                {
                    "task": {
                        "id": "test-parser",
                        "goal": "The parser is covered.",
                        "graders": [
                            {"type": "command", "run": "pytest tests/test_existing.py"}
                        ],
                    },
                    # Names a grader that is not on the task.
                    "authoritative_grader": "pytest tests/other.py",
                    "grader_source": GRADER_SOURCE_REPO_COMMAND,
                    "grader_target": "tests/test_existing.py",
                }
            ],
            "dropped": "",
        }
    )
    result = asyncio.run(
        author_finding(_finding(), repo_root=repo, invoker=_scripted(response))
    )
    assert result.emitted == ()
    assert "authoritative grader is not a command grader" in result.dropped[0].reason


def test_unparseable_authoring_response_drops_the_finding(tmp_path: Path) -> None:
    repo = _seed_repo(tmp_path)
    result = asyncio.run(
        author_finding(
            _finding(), repo_root=repo, invoker=_scripted("no json here")
        )
    )
    assert result.emitted == ()
    assert "unparseable" in result.dropped[0].reason


def test_a_single_finding_may_compile_to_multiple_tasks(tmp_path: Path) -> None:
    repo = _seed_repo(tmp_path)
    (repo / "tests" / "test_other.py").write_text("def test_y():\n    pass\n")
    entry_a = {
        "task": {
            "id": "task-a",
            "goal": "Cover module A.",
            "graders": [{"type": "command", "run": "pytest tests/test_existing.py"}],
        },
        "authoritative_grader": "pytest tests/test_existing.py",
        "grader_source": GRADER_SOURCE_REPO_COMMAND,
        "grader_target": "tests/test_existing.py",
    }
    entry_b = {
        "task": {
            "id": "task-b",
            "goal": "Cover module B.",
            "prerequisites": ["task-a"],
            "graders": [{"type": "command", "run": "pytest tests/test_other.py"}],
        },
        "authoritative_grader": "pytest tests/test_other.py",
        "grader_source": GRADER_SOURCE_REPO_COMMAND,
        "grader_target": "tests/test_other.py",
    }
    response = _fenced({"tasks": [entry_a, entry_b], "dropped": ""})
    result = asyncio.run(
        author_finding(_finding(), repo_root=repo, invoker=_scripted(response))
    )
    assert len(result.emitted) == 2
    # Each independently satisfies criteria #1 and #8.
    assert all(e.task.graders for e in result.emitted)
    assert all(
        (repo / e.grader_target).exists() for e in result.emitted
    )

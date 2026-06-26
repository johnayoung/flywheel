"""Standing-invariant held-out oracles (the missing piece for autopilot).

Per-task-id held-out registrations cannot protect a standing invariant across
autopilot's auto-generated task ids. A standing oracle under ``<root>/_standing/``
re-asserts an invariant on every task that matches its predicate (tags), ANDed
into the gate verdict and graded out-of-band against the committed tree.
"""

from __future__ import annotations

import json
from pathlib import Path

from flywheel_core.task import CommandGrader, Task
from flywheel_orchestrator._held_out_gate import (
    STANDING_SUBDIR,
    FilesystemHeldOutGraderSource,
    GateOutcome,
    evaluate_held_out_gate,
    write_standing_oracle_registration,
)


def _task(task_id: str, *, tags: list[str] | None = None) -> Task:
    return Task(
        goal=f"goal for {task_id}.",
        graders=[CommandGrader(run="true")],
        id=task_id,
        tags=tags or [],
    )


def _write_standing(
    root: Path, name: str, *, run: str, match: dict | None
) -> None:
    standing = root / STANDING_SUBDIR
    standing.mkdir(parents=True, exist_ok=True)
    body: dict = {"graders": [{"type": "command", "run": run, "name": name}]}
    if match is not None:
        body["match"] = match
    (standing / f"{name}.json").write_text(json.dumps(body))


def _evaluate(root: Path, task: Task, tmp_path: Path):
    return evaluate_held_out_gate(
        task,
        FilesystemHeldOutGraderSource(root=root),
        committed_tree=tmp_path,
        run_id="r1",
    )


# --- tag-scoped standing oracle ---------------------------------------------


def test_standing_oracle_applies_to_a_matching_tagged_task(tmp_path: Path) -> None:
    root = tmp_path / "held-out"
    _write_standing(root, "netting", run="true", match={"tags": ["netting"]})
    verdict = _evaluate(root, _task("gh-42", tags=["netting"]), tmp_path)
    assert verdict.outcome is GateOutcome.PASS


def test_standing_oracle_blocks_a_matching_task_when_it_fails(tmp_path: Path) -> None:
    root = tmp_path / "held-out"
    _write_standing(root, "netting", run="false", match={"tags": ["netting"]})
    verdict = _evaluate(root, _task("gh-42", tags=["netting"]), tmp_path)
    assert verdict.outcome is GateOutcome.FAIL
    assert verdict.blocks_landing


def test_standing_oracle_does_not_apply_to_an_unmatched_task(tmp_path: Path) -> None:
    root = tmp_path / "held-out"
    _write_standing(root, "netting", run="false", match={"tags": ["netting"]})
    # The task carries no matching tag and has no per-task registration -> NO_GATE.
    verdict = _evaluate(root, _task("gh-99", tags=["docs"]), tmp_path)
    assert verdict.outcome is GateOutcome.NO_GATE


# --- global standing oracle (no match) --------------------------------------


def test_global_standing_oracle_applies_to_every_task(tmp_path: Path) -> None:
    root = tmp_path / "held-out"
    _write_standing(root, "suite", run="true", match=None)  # no match -> global
    assert _evaluate(root, _task("any", tags=[]), tmp_path).outcome is GateOutcome.PASS
    _write_standing(root, "suite", run="false", match={})  # empty match -> global
    assert _evaluate(root, _task("any", tags=[]), tmp_path).blocks_landing


# --- standing ANDs with the per-task-id registration ------------------------


def test_standing_and_per_task_oracle_both_must_pass(tmp_path: Path) -> None:
    root = tmp_path / "held-out"
    root.mkdir(parents=True)
    # Per-task-id oracle (passes) + a standing oracle (fails) -> the land is
    # blocked: both are ANDed.
    (root / "gh-42.json").write_text(
        json.dumps({"graders": [{"type": "command", "run": "true"}]})
    )
    _write_standing(root, "netting", run="false", match={"tags": ["netting"]})
    verdict = _evaluate(root, _task("gh-42", tags=["netting"]), tmp_path)
    assert verdict.outcome is GateOutcome.FAIL


# --- fail-closed on a malformed standing oracle -----------------------------


def test_malformed_standing_match_fails_closed(tmp_path: Path) -> None:
    root = tmp_path / "held-out"
    _write_standing(
        root, "bad", run="true", match={"unknown_predicate": 1}
    )  # match present but no valid tags list
    verdict = _evaluate(root, _task("gh-1", tags=["netting"]), tmp_path)
    assert verdict.outcome is GateOutcome.FAIL
    assert "failed closed" in verdict.reason


def test_standing_non_command_grader_fails_closed(tmp_path: Path) -> None:
    root = tmp_path / "held-out"
    standing = root / STANDING_SUBDIR
    standing.mkdir(parents=True)
    (standing / "bad.json").write_text(
        json.dumps(
            {
                "match": {"tags": ["netting"]},
                "graders": [{"type": "manual", "instruction": "look at it"}],
            }
        )
    )
    verdict = _evaluate(root, _task("gh-1", tags=["netting"]), tmp_path)
    assert verdict.outcome is GateOutcome.FAIL


# --- the writer helper round-trips through the source -----------------------


def test_write_standing_oracle_registration_round_trips(tmp_path: Path) -> None:
    root = tmp_path / "held-out"
    oracle = tmp_path / "netting_oracle.py"
    oracle.write_text("import sys; sys.exit(0)\n")
    path = write_standing_oracle_registration(
        root, "netting", oracle, tags=("netting",), interpreter="python3"
    )
    assert path == root / STANDING_SUBDIR / "netting.json"
    # The written file applies to a netting-tagged task and runs the oracle.
    source = FilesystemHeldOutGraderSource(root=root)
    graders = source.standing_graders_for(_task("gh-7", tags=["netting"]))
    assert len(graders) == 1 and str(oracle) in graders[0].run
    # And not to an unmatched task.
    assert source.standing_graders_for(_task("gh-8", tags=["docs"])) == []


def test_no_standing_dir_yields_no_standing_graders(tmp_path: Path) -> None:
    root = tmp_path / "held-out"
    root.mkdir()
    source = FilesystemHeldOutGraderSource(root=root)
    assert source.standing_graders_for(_task("gh-1", tags=["netting"])) == []

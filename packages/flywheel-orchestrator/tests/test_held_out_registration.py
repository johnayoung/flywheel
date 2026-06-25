"""Tests for the held-out oracle registration shape (spec 00051, D-2/D-5).

The registration shape is how an admitted ``fw-verify`` oracle becomes a
``<root>/<task_id>.json`` held-out *command* grader that 00050's
``FilesystemHeldOutGraderSource`` loads unchanged. These tests grade three
properties the shape must hold:

* #4 — a registration the shape produces loads via
  ``FilesystemHeldOutGraderSource.graders_for`` without raising and yields
  command graders; a malformed / non-command registration still fails closed.
* #5 — run through ``evaluate_held_out_gate``, the oracle reproduces its
  authoring-time discrimination: PASS on a committed tree with a correct
  reference, FAIL on a committed tree with a plausible-wrong reference.
* #6 — the oracle is referenced by an absolute path OUTSIDE the committed tree
  yet is evaluated with the committed tree as ``cwd``, so the verdict depends on
  the committed content, not on the oracle's own directory.
"""

from __future__ import annotations

import sys
from pathlib import Path

import pytest

from flywheel_core.task import CommandGrader, Task
from flywheel_orchestrator import (
    FilesystemHeldOutGraderSource,
    GateOutcome,
    HeldOutGraderError,
    build_oracle_registration,
    evaluate_held_out_gate,
    write_oracle_registration,
)

# An oracle authored blind by fw-verify: run with the committed tree as cwd, it
# imports the committed module and asserts a discriminating relation. It loads
# the agent's committed code by putting the cwd (the committed tree) on the
# import path — the oracle file itself lives outside that tree, so an absolute
# path is the only way the gate can reach it (D-2).
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


def _task(task_id: str = "task-alpha") -> Task:
    return Task(
        goal="do the work",
        graders=[CommandGrader(run="true", name="in-run")],
        id=task_id,
    )


def _write_oracle(directory: Path) -> Path:
    """Author the oracle at an operator path OUTSIDE any committed tree."""
    directory.mkdir(parents=True, exist_ok=True)
    oracle = directory / "normalize_oracle.py"
    oracle.write_text(_ORACLE_SOURCE, encoding="utf-8")
    return oracle


def _committed_tree(directory: Path, solution: str) -> Path:
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "solution.py").write_text(solution, encoding="utf-8")
    return directory


# --- #4: the produced registration round-trips through 00050's source --------


def test_build_registration_yields_loadable_command_grader(tmp_path: Path) -> None:
    """The shape produces a registration that ``graders_for`` loads without
    raising and that is a command grader invoking the oracle by absolute path."""
    oracle = _write_oracle(tmp_path / "oracle_dir")
    root = tmp_path / "held_out"

    written = write_oracle_registration(
        root,
        "task-alpha",
        oracle,
        interpreter=sys.executable,
    )
    assert written == (root / "task-alpha.json").resolve()

    source = FilesystemHeldOutGraderSource(root=root)
    graders = source.graders_for("task-alpha")

    assert graders is not None
    assert len(graders) == 1
    grader = graders[0]
    assert isinstance(grader, CommandGrader)
    # The oracle is invoked by its absolute operator path (D-2).
    assert str(oracle) in grader.run
    assert Path(oracle).is_absolute()


def test_build_registration_rejects_relative_oracle_path(tmp_path: Path) -> None:
    """A relative oracle path is refused at build time: under cwd = committed
    tree it would resolve into the worktree and fail closed on every run (D-2)."""
    with pytest.raises(ValueError, match="absolute"):
        build_oracle_registration("oracle_dir/normalize_oracle.py")


def test_non_command_registration_at_task_path_fails_closed(tmp_path: Path) -> None:
    """A hand-written non-command registration at the task-id path still fails
    closed through the existing source (the 00050 command-only contract, #4)."""
    root = tmp_path / "held_out"
    root.mkdir()
    (root / "task-alpha.json").write_text(
        '[{"type": "rubric", "assertions": ["looks ok"], "name": "judge"}]',
        encoding="utf-8",
    )
    source = FilesystemHeldOutGraderSource(root=root)
    with pytest.raises(HeldOutGraderError):
        source.graders_for("task-alpha")


def test_write_registration_refuses_path_traversal(tmp_path: Path) -> None:
    oracle = _write_oracle(tmp_path / "oracle_dir")
    root = tmp_path / "held_out"
    with pytest.raises(ValueError, match="traversal"):
        write_oracle_registration(root, "../escape", oracle, interpreter="python3")


# --- #5 / #6: discrimination reproduced through the gate, cwd = committed -----


def test_registration_reproduces_discrimination_through_gate(
    tmp_path: Path,
) -> None:
    """The same kill-and-pass the oracle proved at authoring time holds through
    the execute-time gate: a correct committed tree PASSes, a plausible-wrong one
    FAILs (#5). The oracle file lives outside both committed trees (#6)."""
    oracle = _write_oracle(tmp_path / "oracle_dir")
    root = tmp_path / "held_out"
    write_oracle_registration(root, "task-alpha", oracle, interpreter=sys.executable)
    source = FilesystemHeldOutGraderSource(root=root)

    correct_tree = _committed_tree(tmp_path / "correct", _CORRECT_SOLUTION)
    wrong_tree = _committed_tree(tmp_path / "wrong", _WRONG_SOLUTION)

    # The oracle source is outside every committed tree it grades (#6).
    assert oracle.resolve().parent != correct_tree.resolve()
    assert oracle.resolve().parent != wrong_tree.resolve()
    assert not str(oracle.resolve()).startswith(str(correct_tree.resolve()) + "/")
    assert not str(oracle.resolve()).startswith(str(wrong_tree.resolve()) + "/")

    passing = evaluate_held_out_gate(
        _task(),
        source,
        committed_tree=correct_tree,
        run_id="run-correct",
    )
    assert passing.outcome is GateOutcome.PASS, passing.reason

    failing = evaluate_held_out_gate(
        _task(),
        source,
        committed_tree=wrong_tree,
        run_id="run-wrong",
    )
    assert failing.outcome is GateOutcome.FAIL
    assert failing.blocks_landing


def test_verdict_tracks_committed_tree_not_oracle_directory(
    tmp_path: Path,
) -> None:
    """The gate's cwd is the committed tree, not the oracle's directory: a
    correct and a wrong tree (same registration, same oracle) yield opposite
    verdicts, proving the oracle imports the COMMITTED code, not something beside
    itself. If cwd were wrong the oracle would import nothing and pass-on-all,
    losing discrimination (#5/#6 edge case)."""
    oracle = _write_oracle(tmp_path / "oracle_dir")
    # A decoy solution beside the oracle would be imported instead of the
    # committed one if the gate ran with the oracle's directory as cwd. It is a
    # WRONG implementation, so a cwd bug would surface as a spurious FAIL on the
    # correct committed tree.
    (oracle.parent / "solution.py").write_text(_WRONG_SOLUTION, encoding="utf-8")

    root = tmp_path / "held_out"
    write_oracle_registration(root, "task-alpha", oracle, interpreter=sys.executable)
    source = FilesystemHeldOutGraderSource(root=root)

    correct_tree = _committed_tree(tmp_path / "correct", _CORRECT_SOLUTION)
    verdict = evaluate_held_out_gate(
        _task(),
        source,
        committed_tree=correct_tree,
        run_id="run-cwd",
    )

    # PASS only if the committed (correct) solution was imported — i.e. cwd was
    # the committed tree, not the oracle's directory (which holds a wrong decoy).
    assert verdict.outcome is GateOutcome.PASS, verdict.reason

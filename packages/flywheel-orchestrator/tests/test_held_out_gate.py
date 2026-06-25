"""Tests for the held-out landing gate engine (spec 00050, D-2..D-5).

The engine loads a task's operator-declared held-out command graders from a
source the agent never receives, runs them against the committed result
out-of-band, and computes a fail-closed pass/fail landing verdict. These tests
grade the engine's verdict only; wiring it into the per-task drive is a
separate task.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from flywheel_core.task import CommandGrader, Task
from flywheel_orchestrator import (
    FilesystemHeldOutGraderSource,
    GateOutcome,
    HeldOutGraderError,
    HeldOutGraderSource,
    evaluate_held_out_gate,
)


def _task(task_id: str = "task-alpha") -> Task:
    """A minimal agent-facing task. Its own graders are irrelevant to the gate;
    the gate reads held-out graders from the source, never from the task."""
    return Task(
        goal="do the work",
        graders=[CommandGrader(run="true", name="in-run")],
        id=task_id,
    )


def _register(root: Path, task_id: str, entries: object) -> Path:
    path = root / f"{task_id}.json"
    path.write_text(json.dumps(entries), encoding="utf-8")
    return path


# --- source: registration presence vs absence -------------------------------


def test_no_registration_yields_no_gate(tmp_path: Path) -> None:
    """A task with no held-out file registered yields NO_GATE, distinct from
    FAIL, so the integration layer lands it unchanged (D-7)."""
    source = FilesystemHeldOutGraderSource(root=tmp_path)
    committed = tmp_path / "tree"
    committed.mkdir()

    verdict = evaluate_held_out_gate(
        _task(),
        source,
        committed_tree=committed,
        run_id="run-1",
    )

    assert verdict.outcome is GateOutcome.NO_GATE
    assert not verdict.blocks_landing
    assert not verdict.passed
    assert verdict.results == ()


def test_source_returns_none_for_unregistered_task(tmp_path: Path) -> None:
    source = FilesystemHeldOutGraderSource(root=tmp_path)
    assert source.graders_for("nobody-home") is None


# --- pass path --------------------------------------------------------------


def test_all_passing_held_out_graders_yield_pass(tmp_path: Path) -> None:
    committed = tmp_path / "tree"
    committed.mkdir()
    root = tmp_path / "held_out"
    root.mkdir()
    _register(
        root,
        "task-alpha",
        [
            {"type": "command", "run": "true", "name": "first"},
            {"type": "command", "run": "exit 0", "name": "second"},
        ],
    )
    source = FilesystemHeldOutGraderSource(root=root)

    verdict = evaluate_held_out_gate(
        _task(),
        source,
        committed_tree=committed,
        run_id="run-2",
    )

    assert verdict.outcome is GateOutcome.PASS
    assert verdict.passed
    assert not verdict.blocks_landing
    assert len(verdict.results) == 2
    assert all(r.passed for r in verdict.results)


# --- fail path: grader exits non-zero ---------------------------------------


def test_failing_held_out_grader_blocks_land(tmp_path: Path) -> None:
    committed = tmp_path / "tree"
    committed.mkdir()
    root = tmp_path / "held_out"
    root.mkdir()
    _register(
        root,
        "task-alpha",
        [{"type": "command", "run": "exit 7", "name": "gate-check"}],
    )
    source = FilesystemHeldOutGraderSource(root=root)

    verdict = evaluate_held_out_gate(
        _task(),
        source,
        committed_tree=committed,
        run_id="run-3",
    )

    assert verdict.outcome is GateOutcome.FAIL
    assert verdict.blocks_landing
    assert not verdict.passed
    assert "gate-check" in verdict.reason


def test_first_failure_blocks_even_with_later_passing_grader(
    tmp_path: Path,
) -> None:
    committed = tmp_path / "tree"
    committed.mkdir()
    root = tmp_path / "held_out"
    root.mkdir()
    _register(
        root,
        "task-alpha",
        [
            {"type": "command", "run": "exit 1", "name": "fails"},
            {"type": "command", "run": "true", "name": "would-pass"},
        ],
    )
    source = FilesystemHeldOutGraderSource(root=root)

    verdict = evaluate_held_out_gate(
        _task(),
        source,
        committed_tree=committed,
        run_id="run-4",
    )

    assert verdict.outcome is GateOutcome.FAIL
    # The reason records that not every registered grader ran.
    assert "did not run" in verdict.reason


# --- D-4: agent self-report is never authoritative --------------------------


def test_verdict_ignores_agent_status(tmp_path: Path) -> None:
    """The engine is never handed the agent's terminal status and its verdict
    comes solely from the held-out grader exit code (D-4). A task whose own
    in-run grader is `true` (agent would report DONE) still FAILs the gate when
    the held-out grader exits non-zero."""
    committed = tmp_path / "tree"
    committed.mkdir()
    root = tmp_path / "held_out"
    root.mkdir()
    _register(
        root,
        "task-alpha",
        [{"type": "command", "run": "exit 2", "name": "held-out"}],
    )
    source = FilesystemHeldOutGraderSource(root=root)

    # The agent-facing task reports success via its own (visible) grader.
    done_looking_task = Task(
        goal="agent thinks it is done",
        graders=[CommandGrader(run="true", name="visible")],
        id="task-alpha",
    )

    verdict = evaluate_held_out_gate(
        done_looking_task,
        source,
        committed_tree=committed,
        run_id="run-5",
    )

    assert verdict.outcome is GateOutcome.FAIL


# --- D-3: fail closed -------------------------------------------------------


def test_grader_that_cannot_execute_fails_closed(tmp_path: Path) -> None:
    """A held-out grader that errors on launch (missing executable) blocks the
    land — it is never skipped or treated as a pass (D-3, the anti-hack)."""
    committed = tmp_path / "tree"
    committed.mkdir()
    root = tmp_path / "held_out"
    root.mkdir()
    _register(
        root,
        "task-alpha",
        [
            {
                "type": "command",
                "run": "./definitely-not-a-real-binary-xyz",
                "name": "missing",
            }
        ],
    )
    source = FilesystemHeldOutGraderSource(root=root)

    verdict = evaluate_held_out_gate(
        _task(),
        source,
        committed_tree=committed,
        run_id="run-6",
    )

    assert verdict.outcome is GateOutcome.FAIL
    assert verdict.blocks_landing


def test_malformed_registration_fails_closed(tmp_path: Path) -> None:
    root = tmp_path / "held_out"
    root.mkdir()
    (root / "task-alpha.json").write_text("{not json", encoding="utf-8")
    source = FilesystemHeldOutGraderSource(root=root)
    committed = tmp_path / "tree"
    committed.mkdir()

    verdict = evaluate_held_out_gate(
        _task(),
        source,
        committed_tree=committed,
        run_id="run-7",
    )

    assert verdict.outcome is GateOutcome.FAIL
    assert "failed closed" in verdict.reason


def test_empty_registration_fails_closed(tmp_path: Path) -> None:
    """An operator who registered a held-out file but declared no graders must
    not get a vacuous pass — an empty gate is worse than no gate."""
    root = tmp_path / "held_out"
    root.mkdir()
    _register(root, "task-alpha", [])
    source = FilesystemHeldOutGraderSource(root=root)
    committed = tmp_path / "tree"
    committed.mkdir()

    verdict = evaluate_held_out_gate(
        _task(),
        source,
        committed_tree=committed,
        run_id="run-8",
    )

    assert verdict.outcome is GateOutcome.FAIL


def test_non_command_held_out_grader_fails_closed(tmp_path: Path) -> None:
    """Held-out graders are command-only (D-5); a rubric registration is a
    misconfiguration that fails closed rather than running an LLM judge."""
    root = tmp_path / "held_out"
    root.mkdir()
    _register(
        root,
        "task-alpha",
        [{"type": "rubric", "assertions": ["looks good"], "name": "judge"}],
    )
    source = FilesystemHeldOutGraderSource(root=root)
    committed = tmp_path / "tree"
    committed.mkdir()

    verdict = evaluate_held_out_gate(
        _task(),
        source,
        committed_tree=committed,
        run_id="run-9",
    )

    assert verdict.outcome is GateOutcome.FAIL


def test_missing_committed_tree_fails_closed(tmp_path: Path) -> None:
    """If the runner cannot execute at all (the committed tree path is gone),
    the gate fails closed rather than silently skipping."""
    root = tmp_path / "held_out"
    root.mkdir()
    _register(
        root,
        "task-alpha",
        [{"type": "command", "run": "true", "name": "check"}],
    )
    source = FilesystemHeldOutGraderSource(root=root)

    verdict = evaluate_held_out_gate(
        _task(),
        source,
        committed_tree=tmp_path / "does-not-exist",
        run_id="run-10",
    )

    assert verdict.outcome is GateOutcome.FAIL
    assert verdict.blocks_landing


# --- #9: graded against the committed result --------------------------------


def test_grader_observes_committed_tree(tmp_path: Path) -> None:
    """A held-out grader inspecting a committed file sees the agent's committed
    content, because the committed tree is the grader's working directory."""
    committed = tmp_path / "tree"
    committed.mkdir()
    (committed / "result.txt").write_text("agent-committed-value\n", encoding="utf-8")
    root = tmp_path / "held_out"
    root.mkdir()
    _register(
        root,
        "task-alpha",
        [
            {
                "type": "command",
                "run": "grep -q agent-committed-value result.txt",
                "name": "inspects-commit",
            }
        ],
    )
    source = FilesystemHeldOutGraderSource(root=root)

    passing = evaluate_held_out_gate(
        _task(),
        source,
        committed_tree=committed,
        run_id="run-11",
    )
    assert passing.outcome is GateOutcome.PASS

    # The same grader fails when the committed content is absent — proving the
    # verdict tracks the committed tree, not a stale or empty one.
    other = tmp_path / "empty_tree"
    other.mkdir()
    failing = evaluate_held_out_gate(
        _task(),
        source,
        committed_tree=other,
        run_id="run-12",
    )
    assert failing.outcome is GateOutcome.FAIL


# --- #3 / D-2: held-out graders are absent from the agent's Task -------------


def test_held_out_graders_absent_from_agent_task(tmp_path: Path) -> None:
    """The held-out graders are never merged into the agent-facing Task; the
    gate reads them from the source and still executes them (D-2, #3)."""
    committed = tmp_path / "tree"
    committed.mkdir()
    root = tmp_path / "held_out"
    root.mkdir()
    _register(
        root,
        "task-alpha",
        [{"type": "command", "run": "true", "name": "secret-check"}],
    )
    source = FilesystemHeldOutGraderSource(root=root)

    agent_task = _task()
    # The agent-facing task exposes no held-out grader before the gate runs.
    assert all(
        g.name != "secret-check" for g in agent_task.graders
    )

    verdict = evaluate_held_out_gate(
        agent_task,
        source,
        committed_tree=committed,
        run_id="run-13",
    )

    # The gate executed the held-out grader anyway...
    assert verdict.outcome is GateOutcome.PASS
    assert any(
        r.grader_name == "secret-check" for r in verdict.results
    )
    # ...and did not mutate the agent-facing task to expose it.
    assert all(g.name != "secret-check" for g in agent_task.graders)


def test_source_path_traversal_is_not_registered(tmp_path: Path) -> None:
    """A task id that would escape the source root is treated as unregistered,
    never as a read of an arbitrary file."""
    root = tmp_path / "held_out"
    root.mkdir()
    source = FilesystemHeldOutGraderSource(root=root)
    assert source.graders_for("../escape") is None
    assert source.graders_for("nested/id") is None


# --- source contract: object form & raising ---------------------------------


def test_source_accepts_object_form(tmp_path: Path) -> None:
    root = tmp_path / "held_out"
    root.mkdir()
    _register(
        root,
        "task-alpha",
        {"graders": [{"type": "command", "run": "true", "name": "ok"}]},
    )
    source = FilesystemHeldOutGraderSource(root=root)
    graders = source.graders_for("task-alpha")
    assert graders is not None
    assert [g.name for g in graders] == ["ok"]


def test_source_raises_on_unparseable_registration(tmp_path: Path) -> None:
    root = tmp_path / "held_out"
    root.mkdir()
    (root / "task-alpha.json").write_text("not json at all", encoding="utf-8")
    source = FilesystemHeldOutGraderSource(root=root)
    with pytest.raises(HeldOutGraderError):
        source.graders_for("task-alpha")


def test_filesystem_source_satisfies_protocol(tmp_path: Path) -> None:
    source = FilesystemHeldOutGraderSource(root=tmp_path)
    assert isinstance(source, HeldOutGraderSource)

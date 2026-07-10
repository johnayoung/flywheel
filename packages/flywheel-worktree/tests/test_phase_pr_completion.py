"""Tests for the phase-branch PR-at-completion layer (spec 00079, criterion 3).

Two levels:

* pure rendering of ``render_phase_pr_body`` from hand-built sections (the body
  aggregates per-task receipts + held-out verdicts, ``NO_GATE`` distinct from
  ``PASS``, missing receipts surfaced not omitted);
* end-to-end through the worker's ``archive_phases`` seam with a real git repo
  (phase branch + bare remote, real pushes), a store seeded with DONE
  lifecycles / attempts / grader receipts / held-out verdicts, and the ``gh``
  CLI replaced by a recording fake. These prove: exactly one PR onto the true
  base at completion, a second sweep edits rather than duplicates, ``[phase]
  verify`` is evaluated against the phase-branch tree (not the operator
  checkout), and a merge-strategy phase archives untouched.
"""

from __future__ import annotations

import json
import subprocess
from collections.abc import Sequence
from datetime import datetime, timedelta, timezone
from pathlib import Path

from flywheel_core.events import HeldOutGateEvaluated
from flywheel_core.lifecycle import Attempt, Lifecycle, Outcome, Status
from flywheel_core.store_protocols import GraderResultRecord, GraderType
from flywheel_core.store_sqlite import SqliteStore
from flywheel_orchestrator import GraderReceipt, WorkPolicy

from flywheel_worktree.pr import (
    PhaseTaskSection,
    _phase_branch,
    render_phase_pr_body,
)
from flywheel_worktree.worker import archive_phases

_T0 = datetime(2026, 7, 1, 12, 0, 0, tzinfo=timezone.utc)


# --- body rendering (pure) ----------------------------------------------------


def test_render_phase_body_aggregates_receipts_and_verdicts() -> None:
    sections = [
        PhaseTaskSection(
            task_id="t1",
            goal="Goal for t1.",
            receipts=(
                GraderReceipt(
                    ordinal=0, grader_type="command", name="tests", passed=True
                ),
            ),
            held_out_outcome="pass",
        ),
        PhaseTaskSection(
            task_id="t2",
            goal="Goal for t2.",
            receipts=(),
            held_out_outcome="no_gate",
        ),
    ]
    body = render_phase_pr_body("01-foo", sections)

    # One section per task id, both present (not silently dropped).
    assert "## Task `t1`" in body
    assert "## Task `t2`" in body
    assert "Goal for t1." in body
    assert "Goal for t2." in body
    # t1's harness receipt row is rendered from the store, not the agent.
    assert "| 0 | command | tests | pass |" in body
    # t2 has no persisted receipts -> the unavailable line, not omission.
    assert "(receipt projection unavailable for this run)" in body
    # Held-out verdicts rendered distinctly: NO_GATE is not PASS.
    assert "held-out gate: `PASS`" in body
    assert "held-out gate: `NO_GATE`" in body
    # The phase branch under review is named in the body.
    assert _phase_branch("01-foo") in body


def test_render_phase_body_fail_and_unrecorded_verdicts() -> None:
    body = render_phase_pr_body(
        "01-foo",
        [
            PhaseTaskSection(
                task_id="t1",
                goal="G1.",
                receipts=(
                    GraderReceipt(
                        ordinal=0,
                        grader_type="command",
                        name=None,
                        passed=False,
                    ),
                ),
                held_out_outcome="fail",
            ),
            PhaseTaskSection(
                task_id="t2",
                goal="G2.",
                receipts=(),
                held_out_outcome=None,
            ),
        ],
    )
    assert "| 0 | command | - | FAIL |" in body
    assert "held-out gate: `FAIL`" in body
    # A run with no recorded gate evaluation is distinct from a recorded verdict.
    assert "held-out gate: `not recorded`" in body


# --- git / store fixtures -----------------------------------------------------


def _git(cwd: Path, *args: str) -> str:
    proc = subprocess.run(
        ["git", "-C", str(cwd), *args],
        text=True,
        capture_output=True,
        check=True,
    )
    return proc.stdout.strip()


def _init_repo_with_remote(path: Path) -> Path:
    path.mkdir(parents=True, exist_ok=True)
    _git(path, "init", "-b", "main")
    _git(path, "config", "user.email", "phase-test@example.com")
    _git(path, "config", "user.name", "phase test")
    _git(path, "config", "commit.gpgsign", "false")
    (path / "README.md").write_text("base\n")
    _git(path, "add", "-A")
    _git(path, "commit", "-m", "init")
    remote = path.parent / "origin.git"
    subprocess.run(
        ["git", "init", "--bare", str(remote)],
        capture_output=True,
        check=True,
    )
    _git(path, "remote", "add", "origin", str(remote))
    return remote


def _make_phase_branch(repo: Path, phase: str, files: dict[str, str]) -> None:
    """Land ``files`` onto ``flywheel/phase/<phase>``; leave HEAD back on main.

    The point: after this, the phase's work lives ONLY on the phase branch, so
    a gate run against the operator checkout (main) would not see it.
    """
    branch = _phase_branch(phase)
    _git(repo, "checkout", "-b", branch)
    for name, content in files.items():
        (repo / name).write_text(content)
    _git(repo, "add", "-A")
    _git(repo, "commit", "-m", f"land {phase}")
    _git(repo, "checkout", "main")


def _remote_branch_exists(remote: Path, branch: str) -> bool:
    proc = subprocess.run(
        [
            "git",
            "-C",
            str(remote),
            "show-ref",
            "--verify",
            "--quiet",
            f"refs/heads/{branch}",
        ],
        capture_output=True,
        check=False,
    )
    return proc.returncode == 0


class _FakeGh:
    """Recording ``gh`` runner: ``pr list`` returns a canned URL (or nothing),
    ``pr create`` returns the new PR URL, ``pr edit`` returns nothing."""

    def __init__(self, existing_url: str = "") -> None:
        self.existing_url = existing_url
        self.calls: list[list[str]] = []

    def __call__(self, argv: Sequence[str]) -> str:
        argv = list(argv)
        self.calls.append(argv)
        if argv[:2] == ["pr", "list"]:
            return self.existing_url
        if argv[:2] == ["pr", "create"]:
            return "https://example.test/phase-pr/1\n"
        return ""

    def commands(self) -> list[str]:
        return [" ".join(c[:2]) for c in self.calls]


def _task_file(repo: Path, phase: str, task_id: str) -> None:
    tf = repo / ".flywheel" / "tasks" / "active" / phase / f"{task_id}.json"
    tf.parent.mkdir(parents=True, exist_ok=True)
    tf.write_text(
        json.dumps(
            {
                "id": task_id,
                "goal": f"Goal for {task_id}.",
                "graders": [{"type": "command", "run": "true"}],
            }
        )
    )


def _seed_done_task(
    store: SqliteStore,
    task_id: str,
    *,
    run_id: str,
    phase: str,
    receipts: list[tuple[GraderType, str, bool]] | None,
    held_out: str | None,
) -> None:
    """Seed a DONE lifecycle with optional final-attempt receipts + verdict."""
    lc = Lifecycle(
        task_id=task_id,
        run_id=run_id,
        source=f".flywheel/tasks/active/{phase}/{task_id}.json",
    )
    lc.transition_to(Status.READY, now=_T0)
    lc.transition_to(Status.RUNNING, now=_T0)
    lc.transition_to(Status.VALIDATING, now=_T0)
    lc.transition_to(Status.DONE, now=_T0)
    store.create_lifecycle(lc)

    if receipts is not None:
        store.save_attempt(
            run_id,
            Attempt(
                number=1,
                started_at=_T0,
                run_id=run_id,
                ended_at=_T0 + timedelta(minutes=1),
                outcome=Outcome.SUCCEEDED,
                input_tokens=100,
                iterations_completed=1,
                turns=1,
                total_cost_usd=0.1,
            ),
        )
        for ordinal, (gtype, gname, passed) in enumerate(receipts):
            store.append_grader_result(
                GraderResultRecord(
                    run_id=run_id,
                    attempt_number=1,
                    ordinal=ordinal,
                    grader_type=gtype,
                    grader_spec={"type": gtype, "run": "true"},
                    passed=passed,
                    duration_ms=1,
                    payload={"exit_code": 0 if passed else 1},
                    ts=_T0,
                    grader_name=gname,
                )
            )

    if held_out is not None:
        loaded = store.load_lifecycle(run_id)
        assert loaded is not None
        store.append_domain_event(
            HeldOutGateEvaluated(run_id=run_id, ts=_T0, outcome=held_out),
            expected_version=loaded.version,
        )


def _policy(
    db_path: Path,
    tasks_dir: Path,
    *,
    strategy: str = "phase",
    phase_verify: str | None = None,
) -> WorkPolicy:
    return WorkPolicy(
        source_kind="directory",
        tasks_dir=tasks_dir,
        db_path=db_path,
        submit_strategy=strategy,
        submit_remote="origin",
        submit_pr_base="main",
        phase_verify=phase_verify,
    )


def _complete_phase_fixture(
    tmp_path: Path,
    *,
    phase_files: dict[str, str] | None = None,
) -> tuple[Path, Path, Path]:
    """A repo with a completed phase ``01-foo``: two DONE+landed tasks, a phase
    branch holding their work, and a store seeded for both. Returns
    ``(repo, remote, db_path)``."""
    repo = tmp_path / "repo"
    remote = _init_repo_with_remote(repo)
    _task_file(repo, "01-foo", "t1")
    _task_file(repo, "01-foo", "t2")
    # Commit the active task files onto main (the operator checkout) BEFORE
    # branching, so switching back to main after building the phase branch
    # keeps them (and the ``.flywheel`` dir the store lives under) in place.
    _git(repo, "add", "-A")
    _git(repo, "commit", "-m", "seed active tasks")
    _make_phase_branch(
        repo, "01-foo", phase_files or {"a.txt": "1", "b.txt": "2"}
    )
    db_path = repo / ".flywheel" / "flywheel.db"
    store = SqliteStore(db_path)
    _seed_done_task(
        store,
        "t1",
        run_id="run-t1",
        phase="01-foo",
        receipts=[("command", "tests", True)],
        held_out="pass",
    )
    _seed_done_task(
        store,
        "t2",
        run_id="run-t2",
        phase="01-foo",
        receipts=None,
        held_out="no_gate",
    )
    store.close()
    return repo, remote, db_path


# --- completion behavior (end-to-end through archive_phases) ------------------


def test_completion_pushes_branch_and_opens_one_pr(tmp_path: Path) -> None:
    repo, remote, db_path = _complete_phase_fixture(tmp_path)
    tasks_dir = repo / ".flywheel" / "tasks"
    gh = _FakeGh()

    archive_phases(
        tasks_dir,
        db_path,
        lambda _m: None,
        repo_root=repo,
        landing_base=None,
        policy=_policy(db_path, tasks_dir),
        gh=gh,
    )

    # Exactly one PR: list then create, never a duplicate create.
    assert gh.commands() == ["pr list", "pr create"]
    create = next(c for c in gh.calls if c[:2] == ["pr", "create"])
    assert create[create.index("--head") + 1] == _phase_branch("01-foo")
    assert create[create.index("--base") + 1] == "main"

    body = create[create.index("--body") + 1]
    assert "## Task `t1`" in body
    assert "## Task `t2`" in body
    assert "| 0 | command | tests | pass |" in body
    assert "held-out gate: `PASS`" in body
    assert "held-out gate: `NO_GATE`" in body
    # t2 has no persisted receipts -> surfaced, not omitted.
    assert "(receipt projection unavailable for this run)" in body

    # The branch is on the remote; the phase stays active (archives on merge).
    assert _remote_branch_exists(remote, _phase_branch("01-foo"))
    assert (tasks_dir / "active" / "01-foo").is_dir()
    assert not (tasks_dir / "archive" / "01-foo").exists()


def test_second_sweep_edits_rather_than_duplicates(tmp_path: Path) -> None:
    repo, _remote, db_path = _complete_phase_fixture(tmp_path)
    tasks_dir = repo / ".flywheel" / "tasks"
    gh = _FakeGh(existing_url="https://example.test/phase-pr/9\n")

    archive_phases(
        tasks_dir,
        db_path,
        lambda _m: None,
        repo_root=repo,
        landing_base=None,
        policy=_policy(db_path, tasks_dir),
        gh=gh,
    )

    # An already-open PR is edited (body refreshed), never a second create.
    assert gh.commands() == ["pr list", "pr edit"]
    edit = gh.calls[1]
    assert edit[2] == "https://example.test/phase-pr/9"


def test_phase_verify_runs_against_phase_branch_tree(tmp_path: Path) -> None:
    # ``a.txt`` exists only on the phase branch, never on the operator checkout
    # (main). A gate asserting its presence passes ONLY when evaluated against
    # the phase-branch tree -- the whole point of D-6.
    repo, remote, db_path = _complete_phase_fixture(
        tmp_path, phase_files={"a.txt": "1"}
    )
    tasks_dir = repo / ".flywheel" / "tasks"
    assert not (repo / "a.txt").exists()  # not on the operator checkout
    gh = _FakeGh()

    archive_phases(
        tasks_dir,
        db_path,
        lambda _m: None,
        repo_root=repo,
        landing_base=None,
        policy=_policy(db_path, tasks_dir, phase_verify="test -f a.txt"),
        gh=gh,
    )

    # The gate saw the phase's work, so the PR opened.
    assert gh.commands() == ["pr list", "pr create"]
    assert _remote_branch_exists(remote, _phase_branch("01-foo"))


def test_red_phase_verify_opens_no_pr(tmp_path: Path) -> None:
    repo, remote, db_path = _complete_phase_fixture(tmp_path)
    tasks_dir = repo / ".flywheel" / "tasks"
    logs: list[str] = []
    gh = _FakeGh()

    archive_phases(
        tasks_dir,
        db_path,
        logs.append,
        repo_root=repo,
        landing_base=None,
        policy=_policy(
            db_path, tasks_dir, phase_verify="test -f does-not-exist.txt"
        ),
        gh=gh,
    )

    # A failing phase gate opens no PR, pushes nothing, leaves the phase active.
    assert gh.calls == []
    assert not _remote_branch_exists(remote, _phase_branch("01-foo"))
    assert (tasks_dir / "active" / "01-foo").is_dir()
    assert any("[phase] verify failed" in m for m in logs)


def test_merge_strategy_archives_without_phase_pr(tmp_path: Path) -> None:
    # The opt-in gate: under the default merge strategy the sweep archives the
    # completed phase exactly as before -- no publisher, no gh, no phase branch.
    repo = tmp_path / "repo"
    _init_repo_with_remote(repo)
    tasks_dir = repo / ".flywheel" / "tasks"
    _task_file(repo, "01-foo", "t1")
    db_path = repo / ".flywheel" / "flywheel.db"
    store = SqliteStore(db_path)
    _seed_done_task(
        store,
        "t1",
        run_id="run-t1",
        phase="01-foo",
        receipts=[("command", "tests", True)],
        held_out="pass",
    )
    store.close()
    gh = _FakeGh()

    archive_phases(
        tasks_dir,
        db_path,
        lambda _m: None,
        repo_root=repo,
        landing_base=None,
        policy=_policy(db_path, tasks_dir, strategy="merge"),
        gh=gh,
    )

    assert gh.calls == []
    assert (tasks_dir / "archive" / "01-foo").is_dir()
    assert not (tasks_dir / "active" / "01-foo").exists()

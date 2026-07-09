"""Tests for the phase-branch landing strategy (spec 00079, criteria 1/2/9).

The ``phase`` submit strategy lands each DONE task onto a per-phase integration
branch ``flywheel/phase/<phase>`` derived from the task's phase directory, rather
than onto the single configured base. These tests pin the strategy layer's
observable end-states:

* criterion 1 -- two tasks from one phase land onto the phase branch in order,
  the phase branch is created from the true base on the first landing, and the
  true base SHA is never advanced;
* criterion 2 -- the phase path runs the same verify ladder as the merge path
  (post-rebase re-verify + standing ``[submit] verify`` on both the clean-FF and
  base-advanced paths; the protected-path gate), parking under the existing park
  kinds and leaving the phase branch unchanged;
* criterion 9 -- the ``merge`` strategy creates no ``flywheel/phase/*`` branch.

The discriminator throughout: a land advances the phase integration branch and
tears the worktree down (``on_done="destroy"`` default); a park leaves the
worktree on disk and the phase branch byte-for-byte unchanged. The true base is
inspected on every land to prove it never moves.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

from flywheel_core import CommandGrader, Grader, Status, Task
from flywheel_core.events import DomainEvent, LandingParked
from flywheel_orchestrator import SandboxRequest, SubmitRequest
from flywheel_worktree import worker


# --- git helpers (mirror test_submit_standing_verify.py) --------------------


def _git(cwd: Path, *args: str) -> str:
    proc = subprocess.run(
        ["git", "-C", str(cwd), *args],
        text=True,
        capture_output=True,
        check=True,
    )
    return proc.stdout.strip()


def _init_repo(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True)
    _git(path, "init", "-b", "main")
    _git(path, "config", "user.email", "worker-test@example.com")
    _git(path, "config", "user.name", "worker test")
    _git(path, "config", "commit.gpgsign", "false")
    (path / "README.md").write_text("base\n")
    _git(path, "add", "-A")
    _git(path, "commit", "-m", "init")


def _rev(repo: Path, ref: str) -> str:
    return _git(repo, "rev-parse", ref)


def _branch_exists(repo: Path, branch: str) -> bool:
    return (
        subprocess.run(
            ["git", "-C", str(repo), "rev-parse", "--verify", branch],
            capture_output=True,
        ).returncode
        == 0
    )


def _commit(worktree: Path, filename: str, body: str, message: str) -> None:
    (worktree / filename).write_text(body)
    _git(worktree, "add", "-A")
    _git(worktree, "commit", "-m", message)


def _phase_branch(phase: str) -> str:
    return f"flywheel/phase/{phase}"


class _RecordingLedger:
    """Minimal LandingLedger stub: a non-None lifecycle and a captured event
    list, enough to assert the submitter records a LandingParked witness."""

    def __init__(self) -> None:
        self.events: list[DomainEvent] = []

    class _Lifecycle:
        version = 0

    def load_lifecycle(self, run_id: str) -> "_RecordingLedger._Lifecycle":
        return self._Lifecycle()

    def append_domain_event(
        self, event: DomainEvent, *, expected_version: int
    ) -> "_RecordingLedger._Lifecycle":
        self.events.append(event)
        return self._Lifecycle()


def _phase_submitter(
    repo: Path,
    *,
    verify_command: str | None = None,
    protected_paths: tuple[str, ...] = (),
    store: _RecordingLedger | None = None,
) -> worker.PhaseBranchSubmitter:
    worktrees = repo / ".flywheel" / "worktrees"
    worktrees.mkdir(parents=True, exist_ok=True)
    return worker.PhaseBranchSubmitter(
        repo_root=repo,
        tasks_dir=repo / ".flywheel" / "tasks",
        worktrees_dir=worktrees,
        phase_base="main",
        lock_path=repo / ".flywheel" / ".merge.lock",
        log=lambda _m: None,
        protected_paths=protected_paths,
        verify_command=verify_command,
        store=store,  # type: ignore[arg-type]
    )


def _merge_submitter(repo: Path) -> worker.GitWorktreeSubmitter:
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


def _task_file(repo: Path, phase: str, task_id: str) -> Path:
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
    return tf


def _submit_req(
    tf: Path,
    task_id: str,
    sandbox: Path,
    *,
    run_id: str,
    graders: list[Grader] | None = None,
) -> SubmitRequest:
    return SubmitRequest(
        task_id=task_id,
        task_file=tf,
        task=Task(
            id=task_id,
            goal=f"Goal for {task_id}.",
            graders=(
                graders
                if graders is not None
                else [CommandGrader(run="true")]
            ),
        ),
        run_id=run_id,
        status=Status.DONE,
        sandbox=sandbox,
    )


def _prepare_with_commit(
    s: worker.GitWorktreeSubmitter,
    tf: Path,
    task_id: str,
    *,
    filename: str,
    body: str = "x",
) -> Path:
    wt = s.prepare_sandbox(
        SandboxRequest(task_id=task_id, task_file=tf, run_id=None, mode="fresh")
    )
    _commit(wt, filename, body, f"feat: {filename}")
    return wt


# --- criterion 1: two tasks stack; true base never advances ------------------


def test_two_tasks_stack_on_phase_branch_true_base_unchanged(
    tmp_path: Path,
) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    base_before = _rev(repo, "main")
    phase = "01-phase"
    integration = _phase_branch(phase)
    s = _phase_submitter(repo, verify_command="true")

    tf1 = _task_file(repo, phase, "t1")
    wt1 = _prepare_with_commit(s, tf1, "t1", filename="feature1.txt")
    s.submit(_submit_req(tf1, "t1", wt1, run_id="run-1"))

    # First landing forked the integration branch off the true base and
    # advanced only it -- the true base is untouched.
    assert _branch_exists(repo, integration)
    assert _rev(repo, "main") == base_before
    assert not wt1.exists()

    tf2 = _task_file(repo, phase, "t2")
    wt2 = _prepare_with_commit(s, tf2, "t2", filename="feature2.txt")
    s.submit(_submit_req(tf2, "t2", wt2, run_id="run-2"))

    # Second task stacked: the phase branch now contains both files, in landing
    # order (t2 on top of t1), and the true base is still untouched.
    assert _rev(repo, "main") == base_before
    assert not wt2.exists()
    tree = _git(repo, "ls-tree", "-r", "--name-only", integration).splitlines()
    assert "feature1.txt" in tree
    assert "feature2.txt" in tree
    subjects = _git(
        repo, "log", "--format=%s", integration
    ).splitlines()
    assert subjects[0] == "feat: feature2.txt"
    assert "feat: feature1.txt" in subjects


def test_first_landing_creates_branch_at_true_base_tip(tmp_path: Path) -> None:
    # The integration branch is forked from the then-current true base: its
    # parent chain reaches the pre-run base commit.
    repo = tmp_path / "repo"
    _init_repo(repo)
    base_before = _rev(repo, "main")
    phase = "01-phase"
    integration = _phase_branch(phase)
    s = _phase_submitter(repo, verify_command="true")

    tf = _task_file(repo, phase, "t1")
    wt = _prepare_with_commit(s, tf, "t1", filename="feature.txt")
    s.submit(_submit_req(tf, "t1", wt, run_id="run-1"))

    # base_before is an ancestor of the integration tip (branch forked from it),
    # and the true base ref itself did not move.
    assert _rev(repo, "main") == base_before
    assert (
        subprocess.run(
            [
                "git",
                "-C",
                str(repo),
                "merge-base",
                "--is-ancestor",
                base_before,
                integration,
            ],
            capture_output=True,
        ).returncode
        == 0
    )


def test_two_phases_get_distinct_integration_branches(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    base_before = _rev(repo, "main")
    s = _phase_submitter(repo, verify_command="true")

    tf_a = _task_file(repo, "01-alpha", "a1")
    wt_a = _prepare_with_commit(s, tf_a, "a1", filename="alpha.txt")
    s.submit(_submit_req(tf_a, "a1", wt_a, run_id="run-a"))

    tf_b = _task_file(repo, "02-beta", "b1")
    wt_b = _prepare_with_commit(s, tf_b, "b1", filename="beta.txt")
    s.submit(_submit_req(tf_b, "b1", wt_b, run_id="run-b"))

    alpha = _phase_branch("01-alpha")
    beta = _phase_branch("02-beta")
    assert _branch_exists(repo, alpha)
    assert _branch_exists(repo, beta)
    assert _rev(repo, alpha) != _rev(repo, beta)
    # Each phase branch carries only its own phase's work.
    alpha_tree = _git(repo, "ls-tree", "-r", "--name-only", alpha).splitlines()
    beta_tree = _git(repo, "ls-tree", "-r", "--name-only", beta).splitlines()
    assert "alpha.txt" in alpha_tree and "beta.txt" not in alpha_tree
    assert "beta.txt" in beta_tree and "alpha.txt" not in beta_tree
    assert _rev(repo, "main") == base_before


# --- criterion 2: verify-ladder parity, park kinds, branch unchanged ---------


def test_base_advanced_standing_verify_failure_parks_branch_unchanged(
    tmp_path: Path,
) -> None:
    # The phase path is not a fast lane: when the phase branch advanced under a
    # finished task, a base-advanced task rebases, re-runs its command graders
    # (pass), then the standing [submit] verify runs against the exact tree it
    # would land (fail) -> park standing-verify, phase branch unchanged.
    repo = tmp_path / "repo"
    _init_repo(repo)
    base_before = _rev(repo, "main")
    phase = "01-phase"
    integration = _phase_branch(phase)
    ledger = _RecordingLedger()
    # verify fails iff a "poison" file is present in the tree about to land.
    s = _phase_submitter(
        repo, verify_command="test ! -f poison", store=ledger
    )

    # t1 lands cleanly (no poison) -> integration created + advanced.
    tf1 = _task_file(repo, phase, "t1")
    wt1 = _prepare_with_commit(s, tf1, "t1", filename="feature1.txt")
    s.submit(_submit_req(tf1, "t1", wt1, run_id="run-1"))
    assert _branch_exists(repo, integration)

    # t_poison forks the integration tip and commits a poison file (does NOT
    # land yet).
    tf_p = _task_file(repo, phase, "t2")
    wt_p = _prepare_with_commit(s, tf_p, "t2", filename="poison", body="bad")

    # t_adv forks the same tip and lands, advancing the integration branch so
    # t_poison is now base-advanced.
    tf_a = _task_file(repo, phase, "t3")
    wt_a = _prepare_with_commit(s, tf_a, "t3", filename="adv.txt")
    s.submit(_submit_req(tf_a, "t3", wt_a, run_id="run-3"))
    integration_after_adv = _rev(repo, integration)

    # t_poison submits: rebases onto the advanced integration, re-verify passes,
    # standing verify fails on the poison tree -> park standing-verify.
    s.submit(_submit_req(tf_p, "t2", wt_p, run_id="run-2"))

    parked = [e for e in ledger.events if isinstance(e, LandingParked)]
    assert len(parked) == 1
    assert parked[0].park_kind == "standing-verify"
    # The phase branch did not move (the poison work never landed), and the true
    # base is untouched.
    assert _rev(repo, integration) == integration_after_adv
    assert _rev(repo, "main") == base_before
    assert wt_p.exists()


def test_protected_path_parks_branch_unchanged(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    base_before = _rev(repo, "main")
    phase = "01-phase"
    integration = _phase_branch(phase)
    ledger = _RecordingLedger()
    s = _phase_submitter(
        repo, verify_command="true", protected_paths=("secret.txt",),
        store=ledger,
    )

    # t1 lands cleanly -> integration exists.
    tf1 = _task_file(repo, phase, "t1")
    wt1 = _prepare_with_commit(s, tf1, "t1", filename="feature1.txt")
    s.submit(_submit_req(tf1, "t1", wt1, run_id="run-1"))
    integration_after_t1 = _rev(repo, integration)

    # t2 touches a protected path -> park protected-paths before any FF; the
    # phase branch does not advance.
    tf2 = _task_file(repo, phase, "t2")
    wt2 = _prepare_with_commit(s, tf2, "t2", filename="secret.txt")
    s.submit(_submit_req(tf2, "t2", wt2, run_id="run-2"))

    parked = [e for e in ledger.events if isinstance(e, LandingParked)]
    assert len(parked) == 1
    assert parked[0].park_kind == "protected-paths"
    assert _rev(repo, integration) == integration_after_t1
    assert _rev(repo, "main") == base_before
    assert wt2.exists()


def test_protected_path_on_first_task_creates_no_branch(tmp_path: Path) -> None:
    # A first-task protected-path park must not create the integration branch --
    # materialization happens only at an actual fast-forward.
    repo = tmp_path / "repo"
    _init_repo(repo)
    base_before = _rev(repo, "main")
    phase = "01-phase"
    integration = _phase_branch(phase)
    ledger = _RecordingLedger()
    s = _phase_submitter(
        repo, protected_paths=("secret.txt",), store=ledger
    )

    tf = _task_file(repo, phase, "t1")
    wt = _prepare_with_commit(s, tf, "t1", filename="secret.txt")
    s.submit(_submit_req(tf, "t1", wt, run_id="run-1"))

    parked = [e for e in ledger.events if isinstance(e, LandingParked)]
    assert len(parked) == 1
    assert parked[0].park_kind == "protected-paths"
    assert not _branch_exists(repo, integration)
    assert _rev(repo, "main") == base_before
    assert wt.exists()


def test_clean_ff_standing_verify_failure_creates_no_branch(
    tmp_path: Path,
) -> None:
    # On the clean-FF path the standing gate runs before materialization; a
    # first-task failure parks standing-verify and creates no integration branch.
    repo = tmp_path / "repo"
    _init_repo(repo)
    base_before = _rev(repo, "main")
    phase = "01-phase"
    integration = _phase_branch(phase)
    ledger = _RecordingLedger()
    s = _phase_submitter(repo, verify_command="false", store=ledger)

    tf = _task_file(repo, phase, "t1")
    wt = _prepare_with_commit(s, tf, "t1", filename="feature.txt")
    s.submit(_submit_req(tf, "t1", wt, run_id="run-1"))

    parked = [e for e in ledger.events if isinstance(e, LandingParked)]
    assert len(parked) == 1
    assert parked[0].park_kind == "standing-verify"
    assert not _branch_exists(repo, integration)
    assert _rev(repo, "main") == base_before
    assert wt.exists()


# --- criterion 9: merge/pr involve no phase-branch machinery -----------------


def test_merge_strategy_creates_no_phase_branch(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    base_before = _rev(repo, "main")
    s = _merge_submitter(repo)

    tf = _task_file(repo, "01-phase", "t1")
    wt = _prepare_with_commit(s, tf, "t1", filename="feature.txt")
    s.submit(_submit_req(tf, "t1", wt, run_id="run-1"))

    # The merge strategy landed onto the base (main advanced) and created no
    # flywheel/phase/* integration branch.
    assert _rev(repo, "main") != base_before
    listing = _git(repo, "branch", "--list", "flywheel/phase/*")
    assert listing == ""

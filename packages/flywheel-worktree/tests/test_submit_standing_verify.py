"""Tests for the post-merge standing build gate (spec 00064).

``[submit] verify`` is a repo-wide build invariant the submitter re-runs under
the merge lock against the exact tree about to become the base, on every land
path, independent of the task's own command graders. These tests pin the gate's
observable effects -- a passing verify lands, a failing verify parks the worktree
with the base untouched -- on both the clean fast-forward path and the
base-advanced rebase path, plus the back-compat (unset) and never-raise
guarantees.

The discriminator throughout: a successful land tears the worktree down
(``on_done="destroy"`` default) and advances the base; a park leaves the
worktree on disk and the base byte-for-byte unchanged.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

from flywheel_core import CommandGrader, Grader, Status, Task
from flywheel_core.events import DomainEvent, LandingParked
from flywheel_orchestrator import SandboxRequest, SubmitRequest
from flywheel_worktree import worker


# --- git helpers (mirror test_landability_predicate.py) ---------------------


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


def _commit(worktree: Path, filename: str, body: str, message: str) -> None:
    (worktree / filename).write_text(body)
    _git(worktree, "add", "-A")
    _git(worktree, "commit", "-m", message)


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


def _submitter(
    repo: Path,
    *,
    verify_command: str | None,
    store: _RecordingLedger | None = None,
) -> worker.GitWorktreeSubmitter:
    worktrees = repo / ".flywheel" / "worktrees"
    worktrees.mkdir(parents=True, exist_ok=True)
    return worker.GitWorktreeSubmitter(
        repo_root=repo,
        tasks_dir=repo / ".flywheel" / "tasks",
        worktrees_dir=worktrees,
        phase_base="main",
        lock_path=repo / ".flywheel" / ".merge.lock",
        log=lambda _m: None,
        verify_command=verify_command,
        store=store,  # type: ignore[arg-type]
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
    tf: Path, task_id: str, sandbox: Path, *, graders: list[Grader] | None
) -> SubmitRequest:
    return SubmitRequest(
        task_id=task_id,
        task_file=tf,
        task=Task(
            id=task_id,
            goal=f"Goal for {task_id}.",
            graders=graders if graders is not None else [CommandGrader(run="true")],
        ),
        run_id="run-1",
        status=Status.DONE,
        sandbox=sandbox,
    )


def _prepare_with_commit(
    s: worker.GitWorktreeSubmitter, tf: Path, task_id: str, *, filename: str
) -> Path:
    wt = s.prepare_sandbox(
        SandboxRequest(task_id=task_id, task_file=tf, run_id=None, mode="fresh")
    )
    _commit(wt, filename, "x", f"feat: {filename}")
    return wt


# --- clean fast-forward path -------------------------------------------------


def test_verify_passing_clean_ff_lands(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    base_before = _rev(repo, "main")
    s = _submitter(repo, verify_command="true")
    tf = _task_file(repo, "01-phase", "t1")
    wt = _prepare_with_commit(s, tf, "t1", filename="feature.txt")

    s.submit(_submit_req(tf, "t1", wt, graders=None))

    # Landed: base advanced and the change is in the working tree; worktree torn
    # down (on_done=destroy default).
    assert _rev(repo, "main") != base_before
    assert (repo / "feature.txt").exists()
    assert not wt.exists()


def test_verify_failing_clean_ff_parks_and_records_event(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    base_before = _rev(repo, "main")
    ledger = _RecordingLedger()
    s = _submitter(repo, verify_command="false", store=ledger)
    tf = _task_file(repo, "01-phase", "t1")
    wt = _prepare_with_commit(s, tf, "t1", filename="feature.txt")

    s.submit(_submit_req(tf, "t1", wt, graders=None))

    # Not landed: base untouched, change never reached the base, worktree parked.
    assert _rev(repo, "main") == base_before
    assert not (repo / "feature.txt").exists()
    assert wt.exists()
    parked = [e for e in ledger.events if isinstance(e, LandingParked)]
    assert len(parked) == 1
    assert parked[0].park_kind == "standing-verify"


def test_verify_inspects_the_to_be_landed_tree(tmp_path: Path) -> None:
    # The verify runs against the branch's tree: a command that fails iff a
    # specific file is present blocks exactly that branch.
    repo = tmp_path / "repo"
    _init_repo(repo)
    base_before = _rev(repo, "main")
    s = _submitter(repo, verify_command="test ! -f broken")
    tf = _task_file(repo, "01-phase", "t1")
    wt = _prepare_with_commit(s, tf, "t1", filename="broken")

    s.submit(_submit_req(tf, "t1", wt, graders=None))

    assert _rev(repo, "main") == base_before
    assert wt.exists()


def test_verify_runs_even_with_zero_command_graders(tmp_path: Path) -> None:
    # The standing gate is independent of task graders: a task that declares
    # none is still gated on the clean-FF path.
    repo = tmp_path / "repo"
    _init_repo(repo)
    base_before = _rev(repo, "main")
    s = _submitter(repo, verify_command="false")
    tf = _task_file(repo, "01-phase", "t1")
    wt = _prepare_with_commit(s, tf, "t1", filename="feature.txt")

    s.submit(_submit_req(tf, "t1", wt, graders=[]))

    assert _rev(repo, "main") == base_before
    assert wt.exists()


# --- base-advanced rebase path -----------------------------------------------


def _advance_base(repo: Path) -> None:
    """Advance ``main`` in repo_root out-of-band so a branch forked earlier can
    no longer fast-forward and must take the rebase path."""
    (repo / "other.txt").write_text("advanced\n")
    _git(repo, "add", "-A")
    _git(repo, "commit", "-m", "advance base")


def test_verify_passing_base_advanced_rebases_and_lands(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _submitter(repo, verify_command="true")
    tf = _task_file(repo, "01-phase", "t1")
    wt = _prepare_with_commit(s, tf, "t1", filename="feature.txt")

    _advance_base(repo)  # forces FF to fail -> rebase path
    base_advanced = _rev(repo, "main")

    s.submit(_submit_req(tf, "t1", wt, graders=None))

    # Landed after rebase: base moved past the advanced commit, both files in.
    assert _rev(repo, "main") != base_advanced
    assert (repo / "feature.txt").exists()
    assert (repo / "other.txt").exists()
    assert not wt.exists()


def test_verify_failing_base_advanced_parks(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    ledger = _RecordingLedger()
    s = _submitter(repo, verify_command="false", store=ledger)
    tf = _task_file(repo, "01-phase", "t1")
    wt = _prepare_with_commit(s, tf, "t1", filename="feature.txt")

    _advance_base(repo)
    base_advanced = _rev(repo, "main")

    s.submit(_submit_req(tf, "t1", wt, graders=None))

    # Rebase succeeded but the standing gate refused: base stays at the advanced
    # commit, the branch's change never landed, worktree parked.
    assert _rev(repo, "main") == base_advanced
    assert not (repo / "feature.txt").exists()
    assert wt.exists()
    parked = [e for e in ledger.events if isinstance(e, LandingParked)]
    assert len(parked) == 1
    assert parked[0].park_kind == "standing-verify"


# --- back-compat + never-raise ----------------------------------------------


def test_verify_unset_lands_unchanged(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    base_before = _rev(repo, "main")
    s = _submitter(repo, verify_command=None)
    tf = _task_file(repo, "01-phase", "t1")
    wt = _prepare_with_commit(s, tf, "t1", filename="feature.txt")

    s.submit(_submit_req(tf, "t1", wt, graders=None))

    assert _rev(repo, "main") != base_before
    assert (repo / "feature.txt").exists()
    assert not wt.exists()


def test_submit_never_raises_when_verify_command_errors(tmp_path: Path) -> None:
    # A verify command that exits non-zero (here a bogus invocation) is a park,
    # never an exception escaping submit().
    repo = tmp_path / "repo"
    _init_repo(repo)
    base_before = _rev(repo, "main")
    s = _submitter(repo, verify_command="this-command-does-not-exist --x")
    tf = _task_file(repo, "01-phase", "t1")
    wt = _prepare_with_commit(s, tf, "t1", filename="feature.txt")

    s.submit(_submit_req(tf, "t1", wt, graders=None))  # must not raise

    assert _rev(repo, "main") == base_before
    assert wt.exists()

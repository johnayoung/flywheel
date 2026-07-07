"""Behavior: every successful landing, on every submit strategy, appends a
``Landed`` domain-event record carrying the landed reference; an incomplete
landing appends none (spec 00073, criteria 3 and 4).

The merge path runs real git against a tmp repo; the PR path adds a local bare
remote with the ``gh`` CLI replaced by a recording fake. In both, the store is a
recording ledger capturing appended domain events.

Criterion 3: the recorded reference for a merge land equals the base-branch head
the test observes via ``git rev-parse`` -- a value read from git, never one the
recorder invents. Criterion 4: a parked or errored land records nothing, so a
record written at submit-start (before the land completes) would fail these
tests.
"""

from __future__ import annotations

import json
import subprocess
from collections.abc import Sequence
from pathlib import Path

from flywheel_core import CommandGrader, Status, Task
from flywheel_core.events import DomainEvent, Landed
from flywheel_orchestrator import SandboxRequest, SubmitRequest

from flywheel_worktree import worker
from flywheel_worktree.pr import GitPullRequestSubmitter


# --- store stub ---------------------------------------------------------------


class _RecordingLedger:
    """Minimal LandingLedger stub capturing every appended domain event, enough
    to assert whether a run recorded a ``Landed`` witness."""

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

    def landed(self) -> list[Landed]:
        return [e for e in self.events if isinstance(e, Landed)]


# --- git / fixture helpers ----------------------------------------------------


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
    _git(path, "config", "user.email", "landing-test@example.com")
    _git(path, "config", "user.name", "landing test")
    _git(path, "config", "commit.gpgsign", "false")
    (path / "README.md").write_text("base\n")
    _git(path, "add", "-A")
    _git(path, "commit", "-m", "init")


def _init_repo_with_remote(path: Path) -> Path:
    _init_repo(path)
    remote = path.parent / "origin.git"
    subprocess.run(
        ["git", "init", "--bare", str(remote)],
        capture_output=True,
        check=True,
    )
    _git(path, "remote", "add", "origin", str(remote))
    return remote


def _rev(repo: Path, ref: str) -> str:
    return _git(repo, "rev-parse", ref)


def _commit(worktree: Path, filename: str, body: str, message: str) -> None:
    (worktree / filename).write_text(body)
    _git(worktree, "add", "-A")
    _git(worktree, "commit", "-m", message)


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


def _sandbox_req(tf: Path, task_id: str) -> SandboxRequest:
    return SandboxRequest(
        task_id=task_id, task_file=tf, run_id=None, mode="fresh"
    )


def _submit_req(
    tf: Path, task_id: str, sandbox: Path, status: Status
) -> SubmitRequest:
    return SubmitRequest(
        task_id=task_id,
        task_file=tf,
        task=Task(
            id=task_id,
            goal=f"Goal for {task_id}.",
            graders=[CommandGrader(run="true")],
        ),
        run_id="run-1",
        status=status,
        sandbox=sandbox,
    )


def _merge_submitter(
    repo: Path, ledger: _RecordingLedger, **kwargs: object
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
        store=ledger,  # type: ignore[arg-type]
        **kwargs,  # type: ignore[arg-type]
    )


class _FakeGh:
    """Recording ``gh`` runner: ``pr list`` returns nothing, ``pr create``
    returns the new PR URL."""

    def __init__(self) -> None:
        self.calls: list[list[str]] = []

    def __call__(self, argv: Sequence[str]) -> str:
        argv = list(argv)
        self.calls.append(argv)
        if argv[:2] == ["pr", "create"]:
            return "https://example.test/pr/7\n"
        return ""


def _pr_submitter(
    repo: Path, ledger: _RecordingLedger, gh: _FakeGh, **kwargs: object
) -> GitPullRequestSubmitter:
    worktrees = repo / ".flywheel" / "worktrees"
    worktrees.mkdir(parents=True, exist_ok=True)
    return GitPullRequestSubmitter(
        repo_root=repo,
        tasks_dir=repo / ".flywheel" / "tasks",
        worktrees_dir=worktrees,
        phase_base="main",
        lock_path=repo / ".flywheel" / ".merge.lock",
        log=lambda _m: None,
        gh=gh,
        store=ledger,  # type: ignore[arg-type]
        **kwargs,  # type: ignore[arg-type]
    )


# --- merge landings -----------------------------------------------------------


def test_clean_ff_merge_records_landed_with_base_head(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    ledger = _RecordingLedger()
    s = _merge_submitter(repo, ledger)
    tf = _task_file(repo, "01-phase", "t1")

    wt = s.prepare_sandbox(_sandbox_req(tf, "t1"))
    _commit(wt, "feature.txt", "x", "feat")

    s.submit(_submit_req(tf, "t1", wt, Status.DONE))

    # The land completed (base advanced, worktree cleaned) and left exactly one
    # Landed witness naming the "merge" strategy.
    landed = ledger.landed()
    assert len(landed) == 1
    assert landed[0].strategy == "merge"
    # Criterion 3: the recorded reference is the base head observed via git,
    # not a value the recorder invented.
    assert landed[0].landed_ref == _rev(repo, "main")


def test_post_rebase_merge_records_landed_with_base_head(
    tmp_path: Path,
) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    ledger = _RecordingLedger()
    s = _merge_submitter(repo, ledger)
    tf = _task_file(repo, "01-phase", "t1")

    wt = s.prepare_sandbox(_sandbox_req(tf, "t1"))
    _commit(wt, "feature.txt", "x", "feat")
    # Advance the base out from under the finished branch (non-conflicting) so
    # the clean fast-forward fails and submit rebases before landing.
    _commit(repo, "other.txt", "y", "base moves on")

    s.submit(_submit_req(tf, "t1", wt, Status.DONE))

    # The second land site (post-rebase FF) records the landed reference too.
    landed = ledger.landed()
    assert len(landed) == 1
    assert landed[0].strategy == "merge"
    assert landed[0].landed_ref == _rev(repo, "main")


def test_protected_path_park_records_no_landed(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    ledger = _RecordingLedger()
    s = _merge_submitter(repo, ledger, protected_paths=["conftest.py"])
    tf = _task_file(repo, "01-phase", "t1")

    wt = s.prepare_sandbox(_sandbox_req(tf, "t1"))
    _commit(wt, "conftest.py", "tampered", "edit conftest")
    base_before = _rev(repo, "main")

    s.submit(_submit_req(tf, "t1", wt, Status.DONE))

    # Criterion 4: the land was suppressed (base untouched), so no Landed
    # record exists -- only the park witness.
    assert _rev(repo, "main") == base_before
    assert ledger.landed() == []


def test_failed_status_records_no_landed(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    ledger = _RecordingLedger()
    s = _merge_submitter(repo, ledger)
    tf = _task_file(repo, "01-phase", "t1")

    wt = s.prepare_sandbox(_sandbox_req(tf, "t1"))
    _commit(wt, "feature.txt", "x", "feat")

    s.submit(_submit_req(tf, "t1", wt, Status.FAILED))

    # A non-DONE terminal never lands: nothing to record.
    assert ledger.landed() == []


# --- PR landings --------------------------------------------------------------


def test_pr_landing_records_landed_with_pr_reference(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo_with_remote(repo)
    ledger = _RecordingLedger()
    gh = _FakeGh()
    s = _pr_submitter(repo, ledger, gh)
    tf = _task_file(repo, "01-phase", "t1")

    wt = s.prepare_sandbox(_sandbox_req(tf, "t1"))
    _commit(wt, "feature.txt", "x", "feat")

    s.submit(_submit_req(tf, "t1", wt, Status.DONE))

    # The PR land records the pull-request identifier, not a commit sha.
    landed = ledger.landed()
    assert len(landed) == 1
    assert landed[0].strategy == "pr"
    assert landed[0].landed_ref == "https://example.test/pr/7"


def test_pr_push_failure_records_no_landed(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo_with_remote(repo)
    ledger = _RecordingLedger()
    gh = _FakeGh()
    # A remote that does not exist makes the real push fail before any PR opens.
    s = _pr_submitter(repo, ledger, gh, remote="nonexistent")
    tf = _task_file(repo, "01-phase", "t1")

    wt = s.prepare_sandbox(_sandbox_req(tf, "t1"))
    _commit(wt, "feature.txt", "x", "feat")

    s.submit(_submit_req(tf, "t1", wt, Status.DONE))

    # Push failed, so nothing landed: a push-failed park witness, no Landed.
    assert gh.calls == []
    assert ledger.landed() == []

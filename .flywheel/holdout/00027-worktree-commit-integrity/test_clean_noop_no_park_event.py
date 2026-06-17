"""Held-out acceptance test (spec 00027, criterion 5): a clean zero-commit DONE
run is a legitimate no-op — cleaned up, with NO park event appended.

When a run reaches terminal DONE with a clean worktree and zero commits beyond
the base, ``submit`` must remove the worktree and the task branch (the existing
no-op cleanup) and append ZERO ``LANDING_PARKED`` events to the run's ledger.
This is the counterpart to the dirty-tree park: the two dispositions must stay
externally distinct (D-4), and an implementation that parks unconditionally
(appending the event for every DONE) fails here.

Authored blind from the contract. The grader reads ONLY the store API and git
ref/worktree state, never captured stderr. Discriminators:
  * always-park (append the event for the clean no-op too) fails the
    zero-events assertion;
  * leaving the worktree/branch behind fails the cleanup assertions.

Outside the four pytest testpaths; collected explicitly by the grader.
"""

import json
import subprocess
from pathlib import Path

from flywheel_core import (
    CommandGrader,
    InMemoryStore,
    LandingParked,
    Lifecycle,
    SqliteStore,
    Status,
    Task,
)
from flywheel_orchestrator import SandboxRequest, SubmitRequest
from flywheel_worktree import worker

_RUN_ID = "run-cleannoop-1"
_TASK_ID = "t-noop"


def _git(cwd: Path, *args: str) -> str:
    return subprocess.run(
        ["git", "-C", str(cwd), *args],
        text=True,
        capture_output=True,
        check=True,
    ).stdout.strip()


def _init_repo(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True)
    _git(path, "init", "-b", "main")
    _git(path, "config", "user.email", "worker-test@example.invalid")
    _git(path, "config", "user.name", "worker test")
    _git(path, "config", "commit.gpgsign", "false")
    (path / "README.md").write_text("base\n")
    _git(path, "add", "-A")
    _git(path, "commit", "-m", "init")


def _submitter(repo: Path, store: object) -> "worker.GitWorktreeSubmitter":
    worktrees = repo / ".flywheel" / "worktrees"
    worktrees.mkdir(parents=True, exist_ok=True)
    return worker.GitWorktreeSubmitter(
        repo_root=repo,
        tasks_dir=repo / ".flywheel" / "tasks",
        worktrees_dir=worktrees,
        phase_base="main",
        lock_path=repo / ".flywheel" / ".merge.lock",
        log=lambda _m: None,
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


def _submit_req(tf: Path, sandbox: Path) -> SubmitRequest:
    return SubmitRequest(
        task_id=_TASK_ID,
        task_file=tf,
        task=Task(
            id=_TASK_ID,
            goal=f"Goal for {_TASK_ID}.",
            graders=[CommandGrader(run="true")],
        ),
        run_id=_RUN_ID,
        status=Status.DONE,
        sandbox=sandbox,
    )


def _run_one(store: object, repo: Path) -> None:
    _init_repo(repo)
    store.create_lifecycle(  # type: ignore[attr-defined]
        Lifecycle(task_id=_TASK_ID, run_id=_RUN_ID, status=Status.DONE)
    )

    s = _submitter(repo, store)
    tf = _task_file(repo, "01-phase", _TASK_ID)
    wt = s.prepare_sandbox(
        SandboxRequest(task_id=_TASK_ID, task_file=tf, run_id=None, mode="fresh")
    )
    branch = "flywheel/01-phase/" + _TASK_ID

    # Clean worktree, zero commits beyond base: the legitimate no-op path.
    assert _git(wt, "status", "--porcelain") == ""

    s.submit(_submit_req(tf, wt))

    # (a) NO LANDING_PARKED events were appended for the clean no-op.
    parked = [
        e
        for e in store.list_domain_events(_RUN_ID)  # type: ignore[attr-defined]
        if isinstance(e, LandingParked)
    ]
    assert parked == [], (
        f"a clean zero-commit no-op must append no LANDING_PARKED event; got {parked!r}"
    )

    # (b) The worktree and branch were cleaned up (the existing no-op behavior).
    assert not wt.exists(), "the clean no-op worktree must be removed"
    assert not s._branch_exists(branch), "the clean no-op task branch must be removed"


def test_clean_noop_no_park_event_sqlite(tmp_path: Path) -> None:
    store = SqliteStore(tmp_path / "store.db")
    try:
        _run_one(store, tmp_path / "repo")
    finally:
        store.close()


def test_clean_noop_no_park_event_memory(tmp_path: Path) -> None:
    _run_one(InMemoryStore(), tmp_path / "repo")

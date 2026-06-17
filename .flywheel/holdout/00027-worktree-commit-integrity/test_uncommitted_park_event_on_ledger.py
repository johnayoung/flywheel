"""Held-out acceptance test (spec 00027, criterion 4): the uncommitted-work
park is recorded as a queryable store domain event.

When a run reaches terminal DONE but its worktree still has uncommitted changes,
``submit`` must preserve the worktree AND append exactly one ``LANDING_PARKED``
domain event (``park_kind == "uncommitted-work"``) to the run's store ledger,
queryable via ``store.list_domain_events(run_id)`` — never only on stderr or a
git marker. The run stays terminal ``DONE`` (the event folds to the identity, no
transition).

Authored blind from the contract (D-6 / SI-12). The grader reads ONLY the store
API and git ref/worktree state, never captured stderr. Discriminators:
  * a fix that logs to stderr but appends no event fails the event assertion;
  * a fix that deletes the branch/worktree fails the preservation assertion;
  * minting a lifecycle status / transitioning off DONE fails ``status == DONE``;
  * the wrong ``park_kind`` (the divergent-base value) or a non-landing_parked
    kind fails the field assertions.

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

_RUN_ID = "run-uncommitted-1"
_TASK_ID = "t-unc"


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
    # Seed the run's lifecycle to terminal DONE in the same store the submitter
    # writes to (the run finalized DONE before submit).
    store.create_lifecycle(  # type: ignore[attr-defined]
        Lifecycle(task_id=_TASK_ID, run_id=_RUN_ID, status=Status.DONE)
    )

    s = _submitter(repo, store)
    tf = _task_file(repo, "01-phase", _TASK_ID)
    wt = s.prepare_sandbox(
        SandboxRequest(task_id=_TASK_ID, task_file=tf, run_id=None, mode="fresh")
    )
    branch = "flywheel/01-phase/" + _TASK_ID

    # Make the worktree dirty WITHOUT committing: an uncommitted tracked-file
    # edit plus an untracked file (both are dirty per `git status --porcelain`).
    (wt / "README.md").write_text("uncommitted edit\n")
    (wt / "scratch.txt").write_text("untracked\n")
    assert _git(wt, "status", "--porcelain") != ""

    # Full landing of a DONE run with a dirty tree. Must not raise.
    s.submit(_submit_req(tf, wt))

    # (a) Exactly one LANDING_PARKED event, park_kind == uncommitted-work, on
    #     the store ledger (read via the store API, never stderr).
    parked = [
        e
        for e in store.list_domain_events(_RUN_ID)  # type: ignore[attr-defined]
        if isinstance(e, LandingParked)
    ]
    assert len(parked) == 1, (
        f"expected exactly one LANDING_PARKED event on the ledger, got {parked!r}"
    )
    assert parked[0].park_kind == "uncommitted-work", (
        f"park_kind must be 'uncommitted-work', got {parked[0].park_kind!r}"
    )
    assert parked[0].detail.strip(), "the park event must carry a non-empty detail"

    # (b) The run stays terminal DONE — the event folded to the identity, no
    #     lifecycle transition was performed.
    after = store.load_lifecycle(_RUN_ID)  # type: ignore[attr-defined]
    assert after is not None
    assert after.status is Status.DONE, (
        f"run must stay terminal DONE after the park, got {after.status!r}"
    )

    # (c) The worktree directory and the task branch are preserved for forensics.
    assert wt.exists(), "the dirty worktree must be preserved, not removed"
    assert s._branch_exists(branch), "the task branch ref must be preserved"


def test_uncommitted_park_event_on_ledger_sqlite(tmp_path: Path) -> None:
    store = SqliteStore(tmp_path / "store.db")
    try:
        _run_one(store, tmp_path / "repo")
    finally:
        store.close()


def test_uncommitted_park_event_on_ledger_memory(tmp_path: Path) -> None:
    _run_one(InMemoryStore(), tmp_path / "repo")

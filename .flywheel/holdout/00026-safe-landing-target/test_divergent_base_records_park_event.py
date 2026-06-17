"""Held-out acceptance test (spec 00026, criterion 5): a divergent base that
cannot fast-forward after rebase + re-verify is recorded as a queryable park.

When the configured base has diverged such that the task branch cannot
fast-forward even after the existing rebase path, ``submit`` must append exactly
one ``LANDING_PARKED`` domain event (``park_kind == "divergent-base"``, non-empty
``detail``) to the run's store ledger, leave the worktree AND task branch
preserved, and perform NO lifecycle transition (the run stays terminal ``DONE``).

Authored blind from the contract (D-6 / SI-12). The grader reads ONLY the store
API and git ref/worktree state — never stderr. Discriminators: a stderr-only
park appends no event; an empty detail / wrong park_kind / non-landing_parked
kind fails the field assertions; transitioning off DONE fails ``status == DONE``;
discarding the worktree/branch fails preservation.

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

_RUN_ID = "run-divergent-1"
_TASK_ID = "t-div"
_BRANCH = "flywheel/01-phase/" + _TASK_ID


def _git(cwd: Path, *args: str) -> str:
    return subprocess.run(
        ["git", "-C", str(cwd), *args], text=True, capture_output=True, check=True
    ).stdout.strip()


def _init_repo(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True)
    _git(path, "init", "-b", "main")
    _git(path, "config", "user.email", "op@example.invalid")
    _git(path, "config", "user.name", "op")
    _git(path, "config", "commit.gpgsign", "false")
    (path / "conflict.txt").write_text("base\n")
    _git(path, "add", "-A")
    _git(path, "commit", "-m", "init")


def _submitter(repo: Path, store: object) -> "worker.GitWorktreeSubmitter":
    worktrees = repo / ".flywheel" / "worktrees"
    worktrees.mkdir(parents=True, exist_ok=True)
    return worker.GitWorktreeSubmitter(
        repo_root=repo,
        tasks_dir=repo / ".flywheel" / "tasks",
        worktrees_dir=worktrees,
        phase_base="landing-base",
        lock_path=repo / ".flywheel" / ".merge.lock",
        log=lambda _m: None,
        store=store,  # type: ignore[arg-type]
    )


def _task_file(repo: Path) -> Path:
    tf = repo / ".flywheel" / "tasks" / "active" / "01-phase" / f"{_TASK_ID}.json"
    tf.parent.mkdir(parents=True, exist_ok=True)
    tf.write_text(
        json.dumps(
            {"id": _TASK_ID, "goal": "g.", "graders": [{"type": "command", "run": "true"}]}
        )
    )
    return tf


def _submit_req(tf: Path, sandbox: Path) -> SubmitRequest:
    return SubmitRequest(
        task_id=_TASK_ID,
        task_file=tf,
        task=Task(id=_TASK_ID, goal="g.", graders=[CommandGrader(run="true")]),
        run_id=_RUN_ID,
        status=Status.DONE,
        sandbox=sandbox,
    )


def _advance_base_with_conflict(repo: Path) -> None:
    """Add a commit to the (non-checked-out) landing-base that conflicts with
    the task's change, via a throwaway worktree."""
    tmp = repo / ".flywheel" / "_tmp_base_wt"
    _git(repo, "worktree", "add", str(tmp), "landing-base")
    try:
        (tmp / "conflict.txt").write_text("base-advanced-divergently\n")
        _git(tmp, "add", "-A")
        _git(tmp, "commit", "-m", "base: divergent change to conflict.txt")
    finally:
        _git(repo, "worktree", "remove", "--force", str(tmp))


def _run_one(store: object, repo: Path) -> None:
    _init_repo(repo)
    # Operator works on a branch that is NOT the landing base.
    _git(repo, "branch", "landing-base", "main")
    _git(repo, "checkout", "-b", "op-work")

    store.create_lifecycle(  # type: ignore[attr-defined]
        Lifecycle(task_id=_TASK_ID, run_id=_RUN_ID, status=Status.DONE)
    )

    s = _submitter(repo, store)
    tf = _task_file(repo)
    wt = s.prepare_sandbox(
        SandboxRequest(task_id=_TASK_ID, task_file=tf, run_id=None, mode="fresh")
    )
    # Task changes conflict.txt and commits (branched off the original base).
    (wt / "conflict.txt").write_text("task-change\n")
    _git(wt, "add", "-A")
    _git(wt, "commit", "-m", "feat: task changes conflict.txt")

    # Now advance the base divergently so the task cannot FF and a rebase onto
    # the base conflicts.
    _advance_base_with_conflict(repo)

    base_before = _git(repo, "rev-parse", "landing-base")

    s.submit(_submit_req(tf, wt))

    # (a) Exactly one LANDING_PARKED event, park_kind == divergent-base.
    parked = [
        e
        for e in store.list_domain_events(_RUN_ID)  # type: ignore[attr-defined]
        if isinstance(e, LandingParked)
    ]
    assert len(parked) == 1, (
        f"expected exactly one LANDING_PARKED event on the ledger, got {parked!r}"
    )
    assert parked[0].park_kind == "divergent-base", (
        f"park_kind must be 'divergent-base', got {parked[0].park_kind!r}"
    )
    assert parked[0].detail.strip(), "the park event must carry a non-empty detail"

    # (b) The run stays terminal DONE (no transition).
    after = store.load_lifecycle(_RUN_ID)  # type: ignore[attr-defined]
    assert after is not None
    assert after.status is Status.DONE

    # (c) The worktree and the task branch are preserved for forensics; the base
    #     ref was not advanced by the failed landing.
    assert wt.exists(), "the divergent worktree must be preserved"
    assert s._branch_exists(_BRANCH), "the task branch must be preserved"
    assert _git(repo, "rev-parse", "landing-base") == base_before


def test_divergent_base_records_park_event_sqlite(tmp_path: Path) -> None:
    store = SqliteStore(tmp_path / "store.db")
    try:
        _run_one(store, tmp_path / "repo")
    finally:
        store.close()


def test_divergent_base_records_park_event_memory(tmp_path: Path) -> None:
    _run_one(InMemoryStore(), tmp_path / "repo")

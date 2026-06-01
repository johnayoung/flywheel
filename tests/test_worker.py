"""Tests for the git-worktree consumer ``.workflow/worker.py``.

Two layers:

* Unit tests drive :class:`GitWorktreeSubmitter` against a real git repo in a
  tmp dir (the test plays the agent, making commits in the worktree), covering
  worktree create, reuse-on-retry + rebase-onto-advanced-base, FF-merge on
  DONE, park on FAILED, zero-commit DONE cleanup, and uncommitted-DONE park.
* One integration test runs a full ``run_once`` cycle with a fake invoke that
  commits in the sandbox, asserting the task reaches DONE and its branch
  FF-merges into the base.

``.workflow/worker.py`` is not an importable package module, so it is loaded
via a ``sys.path`` insert of the ``.workflow`` directory.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

from claude_agent_sdk import AssistantMessage, ResultMessage, TextBlock

from flywheel import (
    Intent,
    InvocationRequest,
    InvocationSignals,
    IterationResult,
    SandboxRequest,
    Status,
    SubmitRequest,
    ValidEnvelope,
)
from flywheel.store_sqlite import SqliteStore

_WORKFLOW_DIR = Path(__file__).resolve().parents[1] / ".workflow"
if str(_WORKFLOW_DIR) not in sys.path:
    sys.path.insert(0, str(_WORKFLOW_DIR))

import worker  # noqa: E402  (path-dependent import)


# --- git helpers ------------------------------------------------------------


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


def _submitter(repo: Path) -> "worker.GitWorktreeSubmitter":
    worktrees = repo / ".workflow" / "worktrees"
    worktrees.mkdir(parents=True, exist_ok=True)
    return worker.GitWorktreeSubmitter(
        repo_root=repo,
        tasks_dir=repo / ".workflow" / "tasks",
        worktrees_dir=worktrees,
        phase_base="main",
        lock_path=repo / ".workflow" / ".merge.lock",
        log=lambda _m: None,
    )


def _task_file(repo: Path, phase: str, task_id: str, *, grader: str = "true") -> Path:
    tf = repo / ".workflow" / "tasks" / "active" / phase / f"{task_id}.json"
    tf.parent.mkdir(parents=True, exist_ok=True)
    tf.write_text(
        json.dumps(
            {
                "id": task_id,
                "goal": f"Goal for {task_id}.",
                "graders": [{"type": "command", "run": grader}],
            }
        )
    )
    return tf


def _sandbox_req(tf: Path, task_id: str, mode: str = "fresh") -> SandboxRequest:
    return SandboxRequest(
        task_id=task_id,
        task_file=tf,
        run_id=None if mode == "fresh" else "run-1",
        mode=mode,  # type: ignore[arg-type]
    )


def _submit_req(
    tf: Path, task_id: str, sandbox: Path, status: Status
) -> SubmitRequest:
    return SubmitRequest(
        task_id=task_id,
        task_file=tf,
        run_id="run-1",
        status=status,
        sandbox=sandbox,
    )


# --- phase derivation -------------------------------------------------------


def test_phase_of_task_file(tmp_path: Path) -> None:
    tasks = tmp_path / "tasks"
    nested = tasks / "active" / "03-iso" / "t.json"
    root = tasks / "active" / "t.json"
    assert worker.phase_of_task_file(nested, tasks) == "03-iso"
    assert worker.phase_of_task_file(root, tasks) == "_root"


# --- prepare ----------------------------------------------------------------


def test_prepare_creates_worktree_and_branch(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _submitter(repo)
    tf = _task_file(repo, "01-phase", "t1")

    wt = s.prepare(_sandbox_req(tf, "t1"))

    assert wt == repo / ".workflow" / "worktrees" / "t1"
    assert wt.is_dir()
    assert s._branch_exists("flywheel/01-phase/t1")
    assert s._is_registered_worktree(wt)


def test_prepare_refuses_to_clobber_unregistered_dir(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _submitter(repo)
    tf = _task_file(repo, "01-phase", "t1")
    # Branch exists, and a stray directory of the same name exists, but it is
    # not a registered worktree -> refuse rather than destroy operator state.
    _git(repo, "branch", "flywheel/01-phase/t1", "main")
    (repo / ".workflow" / "worktrees" / "t1").mkdir(parents=True)

    raised = False
    try:
        s.prepare(_sandbox_req(tf, "t1"))
    except worker.PrepareSandboxError:
        raised = True
    assert raised


# --- submit: FF merge on DONE -----------------------------------------------


def test_submit_ff_merges_on_done(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _submitter(repo)
    tf = _task_file(repo, "01-phase", "t1")

    wt = s.prepare(_sandbox_req(tf, "t1"))
    _commit(wt, "feature.txt", "x", "feat")
    base_before = _rev(repo, "main")

    s.submit(_submit_req(tf, "t1", wt, Status.DONE))

    assert _rev(repo, "main") != base_before
    assert (repo / "feature.txt").exists()
    assert not wt.exists()
    assert not s._branch_exists("flywheel/01-phase/t1")


def test_submit_parks_on_failed(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _submitter(repo)
    tf = _task_file(repo, "01-phase", "t1")

    wt = s.prepare(_sandbox_req(tf, "t1"))
    _commit(wt, "feature.txt", "x", "feat")
    base_before = _rev(repo, "main")

    s.submit(_submit_req(tf, "t1", wt, Status.FAILED))

    # Parked for forensics: worktree + branch survive, base untouched.
    assert wt.exists()
    assert s._branch_exists("flywheel/01-phase/t1")
    assert _rev(repo, "main") == base_before


def test_submit_zero_commit_done_cleans_up(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _submitter(repo)
    tf = _task_file(repo, "01-phase", "t1")

    wt = s.prepare(_sandbox_req(tf, "t1"))
    base_before = _rev(repo, "main")
    # No commits beyond base; clean tree.
    s.submit(_submit_req(tf, "t1", wt, Status.DONE))

    assert not wt.exists()
    assert not s._branch_exists("flywheel/01-phase/t1")
    assert _rev(repo, "main") == base_before


def test_submit_uncommitted_done_parks(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _submitter(repo)
    tf = _task_file(repo, "01-phase", "t1")

    wt = s.prepare(_sandbox_req(tf, "t1"))
    (wt / "dirty.txt").write_text("uncommitted")  # not committed
    base_before = _rev(repo, "main")

    s.submit(_submit_req(tf, "t1", wt, Status.DONE))

    # Uncommitted at DONE -> parked, not merged.
    assert wt.exists()
    assert s._branch_exists("flywheel/01-phase/t1")
    assert _rev(repo, "main") == base_before


# --- reuse on retry + rebase onto advanced base -----------------------------


def test_prepare_reuses_and_rebases_onto_advanced_base(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _submitter(repo)
    tf = _task_file(repo, "01-phase", "t1")

    # Attempt 1: commit work, then fail -> worktree parked with the branch.
    wt = s.prepare(_sandbox_req(tf, "t1"))
    _commit(wt, "a.txt", "a", "task work")
    s.submit(_submit_req(tf, "t1", wt, Status.FAILED))
    assert wt.exists()

    # Base advances independently (a peer task merged b.txt).
    _commit(repo, "b.txt", "b", "advance main")
    main_after_advance = _rev(repo, "main")

    # Retry: prepare reuses the parked worktree and rebases it onto the
    # advanced base, carrying the prior commit forward on top.
    wt2 = s.prepare(_sandbox_req(tf, "t1", mode="resume"))
    assert wt2 == wt
    assert (wt2 / "a.txt").exists()  # prior work preserved
    assert (wt2 / "b.txt").exists()  # base advance picked up by the rebase
    # Branch is exactly one commit ahead of the advanced base.
    assert _git(repo, "rev-list", "--count", "main..flywheel/01-phase/t1") == "1"

    # Now it succeeds: FF-merges cleanly onto the advanced base.
    s.submit(_submit_req(tf, "t1", wt2, Status.DONE))
    assert (repo / "a.txt").exists()
    assert (repo / "b.txt").exists()
    assert not wt2.exists()
    assert not s._branch_exists("flywheel/01-phase/t1")
    # Base moved forward from the advance (the task commit FF'd on top).
    assert _rev(repo, "main") != main_after_advance


def test_submit_never_raises_on_git_error(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _submitter(repo)
    tf = _task_file(repo, "01-phase", "gone")
    # Sandbox that is not a git worktree at all: submit must swallow the error.
    bogus = tmp_path / "not-a-worktree"
    bogus.mkdir()
    s.submit(_submit_req(tf, "gone", bogus, Status.DONE))  # must not raise


# --- integration: a full run_once cycle -------------------------------------


def _signals() -> InvocationSignals:
    return InvocationSignals(
        stop_reason="end_turn",
        num_turns=1,
        total_cost_usd=0.0,
        result_is_error=False,
        result_subtype="success",
        api_error_status=None,
        session_id="sess",
    )


def _messages() -> tuple[object, ...]:
    return (
        AssistantMessage(
            content=[TextBlock(text="done")],
            model="claude-test",
            stop_reason="end_turn",
            session_id="sess",
            usage=None,
        ),
        ResultMessage(
            subtype="success",
            duration_ms=1,
            duration_api_ms=1,
            is_error=False,
            num_turns=1,
            session_id="sess",
            stop_reason="end_turn",
            total_cost_usd=0.0,
            usage=None,
        ),
    )


def test_run_once_merges_completed_task(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _submitter(repo)
    tf = _task_file(repo, "01-phase", "t1", grader="true")
    worktree = repo / ".workflow" / "worktrees" / "t1"

    async def _invoke(request: InvocationRequest) -> IterationResult:
        # Play the agent: commit work in the prepared worktree, then claim
        # completion so the harness verifies (grader "true" passes) -> DONE.
        _commit(worktree, "work.txt", "agent output", "agent work")
        return IterationResult(
            transcript="ok",
            messages=_messages(),  # type: ignore[arg-type]
            envelope=ValidEnvelope(intent=Intent.VERIFY),
            signals=_signals(),
            failure=None,
        )

    db_path = repo / ".workflow" / "flywheel.sqlite"
    base_before = _rev(repo, "main")

    report = worker.run_once(
        s,
        tasks_dir=repo / ".workflow" / "tasks",
        db_path=db_path,
        worktrees_dir=repo / ".workflow" / "worktrees",
        model=None,
        max_turns=4,
        max_retries=0,
        invoke=_invoke,
    )

    assert [r.status for r in report.runs] == [Status.DONE]

    # The branch FF-merged into main and the worktree was cleaned up.
    assert (repo / "work.txt").exists()
    assert _rev(repo, "main") != base_before
    assert not worktree.exists()
    assert not s._branch_exists("flywheel/01-phase/t1")

    # The store agrees the task is done.
    store = SqliteStore(db_path)
    try:
        cur = store._connection.execute(  # noqa: SLF001
            "SELECT status FROM lifecycles WHERE task_id = 't1' "
            "ORDER BY updated_at DESC LIMIT 1"
        )
        assert cur.fetchone()["status"] == Status.DONE.value
    finally:
        store.close()

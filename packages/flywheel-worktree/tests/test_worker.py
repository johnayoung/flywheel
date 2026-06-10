"""Tests for the git-worktree consumer ``flywheel_worktree.worker``.

Two layers:

* Unit tests drive :class:`GitWorktreeSubmitter` against a real git repo in a
  tmp dir (the test plays the agent, making commits in the worktree), covering
  worktree create, reuse-on-retry + rebase-onto-advanced-base, FF-merge on
  DONE, park on FAILED, zero-commit DONE cleanup, and uncommitted-DONE park.
* One integration test runs a full ``run_once`` cycle with a fake invoke that
  commits in the sandbox, asserting the task reaches DONE and its branch
  FF-merges into the base.

The worker ships in the ``flywheel-worktree`` package (library only after the
unified-CLI cutover: launched via ``flywheel worker``); tests import it
directly as ``flywheel_worktree.worker``.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

from claude_agent_sdk import AssistantMessage, ResultMessage, TextBlock

from flywheel import (
    Intent,
    InvocationRequest,
    InvocationSignals,
    IterationResult,
    Status,
    ValidEnvelope,
)
from flywheel.store_sqlite import SqliteStore
from flywheel_orchestrator import SandboxRequest, SubmitRequest
from flywheel_worktree import worker


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


def _task_file(repo: Path, phase: str, task_id: str, *, grader: str = "true") -> Path:
    tf = repo / ".flywheel" / "tasks" / "active" / phase / f"{task_id}.json"
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

    assert wt == repo / ".flywheel" / "worktrees" / "t1"
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
    (repo / ".flywheel" / "worktrees" / "t1").mkdir(parents=True)

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
    worktree = repo / ".flywheel" / "worktrees" / "t1"

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

    db_path = repo / ".flywheel" / "flywheel.sqlite"
    base_before = _rev(repo, "main")

    report = worker.run_once(
        s,
        tasks_dir=repo / ".flywheel" / "tasks",
        db_path=db_path,
        worktrees_dir=repo / ".flywheel" / "worktrees",
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


def test_record_phase_bases_captures_once_and_idempotent(tmp_path: Path) -> None:
    """The worker's per-cycle base-capture step records the pre-merge HEAD
    once per phase dir and never moves it forward on re-run.

    Mirrors the production sequence: after ``commit_task_files`` lands new
    task JSON, the worker captures the resulting ``HEAD`` SHA into
    ``.loop-base`` for each active phase that lacks one, then commits the
    dotfile. The recorded SHA is the base the future archive gate diffs
    against -- it must be locked in at phase entry, not derived later when
    task branches have already been merged.
    """
    repo = tmp_path / "repo"
    _init_repo(repo)
    tasks_dir = repo / ".flywheel" / "tasks"
    phase_dir = tasks_dir / "active" / "01-phase"
    phase_dir.mkdir(parents=True)
    lock_path = repo / ".flywheel" / ".merge.lock"

    head_at_capture = _rev(repo, "main")

    worker.record_phase_bases(repo, tasks_dir, lock_path, lambda _m: None)

    base_file = phase_dir / ".loop-base"
    assert base_file.is_file()
    # The recorded SHA is the pre-merge HEAD (the captured commit itself
    # advances HEAD past it, which is fine -- the captured SHA must be
    # what HEAD pointed at *before* the capture).
    assert base_file.read_text().strip() == head_at_capture
    # The dotfile was committed (no untracked entry remains).
    status = subprocess.run(
        ["git", "-C", str(repo), "status", "--porcelain", "--", str(base_file)],
        text=True,
        capture_output=True,
        check=True,
    ).stdout
    assert status == ""

    # Advance base by an unrelated commit (simulating a task FF-merge).
    _commit(repo, "advance.txt", "x", "advance main")
    assert _rev(repo, "main") != head_at_capture

    # Re-run: the dotfile already exists. Must NOT move the recorded base
    # forward -- the first-seen SHA is the true base.
    worker.record_phase_bases(repo, tasks_dir, lock_path, lambda _m: None)
    assert base_file.read_text().strip() == head_at_capture


def test_run_once_writes_per_run_log(tmp_path: Path) -> None:
    """A full ``run_once`` cycle must drop a per-run forensics file keyed to
    the executed run_id under ``log_dir``. Restoration of the behavior the
    legacy bash worker lost when ``.workflow/task-worker.sh`` was collapsed
    into the in-process Python worker."""
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _submitter(repo)
    tf = _task_file(repo, "01-phase", "t1", grader="true")
    worktree = repo / ".flywheel" / "worktrees" / "t1"

    async def _invoke(request: InvocationRequest) -> IterationResult:
        _commit(worktree, "work.txt", "agent output", "agent work")
        return IterationResult(
            transcript="ok",
            messages=_messages(),  # type: ignore[arg-type]
            envelope=ValidEnvelope(intent=Intent.VERIFY),
            signals=_signals(),
            failure=None,
        )

    db_path = repo / ".flywheel" / "flywheel.sqlite"
    log_dir = repo / "logs" / "worker"
    assert not log_dir.exists()  # fresh checkout: dir does not exist yet

    report = worker.run_once(
        s,
        tasks_dir=repo / ".flywheel" / "tasks",
        db_path=db_path,
        worktrees_dir=repo / ".flywheel" / "worktrees",
        model=None,
        max_turns=4,
        max_retries=0,
        invoke=_invoke,
        log_dir=log_dir,
    )

    assert len(report.runs) == 1
    run_id = report.runs[0].run_id

    # The directory was created on demand and a log file keyed to the
    # executed run_id exists, is non-empty, and contains the run header.
    assert log_dir.is_dir()
    short_hash = worker._run_id_hash(run_id)
    matches = sorted(log_dir.glob(f"t1_{short_hash}_*.log"))
    assert len(matches) == 1, f"expected one log file, got {matches}"
    log_file = matches[0]
    body = log_file.read_text()
    assert body
    assert f"run_id={run_id}" in body
    assert "task_id=t1" in body


# --- heartbeat rendering ----------------------------------------------------


_LIVE_TS_DEFAULT = object()


def _live_row(
    *,
    task_id: str = "task-x",
    attempt: int | None = 1,
    iteration: int | None = 2,
    last_kind: str = "ASSISTANT",
    last_detail: str = "Edit(file_path=x.py)",
    last_ts: object = _LIVE_TS_DEFAULT,
    tokens_total: int = 0,
    cost_usd_total: float = 0.0,
    turns_total: int = 0,
    iterations_completed: int = 0,
) -> object:
    """Build a ``flywheel.workflow.LiveRunRow`` without spinning a store."""
    from datetime import datetime, timezone

    from flywheel_orchestrator import LiveRunRow

    if last_ts is _LIVE_TS_DEFAULT:
        ts_val = datetime.now(timezone.utc)
    else:
        ts_val = last_ts  # type: ignore[assignment]
    return LiveRunRow(
        run_id=f"run-{task_id}",
        task_id=task_id,
        status=Status.RUNNING,
        attempt=attempt,
        iteration=iteration,
        last_kind=last_kind,
        last_detail=last_detail,
        last_ts=ts_val,  # type: ignore[arg-type]
        tokens_total=tokens_total,
        cost_usd_total=cost_usd_total,
        turns_total=turns_total,
        iterations_completed=iterations_completed,
    )


def test_heartbeat_renders_breadcrumb_totals_and_action() -> None:
    from datetime import datetime, timezone

    row = _live_row(
        task_id="task-h",
        attempt=2,
        iteration=3,
        tokens_total=2500,
        cost_usd_total=0.123456,
        turns_total=7,
        iterations_completed=3,
    )
    line = worker._format_heartbeat(row, datetime.now(timezone.utc))  # type: ignore[arg-type]
    assert "task-h" in line
    assert "attempt=2" in line
    assert "iter=3" in line
    assert "tokens=2500" in line
    assert "cost=$0.1235" in line  # 4dp rendering
    assert "turns=7" in line
    assert "ASSISTANT" in line


def test_heartbeat_renders_dashes_when_no_iteration_event_yet() -> None:
    from datetime import datetime, timezone

    row = _live_row(
        attempt=1,
        iteration=1,
        iterations_completed=0,
        tokens_total=0,
        cost_usd_total=0.0,
        turns_total=0,
    )
    line = worker._format_heartbeat(row, datetime.now(timezone.utc))  # type: ignore[arg-type]
    assert "cost=--" in line
    assert "tokens=0" in line
    assert "turns=0" in line


def test_heartbeat_unknown_breadcrumb_renders_question_marks() -> None:
    from datetime import datetime, timezone

    row = _live_row(
        attempt=None,
        iteration=None,
        last_kind="(none)",
        last_detail="(no activity yet)",
        last_ts=None,
    )
    line = worker._format_heartbeat(row, datetime.now(timezone.utc))  # type: ignore[arg-type]
    assert "attempt=?" in line
    assert "iter=?" in line
    assert "age=—" in line


def test_heartbeat_truncates_overlong_detail() -> None:
    from datetime import datetime, timezone

    row = _live_row(last_detail="z" * 5000)
    line = worker._format_heartbeat(row, datetime.now(timezone.utc))  # type: ignore[arg-type]
    # Belt-and-braces cap: the heartbeat constant + reasonable prefix.
    assert len(line) < worker._HEARTBEAT_DETAIL_MAX_WIDTH + 200

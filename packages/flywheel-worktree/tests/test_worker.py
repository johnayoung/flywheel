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

from flywheel_core import (
    CommandGrader,
    Intent,
    InvocationRequest,
    InvocationSignals,
    IterationResult,
    Status,
    Task,
    ValidEnvelope,
)
from flywheel_core.store_sqlite import SqliteStore
from flywheel_orchestrator import (
    FilesystemHeldOutGraderSource,
    SandboxRequest,
    StoreConfigError,
    SubmitRequest,
    SubmitStrategy,
    WorkPolicy,
    load_effective_policy,
)
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
    tf: Path,
    task_id: str,
    sandbox: Path,
    status: Status,
    *,
    grader: str = "true",
) -> SubmitRequest:
    return SubmitRequest(
        task_id=task_id,
        task_file=tf,
        task=Task(
            id=task_id,
            goal=f"Goal for {task_id}.",
            graders=[CommandGrader(run=grader)],
        ),
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

    wt = s.prepare_sandbox(_sandbox_req(tf, "t1"))

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
        s.prepare_sandbox(_sandbox_req(tf, "t1"))
    except worker.PrepareSandboxError:
        raised = True
    assert raised


# --- submit: FF merge on DONE -----------------------------------------------


def test_submit_ff_merges_on_done(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _submitter(repo)
    tf = _task_file(repo, "01-phase", "t1")

    wt = s.prepare_sandbox(_sandbox_req(tf, "t1"))
    _commit(wt, "feature.txt", "x", "feat")
    base_before = _rev(repo, "main")

    s.submit(_submit_req(tf, "t1", wt, Status.DONE))

    assert _rev(repo, "main") != base_before
    assert (repo / "feature.txt").exists()
    assert not wt.exists()
    assert not s._branch_exists("flywheel/01-phase/t1")


def test_submit_cleans_branch_when_base_not_checked_out(tmp_path: Path) -> None:
    # Safe-landing config: the landing base ("main") is NOT the operator's
    # checked-out branch, so _ff_merge advances it out-of-tree. _cleanup must
    # still delete the landed branch. ``git branch -d`` would check mergedness
    # against the checked-out HEAD ("operator") rather than the landing base and
    # refuse, silently leaking the ref; ``-D`` deletes against the established
    # containment.
    repo = tmp_path / "repo"
    _init_repo(repo)
    # Move the operator's HEAD off "main" so the landing base is not checked out.
    _git(repo, "checkout", "-b", "operator")
    s = _submitter(repo)
    tf = _task_file(repo, "01-phase", "t1")

    wt = s.prepare_sandbox(_sandbox_req(tf, "t1"))
    _commit(wt, "feature.txt", "x", "feat")
    base_before = _rev(repo, "main")

    s.submit(_submit_req(tf, "t1", wt, Status.DONE))

    assert _rev(repo, "main") != base_before  # base advanced out-of-tree
    assert not wt.exists()
    assert not s._branch_exists("flywheel/01-phase/t1")  # ref not leaked


def test_submit_parks_on_failed(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _submitter(repo)
    tf = _task_file(repo, "01-phase", "t1")

    wt = s.prepare_sandbox(_sandbox_req(tf, "t1"))
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

    wt = s.prepare_sandbox(_sandbox_req(tf, "t1"))
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

    wt = s.prepare_sandbox(_sandbox_req(tf, "t1"))
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
    wt = s.prepare_sandbox(_sandbox_req(tf, "t1"))
    _commit(wt, "a.txt", "a", "task work")
    s.submit(_submit_req(tf, "t1", wt, Status.FAILED))
    assert wt.exists()

    # Base advances independently (a peer task merged b.txt).
    _commit(repo, "b.txt", "b", "advance main")
    main_after_advance = _rev(repo, "main")

    # Retry: prepare reuses the parked worktree and rebases it onto the
    # advanced base, carrying the prior commit forward on top.
    wt2 = s.prepare_sandbox(_sandbox_req(tf, "t1", mode="resume"))
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


# --- submit: post-rebase re-verification --------------------------------------


def test_submit_rebase_reverifies_then_merges(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _submitter(repo)
    tf = _task_file(repo, "01-phase", "t2")

    wt = s.prepare_sandbox(_sandbox_req(tf, "t2"))
    _commit(wt, "feature2.txt", "x", "t2 work")
    # A peer task merges first: the base advances past t2's branch point,
    # so t2's FF fails and submit must rebase.
    _commit(repo, "feature1.txt", "y", "peer merged")

    # This grader passes only against the rebased tree (it needs the peer's
    # file), proving re-verification ran against the exact base landed on.
    s.submit(
        _submit_req(
            tf,
            "t2",
            wt,
            Status.DONE,
            grader="test -f feature1.txt && test -f feature2.txt",
        )
    )

    assert (repo / "feature1.txt").exists()
    assert (repo / "feature2.txt").exists()
    assert not wt.exists()
    assert not s._branch_exists("flywheel/01-phase/t2")


def test_submit_rebase_reverify_failure_parks(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _submitter(repo)
    tf = _task_file(repo, "01-phase", "t2")

    wt = s.prepare_sandbox(_sandbox_req(tf, "t2"))
    _commit(wt, "feature2.txt", "x", "t2 work")
    _commit(repo, "feature1.txt", "y", "peer merged")
    base_after_peer = _rev(repo, "main")

    # The grader held against t2's original tree but is contradicted by the
    # peer's change — the semantic conflict a textually clean rebase hides.
    s.submit(
        _submit_req(
            tf, "t2", wt, Status.DONE, grader="test ! -f feature1.txt"
        )
    )

    # Re-verification failed: parked for forensics, base untouched.
    assert wt.exists()
    assert s._branch_exists("flywheel/01-phase/t2")
    assert _rev(repo, "main") == base_after_peer
    assert not (repo / "feature2.txt").exists()


def test_submit_clean_ff_skips_reverify(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _submitter(repo)
    marker = tmp_path / "reverify-ran"
    tf = _task_file(repo, "01-phase", "t1")

    wt = s.prepare_sandbox(_sandbox_req(tf, "t1"))
    _commit(wt, "feature.txt", "x", "feat")

    s.submit(_submit_req(tf, "t1", wt, Status.DONE, grader=f"touch {marker}"))

    # Base never advanced: pure FF, the in-run receipts already describe the
    # exact tree that landed, so the graders must not re-run.
    assert (repo / "feature.txt").exists()
    assert not marker.exists()


def test_submit_rebase_with_no_command_graders_merges(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _submitter(repo)
    tf = _task_file(repo, "01-phase", "t2")

    wt = s.prepare_sandbox(_sandbox_req(tf, "t2"))
    _commit(wt, "feature2.txt", "x", "t2 work")
    _commit(repo, "feature1.txt", "y", "peer merged")

    # No command graders: nothing tree-dependent to re-check after the
    # rebase; the merge proceeds.
    req = SubmitRequest(
        task_id="t2",
        task_file=tf,
        task=Task(id="t2", goal="Goal for t2.", graders=[]),
        run_id="run-1",
        status=Status.DONE,
        sandbox=wt,
    )
    s.submit(req)

    assert (repo / "feature2.txt").exists()
    assert not wt.exists()


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
    _task_file(repo, "01-phase", "t1", grader="true")
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
    """The worker's per-cycle base-capture step records HEAD once per phase
    as a ``refs/flywheel/loop-base/<phase>`` ref and never moves it forward
    on re-run.

    The recorded SHA is the base the future archive gate diffs against --
    it must be locked in at phase entry, not derived later when task
    branches have already been merged. Pure ref plumbing: the capture
    creates no commits and leaves the operator's working tree untouched.
    """
    repo = tmp_path / "repo"
    _init_repo(repo)
    tasks_dir = repo / ".flywheel" / "tasks"
    phase_dir = tasks_dir / "active" / "01-phase"
    phase_dir.mkdir(parents=True)
    lock_path = repo / ".flywheel" / ".merge.lock"

    head_at_capture = _rev(repo, "main")

    worker.record_phase_bases(repo, tasks_dir, lock_path, lambda _m: None)

    # Recorded as a ref, not a working-tree file, and no commit was made.
    assert not (phase_dir / ".loop-base").exists()
    assert _rev(repo, "refs/flywheel/loop-base/01-phase") == head_at_capture
    assert _rev(repo, "main") == head_at_capture

    # Advance base by an unrelated commit (simulating a task FF-merge).
    _commit(repo, "advance.txt", "x", "advance main")
    assert _rev(repo, "main") != head_at_capture

    # Re-run: the base is already recorded. Must NOT move it forward --
    # the first-seen SHA is the true base.
    worker.record_phase_bases(repo, tasks_dir, lock_path, lambda _m: None)
    assert _rev(repo, "refs/flywheel/loop-base/01-phase") == head_at_capture


def test_run_once_produces_run_jsonl_and_no_log_files(
    tmp_path: Path,
) -> None:
    """Spec 00025 FR-9: the per-run ``.log`` re-render is gone. A full
    ``run_once`` cycle leaves the run's telemetry JSONL (written by the
    harness's sink under ``<db dir>/logs/runs/``) as the only telemetry
    artifact, and no ``.log`` file is produced anywhere in the repo."""
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _submitter(repo)
    _task_file(repo, "01-phase", "t1", grader="true")
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

    assert len(report.runs) == 1
    run_id = report.runs[0].run_id

    # The run telemetry JSONL exists and carries the run's stream.
    run_file = db_path.parent / "logs" / "runs" / f"{run_id}.jsonl"
    assert run_file.is_file()
    assert run_file.read_text(encoding="utf-8").strip()

    # FR-9 acceptance: no .log files are produced by a run.
    assert sorted((repo / ".flywheel").rglob("*.log")) == []
    assert not (repo / "logs").exists()


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
    """Build a ``flywheel_core.workflow.LiveRunRow`` without spinning a store."""
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


# --- headless model resolution ---------------------------------------------


import argparse  # noqa: E402

import pytest  # noqa: E402


def _args(model: str | None = None) -> argparse.Namespace:
    """Build an ``argparse.Namespace`` shaped like ``_build_parser``'s
    output, but trimmed to the fields :func:`_resolve_model` reads."""

    return argparse.Namespace(model=model)


def test_resolve_model_uses_flag_when_provided(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Explicit ``--model`` wins even when ``flywheel.toml`` pins a
    different id -- the documented "CLI flags always override" contract."""

    monkeypatch.chdir(tmp_path)
    (tmp_path / "flywheel.toml").write_text(
        '[source]\nkind = "directory"\n[agent]\nmodel = "claude-from-policy"\n'
    )
    assert worker._resolve_model(
        _args(model="claude-from-flag"), load_effective_policy()
    ) == ("claude-from-flag")


def test_resolve_model_honors_policy_when_flag_absent(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The headless ``flywheel worker`` path picks up
    ``[agent] model`` from the cwd's ``flywheel.toml`` automatically
    (``main`` loads the policy once and hands it to the resolver)."""

    monkeypatch.chdir(tmp_path)
    (tmp_path / "flywheel.toml").write_text(
        '[source]\nkind = "directory"\n[agent]\nmodel = "claude-from-policy"\n'
    )
    assert (
        worker._resolve_model(_args(model=None), load_effective_policy())
        == "claude-from-policy"
    )


def test_resolve_model_returns_none_without_policy_or_flag(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """No ``flywheel.toml`` in cwd and no ``--model``: pre-feature
    behaviour preserved -- ``None`` so the SDK uses the Claude Code
    default."""

    monkeypatch.chdir(tmp_path)
    assert not (tmp_path / "flywheel.toml").exists()
    assert worker._resolve_model(_args(model=None), load_effective_policy()) is None


def test_resolve_model_returns_none_when_policy_omits_agent(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A policy file without an ``[agent]`` table leaves the worker on
    the pre-feature default (``None``)."""

    monkeypatch.chdir(tmp_path)
    (tmp_path / "flywheel.toml").write_text('[source]\nkind = "directory"\n')
    assert worker._resolve_model(_args(model=None), load_effective_policy()) is None


# --- store-factory routing ---------------------------------------------------


def test_run_once_postgres_policy_without_dsn_fails_fast(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A postgres-backend policy with no DSN env var must surface the
    factory's fail-fast error from ``run_once`` -- proof the worker's store
    construction routes through the factory rather than hardcoding sqlite."""

    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _submitter(repo)
    _task_file(repo, "01-phase", "t1", grader="true")
    monkeypatch.delenv("FLYWHEEL_PG_DSN", raising=False)
    monkeypatch.delenv("DATABASE_URL", raising=False)
    policy = WorkPolicy(
        source_kind="directory",
        tasks_dir=repo / ".flywheel" / "tasks",
        store_backend="postgres",
    )

    async def _invoke(request: InvocationRequest) -> IterationResult:
        raise AssertionError("invoke must not run: store construction fails")

    with pytest.raises(StoreConfigError) as excinfo:
        worker.run_once(
            s,
            tasks_dir=repo / ".flywheel" / "tasks",
            db_path=repo / ".flywheel" / "flywheel.sqlite",
            worktrees_dir=repo / ".flywheel" / "worktrees",
            model=None,
            max_turns=1,
            max_retries=0,
            invoke=_invoke,
            policy=policy,
        )
    message = str(excinfo.value)
    assert "FLYWHEEL_PG_DSN" in message
    assert "DATABASE_URL" in message


def test_archive_phases_accepts_repeated_factory_calls(
    tmp_path: Path,
) -> None:
    """The run loop reconstructs the store each cycle; the factory-backed
    helpers must be safe to call repeatedly with the same policy."""

    db_path = tmp_path / "flywheel.sqlite"
    tasks_dir = tmp_path / "tasks"
    (tasks_dir / "active").mkdir(parents=True)
    policy = WorkPolicy(source_kind="directory", tasks_dir=tasks_dir)
    SqliteStore(db_path).close()  # seed the schema once
    for _ in range(3):
        worker.archive_phases(tasks_dir, db_path, lambda _m: None, policy=policy)


def test_submitter_is_a_submit_strategy(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    # GitWorktreeSubmitter is the reference SubmitStrategy: structural
    # conformance is what lets run_once pass it whole to orchestrate.
    assert isinstance(_submitter(repo), SubmitStrategy)


# --- held-out gate activation (spec 00051) ----------------------------------


def test_build_held_out_source_none_when_policy_none(tmp_path: Path) -> None:
    """No policy => no source: landing byte-identical to today (criterion #2)."""
    assert worker.build_held_out_source(None, tmp_path) is None


def test_build_held_out_source_none_when_root_unset(tmp_path: Path) -> None:
    """A policy without [held_out] root builds no source -- the gate is opt-in
    and stays inert on upgrade (criterion #2, D-3)."""
    policy = WorkPolicy(source_kind="directory", tasks_dir=tmp_path)
    assert policy.held_out_root is None
    assert worker.build_held_out_source(policy, tmp_path) is None


def test_build_held_out_source_resolves_relative_against_repo_root(
    tmp_path: Path,
) -> None:
    """A relative [held_out] root resolves to <repo_root>/<root> regardless of
    cwd (criterion #3), constructing the 00050 source unchanged (D-5)."""
    repo_root = tmp_path / "repo"
    policy = WorkPolicy(
        source_kind="directory",
        tasks_dir=repo_root,
        held_out_root=Path(".flywheel/held-out"),
    )
    source = worker.build_held_out_source(policy, repo_root)
    assert isinstance(source, FilesystemHeldOutGraderSource)
    assert source.root == repo_root / ".flywheel" / "held-out"


def test_build_held_out_source_honors_absolute_root(tmp_path: Path) -> None:
    """An absolute [held_out] root is honored verbatim, not re-rooted."""
    repo_root = tmp_path / "repo"
    abs_root = tmp_path / "elsewhere" / "held-out"
    policy = WorkPolicy(
        source_kind="directory",
        tasks_dir=repo_root,
        held_out_root=abs_root,
    )
    source = worker.build_held_out_source(policy, repo_root)
    assert isinstance(source, FilesystemHeldOutGraderSource)
    assert source.root == abs_root


# --- submit: protected-path merge gate -----------------------------------------


def _protected_submitter(
    repo: Path, patterns: list[str]
) -> tuple["worker.GitWorktreeSubmitter", list[str]]:
    logs: list[str] = []
    worktrees = repo / ".flywheel" / "worktrees"
    worktrees.mkdir(parents=True, exist_ok=True)
    s = worker.GitWorktreeSubmitter(
        repo_root=repo,
        tasks_dir=repo / ".flywheel" / "tasks",
        worktrees_dir=worktrees,
        phase_base="main",
        lock_path=repo / ".flywheel" / ".merge.lock",
        log=logs.append,
        protected_paths=patterns,
    )
    return s, logs


def test_submit_protected_path_parks(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    s, logs = _protected_submitter(repo, ["conftest.py"])
    tf = _task_file(repo, "01-phase", "t1")

    wt = s.prepare_sandbox(_sandbox_req(tf, "t1"))
    _commit(wt, "conftest.py", "tampered", "edit conftest")
    base_before = _rev(repo, "main")

    s.submit(_submit_req(tf, "t1", wt, Status.DONE))

    # Work that rewrites the verification surface never lands, even DONE.
    assert wt.exists()
    assert s._branch_exists("flywheel/01-phase/t1")
    assert _rev(repo, "main") == base_before
    assert any("protected path" in m for m in logs)


def test_submit_protected_glob_matches_nested(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    s, _logs = _protected_submitter(repo, [".github/**"])
    tf = _task_file(repo, "01-phase", "t1")

    wt = s.prepare_sandbox(_sandbox_req(tf, "t1"))
    (wt / ".github" / "workflows").mkdir(parents=True)
    _commit(wt, ".github/workflows/ci.yml", "weakened", "edit ci")
    base_before = _rev(repo, "main")

    s.submit(_submit_req(tf, "t1", wt, Status.DONE))

    assert wt.exists()
    assert _rev(repo, "main") == base_before


def test_submit_unprotected_path_merges(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    s, _logs = _protected_submitter(repo, ["conftest.py", ".github/**"])
    tf = _task_file(repo, "01-phase", "t1")

    wt = s.prepare_sandbox(_sandbox_req(tf, "t1"))
    _commit(wt, "feature.txt", "x", "feat")

    s.submit(_submit_req(tf, "t1", wt, Status.DONE))

    # The gate only bites on protected paths; ordinary work lands as before.
    assert (repo / "feature.txt").exists()
    assert not wt.exists()


# --- prepare: sandbox setup hook -----------------------------------------------


def _setup_submitter(
    repo: Path, setup: str
) -> "worker.GitWorktreeSubmitter":
    worktrees = repo / ".flywheel" / "worktrees"
    worktrees.mkdir(parents=True, exist_ok=True)
    return worker.GitWorktreeSubmitter(
        repo_root=repo,
        tasks_dir=repo / ".flywheel" / "tasks",
        worktrees_dir=worktrees,
        phase_base="main",
        lock_path=repo / ".flywheel" / ".merge.lock",
        log=lambda _m: None,
        setup_command=setup,
    )


def test_prepare_runs_setup_in_new_worktree(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    setup_log = tmp_path / "setup-log"
    s = _setup_submitter(repo, f"pwd >> {setup_log}")
    tf = _task_file(repo, "01-phase", "t1")

    wt = s.prepare_sandbox(_sandbox_req(tf, "t1"))

    # Ran exactly once, cwd'd to the new worktree.
    lines = setup_log.read_text().splitlines()
    assert len(lines) == 1
    assert Path(lines[0]).resolve() == wt.resolve()


def test_prepare_skips_setup_on_reused_worktree(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    setup_log = tmp_path / "setup-log"
    s = _setup_submitter(repo, f"echo ran >> {setup_log}")
    tf = _task_file(repo, "01-phase", "t1")

    wt = s.prepare_sandbox(_sandbox_req(tf, "t1"))
    _commit(wt, "a.txt", "a", "work")
    s.submit(_submit_req(tf, "t1", wt, Status.FAILED))  # park

    wt2 = s.prepare_sandbox(_sandbox_req(tf, "t1", mode="resume"))

    # The parked worktree's environment survived with it: one setup total.
    assert wt2 == wt
    assert len(setup_log.read_text().splitlines()) == 1


def test_prepare_setup_failure_raises(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _setup_submitter(repo, "echo broken-env >&2; false")
    tf = _task_file(repo, "01-phase", "t1")

    raised = False
    try:
        s.prepare_sandbox(_sandbox_req(tf, "t1"))
    except worker.PrepareSandboxError as exc:
        raised = True
        assert "broken-env" in str(exc)
    assert raised

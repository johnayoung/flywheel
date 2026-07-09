"""Behavior: a run parked ``AWAITING_APPROVAL`` lands its branch through the
standard landing ladder on the pass that consumes its ``approve`` command, with
no operator-issued git commands (spec 00076, criterion 5).

The run is driven through the real ``orchestrate`` loop against a real SQLite
control store, with a real :class:`GitWorktreeSubmitter` as the submit seam over
a real git repo. A first ``orchestrate`` call drives a gated task to
``AWAITING_APPROVAL``: its command grader passes, its manual gate parks it, and
the submitter preserves the verified worktree+branch (recording no ``Landed``).
The operator's only act is to enqueue an ``approve`` command through the store.
A second ``orchestrate`` call consumes it: block 1b resolves ``approved_done``
and lands the preserved branch inline through the SAME held-out-gate +
``strategy.submit`` ladder a first-attempt DONE lands through -- a clean
fast-forward whose recorded ``Landed`` reference is the base head git itself
reports, advanced to include the branch.

The land is proven WITHOUT the test issuing a single git command after enqueuing
the approve: the parked branch head is read from git *before* the approve, and
the post-land assertion is that the recorded ``landed_ref`` -- and the base
branch git reports -- both equal that head. A record written at submit-start
(before the approval consumes) would leave the base untouched and fail this.
"""

from __future__ import annotations

import asyncio
import io
import json
import subprocess
from datetime import datetime, timezone
from pathlib import Path

from claude_agent_sdk import AssistantMessage, ResultMessage, TextBlock

from flywheel_core import (
    Intent,
    InvocationRequest,
    InvocationSignals,
    IterationResult,
    Status,
    ValidEnvelope,
)
from flywheel_core.events import DomainEvent, Landed
from flywheel_core.store_sqlite import SqliteStore
from flywheel_orchestrator import orchestrate

from flywheel_worktree import worker


# --- store stub ---------------------------------------------------------------


class _RecordingLedger:
    """Minimal LandingLedger stub capturing every appended domain event, enough
    to assert whether the land recorded a ``Landed`` witness."""

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
    _git(path, "config", "user.email", "approval-test@example.com")
    _git(path, "config", "user.name", "approval test")
    _git(path, "config", "commit.gpgsign", "false")
    (path / "README.md").write_text("base\n")
    _git(path, "add", "-A")
    _git(path, "commit", "-m", "init")


def _rev(repo: Path, ref: str) -> str:
    return _git(repo, "rev-parse", ref)


def _write_gated_task(repo: Path, phase: str, task_id: str) -> None:
    """A task whose command grader passes then parks on a single manual gate, so
    a verifying agent drives the lifecycle straight to ``AWAITING_APPROVAL``."""
    tf = repo / ".flywheel" / "tasks" / "active" / phase / f"{task_id}.json"
    tf.parent.mkdir(parents=True, exist_ok=True)
    tf.write_text(
        json.dumps(
            {
                "id": task_id,
                "goal": f"Goal for {task_id}.",
                "graders": [
                    {"type": "command", "run": "true"},
                    {
                        "type": "manual",
                        "instruction": "Confirm the rollout.",
                        "name": "operator-confirm",
                    },
                ],
            }
        )
    )


def _make_submitter(
    repo: Path, worktrees: Path, ledger: _RecordingLedger
) -> worker.GitWorktreeSubmitter:
    worktrees.mkdir(parents=True, exist_ok=True)
    return worker.GitWorktreeSubmitter(
        repo_root=repo,
        tasks_dir=repo / ".flywheel" / "tasks",
        worktrees_dir=worktrees,
        phase_base="main",
        lock_path=repo / ".flywheel" / ".merge.lock",
        log=lambda _m: None,
        store=ledger,  # type: ignore[arg-type]
    )


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


def _verify_result() -> IterationResult:
    return IterationResult(
        transcript="ok",
        messages=_messages(),  # type: ignore[arg-type]
        envelope=ValidEnvelope(intent=Intent.VERIFY),
        signals=_signals(),
        failure=None,
    )


# --- the behavior -------------------------------------------------------------


def test_approved_gate_lands_parked_branch_through_ladder(
    tmp_path: Path,
) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    worktrees = repo / ".flywheel" / "worktrees"
    db_path = repo / ".flywheel" / "flywheel.sqlite"
    ledger = _RecordingLedger()
    # ``sandbox_root`` handed to orchestrate MUST equal the submitter's
    # worktrees dir: block 1b reconstructs the parked tree as
    # ``sandbox_root / task_id`` to land it, exactly the path prepare_sandbox
    # provisioned. (This is how ``run_once`` wires the two together.)
    submitter = _make_submitter(repo, worktrees, ledger)
    _write_gated_task(repo, "01-phase", "gated")

    worktree = worktrees / "gated"
    committed = {"done": False}

    async def _invoke(request: InvocationRequest) -> IterationResult:
        # First (and only) drive: commit a real change onto the task branch in
        # the provisioned worktree, so there is a verified diff to land later.
        if not committed["done"]:
            (worktree / "feature.txt").write_text("landed change\n")
            _git(worktree, "add", "-A")
            _git(worktree, "commit", "-m", "feat: gated change")
            committed["done"] = True
        return _verify_result()

    def _drive() -> object:
        return asyncio.run(
            orchestrate(
                tasks_dir=repo / ".flywheel" / "tasks",
                db_path=db_path,
                sandbox_root=worktrees,
                invoke=_invoke,
                max_retries=0,
                max_turns=4,
                stream=io.StringIO(),
                prepare_sandbox=submitter.prepare_sandbox,
                submit=submitter.submit,
            )
        )

    # Pass 1: the gated task parks AWAITING_APPROVAL; the submitter preserves
    # the verified worktree+branch and records nothing landed.
    report = _drive()
    gated = [r for r in report.runs if r.task_id == "gated"]  # type: ignore[attr-defined]
    assert len(gated) == 1
    assert gated[0].status is Status.AWAITING_APPROVAL
    run_id = gated[0].run_id
    assert ledger.landed() == []
    assert worktree.is_dir()  # the branch/worktree is preserved for the land

    # Read the parked branch head and the base head from git BEFORE the approve
    # -- the test's last git commands. The branch is genuinely ahead of the base
    # that has not moved during the park, so the land is a clean fast-forward.
    branch_head = _git(worktree, "rev-parse", "HEAD")
    base_before = _rev(repo, "main")
    assert branch_head != base_before

    # The operator's ONLY act: enqueue the approve through the control store. No
    # git command is issued past this point.
    store = SqliteStore(db_path)
    try:
        store.enqueue_command(
            run_id, "approve", {}, now=datetime.now(timezone.utc)
        )
    finally:
        store.close()

    # Pass 2: block 1b consumes the approve (``approved_done`` -> DONE) and lands
    # the preserved branch inline through strategy.submit -- no git from the
    # test, no operator merge.
    _drive()

    # Exactly one ``Landed``, naming the merge strategy, whose reference is the
    # base head the land advanced to include the branch -- a value read from
    # git, not invented. Clean FF => the new base head IS the parked branch head.
    landed = ledger.landed()
    assert len(landed) == 1
    assert landed[0].strategy == "merge"
    assert landed[0].landed_ref == branch_head
    assert _rev(repo, "main") == branch_head  # the base advanced onto the branch

    # The lifecycle reached DONE on the same pass that landed it.
    store = SqliteStore(db_path)
    try:
        final = store.load_lifecycle(run_id)
        assert final is not None
        assert final.status is Status.DONE
    finally:
        store.close()

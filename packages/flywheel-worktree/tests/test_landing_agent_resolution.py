"""Behavior: the bounded agentic conflict-resolution landing rung (spec 00076,
criterion 4).

When the merge-fallback rung's ``--no-ff`` merge itself conflicts and the rung
is armed ([submit] ``recovery_agent_max_turns`` > 0), a single bounded agent
session resolves the conflict and the resolved tree lands only after the SAME
out-of-band re-verification bar the merge-fallback rung enforces (task command
graders -> standing build invariant -> held-out gate -> fast-forward). Otherwise
the run parks with its branch+worktree preserved and the session's recorded
turn/wall usage on the ledger, within its configured bounds.

Every scenario runs real git against a tmp repo, shaped like
``test_landing_merge_fallback`` so the branch's rebase *and* merge both conflict
(a single un-reverted edit that collides with the advanced base). The session
driver is injected -- a synchronous stub that mutates the conflicted worktree --
so the rung is exercised offline without ever touching the real SDK, mirroring
the ``resolve_conflict`` seam's contract. The store is a recording ledger, so
each assertion is made against the witness that actually lands on the run's
ledger.

The ``resolve_conflict`` seam reports usage only; the worker decides whether the
tree is resolved from git state alone, since the agent's claim is untrusted.
"""

from __future__ import annotations

import subprocess
from pathlib import Path

import pytest

from flywheel_core import CommandGrader, Status, Task
from flywheel_core.events import (
    LANDING_PARK_KINDS,
    LANDING_STRATEGY_MERGE,
    PARK_KIND_DIVERGENT_BASE,
    PARK_KIND_MERGE_CONFLICT,
    RUNG_AGENT_RESOLVED,
    DomainEvent,
    Landed,
    LandingParked,
)
from flywheel_orchestrator import SandboxRequest, SubmitRequest

from flywheel_worktree import worker

# Bounds the tests arm the rung with; kept small so an assertion that recorded
# usage never exceeds the bound is meaningful.
_MAX_TURNS = 30
_MAX_WALL = 900.0


# --- store stub ---------------------------------------------------------------


class _RecordingLedger:
    """Minimal LandingLedger stub capturing every appended domain event."""

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

    def parked(self) -> list[LandingParked]:
        return [e for e in self.events if isinstance(e, LandingParked)]


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
    (path / "file.txt").write_text("line1\n")
    _git(path, "add", "-A")
    _git(path, "commit", "-m", "init")


def _rev(repo: Path, ref: str) -> str:
    return _git(repo, "rev-parse", ref)


def _is_ancestor(repo: Path, ancestor: str, rev: str) -> bool:
    return (
        subprocess.run(
            ["git", "-C", str(repo), "merge-base", "--is-ancestor",
             ancestor, rev],
            capture_output=True,
        ).returncode
        == 0
    )


def _write_commit(cwd: Path, files: dict[str, str], message: str) -> None:
    for name, body in files.items():
        (cwd / name).write_text(body)
    _git(cwd, "add", "-A")
    _git(cwd, "commit", "-m", message)


def _conflicting_branch(wt: Path) -> str:
    """A single un-reverted edit to ``file.txt``: the branch's surviving change
    collides with the base's change on BOTH the rebase and the ``--no-ff``
    merge, so submit falls through to the agentic resolution rung. Returns the
    branch tip sha."""
    _write_commit(wt, {"file.txt": "line1-branch\n"}, "edit file.txt")
    return _rev(wt, "HEAD")


def _task_file(repo: Path, phase: str, task_id: str, run: str) -> Path:
    import json

    tf = repo / ".flywheel" / "tasks" / "active" / phase / f"{task_id}.json"
    tf.parent.mkdir(parents=True, exist_ok=True)
    tf.write_text(
        json.dumps(
            {
                "id": task_id,
                "goal": f"Goal for {task_id}.",
                "graders": [{"type": "command", "run": run}],
            }
        )
    )
    return tf


def _sandbox_req(tf: Path, task_id: str) -> SandboxRequest:
    return SandboxRequest(
        task_id=task_id, task_file=tf, run_id=None, mode="fresh"
    )


def _submit_req(
    tf: Path, task_id: str, sandbox: Path, run: str
) -> SubmitRequest:
    return SubmitRequest(
        task_id=task_id,
        task_file=tf,
        task=Task(
            id=task_id,
            goal=f"Goal for {task_id}.",
            graders=[CommandGrader(run=run)],
        ),
        run_id="run-1",
        status=Status.DONE,
        sandbox=sandbox,
    )


def _submitter(
    repo: Path,
    ledger: _RecordingLedger,
    *,
    resolver: worker.ConflictResolver | None,
    max_turns: int = _MAX_TURNS,
    max_wall_seconds: float = _MAX_WALL,
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
        recovery_agent_max_turns=max_turns,
        recovery_agent_max_wall_seconds=max_wall_seconds,
        resolve_conflict=resolver,
    )


# --- injectable session drivers (never touch the real SDK) --------------------


def _resolving_driver(
    *,
    turns: int = 3,
    wall: float = 1.5,
    content: str = "line1-resolved\n",
    calls: list[worker.ConflictResolutionRequest] | None = None,
) -> worker.ConflictResolver:
    """A session driver that resolves the conflict: overwrite the conflicted
    file with a merged body and stage it (the worker authors the commit). Reports
    bounded usage. Never commits/aborts/resets -- that is the worker's job."""

    def _resolve(
        request: worker.ConflictResolutionRequest,
    ) -> worker.ConflictResolutionReport:
        if calls is not None:
            calls.append(request)
        # The worktree is mid-merge with conflict markers in file.txt.
        assert (
            subprocess.run(
                ["git", "-C", str(request.worktree), "rev-parse", "-q",
                 "--verify", "MERGE_HEAD"],
                capture_output=True,
            ).returncode
            == 0
        )
        (request.worktree / "file.txt").write_text(content)
        subprocess.run(
            ["git", "-C", str(request.worktree), "add", "-A"], check=True
        )
        return worker.ConflictResolutionReport(turns=turns, wall_seconds=wall)

    return _resolve


def _giveup_driver(
    *, turns: int, wall: float
) -> worker.ConflictResolver:
    """A session driver that exhausts its bound without resolving: it leaves the
    conflicted, unmerged tree untouched and reports usage at the bound."""

    def _resolve(
        request: worker.ConflictResolutionRequest,
    ) -> worker.ConflictResolutionReport:
        return worker.ConflictResolutionReport(turns=turns, wall_seconds=wall)

    return _resolve


def _crashing_driver(
    request: worker.ConflictResolutionRequest,
) -> worker.ConflictResolutionReport:
    """A session driver that crashes mid-session (an SDK error)."""
    raise RuntimeError("agent session crashed")


def _never_driver(
    request: worker.ConflictResolutionRequest,
) -> worker.ConflictResolutionReport:
    """A session driver that must never run (asserts the rung stayed disabled)."""
    raise AssertionError("resolver must not run when the rung is disabled")


def _drive_to_conflict(
    repo: Path, s: worker.GitWorktreeSubmitter, run: str
) -> tuple[Path, str, str]:
    """Provision the worktree, create the conflicting branch, advance the base so
    both the rebase and the merge conflict. Returns (worktree, branch_head,
    base_before)."""
    tf = _task_file(repo, "01-phase", "t1", run=run)
    wt = s.prepare_sandbox(_sandbox_req(tf, "t1"))
    branch_head = _conflicting_branch(wt)
    _write_commit(repo, {"file.txt": "line1-main\n"}, "base advances")
    return wt, branch_head, _rev(repo, "main")


# --- the agent-resolved land --------------------------------------------------


def test_agent_resolution_lands_merge_conflicting_branch(
    tmp_path: Path,
) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    ledger = _RecordingLedger()
    calls: list[worker.ConflictResolutionRequest] = []
    s = _submitter(
        repo,
        ledger,
        resolver=_resolving_driver(turns=4, wall=2.0, calls=calls),
    )
    wt, branch_head, base_before = _drive_to_conflict(repo, s, run="true")
    tf = repo / ".flywheel" / "tasks" / "active" / "01-phase" / "t1.json"

    s.submit(_submit_req(tf, "t1", wt, run="true"))

    # Exactly one session ran, bounded, rooted in the conflicted worktree.
    assert len(calls) == 1
    assert calls[0].worktree == wt
    assert calls[0].max_turns == _MAX_TURNS
    assert calls[0].max_wall_seconds == _MAX_WALL

    # Exactly one Landed witness, naming the agent-resolved rung and carrying the
    # session's usage -- within its configured bounds.
    landed = ledger.landed()
    assert len(landed) == 1
    assert landed[0].strategy == LANDING_STRATEGY_MERGE
    assert landed[0].rung == RUNG_AGENT_RESOLVED
    turns = landed[0].agent_turns
    wall = landed[0].agent_wall_seconds
    assert turns == 4
    assert wall == 2.0
    assert turns is not None and turns <= _MAX_TURNS
    assert wall is not None and wall <= _MAX_WALL

    # The base ref advanced and the branch's tip became an ancestor of it: the
    # commits are reachable from the landed base, not merely marked landed.
    assert _rev(repo, "main") != base_before
    assert _is_ancestor(repo, branch_head, "main")
    assert landed[0].landed_ref == _rev(repo, "main")
    assert ledger.parked() == []


# --- disabled leg: max_turns == 0 parks exactly as merge-fallback -------------


def test_zero_turns_disables_rung_and_parks_without_a_session(
    tmp_path: Path,
) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    ledger = _RecordingLedger()
    # A resolver is present but the rung is disabled, so it must never run.
    s = _submitter(repo, ledger, resolver=_never_driver, max_turns=0)
    wt, branch_head, base_before = _drive_to_conflict(repo, s, run="true")
    tf = repo / ".flywheel" / "tasks" / "active" / "01-phase" / "t1.json"

    s.submit(_submit_req(tf, "t1", wt, run="true"))

    # Parks exactly as the merge-fallback merge-conflict leaf does: nothing
    # landed, base untouched, one merge-conflict park with NO session usage.
    assert ledger.landed() == []
    assert _rev(repo, "main") == base_before
    parked = ledger.parked()
    assert len(parked) == 1
    assert parked[0].park_kind == PARK_KIND_MERGE_CONFLICT
    assert parked[0].agent_turns is None
    assert parked[0].agent_wall_seconds is None
    # The branch+worktree survive and the merge aborted cleanly.
    assert _rev(repo, "flywheel/01-phase/t1") == branch_head
    assert wt.is_dir()
    assert _git(wt, "status", "--porcelain") == ""


# --- bound exhaustion without a resolved tree parks preserved -----------------


def test_bound_exhaustion_parks_preserved_with_usage(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    ledger = _RecordingLedger()
    s = _submitter(
        repo,
        ledger,
        resolver=_giveup_driver(turns=_MAX_TURNS, wall=_MAX_WALL),
    )
    wt, branch_head, base_before = _drive_to_conflict(repo, s, run="true")
    tf = repo / ".flywheel" / "tasks" / "active" / "01-phase" / "t1.json"

    s.submit(_submit_req(tf, "t1", wt, run="true"))

    # Nothing landed, base byte-identical.
    assert ledger.landed() == []
    assert _rev(repo, "main") == base_before
    # A single merge-conflict park witness carrying the session's usage, at or
    # below the configured bound (the bound is enforced, not advisory).
    parked = ledger.parked()
    assert len(parked) == 1
    assert parked[0].park_kind == PARK_KIND_MERGE_CONFLICT
    assert parked[0].park_kind in LANDING_PARK_KINDS
    assert parked[0].agent_turns is not None
    assert parked[0].agent_wall_seconds is not None
    assert parked[0].agent_turns <= _MAX_TURNS
    assert parked[0].agent_wall_seconds <= _MAX_WALL
    # The branch+worktree survive for the recovery re-drive, merge aborted clean.
    assert _rev(repo, "flywheel/01-phase/t1") == branch_head
    assert wt.is_dir()
    assert _git(wt, "status", "--porcelain") == ""


# --- a resolved tree that fails re-verify leaves the base untouched -----------


def test_resolved_tree_failing_reverify_parks_no_land(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    ledger = _RecordingLedger()
    s = _submitter(
        repo,
        ledger,
        resolver=_resolving_driver(turns=5, wall=3.0),
    )
    # The grader passes on the branch tree alone but fails on the merged tree the
    # advanced base poisons; the session still resolves the file.txt conflict.
    tf = _task_file(repo, "01-phase", "t1", run="test ! -f poison.txt")
    wt = s.prepare_sandbox(_sandbox_req(tf, "t1"))
    branch_head = _conflicting_branch(wt)
    _write_commit(
        repo,
        {"file.txt": "line1-main\n", "poison.txt": "boom\n"},
        "base advances and poisons",
    )
    base_before = _rev(repo, "main")

    s.submit(_submit_req(tf, "t1", wt, run="test ! -f poison.txt"))

    # The resolved tree failed re-verification: base ref byte-identical, no
    # Landed record -- unverified merged content did not land.
    assert _rev(repo, "main") == base_before
    assert ledger.landed() == []
    # A divergent-base park carrying the deciding grader's receipt AND the
    # session usage (the park counts toward the re-driver's bound).
    parked = ledger.parked()
    assert len(parked) == 1
    assert parked[0].park_kind == PARK_KIND_DIVERGENT_BASE
    assert parked[0].receipts
    assert any(not r.passed for r in parked[0].receipts)
    assert parked[0].agent_turns == 5
    assert parked[0].agent_wall_seconds == 3.0
    # Branch+worktree survive; the branch was reset off the merge commit.
    assert _rev(repo, "flywheel/01-phase/t1") == branch_head
    assert wt.is_dir()


# --- a session crash must not unwind submit -----------------------------------


def test_session_crash_parks_and_submit_does_not_raise(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    ledger = _RecordingLedger()
    s = _submitter(repo, ledger, resolver=_crashing_driver)
    wt, branch_head, base_before = _drive_to_conflict(repo, s, run="true")
    tf = repo / ".flywheel" / "tasks" / "active" / "01-phase" / "t1.json"

    # submit MUST NOT raise even though the session crashed.
    s.submit(_submit_req(tf, "t1", wt, run="true"))

    assert ledger.landed() == []
    assert _rev(repo, "main") == base_before
    parked = ledger.parked()
    assert len(parked) == 1
    assert parked[0].park_kind == PARK_KIND_MERGE_CONFLICT
    # A crashed session leaves a preserved worktree, the merge aborted cleanly.
    assert _rev(repo, "flywheel/01-phase/t1") == branch_head
    assert wt.is_dir()
    assert (
        subprocess.run(
            ["git", "-C", str(wt), "rev-parse", "-q", "--verify", "MERGE_HEAD"],
            capture_output=True,
        ).returncode
        != 0
    )
    assert _git(wt, "status", "--porcelain") == ""


# --- the default resolver stays behind the lazy SDK boundary ------------------


def test_default_resolver_is_wired_when_none_injected(tmp_path: Path) -> None:
    # A submitter built without an explicit resolver falls back to the real
    # SDK-backed driver (never invoked here) -- the seam defaults, it is not None.
    repo = tmp_path / "repo"
    _init_repo(repo)
    ledger = _RecordingLedger()
    s = _submitter(repo, ledger, resolver=None)
    assert s._resolve_conflict is worker._default_resolve_conflict


def test_module_imports_without_agent_sdk() -> None:
    # Importing the worker module (done at test collection) must not require the
    # optional claude-agent-sdk extra: the SDK is reached only lazily inside the
    # default driver. A plain attribute access proves the seam objects exist.
    assert worker.ConflictResolutionRequest is not None
    assert worker.ConflictResolutionReport is not None
    assert callable(worker._default_resolve_conflict)


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(pytest.main([__file__, "-q"]))

"""The bounded landing re-driver (spec 00069, criteria #3/#4/#13).

A run parked *unlanded* -- a :class:`~flywheel_core.events.LandingParked` witness
on a ``DONE`` run whose work graded green but whose strategy could not land it (a
failed ``[submit] verify`` standing build invariant, a divergent base) -- is a
strand the loop must actively clear, not leave to accrue. These tests pin the
re-driver's observable effects against the REAL git submit strategy
(``flywheel_worktree.worker.GitWorktreeSubmitter``), so the re-attempt genuinely
re-runs the strategy's own rebase + command/standing re-verification against the
exact base it lands on -- no stubbed land:

* #3  -- a parked run whose cause has cleared (the standing invariant now passes,
  the base no longer diverges) lands on a re-attempt within the bound; the base
  advances and the worktree is torn down.
* #13 -- re-attempting re-runs the rebase + command/standing graders against the
  exact base it lands on (the base-advanced path rebases inside the re-drive
  before the standing gate re-runs).
* #4  -- a parked run whose cause never clears makes at most ``bound`` automatic
  land re-attempts, then is routed to the single human-review queue with its
  ``park_kind`` as the machine-readable reason, and NO ``bound+1``-th attempt is
  ever made (a second re-drive pass short-circuits on the already-queued guard).

The discriminators throughout mirror the standing-verify gate's own tests: a
successful land advances ``main`` and destroys the worktree (``on_done=destroy``
default); a park leaves the base byte-for-byte unchanged and the worktree on
disk. The re-driver adds one more observable: the human-review queue entry and
the ``LandingParked`` park-count on the run's ledger.
"""

from __future__ import annotations

import json
import subprocess
from datetime import datetime, timezone
from pathlib import Path

from flywheel_core import CommandGrader, Grader, Lifecycle, Status, Task
from flywheel_core.events import LandingParked
from flywheel_core.store_sqlite import SqliteStore
from flywheel_orchestrator import SandboxRequest, SqliteClaimStore, SubmitRequest
from flywheel_orchestrator._orchestrate import redrive_parked_landings
from flywheel_worktree import worker

_BASE = datetime(2026, 7, 1, 12, 0, 0, tzinfo=timezone.utc)


# --- git helpers (mirror test_submit_standing_verify.py) --------------------


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


def _advance_base(repo: Path) -> None:
    """Advance ``main`` out-of-band so a branch forked earlier can no longer
    fast-forward and must take the rebase path."""
    (repo / "other.txt").write_text("advanced\n")
    _git(repo, "add", "-A")
    _git(repo, "commit", "-m", "advance base")


# --- store / strategy helpers -----------------------------------------------


def _frozen(at: datetime):
    def _now() -> datetime:
        return at

    return _now


def _seed_done(store: SqliteStore, task_id: str, run_id: str) -> None:
    """A lifecycle finalized ``DONE`` -- the terminal state a parked-unlanded
    run sits in. ``_record_landing_park`` loads this to stamp the park's
    ``expected_version``, so a real row must exist."""
    lc = Lifecycle(task_id=task_id, run_id=run_id)
    lc.transition_to(Status.READY, now=_BASE)
    lc.transition_to(Status.RUNNING, now=_BASE)
    lc.transition_to(Status.VALIDATING, now=_BASE)
    lc.transition_to(Status.DONE, now=_BASE)
    store.create_lifecycle(lc)


def _submitter(
    repo: Path,
    *,
    verify_command: str | None,
    store: SqliteStore,
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
        store=store,
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
    tf: Path,
    task_id: str,
    run_id: str,
    sandbox: Path,
    *,
    graders: list[Grader] | None = None,
) -> SubmitRequest:
    return SubmitRequest(
        task_id=task_id,
        task_file=tf,
        task=Task(
            id=task_id,
            goal=f"Goal for {task_id}.",
            graders=graders if graders is not None else [CommandGrader(run="true")],
        ),
        run_id=run_id,
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


def _parks(store: SqliteStore, run_id: str) -> list[LandingParked]:
    return [
        e
        for e in store.list_domain_events(run_id)
        if isinstance(e, LandingParked)
    ]


# --- #3: a cleared cause lands within the bound (clean-FF path) --------------


def test_redrive_lands_when_standing_verify_clears(tmp_path: Path) -> None:
    """A run parked on a failed standing invariant lands on the first re-attempt
    once the invariant passes -- base advances, worktree torn down, queue empty."""
    repo = tmp_path / "repo"
    _init_repo(repo)
    base_before = _rev(repo, "main")
    db = tmp_path / "flywheel.sqlite"
    control = SqliteStore(db)
    claims = SqliteClaimStore(db)
    try:
        _seed_done(control, "t1", "run-1")
        s = _submitter(repo, verify_command="false", store=control)
        tf = _task_file(repo, "01-phase", "t1")
        wt = _prepare_with_commit(s, tf, "t1", filename="feature.txt")
        req = _submit_req(tf, "t1", "run-1", wt)

        # Original land-suppression: the standing invariant fails -> park #1.
        s.submit(req)
        assert _rev(repo, "main") == base_before
        assert wt.exists()
        assert len(_parks(control, "run-1")) == 1

        # The cause clears: the standing build invariant now passes.
        s.verify_command = "true"
        outcomes = redrive_parked_landings(
            control,
            claims,
            s,
            "worker-a",
            requests=[req],
            bound=3,
            lease_seconds=3600,
            now=_frozen(_BASE),
        )

        assert [o.result for o in outcomes] == ["landed"]
        assert outcomes[0].run_id == "run-1"
        assert outcomes[0].attempts == 1
        # Landed: base advanced, the change is in the base tree, worktree gone.
        assert _rev(repo, "main") != base_before
        assert (repo / "feature.txt").exists()
        assert not wt.exists()
        # No fresh park appended by the landing re-attempt.
        assert len(_parks(control, "run-1")) == 1
        # A cleared strand is never routed to the queue.
        assert claims.list_human_review_queue() == []
    finally:
        control.close()
        claims.close()


# --- #13: re-attempt rebases + re-verifies against the exact base it lands on -


def test_redrive_rebases_and_reverifies_before_landing(tmp_path: Path) -> None:
    """The re-attempt is a full re-drive, not a blind re-land: after the base
    advances out-of-band the re-drive rebases the branch, re-runs the task's
    command graders against the rebased tree, re-runs the (now-passing) standing
    invariant, and only then fast-forwards -- both files land."""
    repo = tmp_path / "repo"
    _init_repo(repo)
    base_before = _rev(repo, "main")
    db = tmp_path / "flywheel.sqlite"
    control = SqliteStore(db)
    claims = SqliteClaimStore(db)
    try:
        _seed_done(control, "t1", "run-1")
        s = _submitter(repo, verify_command="false", store=control)
        tf = _task_file(repo, "01-phase", "t1")
        wt = _prepare_with_commit(s, tf, "t1", filename="feature.txt")
        req = _submit_req(tf, "t1", "run-1", wt)

        # Park #1 on the clean-FF path (standing invariant fails).
        s.submit(req)
        assert _rev(repo, "main") == base_before
        assert len(_parks(control, "run-1")) == 1

        # Now the base diverges: the next land can no longer fast-forward and
        # must rebase inside the re-drive before re-verifying.
        _advance_base(repo)
        base_advanced = _rev(repo, "main")
        assert base_advanced != base_before

        s.verify_command = "true"
        outcomes = redrive_parked_landings(
            control,
            claims,
            s,
            "worker-a",
            requests=[req],
            bound=3,
            lease_seconds=3600,
            now=_frozen(_BASE),
        )

        assert [o.result for o in outcomes] == ["landed"]
        # Landed past the advanced commit: the rebase interleaved both changes.
        assert _rev(repo, "main") != base_advanced
        assert (repo / "feature.txt").exists()
        assert (repo / "other.txt").exists()
        assert not wt.exists()
        assert claims.list_human_review_queue() == []
    finally:
        control.close()
        claims.close()


# --- #4: never-clearing routes to the queue, with no (bound+1)-th attempt -----


def test_redrive_routes_to_queue_after_bound_and_makes_no_further_attempt(
    tmp_path: Path,
) -> None:
    """A run whose standing invariant never clears makes exactly ``bound``
    re-attempts, then is routed to the human-review queue keyed to its
    ``park_kind``; a second re-drive pass makes NO further attempt and does not
    re-queue it (criterion #4)."""
    repo = tmp_path / "repo"
    _init_repo(repo)
    base_before = _rev(repo, "main")
    db = tmp_path / "flywheel.sqlite"
    control = SqliteStore(db)
    claims = SqliteClaimStore(db)
    try:
        _seed_done(control, "t1", "run-1")
        s = _submitter(repo, verify_command="false", store=control)
        tf = _task_file(repo, "01-phase", "t1")
        wt = _prepare_with_commit(s, tf, "t1", filename="feature.txt")
        req = _submit_req(tf, "t1", "run-1", wt)

        # Park #1 -- the original land-suppression.
        s.submit(req)
        assert len(_parks(control, "run-1")) == 1

        outcomes = redrive_parked_landings(
            control,
            claims,
            s,
            "worker-a",
            requests=[req],
            bound=3,
            lease_seconds=3600,
            now=_frozen(_BASE),
        )

        # Exactly `bound` re-attempts, then routed to the queue.
        assert [o.result for o in outcomes] == ["queued"]
        assert outcomes[0].attempts == 3
        assert outcomes[0].park_kind == "standing-verify"
        # Park #1 plus one fresh park per re-attempt = 1 + bound = 4 total.
        assert len(_parks(control, "run-1")) == 4
        # Not landed: base untouched, worktree still parked on disk.
        assert _rev(repo, "main") == base_before
        assert not (repo / "feature.txt").exists()
        assert wt.exists()
        # Routed once, keyed to the run with its park_kind as the reason.
        queue = claims.list_human_review_queue()
        assert len(queue) == 1
        assert queue[0].reason == "standing-verify"
        assert queue[0].task_id == "t1"
        assert queue[0].run_id == "run-1"

        # Second pass: the already-queued guard short-circuits. No (bound+1)-th
        # land attempt is made and the run is not re-queued.
        outcomes2 = redrive_parked_landings(
            control,
            claims,
            s,
            "worker-a",
            requests=[req],
            bound=3,
            lease_seconds=3600,
            now=_frozen(_BASE),
        )
        assert [o.result for o in outcomes2] == ["queued"]
        assert outcomes2[0].attempts == 0
        # No fresh park appended (no bound+1 submit), no second queue entry.
        assert len(_parks(control, "run-1")) == 4
        assert len(claims.list_human_review_queue()) == 1
    finally:
        control.close()
        claims.close()


# --- an unparked / already-landed run is neither re-driven nor queued ---------


def test_redrive_skips_a_run_that_never_parked(tmp_path: Path) -> None:
    """A ``DONE`` run with no ``LandingParked`` witness (it landed cleanly, or
    has no landing notion) is dropped by the re-driver -- no outcome, no
    re-attempt, no queue entry."""
    repo = tmp_path / "repo"
    _init_repo(repo)
    db = tmp_path / "flywheel.sqlite"
    control = SqliteStore(db)
    claims = SqliteClaimStore(db)
    try:
        _seed_done(control, "t1", "run-1")
        s = _submitter(repo, verify_command="true", store=control)
        tf = _task_file(repo, "01-phase", "t1")
        wt = _prepare_with_commit(s, tf, "t1", filename="feature.txt")
        req = _submit_req(tf, "t1", "run-1", wt)

        # No prior submit/park: the run never parked.
        assert _parks(control, "run-1") == []

        outcomes = redrive_parked_landings(
            control,
            claims,
            s,
            "worker-a",
            requests=[req],
            bound=3,
            lease_seconds=3600,
            now=_frozen(_BASE),
        )

        assert outcomes == ()
        assert claims.list_human_review_queue() == []
    finally:
        control.close()
        claims.close()

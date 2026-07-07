"""Behavior: redriving a parked landing appends a ``LandingRedriven`` record
paired with its REAL outcome witness (spec 00073, criterion 5).

Retro 08-agent-usage-surface (F1) had a real block-and-redrive that left no
store record -- its only witness was ephemeral worker stdout. This pins the
store-side fix: every re-drive disposition appends a ``LandingRedriven`` record,
and each such record is paired with the durable witness that proves the
disposition actually happened:

* ``"landed"``    -- a :class:`~flywheel_core.events.Landed` record on the run's
  ledger (the submit strategy appended it when the branch merged).
* ``"re-parked"`` -- a fresh :class:`~flywheel_core.events.LandingParked` on the
  ledger (one per failed re-attempt, beyond the original suppression).
* ``"routed"``    -- a human-review queue entry keyed to the run.

These run the REAL git submit strategy against a tmp repo and a real SqliteStore,
exactly as ``test_redriver_landing.py`` does, so the witnesses are produced by a
genuine land / park / route -- never stubbed. The named cheat -- recording
``"redriven"`` without a real re-attempt -- is foreclosed by
``_assert_redrives_all_paired``: a record with no paired witness fails it.
"""

from __future__ import annotations

import json
import subprocess
from datetime import datetime, timezone
from pathlib import Path

from flywheel_core import CommandGrader, Grader, Lifecycle, Status, Task
from flywheel_core.events import (
    REDRIVE_RESULT_LANDED,
    REDRIVE_RESULT_REPARKED,
    REDRIVE_RESULT_ROUTED,
    Landed,
    LandingParked,
    LandingRedriven,
)
from flywheel_core.store_sqlite import SqliteStore
from flywheel_orchestrator import SandboxRequest, SqliteClaimStore, SubmitRequest
from flywheel_orchestrator._orchestrate import redrive_parked_landings
from flywheel_worktree import worker

_BASE = datetime(2026, 7, 1, 12, 0, 0, tzinfo=timezone.utc)


# --- git helpers (mirror test_redriver_landing.py) --------------------------


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


# --- store / strategy helpers -----------------------------------------------


def _frozen(at: datetime):
    def _now() -> datetime:
        return at

    return _now


def _seed_done(store: SqliteStore, task_id: str, run_id: str) -> None:
    lc = Lifecycle(task_id=task_id, run_id=run_id)
    lc.transition_to(Status.READY, now=_BASE)
    lc.transition_to(Status.RUNNING, now=_BASE)
    lc.transition_to(Status.VALIDATING, now=_BASE)
    lc.transition_to(Status.DONE, now=_BASE)
    store.create_lifecycle(lc)


def _submitter(
    repo: Path, *, verify_command: str | None, store: SqliteStore
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


# --- ledger readers ----------------------------------------------------------


def _redrives(store: SqliteStore, run_id: str) -> list[LandingRedriven]:
    return [
        e
        for e in store.list_domain_events(run_id)
        if isinstance(e, LandingRedriven)
    ]


def _landed(store: SqliteStore, run_id: str) -> list[Landed]:
    return [
        e for e in store.list_domain_events(run_id) if isinstance(e, Landed)
    ]


def _parks(store: SqliteStore, run_id: str) -> list[LandingParked]:
    return [
        e
        for e in store.list_domain_events(run_id)
        if isinstance(e, LandingParked)
    ]


def _assert_redrives_all_paired(
    store: SqliteStore,
    claims: SqliteClaimStore,
    *,
    run_id: str,
) -> None:
    """Every ``LandingRedriven`` record is paired with a real outcome witness.

    This is the anti-cheat the spec names: a record claiming a disposition it
    never produced (a ``"redriven"`` with no landing / park / routing behind it)
    fails here. A parked run always carries exactly one original suppression
    park, so the fresh parks -- the ones a re-attempt produced -- are
    ``len(parks) - 1``.
    """
    redrives = _redrives(store, run_id)
    landed_witnesses = _landed(store, run_id)
    fresh_parks = len(_parks(store, run_id)) - 1
    queue_entries = [
        e for e in claims.list_human_review_queue() if e.run_id == run_id
    ]

    landed_records = [r for r in redrives if r.result == REDRIVE_RESULT_LANDED]
    reparked_records = [
        r for r in redrives if r.result == REDRIVE_RESULT_REPARKED
    ]
    routed_records = [r for r in redrives if r.result == REDRIVE_RESULT_ROUTED]

    # No redrive record carries an out-of-vocabulary result.
    assert len(landed_records) + len(reparked_records) + len(
        routed_records
    ) == len(redrives)

    # A "landed" record is witnessed by a Landed on the ledger, one for one.
    if landed_records:
        assert landed_witnesses, "landed redrive with no Landed witness"
    assert len(landed_records) == len(landed_witnesses)
    # Each "re-parked" record is witnessed by a distinct fresh LandingParked.
    assert len(reparked_records) == fresh_parks
    # Each "routed" record is witnessed by a human-review queue entry.
    if routed_records:
        assert queue_entries, "routed redrive with no human-review queue entry"
    assert len(routed_records) == len(queue_entries)


# --- landed: the re-drive merged, paired with a Landed witness ---------------


def test_landed_redrive_records_paired_with_landed_witness(
    tmp_path: Path,
) -> None:
    """A parked run whose cause clears lands on the first re-attempt: the ledger
    gains exactly one ``LandingRedriven(result="landed")`` AND the ``Landed``
    witness it pairs with."""
    repo = tmp_path / "repo"
    _init_repo(repo)
    db = tmp_path / "flywheel.sqlite"
    control = SqliteStore(db)
    claims = SqliteClaimStore(db)
    try:
        _seed_done(control, "t1", "run-1")
        s = _submitter(repo, verify_command="false", store=control)
        tf = _task_file(repo, "01-phase", "t1")
        wt = _prepare_with_commit(s, tf, "t1", filename="feature.txt")
        req = _submit_req(tf, "t1", "run-1", wt)

        # Original land-suppression: standing invariant fails -> park #1.
        s.submit(req)
        assert _redrives(control, "run-1") == []  # no redrive yet

        # The cause clears; the re-drive lands on the first re-attempt.
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

        redrives = _redrives(control, "run-1")
        assert len(redrives) == 1
        assert redrives[0].result == REDRIVE_RESULT_LANDED
        assert redrives[0].park_kind == "standing-verify"
        # The paired witness: a real Landed record the submit strategy appended.
        assert len(_landed(control, "run-1")) == 1
        _assert_redrives_all_paired(control, claims, run_id="run-1")
    finally:
        control.close()
        claims.close()


# --- re-parked + routed: failed re-attempts, then routed to human review ------


def test_reparked_and_routed_redrives_each_pair_with_their_witness(
    tmp_path: Path,
) -> None:
    """A run whose cause never clears re-parks on every bounded re-attempt, then
    is routed. Each failed re-attempt appends a ``LandingRedriven(re-parked)``
    paired with the fresh ``LandingParked`` it produced; the terminal routing
    appends a ``LandingRedriven(routed)`` paired with the queue entry."""
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

        # Original land-suppression -> park #1.
        s.submit(req)

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
        assert [o.result for o in outcomes] == ["queued"]
        # Never landed: base untouched, no Landed witness ever appended.
        assert _rev(repo, "main") == base_before
        assert _landed(control, "run-1") == []

        redrives = _redrives(control, "run-1")
        reparked = [r for r in redrives if r.result == REDRIVE_RESULT_REPARKED]
        routed = [r for r in redrives if r.result == REDRIVE_RESULT_ROUTED]
        # One re-parked record per failed re-attempt (= bound), one routed.
        assert len(reparked) == 3
        assert len(routed) == 1
        # The re-parked records pair with the fresh parks (1 original + 3 = 4).
        assert len(_parks(control, "run-1")) == 4
        # The routed record pairs with exactly the one queue entry for this run.
        queue = [
            e for e in claims.list_human_review_queue() if e.run_id == "run-1"
        ]
        assert len(queue) == 1
        assert queue[0].reason == "standing-verify"
        assert routed[0].park_kind == "standing-verify"
        _assert_redrives_all_paired(control, claims, run_id="run-1")

        # A second pass short-circuits on the already-queued guard: it makes no
        # re-attempt, so it appends NO new redrive record (no unpaired cheat).
        before = len(_redrives(control, "run-1"))
        redrive_parked_landings(
            control,
            claims,
            s,
            "worker-a",
            requests=[req],
            bound=3,
            lease_seconds=3600,
            now=_frozen(_BASE),
        )
        assert len(_redrives(control, "run-1")) == before
        _assert_redrives_all_paired(control, claims, run_id="run-1")
    finally:
        control.close()
        claims.close()


# --- a run that never parked records no redrive at all ------------------------


def test_unparked_run_records_no_redrive(tmp_path: Path) -> None:
    """A run with no ``LandingParked`` witness is dropped by the re-driver, so no
    ``LandingRedriven`` record is ever appended -- there is no disposition, hence
    nothing to pair."""
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
        assert _redrives(control, "run-1") == []
    finally:
        control.close()
        claims.close()

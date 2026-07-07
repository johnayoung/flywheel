"""A re-driven landing clears a FRESH held-out gate before it lands (spec 00074).

The landing re-driver (spec 00069) re-attempts a parked land through the
strategy's own rebase/reverify/standing/FF path. Until now it re-invoked
``strategy.submit`` *without* re-evaluating the execute-time held-out landing
gate, so a park the gate had blocked could slip in on re-drive via the
strategy's own checks alone (the live bypass recorded in the 00073 verify
addendum). These tests pin the closed behavior against the REAL git submit
strategy (``flywheel_worktree.worker.GitWorktreeSubmitter``) plus a real
``FilesystemHeldOutGraderSource``:

* A re-attempt lands only after a fresh gate evaluation PASSES against the
  content it is about to land, and that evaluation appends its OWN verdict
  record (criteria 1 + 3).
* A gate FAIL on a re-attempt suppresses the land, re-parks with the
  ``held-out-gate`` kind, consumes a re-drive-bound attempt, and routes to the
  human-review queue at the bound (criteria 5 + 7).
* An unexecutable/fail-closed oracle at re-drive time must not land (criterion
  5).
* A re-drive with no held-out source wired lands byte-for-byte as today and
  never runs the gate (criterion 6).

The discriminators mirror ``test_redriver_landing.py``: a land advances ``main``
and destroys the worktree; a park leaves the base byte-for-byte unchanged and
the worktree on disk. Isolation of the gate as the sole blocker is deliberate --
the strategy is left *willing* to land (``verify_command`` cleared to ``true``,
a landable committed change), so a re-drive that does not land did so because
the gate blocked it, not the strategy.
"""

from __future__ import annotations

import json
import subprocess
from datetime import datetime, timezone
from pathlib import Path

from flywheel_core import CommandGrader, Grader, Lifecycle, Status, Task
from flywheel_core.events import (
    PARK_KIND_HELD_OUT_GATE,
    GateGraderReceipt,
    HeldOutGateEvaluated,
    LandingParked,
)
from flywheel_core.store_sqlite import SqliteStore
from flywheel_orchestrator import (
    FilesystemHeldOutGraderSource,
    SandboxRequest,
    SqliteClaimStore,
    SubmitRequest,
)
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
    """A lifecycle finalized ``DONE`` -- the terminal a parked-unlanded run sits
    in. The park/verdict recorders load this to stamp ``expected_version``."""
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
            graders=graders
            if graders is not None
            else [CommandGrader(run="true")],
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


def _verdicts(store: SqliteStore, run_id: str) -> list[HeldOutGateEvaluated]:
    return [
        e
        for e in store.list_domain_events(run_id)
        if isinstance(e, HeldOutGateEvaluated)
    ]


def _register_held_out(root: Path, task_id: str, entries: object) -> None:
    root.mkdir(parents=True, exist_ok=True)
    (root / f"{task_id}.json").write_text(json.dumps(entries))


def _seed_stale_verdict(store: SqliteStore, run_id: str, nonce: str) -> None:
    """Append a prior ``HeldOutGateEvaluated`` (a 'first evaluation' record) so
    a re-drive that merely REUSES it -- rather than running its own fresh gate --
    is observably distinct: the fresh re-evaluation must append a NEW record
    carrying its OWN grader output, never lean on this stale one."""
    lifecycle = store.load_lifecycle(run_id)
    assert lifecycle is not None
    store.append_domain_event(
        HeldOutGateEvaluated(
            run_id=run_id,
            ts=_BASE,
            outcome="fail",
            reason=f"stale prior evaluation {nonce}",
            receipts=(
                GateGraderReceipt(
                    grader_name="stale-oracle",
                    passed=False,
                    output_excerpt=nonce,
                ),
            ),
        ),
        expected_version=lifecycle.version,
    )


# --- criteria 1 + 3: a passing re-evaluation lands and records its OWN verdict


def test_reevaluation_gate_pass_lands_and_records_fresh_verdict(
    tmp_path: Path,
) -> None:
    """A run parked on a failed standing invariant lands on re-drive once the
    invariant clears AND a fresh held-out gate passes against the content -- and
    the re-evaluation appends its own verdict record carrying the live oracle
    output, never reusing the seeded prior (stale) record."""
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
        assert len(_parks(control, "run-1")) == 1

        # A prior (stale) verdict record on the ledger. A conforming re-drive
        # runs a fresh gate and appends its OWN record; a reuse cheat would not.
        _seed_stale_verdict(control, "run-1", "STALE_NONCE_zzz9")

        # The cause clears and a passing held-out oracle is registered; the
        # oracle prints a live marker so the fresh verdict is identifiable.
        s.verify_command = "true"
        held_out = tmp_path / "held_out"
        _register_held_out(
            held_out,
            "t1",
            [
                {
                    "type": "command",
                    "run": "echo GATE_LIVE_NONCE_a1b2c3",
                    "name": "oracle-pass",
                }
            ],
        )

        outcomes = redrive_parked_landings(
            control,
            claims,
            s,
            "worker-a",
            requests=[req],
            bound=3,
            held_out_source=FilesystemHeldOutGraderSource(root=held_out),
            lease_seconds=3600,
            now=_frozen(_BASE),
        )

        # Landed after the fresh gate passed: base advanced, worktree gone.
        assert [o.result for o in outcomes] == ["landed"]
        assert outcomes[0].attempts == 1
        assert _rev(repo, "main") != base_before
        assert (repo / "feature.txt").exists()
        assert not wt.exists()
        assert claims.list_human_review_queue() == []

        # The re-evaluation appended its OWN verdict record: the stale prior
        # record is still there, plus a fresh PASS carrying the live oracle
        # output -- never the stale nonce.
        records = _verdicts(control, "run-1")
        assert len(records) == 2
        passes = [r for r in records if r.outcome == "pass"]
        assert len(passes) == 1
        fresh = passes[0]
        assert any(
            "GATE_LIVE_NONCE_a1b2c3" in rec.output_excerpt
            for rec in fresh.receipts
        )
        assert all(
            "STALE_NONCE_zzz9" not in rec.output_excerpt
            for rec in fresh.receipts
        )
    finally:
        control.close()
        claims.close()


# --- criteria 5 + 7: a failing re-evaluation re-parks, counts, then routes ----


def test_reevaluation_gate_fail_reparks_consumes_bound_and_routes(
    tmp_path: Path,
) -> None:
    """The strategy is willing to land (standing invariant cleared, a landable
    change) but the held-out gate FAILS on every re-attempt: the land is
    suppressed, each re-attempt re-parks with the ``held-out-gate`` kind and
    consumes a re-drive-bound attempt, and at the bound the run routes to the
    human-review queue keyed to ``held-out-gate`` -- with no ``bound+1``-th
    attempt on a second pass."""
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

        # Park #1 -- the original land-suppression (standing invariant fails).
        s.submit(req)
        assert len(_parks(control, "run-1")) == 1

        # Strategy now WILLING to land; only the failing held-out gate blocks.
        s.verify_command = "true"
        held_out = tmp_path / "held_out"
        _register_held_out(
            held_out,
            "t1",
            [{"type": "command", "run": "false", "name": "oracle-fail"}],
        )
        source = FilesystemHeldOutGraderSource(root=held_out)

        outcomes = redrive_parked_landings(
            control,
            claims,
            s,
            "worker-a",
            requests=[req],
            bound=2,
            held_out_source=source,
            lease_seconds=3600,
            now=_frozen(_BASE),
        )

        # Routed after exactly `bound` re-attempts, keyed to the gate.
        assert [o.result for o in outcomes] == ["queued"]
        assert outcomes[0].attempts == 2
        assert outcomes[0].park_kind == PARK_KIND_HELD_OUT_GATE
        # Nothing landed: base untouched, worktree still parked on disk.
        assert _rev(repo, "main") == base_before
        assert not (repo / "feature.txt").exists()
        assert wt.exists()
        # Park #1 (standing-verify) + one held-out-gate park per re-attempt.
        parks = _parks(control, "run-1")
        assert [p.park_kind for p in parks] == [
            "standing-verify",
            PARK_KIND_HELD_OUT_GATE,
            PARK_KIND_HELD_OUT_GATE,
        ]
        # Each re-attempt ran a fresh gate: one FAIL verdict record per attempt.
        assert [v.outcome for v in _verdicts(control, "run-1")] == [
            "fail",
            "fail",
        ]
        # Routed once, to the human-review queue, keyed to the gate.
        queue = claims.list_human_review_queue()
        assert len(queue) == 1
        assert queue[0].reason == PARK_KIND_HELD_OUT_GATE
        assert queue[0].task_id == "t1"
        assert queue[0].run_id == "run-1"

        # Second pass: the already-queued guard short-circuits. No bound+1 land
        # attempt, no fresh park, no fresh gate evaluation, no re-queue.
        outcomes2 = redrive_parked_landings(
            control,
            claims,
            s,
            "worker-a",
            requests=[req],
            bound=2,
            held_out_source=source,
            lease_seconds=3600,
            now=_frozen(_BASE),
        )
        assert [o.result for o in outcomes2] == ["queued"]
        assert outcomes2[0].attempts == 0
        assert len(_parks(control, "run-1")) == 3
        assert len(_verdicts(control, "run-1")) == 2
        assert len(claims.list_human_review_queue()) == 1
    finally:
        control.close()
        claims.close()


# --- criterion 5: an unexecutable oracle at re-drive time must not land -------


def test_reevaluation_unexecutable_oracle_does_not_land(tmp_path: Path) -> None:
    """A registered but unexecutable held-out oracle fails the gate closed on
    re-drive, so the run does NOT land -- even though the strategy itself would
    have (invariant cleared, landable change). The block is the gate's: a fresh
    ``held-out-gate`` park witnesses it."""
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

        # Strategy willing to land; the oracle points at a command that cannot
        # execute -> the gate fails closed (never a silent pass / land).
        s.verify_command = "true"
        held_out = tmp_path / "held_out"
        _register_held_out(
            held_out,
            "t1",
            [
                {
                    "type": "command",
                    "run": "/nonexistent/held-out-oracle-xyz-missing",
                    "name": "oracle-unexecutable",
                }
            ],
        )

        outcomes = redrive_parked_landings(
            control,
            claims,
            s,
            "worker-a",
            requests=[req],
            bound=1,
            held_out_source=FilesystemHeldOutGraderSource(root=held_out),
            lease_seconds=3600,
            now=_frozen(_BASE),
        )

        # It must NOT land: base untouched, worktree still parked on disk.
        assert outcomes[0].result != "landed"
        assert _rev(repo, "main") == base_before
        assert not (repo / "feature.txt").exists()
        assert wt.exists()
        # A held-out-gate park witnesses the fail-closed block, plus a FAIL
        # verdict record from the fresh evaluation.
        parks = _parks(control, "run-1")
        assert parks[-1].park_kind == PARK_KIND_HELD_OUT_GATE
        assert [v.outcome for v in _verdicts(control, "run-1")] == ["fail"]
    finally:
        control.close()
        claims.close()


# --- criterion 6: an ungated re-drive lands byte-for-byte as today -----------


def test_reevaluation_no_source_lands_byte_for_byte(tmp_path: Path) -> None:
    """With no held-out source wired, the re-driver never runs the gate and the
    parked change lands exactly as it does today once its cause clears -- no
    verdict record is written."""
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

        s.submit(req)
        assert len(_parks(control, "run-1")) == 1

        s.verify_command = "true"
        outcomes = redrive_parked_landings(
            control,
            claims,
            s,
            "worker-a",
            requests=[req],
            bound=3,
            held_out_source=None,
            lease_seconds=3600,
            now=_frozen(_BASE),
        )

        # Landed exactly as the un-gated re-driver does today.
        assert [o.result for o in outcomes] == ["landed"]
        assert outcomes[0].attempts == 1
        assert _rev(repo, "main") != base_before
        assert (repo / "feature.txt").exists()
        assert not wt.exists()
        assert claims.list_human_review_queue() == []
        assert len(_parks(control, "run-1")) == 1
        # The gate never ran: no verdict record on the ledger.
        assert _verdicts(control, "run-1") == []
    finally:
        control.close()
        claims.close()

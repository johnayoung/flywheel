"""Operator strand-resolution verb (spec 00077, criterion 5 / D-3).

``flywheel resolve TASK_ID --reason TEXT`` deliberately abandons a strand: it
records an operator-attributed
:data:`~flywheel_orchestrator._claims.STOP_RESOLVED` marker keyed to the task id
(the stop-event subject), carrying the reason verbatim, through the
policy-selected claim store. The next archive sweep then treats that strand as
deliberately abandoned -- no longer blocking -- and archives the otherwise-landed
phase.

These build a real git repo so the sweep's ancestry probe has branches to walk,
and drive the verb through the orchestrator CLI entry point (``main``) against an
on-file sqlite store shared with the claim store, so the verb-then-sweep path is
exercised end to end exactly as an operator runs it.
"""

from __future__ import annotations

import json
import subprocess
from datetime import datetime, timezone
from pathlib import Path

import pytest

from flywheel_core.events import LANDING_STRAND_KINDS, PARK_KIND_DIVERGENT_BASE
from flywheel_core.lifecycle import Lifecycle, Status
from flywheel_core.store_sqlite import SqliteStore
from flywheel_orchestrator._claims import (
    RESOLUTION_ATTRIBUTION_OPERATOR,
    STOP_RESOLVED,
    SqliteClaimStore,
)
from flywheel_orchestrator._workflow import archive_completed_phases
from flywheel_orchestrator._workflow import main as orch_main

# ---------- git helpers ----------


def _git(repo: Path, *args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git", "-C", str(repo), *args],
        check=True,
        capture_output=True,
        text=True,
    )


def _git_init_repo(repo: Path) -> None:
    repo.mkdir(parents=True, exist_ok=True)
    _git(repo, "init", "-b", "main")
    _git(repo, "config", "user.email", "resolve-test@example.com")
    _git(repo, "config", "user.name", "resolve test")
    _git(repo, "config", "commit.gpgsign", "false")
    (repo / "README.md").write_text("base\n")
    _git(repo, "add", "-A")
    _git(repo, "commit", "-m", "init")


def _git_head(repo: Path) -> str:
    return _git(repo, "rev-parse", "HEAD").stdout.strip()


def _branch_off_main(repo: Path, branch: str, filename: str) -> str:
    """Point ``branch`` at a commit that is NOT an ancestor of ``main``.

    A side commit main never merged, so ``git merge-base --is-ancestor`` is
    false and the landing probe reports NOT_LANDED -- the divergent-base park
    that stays a strand until git-truth or an operator clears it.
    """
    base = _git_head(repo)
    _git(repo, "checkout", "-b", branch, base)
    (repo / filename).write_text("divergent work\n")
    _git(repo, "add", "-A")
    _git(repo, "commit", "-m", f"feat: {branch} side work")
    head = _git_head(repo)
    _git(repo, "checkout", "main")
    return head


# ---------- store / task helpers ----------


def _tasks_dir(repo: Path) -> Path:
    return repo / ".flywheel" / "tasks"


def _write_task(phase_dir: Path, task_id: str) -> Path:
    phase_dir.mkdir(parents=True, exist_ok=True)
    path = phase_dir / f"{task_id}.json"
    path.write_text(
        json.dumps(
            {
                "id": task_id,
                "goal": f"Goal for {task_id}.",
                "graders": [{"type": "command", "run": "true"}],
            }
        )
    )
    return path


def _seed_done(store: SqliteStore, task_id: str) -> Lifecycle:
    now = datetime.now(timezone.utc)
    lc = Lifecycle(task_id=task_id, run_id=f"run-{task_id}")
    lc.transition_to(Status.READY, now=now)
    lc.transition_to(Status.RUNNING, now=now)
    lc.transition_to(Status.VALIDATING, now=now)
    lc.transition_to(Status.DONE, now=now)
    store.create_lifecycle(lc)
    return lc


def _route_landing_strand(
    claims: SqliteClaimStore, task_id: str, park_kind: str, run_id: str
) -> None:
    """Seed a landing strand exactly as the redrive router does.

    A parked DONE landing routed past its re-drive bound lands in the shared
    ledger via ``record_human_review`` with its ``park_kind`` AS the stop kind,
    so the strand's latest stop row's ``kind`` is a member of
    :data:`LANDING_STRAND_KINDS`.
    """
    assert park_kind in LANDING_STRAND_KINDS
    claims.record_human_review(
        reason=park_kind,
        task_id=task_id,
        run_id=run_id,
        detail=f"landing re-drive exhausted; last park cause {park_kind!r}",
        occurred_at=datetime.now(timezone.utc),
    )


def _sweep(tasks_dir: Path, repo: Path, db: Path) -> list[Path]:
    """One armed archive sweep over ``tasks_dir`` sharing the on-file store."""
    store = SqliteStore(db)
    claims = SqliteClaimStore(db)
    try:
        return archive_completed_phases(
            tasks_dir,
            store,
            repo_root=repo,
            landing_base="main",
            claims=claims,
        )
    finally:
        claims.close()
        store.close()


def _subject_rows(db: Path, task_id: str) -> list:
    claims = SqliteClaimStore(db)
    try:
        return claims.list_subject_stop_events(task_id)
    finally:
        claims.close()


# ---------- tests ----------


def test_resolve_verb_records_operator_marker_and_next_sweep_archives(
    tmp_path: Path,
) -> None:
    """Criterion 5 end-to-end: an operator resolves a still-diverged landing
    strand through the verb; the stop event records operator attribution plus
    the reason text, and the next sweep archives the otherwise-landed phase.

    Before the verb the strand blocks archival (its branch is not an ancestor
    of the base); the operator resolution is the only non-probe path that clears
    it, so the same sweep that refused now archives.
    """
    repo = tmp_path / "repo"
    _git_init_repo(repo)
    tasks_dir = _tasks_dir(repo)
    phase_dir = tasks_dir / "active" / "01-landed"
    _branch_off_main(repo, "flywheel/01-landed/feat-a", "feat_a.txt")
    _write_task(phase_dir, "feat-a")

    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    claims = SqliteClaimStore(db)
    try:
        _seed_done(store, "feat-a")
        _route_landing_strand(
            claims, "feat-a", PARK_KIND_DIVERGENT_BASE, run_id="run-feat-a"
        )
    finally:
        claims.close()
        store.close()

    # Before resolve: the unlanded strand keeps the phase active.
    assert _sweep(tasks_dir, repo, db) == []
    assert phase_dir.is_dir()

    reason = "superseded by a follow-up rewrite; will not land"
    rc = orch_main(["resolve", "feat-a", "--reason", reason, "--db", str(db)])
    assert rc == 0

    # The marker is operator-attributed and carries the reason verbatim.
    rows = _subject_rows(db, "feat-a")
    assert rows[-1].kind == STOP_RESOLVED
    assert rows[-1].subject == "feat-a"
    assert rows[-1].attribution == RESOLUTION_ATTRIBUTION_OPERATOR
    assert reason in rows[-1].detail

    # The next sweep archives the otherwise-landed phase in one pass.
    moved = _sweep(tasks_dir, repo, db)
    assert [p.name for p in moved] == [phase_dir.name]
    assert (tasks_dir / "archive" / phase_dir.name).is_dir()
    assert not phase_dir.exists()

    # Archival did NOT append a second marker over the operator resolution.
    after = _subject_rows(db, "feat-a")
    assert [r.kind for r in after] == [PARK_KIND_DIVERGENT_BASE, STOP_RESOLVED]
    assert after[-1].attribution == RESOLUTION_ATTRIBUTION_OPERATOR


def test_resolve_reason_round_trips_verbatim_into_the_marker(
    tmp_path: Path,
) -> None:
    """The exact reason text the operator passes is recorded verbatim on the
    marker the audit trail shows -- criterion 5 asserts on the reason."""
    repo = tmp_path / "repo"
    _git_init_repo(repo)
    tasks_dir = _tasks_dir(repo)
    phase_dir = tasks_dir / "active" / "01-landed"
    _branch_off_main(repo, "flywheel/01-landed/feat-a", "feat_a.txt")
    _write_task(phase_dir, "feat-a")

    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    claims = SqliteClaimStore(db)
    try:
        _seed_done(store, "feat-a")
        _route_landing_strand(
            claims, "feat-a", PARK_KIND_DIVERGENT_BASE, run_id="run-feat-a"
        )
    finally:
        claims.close()
        store.close()

    reason = "abandoned: replaced by PR #4021 (branch 'feat/x-v2'), 90% overlap"
    rc = orch_main(["resolve", "feat-a", "--reason", reason, "--db", str(db)])
    assert rc == 0

    rows = _subject_rows(db, "feat-a")
    assert reason in rows[-1].detail, (
        f"reason must round-trip verbatim, got detail {rows[-1].detail!r}"
    )


def test_resolve_without_an_unresolved_stop_event_refuses_cleanly(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """Edge: resolving a task id with no unresolved stop event is a
    deterministic, human-readable refusal (exit 1), never a traceback, and it
    writes nothing to the ledger."""
    db = tmp_path / "db.sqlite"
    SqliteClaimStore(db).close()

    rc = orch_main(
        ["resolve", "ghost", "--reason", "nothing to abandon", "--db", str(db)]
    )
    assert rc == 1
    err = capsys.readouterr().err
    assert "ghost" in err
    assert "no unresolved stop event" in err
    assert _subject_rows(db, "ghost") == []


def test_resolve_is_idempotent_refusal_on_an_already_resolved_strand(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """A second resolve on an already-resolved subject refuses (exit 1) rather
    than stacking a duplicate marker: the latest row is already a resolution,
    so there is no unresolved strand to abandon."""
    db = tmp_path / "db.sqlite"
    claims = SqliteClaimStore(db)
    try:
        claims.record_human_review(
            reason=PARK_KIND_DIVERGENT_BASE,
            task_id="feat-a",
            run_id="run-feat-a",
            detail="parked",
            occurred_at=datetime.now(timezone.utc),
        )
    finally:
        claims.close()

    assert orch_main(
        ["resolve", "feat-a", "--reason", "first abandon", "--db", str(db)]
    ) == 0
    capsys.readouterr()

    rc = orch_main(
        ["resolve", "feat-a", "--reason", "second abandon", "--db", str(db)]
    )
    assert rc == 1
    assert "already resolved" in capsys.readouterr().err

    rows = _subject_rows(db, "feat-a")
    assert [r.kind for r in rows] == [PARK_KIND_DIVERGENT_BASE, STOP_RESOLVED]


def test_resolve_empty_reason_exits_two_without_touching_the_store(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """A whitespace-only ``--reason`` is a usage error (exit 2) and records
    nothing -- the reason is the audit trail's whole point."""
    db = tmp_path / "db.sqlite"
    claims = SqliteClaimStore(db)
    try:
        claims.record_human_review(
            reason=PARK_KIND_DIVERGENT_BASE,
            task_id="feat-a",
            run_id="run-feat-a",
            detail="parked",
            occurred_at=datetime.now(timezone.utc),
        )
    finally:
        claims.close()

    rc = orch_main(["resolve", "feat-a", "--reason", "   ", "--db", str(db)])
    assert rc == 2
    assert "reason" in capsys.readouterr().err.lower()
    # The strand's ledger is untouched: only the park row, no resolution.
    assert [r.kind for r in _subject_rows(db, "feat-a")] == [
        PARK_KIND_DIVERGENT_BASE
    ]


def test_resolving_one_strand_does_not_unblock_a_different_strand(
    tmp_path: Path,
) -> None:
    """Edge: an operator resolution on one strand must not unblock a phase that
    still has a different unlanded, unresolved task. Each task is judged on its
    own latest marker, so the phase stays active until every strand clears."""
    repo = tmp_path / "repo"
    _git_init_repo(repo)
    tasks_dir = _tasks_dir(repo)
    phase_dir = tasks_dir / "active" / "01-landed"
    _branch_off_main(repo, "flywheel/01-landed/feat-a", "feat_a.txt")
    _branch_off_main(repo, "flywheel/01-landed/feat-b", "feat_b.txt")
    _write_task(phase_dir, "feat-a")
    _write_task(phase_dir, "feat-b")

    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    claims = SqliteClaimStore(db)
    try:
        _seed_done(store, "feat-a")
        _seed_done(store, "feat-b")
        _route_landing_strand(
            claims, "feat-a", PARK_KIND_DIVERGENT_BASE, run_id="run-feat-a"
        )
        _route_landing_strand(
            claims, "feat-b", PARK_KIND_DIVERGENT_BASE, run_id="run-feat-b"
        )
    finally:
        claims.close()
        store.close()

    rc = orch_main(
        ["resolve", "feat-a", "--reason", "abandon a only", "--db", str(db)]
    )
    assert rc == 0

    # feat-b is still an unresolved, unlanded strand -> phase stays active.
    assert _sweep(tasks_dir, repo, db) == []
    assert phase_dir.is_dir()
    assert not (tasks_dir / "archive" / phase_dir.name).exists()

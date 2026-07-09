"""Landed predicate for the archive sweep (spec 00077, D-1/D-4).

A phase archives only when every DONE task's verified work is *landed*: a
:class:`~flywheel_core.events.Landed` receipt on its latest run (the fast path,
D-1), or -- for receipt-less / hand-landed work -- its recorded work
(the ``flywheel/<phase>/<task-id>`` branch head) is an ancestor of the landing
base at sweep time. A DONE task whose branch head is *not* an ancestor is a
determinate strand that blocks the phase and names the task; a task with no
receipt and no resolvable head is *indeterminate* -- landing state cannot be
determined -- so it fails closed, blocking the phase AND surfacing an
``indeterminate-landing`` stop row keyed to the task id (D-4). Cannot-check
never counts as landed.

These build a real git repo so the ancestry probe has branches to walk, and an
on-file sqlite store shared with the orchestrator claim store so the
indeterminate strand's ``status`` surfacing is exercised end to end.
"""

from __future__ import annotations

import json
import subprocess
from datetime import datetime, timezone
from pathlib import Path

import pytest

from flywheel_core.events import Landed
from flywheel_core.lifecycle import Lifecycle, Status
from flywheel_core.store_sqlite import SqliteStore
from flywheel_orchestrator._claims import (
    STOP_INDETERMINATE_LANDING,
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
    _git(repo, "config", "user.email", "landed-test@example.com")
    _git(repo, "config", "user.name", "landed test")
    _git(repo, "config", "commit.gpgsign", "false")
    (repo / "README.md").write_text("base\n")
    _git(repo, "add", "-A")
    _git(repo, "commit", "-m", "init")


def _git_commit_file(repo: Path, name: str, body: str, message: str) -> None:
    (repo / name).write_text(body)
    _git(repo, "add", "-A")
    _git(repo, "commit", "-m", message)


def _git_head(repo: Path) -> str:
    return _git(repo, "rev-parse", "HEAD").stdout.strip()


def _branch_on_main(repo: Path, branch: str, filename: str) -> str:
    """Point ``branch`` at a commit that IS an ancestor of ``main``.

    Commits ``filename`` on main, pins ``branch`` at that commit, then advances
    main one more commit so the branch head is strictly behind main -- the
    landed-by-ancestry (hand-landed) case, where no receipt exists yet the work
    is provably part of the landing base.
    """
    _git_commit_file(repo, filename, "landed\n", f"feat: {branch} landed")
    head = _git_head(repo)
    _git(repo, "branch", branch, head)
    _git_commit_file(repo, f"advance-{filename}", "x\n", "chore: advance base")
    return head


def _branch_off_main(repo: Path, branch: str, filename: str) -> str:
    """Point ``branch`` at a commit that is NOT an ancestor of ``main``.

    Creates a side commit off the current main head that main never merges, so
    ``git merge-base --is-ancestor <head> main`` is false -- the determinate
    not-landed (divergent-base park) case. Restores main as the checked-out
    branch so callers can keep committing to it.
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


def _seed_failed(store: SqliteStore, task_id: str) -> Lifecycle:
    now = datetime.now(timezone.utc)
    lc = Lifecycle(task_id=task_id, run_id=f"run-{task_id}-fail")
    lc.transition_to(Status.READY, now=now)
    lc.transition_to(Status.RUNNING, now=now)
    lc.transition_to(Status.FAILED, error="boom", now=now)
    store.create_lifecycle(lc)
    return lc


def _record_landed(store: SqliteStore, lc: Lifecycle, landed_ref: str) -> None:
    """Append a ``Landed`` receipt on ``lc``'s run, as a machine land does."""
    loaded = store.load_lifecycle(lc.run_id)
    assert loaded is not None
    store.append_domain_event(
        Landed(
            run_id=lc.run_id,
            ts=datetime.now(timezone.utc),
            strategy="merge",
            landed_ref=landed_ref,
            rung="fast-forward",
        ),
        expected_version=loaded.version,
    )


# ---------- tests ----------


def test_divergent_base_task_blocks_archive_and_names_blocking_task(
    tmp_path: Path,
) -> None:
    """Criterion 1 + anti-cheat: a DONE task whose branch head is not an
    ancestor of the landing base keeps the phase active and names the task.

    This forecloses the "probe that always answers landed" cheat: if the probe
    ignored real ancestry the divergent-base strand would archive. The verdict
    is keyed on ``git merge-base --is-ancestor``, so a branch main never merged
    stays a strand.
    """
    repo = tmp_path / "repo"
    _git_init_repo(repo)
    tasks_dir = _tasks_dir(repo)
    phase_dir = tasks_dir / "active" / "01-landed"

    _branch_off_main(repo, "flywheel/01-landed/feat-a", "feat_a.txt")
    _write_task(phase_dir, "feat-a")

    logged: list[str] = []
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    claims = SqliteClaimStore(db)
    try:
        _seed_done(store, "feat-a")
        moved = archive_completed_phases(
            tasks_dir,
            store,
            repo_root=repo,
            log=logged.append,
            landing_base="main",
            claims=claims,
        )
        # Determinate not-landed: a strand, but NOT indeterminate -- no
        # fail-closed stop row is minted for it.
        assert claims.list_subject_stop_events("feat-a") == []
    finally:
        claims.close()
        store.close()

    assert moved == []
    assert phase_dir.is_dir(), "unlanded phase must remain in active/"
    assert not (tasks_dir / "archive" / phase_dir.name).exists()
    assert any(
        "Refusing to archive" in line
        and phase_dir.name in line
        and "feat-a" in line
        and "not landed" in line
        for line in logged
    ), f"expected a named landing refusal, got {logged!r}"


def test_all_landed_mix_of_receipt_and_ancestry_archives_in_one_sweep(
    tmp_path: Path,
) -> None:
    """Criterion 2: a phase whose tasks are all landed -- one by receipt, one by
    ancestry only (hand-landed, no receipt) -- archives in a single sweep with
    no refusal logged."""
    repo = tmp_path / "repo"
    _git_init_repo(repo)
    tasks_dir = _tasks_dir(repo)
    phase_dir = tasks_dir / "active" / "01-landed"

    # Hand-landed task: no receipt, branch head is an ancestor of main.
    _branch_on_main(repo, "flywheel/01-landed/feat-hand", "hand.txt")
    _write_task(phase_dir, "feat-hand")
    _write_task(phase_dir, "feat-receipt")

    logged: list[str] = []
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    claims = SqliteClaimStore(db)
    try:
        _seed_done(store, "feat-hand")
        lc = _seed_done(store, "feat-receipt")
        # Machine-landed task: a receipt exists; no branch is needed.
        _record_landed(store, lc, landed_ref=_git_head(repo))
        moved = archive_completed_phases(
            tasks_dir,
            store,
            repo_root=repo,
            log=logged.append,
            landing_base="main",
            claims=claims,
        )
    finally:
        claims.close()
        store.close()

    assert [p.name for p in moved] == [phase_dir.name]
    assert not phase_dir.exists()
    assert (tasks_dir / "archive" / phase_dir.name).is_dir()
    assert logged == [], f"clean archive must not log a refusal, got {logged!r}"


def test_receipt_counts_landed_even_after_branch_deleted(
    tmp_path: Path,
) -> None:
    """Fast path (D-1): a ``Landed`` receipt is authoritative proof even when
    the task's branch has been deleted after a clean land."""
    repo = tmp_path / "repo"
    _git_init_repo(repo)
    tasks_dir = _tasks_dir(repo)
    phase_dir = tasks_dir / "active" / "01-landed"
    _write_task(phase_dir, "feat-a")

    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    claims = SqliteClaimStore(db)
    try:
        lc = _seed_done(store, "feat-a")
        # No ``flywheel/01-landed/feat-a`` branch exists (deleted post-land);
        # only the receipt remains.
        _record_landed(store, lc, landed_ref=_git_head(repo))
        moved = archive_completed_phases(
            tasks_dir,
            store,
            repo_root=repo,
            landing_base="main",
            claims=claims,
        )
        assert claims.list_subject_stop_events("feat-a") == []
    finally:
        claims.close()
        store.close()

    assert [p.name for p in moved] == [phase_dir.name]
    assert (tasks_dir / "archive" / phase_dir.name).is_dir()


def test_indeterminate_landing_blocks_and_surfaces_stop_marker(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """D-4: a DONE task with no receipt and no resolvable branch fails closed --
    the phase stays active and an ``indeterminate-landing`` stop row keyed to
    the task id surfaces on ``flywheel status``."""
    repo = tmp_path / "repo"
    _git_init_repo(repo)
    tasks_dir = _tasks_dir(repo)
    phase_dir = tasks_dir / "active" / "01-landed"
    # No branch, no receipt -> landing state cannot be determined.
    _write_task(phase_dir, "feat-a")

    logged: list[str] = []
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    claims = SqliteClaimStore(db)
    try:
        _seed_done(store, "feat-a")
        moved = archive_completed_phases(
            tasks_dir,
            store,
            repo_root=repo,
            log=logged.append,
            landing_base="main",
            claims=claims,
        )
        rows = claims.list_subject_stop_events("feat-a")
        assert rows, "indeterminate strand must record a stop row"
        assert rows[-1].kind == STOP_INDETERMINATE_LANDING
        assert rows[-1].subject == "feat-a"
    finally:
        claims.close()
        store.close()

    assert moved == []
    assert phase_dir.is_dir()
    assert any(
        "Refusing to archive" in line
        and "feat-a" in line
        and "cannot be determined" in line
        for line in logged
    ), f"expected an indeterminate landing refusal, got {logged!r}"

    # The fail-closed strand surfaces on the status view (D-4).
    rc = orch_main(["status", "--tasks-dir", str(tasks_dir), "--db", str(db)])
    assert rc == 0
    assert STOP_INDETERMINATE_LANDING in capsys.readouterr().out


def test_indeterminate_marker_is_idempotent_over_repeated_sweeps(
    tmp_path: Path,
) -> None:
    """Edge: repeated sweeps over a still-indeterminate phase surface the strand
    once -- the marker is appended only when it is not already the subject's
    latest stop row."""
    repo = tmp_path / "repo"
    _git_init_repo(repo)
    tasks_dir = _tasks_dir(repo)
    phase_dir = tasks_dir / "active" / "01-landed"
    _write_task(phase_dir, "feat-a")

    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    claims = SqliteClaimStore(db)
    try:
        _seed_done(store, "feat-a")
        for _ in range(3):
            moved = archive_completed_phases(
                tasks_dir,
                store,
                repo_root=repo,
                landing_base="main",
                claims=claims,
            )
            assert moved == []
        rows = claims.list_subject_stop_events("feat-a")
        assert [r.kind for r in rows] == [STOP_INDETERMINATE_LANDING], (
            f"repeated sweeps must not flood the ledger, got {rows!r}"
        )
    finally:
        claims.close()
        store.close()


def test_landed_verdict_keys_on_real_ancestry(tmp_path: Path) -> None:
    """Differential anti-cheat: identical DONE task + branch setup, flipped only
    by whether the branch head is an ancestor of the landing base. The
    ancestor case archives; the non-ancestor case stays active. A probe that
    ignored ancestry could not produce both outcomes."""
    # Ancestor -> LANDED -> archives.
    repo_ok = tmp_path / "ok"
    _git_init_repo(repo_ok)
    tasks_ok = _tasks_dir(repo_ok)
    phase_ok = tasks_ok / "active" / "01-landed"
    _branch_on_main(repo_ok, "flywheel/01-landed/feat-a", "a.txt")
    _write_task(phase_ok, "feat-a")

    # Non-ancestor -> NOT_LANDED -> stays active.
    repo_bad = tmp_path / "bad"
    _git_init_repo(repo_bad)
    tasks_bad = _tasks_dir(repo_bad)
    phase_bad = tasks_bad / "active" / "01-landed"
    _branch_off_main(repo_bad, "flywheel/01-landed/feat-a", "a.txt")
    _write_task(phase_bad, "feat-a")

    store_ok = SqliteStore(repo_ok / "db.sqlite")
    store_bad = SqliteStore(repo_bad / "db.sqlite")
    try:
        _seed_done(store_ok, "feat-a")
        _seed_done(store_bad, "feat-a")
        moved_ok = archive_completed_phases(
            tasks_ok, store_ok, repo_root=repo_ok, landing_base="main"
        )
        moved_bad = archive_completed_phases(
            tasks_bad, store_bad, repo_root=repo_bad, landing_base="main"
        )
    finally:
        store_ok.close()
        store_bad.close()

    assert [p.name for p in moved_ok] == ["01-landed"]
    assert moved_bad == []
    assert phase_bad.is_dir()


def test_predicate_disarmed_without_landing_base_preserves_legacy_archival(
    tmp_path: Path,
) -> None:
    """Opt-in gating: with ``repo_root`` but no ``landing_base`` the landed
    predicate is disarmed, so an all-DONE phase archives even though its task's
    branch is a divergent (would-block) strand -- the legacy DONE-only contract
    that the loop-path/worker-seam pins depend on."""
    repo = tmp_path / "repo"
    _git_init_repo(repo)
    tasks_dir = _tasks_dir(repo)
    phase_dir = tasks_dir / "active" / "01-landed"
    _branch_off_main(repo, "flywheel/01-landed/feat-a", "a.txt")
    _write_task(phase_dir, "feat-a")

    store = SqliteStore(tmp_path / "db.sqlite")
    try:
        _seed_done(store, "feat-a")
        moved = archive_completed_phases(tasks_dir, store, repo_root=repo)
    finally:
        store.close()

    assert [p.name for p in moved] == [phase_dir.name]
    assert (tasks_dir / "archive" / phase_dir.name).is_dir()


def test_non_done_task_keeps_phase_active_even_with_predicate_armed(
    tmp_path: Path,
) -> None:
    """No-regress: a phase with any non-DONE task stays active and the all-DONE
    short-circuit fires before the landed predicate -- no landing refusal is
    logged for a phase that was never eligible."""
    repo = tmp_path / "repo"
    _git_init_repo(repo)
    tasks_dir = _tasks_dir(repo)
    phase_dir = tasks_dir / "active" / "01-landed"
    _branch_on_main(repo, "flywheel/01-landed/feat-a", "a.txt")
    _write_task(phase_dir, "feat-a")
    _write_task(phase_dir, "feat-b")

    logged: list[str] = []
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    claims = SqliteClaimStore(db)
    try:
        _seed_done(store, "feat-a")
        _seed_failed(store, "feat-b")
        moved = archive_completed_phases(
            tasks_dir,
            store,
            repo_root=repo,
            log=logged.append,
            landing_base="main",
            claims=claims,
        )
    finally:
        claims.close()
        store.close()

    assert moved == []
    assert phase_dir.is_dir()
    assert not any("not landed" in line for line in logged), (
        f"predicate must not run for an ineligible phase, got {logged!r}"
    )

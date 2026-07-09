"""Landing-strand stop-event resolution attribution (spec 00077, criteria 3/4).

A landing-strand stop event -- a DONE task parked under one of the four
git-truth-gated park kinds in
:data:`~flywheel_core.events.LANDING_STRAND_KINDS` (``uncommitted-work``,
``divergent-base``, ``standing-verify``, ``protected-paths``) and routed to the
human-review queue -- survives *every* archive sweep unresolved until git-truth
or an operator clears it. The sweep never stamps a resolution on an unlanded
strand (criterion 3); when the sweep's own landability probe confirms the
stranded work reachable from the landing base, it appends exactly one
probe-attributed :data:`~flywheel_orchestrator._claims.STOP_RESOLVED` marker and
archives the otherwise-landed phase in that same sweep (criterion 4 / D-2).

The attribution is a machine-readable token on the stop-event *record* (its
``attribution`` column -- :data:`RESOLUTION_ATTRIBUTION_PROBE` or
:data:`RESOLUTION_ATTRIBUTION_OPERATOR`), queried directly and never parsed out
of the free-text ``detail`` prose. Archival is never an attributor: a
non-landing strand keeps the plain, unattributed archival-supersession marker,
and an unarmed sweep (no probe) leaves every landing strand surfaced.

These build a real git repo so the ancestry probe has branches to walk, and an
on-file sqlite store shared with the orchestrator claim store so the strand's
end-to-end ledger surfacing is exercised.
"""

from __future__ import annotations

import json
import subprocess
from datetime import datetime, timezone
from pathlib import Path

from flywheel_core.events import (
    LANDING_STRAND_KINDS,
    PARK_KIND_DIVERGENT_BASE,
    PARK_KIND_STANDING_VERIFY,
    Landed,
)
from flywheel_core.lifecycle import Lifecycle, Status
from flywheel_core.store_sqlite import SqliteStore
from flywheel_orchestrator._claims import (
    RESOLUTION_ATTRIBUTION_OPERATOR,
    RESOLUTION_ATTRIBUTION_PROBE,
    STOP_DANGLING_PREREQUISITE,
    STOP_RESOLVED,
    SqliteClaimStore,
)
from flywheel_orchestrator._workflow import archive_completed_phases

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
    _git(repo, "config", "user.email", "strand-test@example.com")
    _git(repo, "config", "user.name", "strand test")
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

    Models an operator who hand-landed the stranded work: the branch head is
    strictly behind main, so ``git merge-base --is-ancestor`` is true and the
    landing probe reports LANDED even though no ``Landed`` receipt was ever
    written.
    """
    _git_commit_file(repo, filename, "landed\n", f"feat: {branch} landed")
    head = _git_head(repo)
    _git(repo, "branch", branch, head)
    _git_commit_file(repo, f"advance-{filename}", "x\n", "chore: advance base")
    return head


def _branch_off_main(repo: Path, branch: str, filename: str) -> str:
    """Point ``branch`` at a commit that is NOT an ancestor of ``main``.

    Models a still-diverged strand: a side commit main never merged, so
    ``git merge-base --is-ancestor <head> main`` is false and the probe reports
    NOT_LANDED -- the divergent-base park that must stay a strand.
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


def _route_landing_strand(
    claims: SqliteClaimStore, task_id: str, park_kind: str, run_id: str
) -> None:
    """Seed the landing strand exactly as the redrive router does.

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


# ---------- tests ----------


def test_unlanded_landing_strand_never_resolved_over_repeated_sweeps(
    tmp_path: Path,
) -> None:
    """Criterion 3 + anti-cheat: repeated sweeps over a phase blocked by a
    still-diverged landing strand append NO resolution marker to that strand.

    This forecloses the "probe that always answers landed" cheat: a divergent
    branch main never merged is NOT_LANDED, so the phase never archives and the
    sweep never stamps a resolution. If the probe ignored real ancestry the
    strand would archive and be falsely resolved.
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
    finally:
        claims.close()
        store.close()

    # The strand's ledger is untouched by the sweeps: exactly the one park
    # row, no STOP_RESOLVED. The unlanded strand survives every sweep.
    assert [r.kind for r in rows] == [PARK_KIND_DIVERGENT_BASE]
    assert not any(r.kind == STOP_RESOLVED for r in rows)
    assert phase_dir.is_dir(), "unlanded phase must remain in active/"
    assert not (tasks_dir / "archive" / phase_dir.name).exists()


def test_probe_confirmed_landing_appends_probe_resolution_and_archives(
    tmp_path: Path,
) -> None:
    """Criterion 4 / D-2: when the ancestry probe confirms a stranded task's
    work reachable from the landing base, the sweep appends ONE probe-attributed
    resolution and archives the otherwise-landed phase in that same sweep.

    The strand was parked ``divergent-base`` and then hand-landed (its branch
    rebased onto main). One armed sweep both clears the strand -- attributed to
    the probe -- and moves the phase to ``archive/``.
    """
    repo = tmp_path / "repo"
    _git_init_repo(repo)
    tasks_dir = _tasks_dir(repo)
    phase_dir = tasks_dir / "active" / "01-landed"

    _branch_on_main(repo, "flywheel/01-landed/feat-a", "a.txt")
    _write_task(phase_dir, "feat-a")

    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    claims = SqliteClaimStore(db)
    try:
        _seed_done(store, "feat-a")
        _route_landing_strand(
            claims, "feat-a", PARK_KIND_DIVERGENT_BASE, run_id="run-feat-a"
        )
        moved = archive_completed_phases(
            tasks_dir,
            store,
            repo_root=repo,
            landing_base="main",
            claims=claims,
        )
        rows = claims.list_subject_stop_events("feat-a")
    finally:
        claims.close()
        store.close()

    # Archived in the same sweep.
    assert [p.name for p in moved] == [phase_dir.name]
    assert (tasks_dir / "archive" / phase_dir.name).is_dir()
    assert not phase_dir.exists()

    # Exactly one resolution was appended after the park, attributed to the
    # probe -- read from the record's attribution field, never the detail prose.
    assert [r.kind for r in rows] == [
        PARK_KIND_DIVERGENT_BASE,
        STOP_RESOLVED,
    ]
    resolution = rows[-1]
    assert resolution.kind == STOP_RESOLVED
    assert resolution.subject == "feat-a"
    assert resolution.attribution == RESOLUTION_ATTRIBUTION_PROBE


def test_landing_strand_and_plain_strand_diverge_by_attribution_in_one_sweep(
    tmp_path: Path,
) -> None:
    """Differential: in a single armed sweep that archives a fully-landed phase,
    a landing strand is resolved with the probe attribution while a non-landing
    strand keeps the plain, unattributed archival-supersession marker.

    Archival is never an attributor: only the git-truth-gated landing strand
    carries a token. A probe that stamped every strand identically, or archival
    that attributed itself, could not produce both an empty and a ``probe``
    attribution from the same sweep.
    """
    repo = tmp_path / "repo"
    _git_init_repo(repo)
    tasks_dir = _tasks_dir(repo)
    phase_dir = tasks_dir / "active" / "01-landed"

    # A hand-landed landing strand (ancestor branch, parked standing-verify).
    _branch_on_main(repo, "flywheel/01-landed/feat-landing", "landing.txt")
    _write_task(phase_dir, "feat-landing")
    # A non-landing strand: a dangling prerequisite that later resolved. Its
    # task landed cleanly (a receipt), so the phase is eligible to archive.
    _write_task(phase_dir, "feat-plain")

    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    claims = SqliteClaimStore(db)
    try:
        _seed_done(store, "feat-landing")
        lc_plain = _seed_done(store, "feat-plain")
        _record_landed(store, lc_plain, landed_ref=_git_head(repo))
        _route_landing_strand(
            claims,
            "feat-landing",
            PARK_KIND_STANDING_VERIFY,
            run_id="run-feat-landing",
        )
        claims.record_stop_event(
            kind=STOP_DANGLING_PREREQUISITE,
            subject="feat-plain",
            detail="prerequisite 'dep' resolved to no work item",
            occurred_at=datetime.now(timezone.utc),
        )
        moved = archive_completed_phases(
            tasks_dir,
            store,
            repo_root=repo,
            landing_base="main",
            claims=claims,
        )
        landing_rows = claims.list_subject_stop_events("feat-landing")
        plain_rows = claims.list_subject_stop_events("feat-plain")
    finally:
        claims.close()
        store.close()

    assert [p.name for p in moved] == [phase_dir.name]

    # Landing strand: probe-attributed resolution.
    assert landing_rows[-1].kind == STOP_RESOLVED
    assert landing_rows[-1].attribution == RESOLUTION_ATTRIBUTION_PROBE

    # Non-landing strand: plain archival supersession, NO attribution --
    # archival is never an attributor.
    assert plain_rows[-1].kind == STOP_RESOLVED
    assert plain_rows[-1].attribution == ""


def test_unarmed_sweep_never_machine_resolves_a_landing_strand(
    tmp_path: Path,
) -> None:
    """The sweep never stamps a resolution on an unlanded strand: an unarmed
    sweep (no probe) archives under the legacy DONE-only contract yet leaves the
    landing strand surfaced, because archival alone can never confirm a landing.

    Without ``landing_base`` the landability probe is disarmed, so the sweep
    proves nothing about the strand's reachability -- and must not clear it.
    """
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
        _route_landing_strand(
            claims, "feat-a", PARK_KIND_DIVERGENT_BASE, run_id="run-feat-a"
        )
        # Armed with repo_root but no landing_base -> probe disarmed, legacy
        # DONE-only archival.
        moved = archive_completed_phases(
            tasks_dir,
            store,
            repo_root=repo,
            claims=claims,
        )
        rows = claims.list_subject_stop_events("feat-a")
    finally:
        claims.close()
        store.close()

    # The phase archived (legacy contract) but the strand was NOT resolved:
    # archival is never an attributor and never a landing witness.
    assert [p.name for p in moved] == [phase_dir.name]
    assert (tasks_dir / "archive" / phase_dir.name).is_dir()
    assert [r.kind for r in rows] == [PARK_KIND_DIVERGENT_BASE]
    assert not any(r.kind == STOP_RESOLVED for r in rows)


def test_probe_resolution_is_distinguishable_from_an_operator_resolution(
    tmp_path: Path,
) -> None:
    """A machine (probe) resolution never masquerades as an operator one: the
    attribution token on the record tells them apart.

    A probe-confirmed landing carries :data:`RESOLUTION_ATTRIBUTION_PROBE`
    (never the operator token), while an operator-abandoned strand carries
    :data:`RESOLUTION_ATTRIBUTION_OPERATOR`. Both are stable tokens read from
    the record's ``attribution`` column, so a reader distinguishes WHO cleared a
    strand without parsing free text.
    """
    repo = tmp_path / "repo"
    _git_init_repo(repo)
    tasks_dir = _tasks_dir(repo)
    phase_dir = tasks_dir / "active" / "01-landed"
    _branch_on_main(repo, "flywheel/01-landed/feat-a", "a.txt")
    _write_task(phase_dir, "feat-a")

    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    claims = SqliteClaimStore(db)
    try:
        _seed_done(store, "feat-a")
        _route_landing_strand(
            claims, "feat-a", PARK_KIND_DIVERGENT_BASE, run_id="run-feat-a"
        )
        archive_completed_phases(
            tasks_dir,
            store,
            repo_root=repo,
            landing_base="main",
            claims=claims,
        )
        probe_resolution = claims.list_subject_stop_events("feat-a")[-1]

        # An operator resolution on a different subject records the operator
        # token; the two attributions are drawn from the same vocabulary yet are
        # distinct, so neither can be mistaken for the other.
        claims.record_stop_event(
            kind=STOP_RESOLVED,
            subject="feat-b",
            detail="operator abandoned the strand: superseded by a rewrite",
            occurred_at=datetime.now(timezone.utc),
            attribution=RESOLUTION_ATTRIBUTION_OPERATOR,
        )
        operator_resolution = claims.list_subject_stop_events("feat-b")[-1]
    finally:
        claims.close()
        store.close()

    assert probe_resolution.attribution == RESOLUTION_ATTRIBUTION_PROBE
    assert probe_resolution.attribution != RESOLUTION_ATTRIBUTION_OPERATOR
    assert operator_resolution.attribution == RESOLUTION_ATTRIBUTION_OPERATOR
    assert probe_resolution.attribution != operator_resolution.attribution

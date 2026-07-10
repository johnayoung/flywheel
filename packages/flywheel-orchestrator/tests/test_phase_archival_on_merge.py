"""Phase-branch merge predicate for the archive sweep (spec 00079, D-2/D-3).

Under the ``phase`` submit strategy each DONE task lands on the per-phase
integration branch ``flywheel/phase/<phase>``; the completed phase archives
only once that branch tip is an *ancestor* of the true base -- the phase PR
merged as a merge commit, the single shape that preserves ``git revert -m 1``
of the whole phase (D-3: merged-ness is git ancestry, never a remote PR state
string). Two non-merged states each stay active under a DISTINCT, visible
reason rather than a silent forever-block:

* an open PR (branch exists, tip not an ancestor, content the true base still
  lacks) waits for the merge and never advances the true base locally;
* a squash/rebase merge (content already applied, ancestry broken) surfaces a
  merge-method-mismatch so a squash-merged phase is never mistaken for the
  generic open-PR reason.

These build real git repos so the ancestry/tree probes have commits to walk,
and gate on the existence of ``flywheel/phase/<phase>`` -- never created under
the merge/pr strategies, so those repos stay byte-identical (criterion 9). The
per-task spec-00077 landed predicate keeps precedence: a DONE-but-not-landed
strand blocks before this gate ever runs.
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
from flywheel_orchestrator._claims import SqliteClaimStore
from flywheel_orchestrator._workflow import (
    LOOP_BASE_FILENAME,
    archive_completed_phases,
    write_phase_base_if_missing,
)
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
    _git(repo, "config", "user.email", "phase-merge-test@example.com")
    _git(repo, "config", "user.name", "phase merge test")
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


def _phase_branch(phase: str) -> str:
    return f"flywheel/phase/{phase}"


def _build_phase_branch(
    repo: Path, phase: str, filename: str, body: str
) -> str:
    """Create ``flywheel/phase/<phase>`` off main with one commit; return tip.

    Leaves ``main`` checked out so the caller can decide how (or whether) the
    branch merges back -- the three merge shapes (merge commit, open PR,
    squash) differ only in what main does next.
    """
    branch = _phase_branch(phase)
    base = _git_head(repo)
    _git(repo, "checkout", "-b", branch, base)
    (repo / filename).write_text(body)
    _git(repo, "add", "-A")
    _git(repo, "commit", "-m", f"feat: {phase} work")
    tip = _git_head(repo)
    _git(repo, "checkout", "main")
    return tip


def _divergent_task_branch(repo: Path, branch: str, filename: str) -> str:
    """Point ``branch`` at a side commit main never merges; return its head.

    The spec-00077 not-landed (divergent-base) case: the branch head is not an
    ancestor of main. Restores main as checked out.
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
    """Append a ``Landed`` receipt on ``lc``'s run (the spec-00077 fast path).

    A receipt makes the per-task landed predicate pass without a resolvable
    branch, so a test that arms both predicates (as the CLI does, resolving
    landing_base and true_base to HEAD) reaches the phase-merge gate.
    """
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


def test_unmerged_phase_branch_keeps_phase_active_and_names_open_pr(
    tmp_path: Path,
) -> None:
    """Criterion 4: an all-DONE phase whose integration branch is unmerged (an
    open PR -- tip not an ancestor, content the true base lacks) stays in
    ``active/``, surfaces the open PR as the blocking reason, and never advances
    the true base (the worker performs no local merge)."""
    repo = tmp_path / "repo"
    _git_init_repo(repo)
    tasks_dir = _tasks_dir(repo)
    phase = "20-phase-branch-landing"
    phase_dir = tasks_dir / "active" / phase

    _build_phase_branch(repo, phase, "feat.txt", "phase content\n")
    _write_task(phase_dir, "feat-a")

    base_before = _git_head(repo)
    logged: list[str] = []
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_done(store, "feat-a")
        moved = archive_completed_phases(
            tasks_dir,
            store,
            repo_root=repo,
            log=logged.append,
            true_base="main",
        )
    finally:
        store.close()

    assert moved == []
    assert phase_dir.is_dir(), "unmerged phase must remain in active/"
    assert not (tasks_dir / "archive" / phase).exists()
    assert _git_head(repo) == base_before, (
        "the phase strategy must never advance the true base by archiving"
    )
    assert any(
        "Refusing to archive" in line
        and "the phase PR is not merged" in line
        and _phase_branch(phase) in line
        and "still open" in line
        for line in logged
    ), f"expected an open-PR archival refusal, got {logged!r}"
    assert not any("merge-method mismatch" in line for line in logged), (
        f"an open PR must not read as a squash mismatch, got {logged!r}"
    )


def test_merged_phase_archives_with_loop_base_and_revertable_merge(
    tmp_path: Path,
) -> None:
    """Criterion 5: when the phase integration branch merged as a merge commit
    (its tip is an ancestor of the true base) the phase archives, the
    ``.loop-base`` dotfile materializes into the archived dir, and the merge is
    revertable as a unit (``git revert -m 1`` applies cleanly)."""
    repo = tmp_path / "repo"
    _git_init_repo(repo)
    tasks_dir = _tasks_dir(repo)
    phase = "20-phase-branch-landing"
    phase_dir = tasks_dir / "active" / phase

    _build_phase_branch(repo, phase, "feat.txt", "phase content\n")
    _write_task(phase_dir, "feat-a")
    # A real merge commit: tip stays reachable from main -> MERGED.
    _git(repo, "merge", "--no-ff", "-m", f"Merge {phase}", _phase_branch(phase))
    merge_commit = _git_head(repo)
    # A recorded loop-base ref must survive the new gate and materialize.
    assert write_phase_base_if_missing(repo, phase_dir)

    logged: list[str] = []
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_done(store, "feat-a")
        moved = archive_completed_phases(
            tasks_dir,
            store,
            repo_root=repo,
            log=logged.append,
            true_base="main",
        )
    finally:
        store.close()

    assert [p.name for p in moved] == [phase]
    assert not phase_dir.exists()
    archived = tasks_dir / "archive" / phase
    assert archived.is_dir()
    assert (archived / LOOP_BASE_FILENAME).is_file(), (
        "archival must still materialize the loop-base dotfile"
    )
    assert logged == [], f"a merged phase must archive cleanly, got {logged!r}"

    # The whole phase reverts as one unit -- the merge-commit shape is what
    # gives the phase PR its ``git revert -m 1`` review contract (D-3).
    revert = subprocess.run(
        ["git", "-C", str(repo), "revert", "--no-edit", "-m", "1", merge_commit],
        capture_output=True,
        text=True,
    )
    assert revert.returncode == 0, (
        f"the merged phase must revert cleanly as a unit: {revert.stderr}"
    )
    assert not (repo / "feat.txt").exists(), (
        "reverting the phase merge must undo the whole phase's content"
    )


def test_squash_merge_parks_with_distinct_merge_method_mismatch_reason(
    tmp_path: Path,
) -> None:
    """Criterion 6: a squash/rebase merge (the phase's content is applied to the
    true base but the branch commit is discarded, so ancestry is broken) leaves
    the phase active under a DISTINCT merge-method-mismatch reason -- never the
    generic open-PR reason, never a silent forever-block."""
    repo = tmp_path / "repo"
    _git_init_repo(repo)
    tasks_dir = _tasks_dir(repo)
    phase = "20-phase-branch-landing"
    phase_dir = tasks_dir / "active" / phase

    _build_phase_branch(repo, phase, "feat.txt", "phase content\n")
    # Squash-equivalent: apply the branch's identical content to main as a
    # fresh commit. Trees match (git diff --quiet exits 0) yet the branch tip
    # is not an ancestor -> MERGE_METHOD_MISMATCH. Committed BEFORE the task
    # JSON is written so ``git add -A`` cannot sweep an untracked task file
    # into main's tree and break the tree-equality the mismatch turns on.
    _git_commit_file(repo, "feat.txt", "phase content\n", "feat: squash phase")
    _write_task(phase_dir, "feat-a")

    base_before = _git_head(repo)
    logged: list[str] = []
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_done(store, "feat-a")
        moved = archive_completed_phases(
            tasks_dir,
            store,
            repo_root=repo,
            log=logged.append,
            true_base="main",
        )
    finally:
        store.close()

    assert moved == []
    assert phase_dir.is_dir()
    assert not (tasks_dir / "archive" / phase).exists()
    assert _git_head(repo) == base_before
    assert any(
        "Refusing to archive" in line
        and "merge-method mismatch" in line
        and "squash or rebase" in line
        and _phase_branch(phase) in line
        for line in logged
    ), f"expected a distinct merge-method-mismatch refusal, got {logged!r}"
    assert not any("still open" in line for line in logged), (
        f"a squash merge must not read as an open PR, got {logged!r}"
    )


def test_phase_merge_verdict_keys_on_real_ancestry(tmp_path: Path) -> None:
    """Differential anti-cheat: identical all-DONE phase + integration branch,
    flipped only by whether main merged the branch as a merge commit. The merged
    case archives; the open-PR case stays active. A gate that ignored ancestry
    could not produce both outcomes from the same task/branch setup."""
    phase = "20-phase-branch-landing"

    # Merged (merge commit) -> archives.
    repo_ok = tmp_path / "ok"
    _git_init_repo(repo_ok)
    tasks_ok = _tasks_dir(repo_ok)
    _build_phase_branch(repo_ok, phase, "feat.txt", "phase content\n")
    _write_task(tasks_ok / "active" / phase, "feat-a")
    _git(
        repo_ok, "merge", "--no-ff", "-m", f"Merge {phase}", _phase_branch(phase)
    )

    # Open PR (branch never merged) -> stays active.
    repo_bad = tmp_path / "bad"
    _git_init_repo(repo_bad)
    tasks_bad = _tasks_dir(repo_bad)
    _build_phase_branch(repo_bad, phase, "feat.txt", "phase content\n")
    _write_task(tasks_bad / "active" / phase, "feat-a")

    store_ok = SqliteStore(repo_ok / "db.sqlite")
    store_bad = SqliteStore(repo_bad / "db.sqlite")
    try:
        _seed_done(store_ok, "feat-a")
        _seed_done(store_bad, "feat-a")
        moved_ok = archive_completed_phases(
            tasks_ok, store_ok, repo_root=repo_ok, true_base="main"
        )
        moved_bad = archive_completed_phases(
            tasks_bad, store_bad, repo_root=repo_bad, true_base="main"
        )
    finally:
        store_ok.close()
        store_bad.close()

    assert [p.name for p in moved_ok] == [phase]
    assert moved_bad == []
    assert (tasks_bad / "active" / phase).is_dir()


def test_no_phase_branch_leaves_archival_byte_identical(
    tmp_path: Path,
) -> None:
    """Criterion 9: a repo with no ``flywheel/phase/<phase>`` branch (the merge
    and pr strategies never create one) archives an all-DONE phase exactly as
    before even when ``true_base`` is threaded -- the gate simply never arms."""
    repo = tmp_path / "repo"
    _git_init_repo(repo)
    tasks_dir = _tasks_dir(repo)
    phase = "20-merge-strategy"
    phase_dir = tasks_dir / "active" / phase
    _write_task(phase_dir, "feat-a")

    logged: list[str] = []
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_done(store, "feat-a")
        moved = archive_completed_phases(
            tasks_dir,
            store,
            repo_root=repo,
            log=logged.append,
            true_base="main",
        )
    finally:
        store.close()

    assert [p.name for p in moved] == [phase]
    assert (tasks_dir / "archive" / phase).is_dir()
    assert not any("the phase PR is not merged" in line for line in logged), (
        f"the merge gate must not arm without a phase branch, got {logged!r}"
    )


def test_per_task_strand_precedes_phase_merge_gate(tmp_path: Path) -> None:
    """Spec 00077 precedence: a DONE-but-not-landed task blocks archival on the
    per-task predicate BEFORE the phase-merge gate runs, even when the phase
    branch itself has merged. The strand names the task ("not landed"); the
    phase-PR reason never fires because the earlier gate already refused."""
    repo = tmp_path / "repo"
    _git_init_repo(repo)
    tasks_dir = _tasks_dir(repo)
    phase = "20-phase-branch-landing"
    phase_dir = tasks_dir / "active" / phase

    # The phase integration branch IS merged (would pass the merge gate)...
    _build_phase_branch(repo, phase, "feat.txt", "phase content\n")
    _git(repo, "merge", "--no-ff", "-m", f"Merge {phase}", _phase_branch(phase))
    # ...but the task's own work is a divergent, unlanded strand.
    _divergent_task_branch(repo, f"flywheel/{phase}/feat-a", "a.txt")
    _write_task(phase_dir, "feat-a")

    logged: list[str] = []
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    claims = SqliteClaimStore(db)
    try:
        _seed_done(store, "feat-a")
        # Arm BOTH predicates: landing_base -> spec-00077, true_base -> 00079.
        moved = archive_completed_phases(
            tasks_dir,
            store,
            repo_root=repo,
            log=logged.append,
            landing_base="main",
            true_base="main",
            claims=claims,
        )
    finally:
        claims.close()
        store.close()

    assert moved == []
    assert phase_dir.is_dir()
    assert any(
        "not landed" in line and "feat-a" in line for line in logged
    ), f"the per-task strand must block and name the task, got {logged!r}"
    assert not any("the phase PR is not merged" in line for line in logged), (
        f"the per-task predicate must take precedence, got {logged!r}"
    )


def test_repeated_sweeps_over_unmerged_phase_are_idempotent(
    tmp_path: Path,
) -> None:
    """Edge: an unmerged phase surviving repeated sweeps stays active every
    time -- no archival, no true-base advance, a refusal each pass. The gate is
    a stable park, never a one-shot that leaks the phase on a later sweep."""
    repo = tmp_path / "repo"
    _git_init_repo(repo)
    tasks_dir = _tasks_dir(repo)
    phase = "20-phase-branch-landing"
    phase_dir = tasks_dir / "active" / phase
    _build_phase_branch(repo, phase, "feat.txt", "phase content\n")
    _write_task(phase_dir, "feat-a")

    base_before = _git_head(repo)
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_done(store, "feat-a")
        for _ in range(3):
            logged: list[str] = []
            moved = archive_completed_phases(
                tasks_dir,
                store,
                repo_root=repo,
                log=logged.append,
                true_base="main",
            )
            assert moved == []
            assert phase_dir.is_dir()
            assert _git_head(repo) == base_before
            assert any(
                "the phase PR is not merged" in line for line in logged
            ), f"each sweep must re-surface the refusal, got {logged!r}"
    finally:
        store.close()


def test_cli_archive_honors_phase_merge_gate(
    tmp_path: Path,
    capsys: pytest.CaptureFixture[str],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Criterion 8: the ``flywheel archive`` CLI sweep honors the same gate --
    it is not a gateless one-command bypass. An unmerged phase stays active and
    the CLI prints the open-PR refusal to stderr (true base is the operator's
    checked-out branch, HEAD)."""
    repo = tmp_path / "repo"
    _git_init_repo(repo)
    tasks_dir = _tasks_dir(repo)
    phase = "20-phase-branch-landing"
    phase_dir = tasks_dir / "active" / phase
    _build_phase_branch(repo, phase, "feat.txt", "phase content\n")
    _write_task(phase_dir, "feat-a")

    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        lc = _seed_done(store, "feat-a")
        # The CLI arms the spec-00077 predicate too (landing_base resolves to
        # HEAD); a landed receipt clears it so the phase-merge gate is what
        # refuses -- isolating criterion 8 from the per-task predicate.
        _record_landed(store, lc, landed_ref=_git_head(repo))
    finally:
        store.close()

    base_before = _git_head(repo)
    # cwd = the tmp repo (no flywheel.toml) so no ambient policy is detected;
    # the CLI then resolves true_base to HEAD, the checked-out branch.
    monkeypatch.chdir(repo)
    rc = orch_main(
        ["archive", "--tasks-dir", str(tasks_dir), "--db", str(db)]
    )

    assert rc == 0
    captured = capsys.readouterr()
    assert phase_dir.is_dir(), "the CLI sweep must honor the phase-merge gate"
    assert not (tasks_dir / "archive" / phase).exists()
    assert _git_head(repo) == base_before
    assert str(tasks_dir / "archive" / phase) not in captured.out, (
        "an unmerged phase must not be reported as moved"
    )
    assert "the phase PR is not merged" in captured.err, (
        f"the CLI must surface the archival refusal, got {captured.err!r}"
    )

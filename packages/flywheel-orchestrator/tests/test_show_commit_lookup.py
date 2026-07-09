"""``fw show <commit-sha>`` resolves a landed commit to its run (spec 00078).

Criteria 5 and 6 of the commit-provenance feature: given a landed commit's SHA,
``show`` reads the harness-stamped ``Flywheel-Run`` trailer straight off the
commit object (git-truth, D-3) and renders the *same* run detail it renders for
that run's id; given a commit with no such trailer (a human commit) it exits
non-zero naming the commit un-attributed and prints no run detail — never a
fuzzy fallback to a nearby run.

The tests build a real git repo, stamp a commit with the production engine
(``flywheel_worktree._trailers.stamp_commit_messages``) so the lookup is proven
against exactly what lands, seed the producing run in a file-backed store, and
drive the real ``show`` CLI end to end.
"""

from __future__ import annotations

import os
import subprocess
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest

from flywheel_core.events import Landed
from flywheel_core.lifecycle import Lifecycle, Status
from flywheel_core.store_sqlite import SqliteStore
from flywheel_orchestrator._workflow import TRAILER_KEY_RUN
from flywheel_orchestrator._workflow import main as orch_main
from flywheel_worktree._trailers import (
    TRAILER_KEY_RUN as WORKTREE_TRAILER_KEY_RUN,
    provenance_trailers,
    stamp_commit_messages,
)

_T0 = datetime(2026, 6, 1, 12, 0, 0, tzinfo=timezone.utc)


# --- git + store fixtures ----------------------------------------------------


def _git(repo: Path, *args: str) -> str:
    """Run ``git -C <repo> <args>`` with a deterministic identity, return stdout."""
    env = {
        **os.environ,
        "GIT_AUTHOR_NAME": "Flywheel Test",
        "GIT_AUTHOR_EMAIL": "test@flywheel.invalid",
        "GIT_COMMITTER_NAME": "Flywheel Test",
        "GIT_COMMITTER_EMAIL": "test@flywheel.invalid",
    }
    return subprocess.run(
        ["git", "-C", str(repo), *args],
        text=True,
        capture_output=True,
        check=True,
        env=env,
    ).stdout.strip()


def _init_repo(repo: Path) -> str:
    """Init a repo with one seed commit (a trailer-less "human" commit)."""
    repo.mkdir(parents=True, exist_ok=True)
    _git(repo, "init", "-q")
    _git(repo, "config", "user.name", "Flywheel Test")
    _git(repo, "config", "user.email", "test@flywheel.invalid")
    _git(repo, "config", "commit.gpgsign", "false")
    (repo / "seed.txt").write_text("seed\n")
    _git(repo, "add", "seed.txt")
    _git(repo, "commit", "-q", "-m", "seed the repo")
    return _git(repo, "rev-parse", "HEAD")


def _work_commit(repo: Path, name: str) -> str:
    """A commit with no provenance trailers, as an untrusted agent authors it."""
    (repo / name).write_text(f"content of {name}\n")
    _git(repo, "add", name)
    _git(repo, "commit", "-q", "-m", f"implement {name}")
    return _git(repo, "rev-parse", "HEAD")


def _stamp(repo: Path, *, base: str, branch: str, run_id: str) -> str:
    """Stamp ``base..branch`` with the real engine; FF a ref onto the tip.

    Returns the stamped tip SHA — the landed commit the lookup must resolve.
    """
    tip = stamp_commit_messages(
        repo,
        base=base,
        branch=branch,
        trailers=provenance_trailers(
            task_id="t-land", run_id=run_id, phase="00078-provenance"
        ),
    )
    _git(repo, "update-ref", "refs/heads/landed", tip)
    return tip


def _seed_landed_run(
    store: SqliteStore, *, run_id: str, landed_ref: str = "cafef00d"
) -> None:
    """Persist a DONE run whose ledger carries a positive landing decision."""
    lc = Lifecycle(task_id="t-land", run_id=run_id)
    lc.transition_to(Status.READY, now=_T0)
    lc.transition_to(Status.RUNNING, now=_T0 + timedelta(seconds=1))
    lc.transition_to(Status.VALIDATING, now=_T0 + timedelta(seconds=2))
    lc.transition_to(Status.DONE, now=_T0 + timedelta(minutes=5))
    store.create_lifecycle(lc)
    store.append_domain_event(
        Landed(
            run_id=run_id,
            ts=_T0 + timedelta(minutes=6),
            strategy="merge",
            landed_ref=landed_ref,
        ),
        expected_version=lc.version,
    )


def _show(
    repo: Path,
    db: Path,
    arg: str,
    capsys: "pytest.CaptureFixture[str]",
) -> tuple[int, str]:
    """Drive ``show`` with a tasks dir under the repo's ``.flywheel`` tree."""
    rc = orch_main(
        [
            "show",
            arg,
            "--db",
            str(db),
            "--tasks-dir",
            str(repo / ".flywheel" / "tasks"),
        ]
    )
    return rc, capsys.readouterr().out


# --- criterion 5: a landed SHA renders that run's detail ---------------------


def test_show_by_commit_sha_renders_producing_run_detail(
    tmp_path: Path, capsys: "pytest.CaptureFixture[str]"
) -> None:
    repo = tmp_path / "repo"
    base = _init_repo(repo)
    branch = _work_commit(repo, "feature.py")
    sha = _stamp(repo, base=base, branch=branch, run_id="run-1")

    db = repo / ".flywheel" / "flywheel.sqlite"
    db.parent.mkdir(parents=True, exist_ok=True)
    store = SqliteStore(db)
    _seed_landed_run(store, run_id="run-1", landed_ref="cafef00d")
    store.close()

    rc, out = _show(repo, db, sha, capsys)
    assert rc == 0
    # The run id and its landing decision line are on the SHA-rendered view.
    assert "run-1" in out
    assert "landed" in out
    assert "cafef00d" in out


def test_show_by_commit_sha_is_identical_to_show_by_run_id(
    tmp_path: Path, capsys: "pytest.CaptureFixture[str]"
) -> None:
    """The SHA view is the same run-detail render as the run-id view (D-3)."""
    repo = tmp_path / "repo"
    base = _init_repo(repo)
    branch = _work_commit(repo, "feature.py")
    sha = _stamp(repo, base=base, branch=branch, run_id="run-1")

    db = repo / ".flywheel" / "flywheel.sqlite"
    db.parent.mkdir(parents=True, exist_ok=True)
    store = SqliteStore(db)
    _seed_landed_run(store, run_id="run-1")
    store.close()

    rc_sha, out_sha = _show(repo, db, sha, capsys)
    rc_run, out_run = _show(repo, db, "run-1", capsys)
    assert rc_sha == 0
    assert rc_run == 0
    assert out_sha == out_run


# --- criterion 6: an un-attributed commit is a hard, named error -------------


def test_show_by_unattributed_commit_exits_nonzero_naming_the_commit(
    tmp_path: Path, capsys: "pytest.CaptureFixture[str]"
) -> None:
    repo = tmp_path / "repo"
    human_sha = _init_repo(repo)  # a plain commit, no Flywheel-Run trailer

    db = repo / ".flywheel" / "flywheel.sqlite"
    db.parent.mkdir(parents=True, exist_ok=True)
    store = SqliteStore(db)
    _seed_landed_run(store, run_id="run-1")  # a run exists but is unrelated
    store.close()

    rc, out = _show(repo, db, human_sha, capsys)
    assert rc != 0
    assert "un-attributed" in out
    assert human_sha in out
    # No run detail is rendered — never a fuzzy fallback to the seeded run.
    assert "run-1" not in out
    assert "task     :" not in out
    assert "run      :" not in out
    assert "decisions:" not in out


# --- edge: trailer names a run absent from the store -------------------------


def test_show_commit_naming_missing_run_names_the_missing_run(
    tmp_path: Path, capsys: "pytest.CaptureFixture[str]"
) -> None:
    """A dangling provenance pointer is distinct from an un-attributed commit."""
    repo = tmp_path / "repo"
    base = _init_repo(repo)
    branch = _work_commit(repo, "orphan.py")
    sha = _stamp(repo, base=base, branch=branch, run_id="run-ghost")

    db = repo / ".flywheel" / "flywheel.sqlite"
    db.parent.mkdir(parents=True, exist_ok=True)
    store = SqliteStore(db)
    _seed_landed_run(store, run_id="run-1")  # run-ghost is NOT seeded
    store.close()

    rc, out = _show(repo, db, sha, capsys)
    assert rc != 0
    # Names the missing run id, and is not the un-attributed message.
    assert "run-ghost" in out
    assert "un-attributed" not in out
    # No run detail rendered for the absent run.
    assert "task     :" not in out


# --- edge: neither run id, task id, nor a commit keeps today's error ---------


def test_show_unknown_argument_keeps_not_found_behavior(
    tmp_path: Path, capsys: "pytest.CaptureFixture[str]"
) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)

    db = repo / ".flywheel" / "flywheel.sqlite"
    db.parent.mkdir(parents=True, exist_ok=True)
    store = SqliteStore(db)
    _seed_landed_run(store, run_id="run-1")
    store.close()

    rc, out = _show(repo, db, "not-a-run-task-or-commit", capsys)
    assert rc != 0
    assert "no run or task with that id" in out
    assert "un-attributed" not in out


# --- shared vocabulary: the lookup key is pinned to the engine's -------------


def test_run_trailer_key_matches_worktree_engine() -> None:
    """The lookup's trailer key is the exact one the stamping engine writes."""
    assert TRAILER_KEY_RUN == WORKTREE_TRAILER_KEY_RUN

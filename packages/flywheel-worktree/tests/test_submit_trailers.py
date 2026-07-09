"""Behavior: the merge submit strategy stamps harness-authoritative provenance
trailers onto every commit it lands, message-only, on both the clean
fast-forward and the base-advanced rebase paths (spec 00078, criteria 1/3/4).

Every scenario runs real git against a tmp repo through the real
``GitWorktreeSubmitter._submit`` landing path. The scripted "agent" commits are
authored WITH NO trailers (criterion 1) or with deliberately forged
``Flywheel-*`` trailers (criterion 3), so only mechanical worker-side stamping
-- never agent compliance -- can make the assertions hold. Criterion 4 records
the ordered tree list of ``base..branch`` before the land and proves the landed
range's trees are byte-identical, so an implementation that re-committed the
worktree (new trees) rather than rewriting messages would fail.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

from flywheel_core import CommandGrader, Status, Task
from flywheel_orchestrator import SandboxRequest, SubmitRequest

from flywheel_worktree import worker

_TASK_ID = "t1"
_RUN_ID = "run-1"
_PHASE = "01-phase"
_KEYS = ("Flywheel-Task", "Flywheel-Run", "Flywheel-Phase")
_EXPECTED = {
    "Flywheel-Task": _TASK_ID,
    "Flywheel-Run": _RUN_ID,
    "Flywheel-Phase": _PHASE,
}


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
    _git(path, "config", "user.email", "trailer-test@example.com")
    _git(path, "config", "user.name", "trailer test")
    _git(path, "config", "commit.gpgsign", "false")
    (path / "README.md").write_text("base\n")
    _git(path, "add", "-A")
    _git(path, "commit", "-m", "init")


def _rev(repo: Path, ref: str) -> str:
    return _git(repo, "rev-parse", ref)


def _commit(worktree: Path, filename: str, body: str, *message: str) -> None:
    """Commit ``filename`` with a multi-part message: each ``message`` element
    is a ``-m`` paragraph, so a trailer block can be forged as its own part."""
    (worktree / filename).write_text(body)
    _git(worktree, "add", "-A")
    args = ["commit"]
    for part in message:
        args += ["-m", part]
    _git(worktree, *args)


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


def _sandbox_req(tf: Path, task_id: str) -> SandboxRequest:
    return SandboxRequest(
        task_id=task_id, task_file=tf, run_id=None, mode="fresh"
    )


def _submit_req(tf: Path, task_id: str, sandbox: Path) -> SubmitRequest:
    return SubmitRequest(
        task_id=task_id,
        task_file=tf,
        task=Task(
            id=task_id,
            goal=f"Goal for {task_id}.",
            graders=[CommandGrader(run="true")],
        ),
        run_id=_RUN_ID,
        status=Status.DONE,
        sandbox=sandbox,
    )


def _merge_submitter(repo: Path) -> worker.GitWorktreeSubmitter:
    worktrees = repo / ".flywheel" / "worktrees"
    worktrees.mkdir(parents=True, exist_ok=True)
    return worker.GitWorktreeSubmitter(
        repo_root=repo,
        tasks_dir=repo / ".flywheel" / "tasks",
        worktrees_dir=worktrees,
        phase_base="main",
        lock_path=repo / ".flywheel" / ".merge.lock",
        log=lambda _m: None,
    )


# --- trailer / range inspection ----------------------------------------------


def _range_shas(repo: Path, rng: str) -> list[str]:
    """Commit shas in ``rng`` (``<before>..<after>``), ancestor-first."""
    out = _git(repo, "rev-list", "--reverse", rng)
    return out.split() if out else []


def _trailer_value(repo: Path, sha: str, key: str) -> str:
    """The ``valueonly`` render of ``key``'s trailer on ``sha`` -- empty when
    absent, the single value when present once."""
    return _git(
        repo, "show", "-s", f"--format=%(trailers:key={key},valueonly)", sha
    ).strip()


def _trailer_count(repo: Path, sha: str, key: str) -> int:
    """How many times ``key`` appears as a trailer line in ``sha``'s message."""
    body = _git(repo, "show", "-s", "--format=%B", sha)
    prefix = f"{key.lower()}:"
    return sum(
        1 for line in body.splitlines() if line.strip().lower().startswith(prefix)
    )


def _assert_stamped(repo: Path, sha: str) -> None:
    """Every provenance key is present exactly once with the harness value."""
    for key in _KEYS:
        assert _trailer_value(repo, sha, key) == _EXPECTED[key]
        assert _trailer_count(repo, sha, key) == 1


# --- criterion 1: stamped on every landed commit -----------------------------


def test_clean_ff_stamps_all_three_keys_on_every_commit(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _merge_submitter(repo)
    tf = _task_file(repo, _PHASE, _TASK_ID)

    wt = s.prepare_sandbox(_sandbox_req(tf, _TASK_ID))
    # A scripted agent that commits with NO trailers at all: only mechanical
    # stamping can put the three keys on both commits.
    _commit(wt, "a.txt", "1", "feat: a")
    _commit(wt, "b.txt", "2", "feat: b")
    before = _rev(repo, "main")

    s.submit(_submit_req(tf, _TASK_ID, wt))

    after = _rev(repo, "main")
    shas = _range_shas(repo, f"{before}..{after}")
    assert len(shas) == 2
    for sha in shas:
        _assert_stamped(repo, sha)
    # The land actually completed (base advanced, worktree torn down).
    assert after != before
    assert not wt.exists()


def test_clean_ff_preserves_the_original_subject(tmp_path: Path) -> None:
    # Message-only: stamping appends trailers without disturbing the subject.
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _merge_submitter(repo)
    tf = _task_file(repo, _PHASE, _TASK_ID)

    wt = s.prepare_sandbox(_sandbox_req(tf, _TASK_ID))
    _commit(wt, "a.txt", "1", "feat: keep this subject")
    before = _rev(repo, "main")

    s.submit(_submit_req(tf, _TASK_ID, wt))

    sha = _range_shas(repo, f"{before}..{_rev(repo, 'main')}")[0]
    assert _git(repo, "show", "-s", "--format=%s", sha) == "feat: keep this subject"
    _assert_stamped(repo, sha)


# --- criterion 4: message-only, trees byte-identical in order -----------------


def test_stamping_preserves_every_tree_in_order(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _merge_submitter(repo)
    tf = _task_file(repo, _PHASE, _TASK_ID)

    wt = s.prepare_sandbox(_sandbox_req(tf, _TASK_ID))
    _commit(wt, "a.txt", "1", "feat: a")
    _commit(wt, "b.txt", "2", "feat: b")
    _commit(wt, "c.txt", "3", "feat: c")

    # Record the ordered tree list of base..branch BEFORE the land.
    pre_shas = _range_shas(wt, "main..HEAD")
    pre_trees = [_git(wt, "rev-parse", f"{c}^{{tree}}") for c in pre_shas]

    before = _rev(repo, "main")
    s.submit(_submit_req(tf, _TASK_ID, wt))
    after = _rev(repo, "main")

    post_shas = _range_shas(repo, f"{before}..{after}")
    post_trees = [_git(repo, "rev-parse", f"{c}^{{tree}}") for c in post_shas]
    # Same number of commits, same trees, in the same order -- an implementation
    # that re-committed the worktree would produce different trees and fail here.
    assert post_trees == pre_trees
    assert len(post_trees) == 3


# --- criterion 3: forged agent trailers are replaced -------------------------


def test_forged_trailers_are_stripped_and_replaced(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _merge_submitter(repo)
    tf = _task_file(repo, _PHASE, _TASK_ID)

    wt = s.prepare_sandbox(_sandbox_req(tf, _TASK_ID))
    # The agent forges provenance: a run/task/phase it did not own.
    _commit(
        wt,
        "a.txt",
        "1",
        "feat: launder provenance",
        "Flywheel-Run: forged-123\n"
        "Flywheel-Task: forged-task\n"
        "Flywheel-Phase: forged-phase",
    )
    before = _rev(repo, "main")

    s.submit(_submit_req(tf, _TASK_ID, wt))

    after = _rev(repo, "main")
    shas = _range_shas(repo, f"{before}..{after}")
    assert len(shas) == 1
    _assert_stamped(repo, shas[0])
    # The forged values appear nowhere in the landed range's messages.
    for forged in ("forged-123", "forged-task", "forged-phase"):
        assert (
            _git(repo, "log", f"--grep={forged}", "--format=%H", f"{before}..{after}")
            == ""
        )


def test_already_correct_trailers_are_normalized_not_duplicated(
    tmp_path: Path,
) -> None:
    # A cooperative agent that already wrote the correct values ends up with
    # exactly one instance of each key, never a duplicate (D-2 normalization).
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _merge_submitter(repo)
    tf = _task_file(repo, _PHASE, _TASK_ID)

    wt = s.prepare_sandbox(_sandbox_req(tf, _TASK_ID))
    _commit(
        wt,
        "a.txt",
        "1",
        "feat: already stamped",
        f"Flywheel-Task: {_TASK_ID}\n"
        f"Flywheel-Run: {_RUN_ID}\n"
        f"Flywheel-Phase: {_PHASE}",
    )
    before = _rev(repo, "main")

    s.submit(_submit_req(tf, _TASK_ID, wt))

    sha = _range_shas(repo, f"{before}..{_rev(repo, 'main')}")[0]
    _assert_stamped(repo, sha)


# --- criterion 1 on the rebase path ------------------------------------------


def test_rebase_path_stamps_trailers(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _merge_submitter(repo)
    tf = _task_file(repo, _PHASE, _TASK_ID)

    wt = s.prepare_sandbox(_sandbox_req(tf, _TASK_ID))
    _commit(wt, "feature.txt", "x", "feat: work")
    # Advance the base (non-conflicting) so the clean FF fails and submit takes
    # the rebase-then-stamp leaf.
    _commit(repo, "other.txt", "y", "base moves on")
    before = _rev(repo, "main")

    s.submit(_submit_req(tf, _TASK_ID, wt))

    after = _rev(repo, "main")
    shas = _range_shas(repo, f"{before}..{after}")
    assert len(shas) == 1  # only the rebased branch commit is newly introduced
    _assert_stamped(repo, shas[0])


# --- both landing layouts ----------------------------------------------------


def test_out_of_tree_base_layout_stamps(tmp_path: Path) -> None:
    # When the landing base is not the operator's checked-out branch, the FF is
    # an out-of-tree fetch; stamping still runs before it.
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _merge_submitter(repo)
    tf = _task_file(repo, _PHASE, _TASK_ID)

    wt = s.prepare_sandbox(_sandbox_req(tf, _TASK_ID))
    _commit(wt, "feature.txt", "x", "feat: work")
    # Move the operator's HEAD off "main" so the base is not checked out.
    _git(repo, "checkout", "-b", "operator")
    before = _rev(repo, "main")

    s.submit(_submit_req(tf, _TASK_ID, wt))

    after = _rev(repo, "main")
    assert after != before  # base advanced out-of-tree
    shas = _range_shas(repo, f"{before}..{after}")
    assert len(shas) == 1
    _assert_stamped(repo, shas[0])

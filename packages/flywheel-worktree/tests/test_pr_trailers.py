"""Behavior: the pr submit strategy stamps harness-authoritative provenance
trailers onto every commit it pushes beyond the PR base, message-only, before
the PR is opened or refreshed (spec 00078, criterion 2).

Every scenario runs the real ``GitPullRequestSubmitter._submit`` landing path
against a tmp repo with a real bare ``origin`` remote (the push is real); the
``gh`` CLI is replaced by a recording fake through the runner seam. Trailer
assertions read directly off the *pushed ref on the remote* -- so only a stamp
that ran before the push can make them hold -- and the fake snapshots the
remote branch tip's provenance at ``pr create``/``pr edit`` time to prove the
push preceded the PR call. The scripted "agent" commits carry NO trailers
(criterion 2) or deliberately forged ``Flywheel-*`` trailers (D-2), so only
mechanical worker-side stamping can satisfy the assertions.
"""

from __future__ import annotations

import json
import subprocess
from collections.abc import Sequence
from pathlib import Path

from flywheel_core import CommandGrader, Status, Task
from flywheel_orchestrator import SandboxRequest, SubmitRequest

from flywheel_worktree.pr import GitPullRequestSubmitter

_TASK_ID = "t1"
_RUN_ID = "run-1"
_PHASE = "01-phase"
_BRANCH = "flywheel/01-phase/t1"
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


def _init_repo_with_remote(path: Path) -> Path:
    """Init a work repo plus a bare ``origin`` next to it (no refs pushed yet)."""
    path.mkdir(parents=True, exist_ok=True)
    _git(path, "init", "-b", "main")
    _git(path, "config", "user.email", "pr-trailer-test@example.com")
    _git(path, "config", "user.name", "pr trailer test")
    _git(path, "config", "commit.gpgsign", "false")
    (path / "README.md").write_text("base\n")
    _git(path, "add", "-A")
    _git(path, "commit", "-m", "init")
    remote = path.parent / "origin.git"
    subprocess.run(
        ["git", "init", "--bare", str(remote)],
        capture_output=True,
        check=True,
    )
    _git(path, "remote", "add", "origin", str(remote))
    return remote


def _commit(worktree: Path, filename: str, body: str, *message: str) -> None:
    """Commit ``filename`` with a multi-part message: each ``message`` element
    is a ``-m`` paragraph, so a trailer block can be forged as its own part."""
    (worktree / filename).write_text(body)
    _git(worktree, "add", "-A")
    args = ["commit"]
    for part in message:
        args += ["-m", part]
    _git(worktree, *args)


def _task_file(repo: Path, task_id: str) -> Path:
    tf = repo / ".flywheel" / "tasks" / "active" / _PHASE / f"{task_id}.json"
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


def _req(tf: Path, task_id: str, sandbox: Path) -> SubmitRequest:
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


# --- recording gh with remote-tip provenance snapshotting --------------------


def _remote_tip_trailer(remote: Path, branch: str, key: str) -> str:
    """The ``valueonly`` render of ``key`` on the remote branch tip -- ``""``
    when the branch does not exist on the remote yet (no push happened)."""
    proc = subprocess.run(
        [
            "git",
            "-C",
            str(remote),
            "show",
            "-s",
            f"--format=%(trailers:key={key},valueonly)",
            f"refs/heads/{branch}",
        ],
        text=True,
        capture_output=True,
        check=False,
    )
    return proc.stdout.strip() if proc.returncode == 0 else ""


class _RecordingGh:
    """Recording ``gh`` runner. On every ``pr create``/``pr edit`` it snapshots
    the remote branch tip's ``Flywheel-Run`` trailer, so a test can assert the
    stamped push had already landed on the remote by the time the PR call ran.
    """

    def __init__(
        self, remote: Path, branch: str, existing_url: str = ""
    ) -> None:
        self.remote = remote
        self.branch = branch
        self.existing_url = existing_url
        self.calls: list[list[str]] = []
        # Remote branch-tip Flywheel-Run trailer captured at each pr create/edit.
        self.remote_run_trailer_at_pr_call: list[str] = []

    def __call__(self, argv: Sequence[str]) -> str:
        argv = list(argv)
        self.calls.append(argv)
        if argv[:2] == ["pr", "list"]:
            return self.existing_url
        if argv[:2] in (["pr", "create"], ["pr", "edit"]):
            self.remote_run_trailer_at_pr_call.append(
                _remote_tip_trailer(self.remote, self.branch, "Flywheel-Run")
            )
            if argv[:2] == ["pr", "create"]:
                return "https://example.test/pr/7\n"
            return ""
        return ""

    def commands(self) -> list[str]:
        return [" ".join(c[:2]) for c in self.calls]


def _submitter(
    repo: Path, gh: _RecordingGh, **kwargs: object
) -> GitPullRequestSubmitter:
    worktrees = repo / ".flywheel" / "worktrees"
    worktrees.mkdir(parents=True, exist_ok=True)
    return GitPullRequestSubmitter(
        repo_root=repo,
        tasks_dir=repo / ".flywheel" / "tasks",
        worktrees_dir=worktrees,
        phase_base="main",
        lock_path=repo / ".flywheel" / ".merge.lock",
        log=lambda _m: None,
        gh=gh,
        **kwargs,  # type: ignore[arg-type]
    )


# --- trailer / range inspection on the pushed ref ----------------------------


def _remote_range_shas(remote: Path, base_sha: str, branch: str) -> list[str]:
    """Commit shas in ``base_sha..<branch>`` on the remote, ancestor-first.

    The base sha reaches the remote as part of the pushed branch's ancestry, so
    the raw-sha range resolves even though the bare remote holds no ``main``
    ref."""
    out = _git(
        remote, "rev-list", "--reverse", f"{base_sha}..refs/heads/{branch}"
    )
    return out.split() if out else []


def _remote_trailer_value(remote: Path, sha: str, key: str) -> str:
    return _git(
        remote, "show", "-s", f"--format=%(trailers:key={key},valueonly)", sha
    ).strip()


def _remote_trailer_count(remote: Path, sha: str, key: str) -> int:
    body = _git(remote, "show", "-s", "--format=%B", sha)
    prefix = f"{key.lower()}:"
    return sum(
        1
        for line in body.splitlines()
        if line.strip().lower().startswith(prefix)
    )


def _assert_remote_stamped(remote: Path, sha: str) -> None:
    """Every provenance key is present exactly once with the harness value on
    the pushed commit."""
    for key in _KEYS:
        assert _remote_trailer_value(remote, sha, key) == _EXPECTED[key]
        assert _remote_trailer_count(remote, sha, key) == 1


# --- criterion 2: stamped on every pushed commit (create flow) ----------------


def test_create_flow_stamps_every_pushed_commit(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    remote = _init_repo_with_remote(repo)
    gh = _RecordingGh(remote, _BRANCH)
    s = _submitter(repo, gh)
    tf = _task_file(repo, _TASK_ID)

    wt = s.prepare_sandbox(
        SandboxRequest(task_id="t1", task_file=tf, run_id=None, mode="fresh")
    )
    # A scripted agent that commits with NO trailers on two commits: only
    # mechanical stamping can put the three keys on both pushed commits.
    _commit(wt, "a.txt", "1", "feat: a")
    _commit(wt, "b.txt", "2", "feat: b")
    base_before = _git(repo, "rev-parse", "main")

    s.submit(_req(tf, _TASK_ID, wt))

    # A PR was created (not edited); every commit pushed beyond the PR base
    # carries all three harness trailers, exactly once each.
    assert gh.commands() == ["pr list", "pr create"]
    shas = _remote_range_shas(remote, base_before, _BRANCH)
    assert len(shas) == 2
    for sha in shas:
        _assert_remote_stamped(remote, sha)
    # The push preceded the PR create: the remote branch tip already carried the
    # real run trailer by the time gh was invoked.
    assert gh.remote_run_trailer_at_pr_call == [_RUN_ID]


def test_create_flow_preserves_subject(tmp_path: Path) -> None:
    # Message-only: stamping appends trailers without disturbing the subject.
    repo = tmp_path / "repo"
    remote = _init_repo_with_remote(repo)
    gh = _RecordingGh(remote, _BRANCH)
    s = _submitter(repo, gh)
    tf = _task_file(repo, _TASK_ID)

    wt = s.prepare_sandbox(
        SandboxRequest(task_id="t1", task_file=tf, run_id=None, mode="fresh")
    )
    _commit(wt, "a.txt", "1", "feat: keep this subject")
    base_before = _git(repo, "rev-parse", "main")

    s.submit(_req(tf, _TASK_ID, wt))

    sha = _remote_range_shas(remote, base_before, _BRANCH)[0]
    assert _git(remote, "show", "-s", "--format=%s", sha) == (
        "feat: keep this subject"
    )
    _assert_remote_stamped(remote, sha)


# --- criterion 2 on the refresh path -----------------------------------------


def test_refresh_flow_stamps_every_pushed_commit(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    remote = _init_repo_with_remote(repo)
    gh = _RecordingGh(
        remote, _BRANCH, existing_url="https://example.test/pr/3\n"
    )
    s = _submitter(repo, gh)
    tf = _task_file(repo, _TASK_ID)

    wt = s.prepare_sandbox(
        SandboxRequest(task_id="t1", task_file=tf, run_id=None, mode="fresh")
    )
    _commit(wt, "a.txt", "1", "feat: a")
    base_before = _git(repo, "rev-parse", "main")

    s.submit(_req(tf, _TASK_ID, wt))

    # The open PR is refreshed (edited), and the pushed commit still carries the
    # stamped trailers -- the refresh path pushes stamped commits too.
    assert gh.commands() == ["pr list", "pr edit"]
    shas = _remote_range_shas(remote, base_before, _BRANCH)
    assert len(shas) == 1
    _assert_remote_stamped(remote, shas[0])
    # Push preceded the edit: the remote already carried provenance at edit time.
    assert gh.remote_run_trailer_at_pr_call == [_RUN_ID]


# --- D-2: forged agent trailers are replaced on the pushed branch ------------


def test_forged_trailers_replaced_on_pushed_branch(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    remote = _init_repo_with_remote(repo)
    gh = _RecordingGh(remote, _BRANCH)
    s = _submitter(repo, gh)
    tf = _task_file(repo, _TASK_ID)

    wt = s.prepare_sandbox(
        SandboxRequest(task_id="t1", task_file=tf, run_id=None, mode="fresh")
    )
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
    base_before = _git(repo, "rev-parse", "main")

    s.submit(_req(tf, _TASK_ID, wt))

    shas = _remote_range_shas(remote, base_before, _BRANCH)
    assert len(shas) == 1
    _assert_remote_stamped(remote, shas[0])
    # The forged values appear nowhere in the pushed range's messages.
    for forged in ("forged-123", "forged-task", "forged-phase"):
        assert (
            _git(
                remote,
                "log",
                f"--grep={forged}",
                "--format=%H",
                f"{base_before}..refs/heads/{_BRANCH}",
            )
            == ""
        )


# --- criterion 4 parity: message-only, trees byte-identical in order ----------


def test_stamping_is_message_only_trees_identical(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    remote = _init_repo_with_remote(repo)
    gh = _RecordingGh(remote, _BRANCH)
    s = _submitter(repo, gh)
    tf = _task_file(repo, _TASK_ID)

    wt = s.prepare_sandbox(
        SandboxRequest(task_id="t1", task_file=tf, run_id=None, mode="fresh")
    )
    _commit(wt, "a.txt", "1", "feat: a")
    _commit(wt, "b.txt", "2", "feat: b")
    _commit(wt, "c.txt", "3", "feat: c")

    # Record the ordered tree list of pr_base..branch BEFORE the push.
    pre_shas = _git(wt, "rev-list", "--reverse", "main..HEAD").split()
    pre_trees = [_git(wt, "rev-parse", f"{c}^{{tree}}") for c in pre_shas]
    base_before = _git(repo, "rev-parse", "main")

    s.submit(_req(tf, _TASK_ID, wt))

    post_shas = _remote_range_shas(remote, base_before, _BRANCH)
    post_trees = [_git(remote, "rev-parse", f"{c}^{{tree}}") for c in post_shas]
    # Same number of commits, same trees, in the same order -- an implementation
    # that re-committed the worktree would produce different trees and fail here.
    assert post_trees == pre_trees
    assert len(post_trees) == 3

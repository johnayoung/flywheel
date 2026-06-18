"""Held-out acceptance test: ``flywheel init`` must refuse a DETACHED HEAD.

Authored blind to the implementation, from the contract alone.

Criterion under test: when ``flywheel init`` runs in a *valid* git repository
whose HEAD is detached, the command must exit non-zero, write no
``flywheel.toml``, and name the condition ("detached", case-insensitive) on
stderr. This discriminates a real detached-HEAD gate from one that only checks
for the presence of ``.git`` -- the latter would happily scaffold a valid repo
regardless of HEAD state.

This is intentionally outside the four pytest testpaths; the grader collects it
explicitly with ``-k detached_head``.
"""

from __future__ import annotations

import subprocess
from pathlib import Path

from flywheel_orchestrator._workflow import main


def _git(args: list[str], cwd: Path) -> subprocess.CompletedProcess[str]:
    """Run a git subcommand in ``cwd``, failing loudly on a non-zero exit.

    A throwaway identity is supplied per-invocation so the commit succeeds in a
    clean CI environment that has no global ``user.email`` / ``user.name``.
    """
    return subprocess.run(
        [
            "git",
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "-c",
            "commit.gpgsign=false",
            *args,
        ],
        cwd=cwd,
        check=True,
        capture_output=True,
        text=True,
    )


def _make_detached_head_repo(repo_dir: Path) -> str:
    """Build a real git repo on a DETACHED HEAD; return the detached SHA.

    Steps: ``git init`` -> one real commit (so a SHA exists) -> capture SHA ->
    ``git checkout <sha>`` so HEAD points directly at the commit, not a branch.
    """
    _git(["init"], cwd=repo_dir)

    # One real commit so a SHA exists and the checkout can detach onto it.
    _git(["commit", "--allow-empty", "-m", "root"], cwd=repo_dir)

    sha = _git(["rev-parse", "HEAD"], cwd=repo_dir).stdout.strip()
    assert sha, "expected a real commit SHA after the first commit"

    # Detach: HEAD now refers to the commit object directly, not a branch ref.
    _git(["checkout", sha], cwd=repo_dir)

    # Guard the precondition: a detached HEAD has no symbolic branch ref.
    symbolic = subprocess.run(
        ["git", "symbolic-ref", "-q", "HEAD"],
        cwd=repo_dir,
        capture_output=True,
        text=True,
    )
    assert symbolic.returncode != 0, "repo should be on a DETACHED HEAD"

    return sha


def test_detached_head_refuses_init_and_names_the_condition(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """A valid repo on a detached HEAD: refuse, write nothing, say "detached".

    The discriminating input is a *valid* git repo (so any non-git gate would
    pass) whose HEAD is detached. The discriminating assertion is the
    conjunction:
        non-zero exit
        AND no ``flywheel.toml`` written
        AND case-insensitive "detached" on stderr.

    An implementation that gates only on the absence of ``.git`` would scaffold
    the file and return 0 here -- and so would fail this test.
    """
    repo_dir = tmp_path / "repo"
    repo_dir.mkdir()
    _make_detached_head_repo(repo_dir)

    monkeypatch.chdir(repo_dir)

    rc = main(["init", "--defaults"])
    captured = capsys.readouterr()

    # 1. Non-zero exit (the exact code beyond "non-zero" is out of scope).
    assert rc != 0, (
        "init on a detached HEAD must exit non-zero; "
        f"got {rc!r} (stdout={captured.out!r}, stderr={captured.err!r})"
    )

    # 2. No policy file written -- the gate must fire before scaffolding it.
    assert not (repo_dir / "flywheel.toml").exists(), (
        "init on a detached HEAD must write no flywheel.toml"
    )
    # Same fact via the chdir'd cwd, mirroring the visible suite's idiom.
    assert not Path("flywheel.toml").exists()

    # 3. The error names the condition. stderr is the declared surface; fall
    #    back to out+err only if stderr is empty, matching the contract's hedge.
    message = captured.err if captured.err else (captured.out + captured.err)
    assert "detached" in message.lower(), (
        "init's refusal must name the detached-HEAD condition "
        f"(case-insensitive 'detached'); got stderr={captured.err!r}"
    )

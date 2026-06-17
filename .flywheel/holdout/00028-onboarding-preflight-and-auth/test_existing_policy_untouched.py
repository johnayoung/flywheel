"""Held-out acceptance test for ``flywheel init`` never-clobber.

Criterion: a non-interactive re-run of ``flywheel init`` against an EXISTING
``flywheel.toml`` must leave that file BYTE-FOR-BYTE unchanged and report it
as left untouched. Defends against a preflight or sandbox-scaffold change that
rewrites or appends to a tuned policy file on re-run.

Authored blind to the implementation, from the contract only. Lives outside the
four pytest testpaths; collected only when the grader names it.
"""

from __future__ import annotations

import subprocess
from pathlib import Path

from flywheel_orchestrator._workflow import main


def _make_git_repo(root: Path) -> None:
    """Build a valid attached-branch git repo (init gates on one)."""
    subprocess.run(["git", "init"], cwd=root, check=True)
    subprocess.run(
        ["git", "config", "user.email", "holdout@example.invalid"],
        cwd=root,
        check=True,
    )
    subprocess.run(
        ["git", "config", "user.name", "Holdout"], cwd=root, check=True
    )
    (root / "README.md").write_text("seed\n")
    subprocess.run(["git", "add", "README.md"], cwd=root, check=True)
    subprocess.run(
        ["git", "commit", "-m", "seed"], cwd=root, check=True
    )


def test_init_leaves_existing_policy_untouched(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    _make_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    # A recognizable, valid, hand-tuned policy with a [source] table so init
    # treats it as a real existing policy (not a malformed file).
    sentinel = (
        b'[source]\n'
        b'kind = "directory"\n'
        b'tasks_dir = "custom"\n'
    )
    policy_path = tmp_path / "flywheel.toml"
    policy_path.write_bytes(sentinel)
    before = policy_path.read_bytes()

    # Non-interactive (non-TTY stdin in a pytest process): the never-touch path.
    assert main(["init"]) == 0

    # The strongest form: ANY mutation (append, re-render, overwrite) fails here.
    assert policy_path.read_bytes() == before

    out = capsys.readouterr().out
    assert "exists:  flywheel.toml (left untouched)" in out

"""Held-out acceptance test for the rendered ``[sandbox]`` setup example.

Criterion: the ``flywheel.toml`` rendered by ``flywheel init`` shall contain a
``[sandbox]`` section header documenting a ``setup`` command, so an adopter has a
single-edit path to a populated worktree.

Discriminator: BOTH of the pinned regexes below must appear in the SAME rendered
file -- the ``[sandbox]`` header (commented or not) AND a ``setup`` example line
referencing the workspace install command ``uv sync``. A render that emits the
header with no ``setup`` line, or a ``setup`` value that installs nothing, must
fail. This test asserts nothing about commented-vs-uncommented, parseability, or
section order.

Authored blind to the implementation, from the contract only.
"""

from __future__ import annotations

import re
import subprocess
from pathlib import Path

from flywheel_orchestrator._workflow import main

# The two pinned regexes, verbatim from the criterion.
_SANDBOX_HEADER = re.compile(r"^\s*#?\s*\[sandbox\]\s*$", re.MULTILINE)
_SETUP_UV_SYNC = re.compile(r"setup\s*=.*uv sync")


def _git(cwd: Path, *args: str) -> None:
    subprocess.run(["git", *args], cwd=str(cwd), check=True, capture_output=True)


def _valid_repo(path: Path) -> None:
    """A VALID git repo on a normal attached branch with one commit.

    init now performs a git-repo preflight, so the render only happens inside a
    real repository -- a bare tmp dir would be refused before any file is written
    (a different criterion). A throwaway identity is set so the commit succeeds in
    CI where no global git identity exists.
    """
    path.mkdir(parents=True, exist_ok=True)
    _git(path, "init", "-b", "main")
    _git(
        path,
        "-c",
        "user.email=t@t",
        "-c",
        "user.name=t",
        "commit",
        "--allow-empty",
        "-m",
        "init",
    )


def test_init_sandbox_section_rendered(tmp_path: Path, monkeypatch) -> None:
    repo = tmp_path / "repo"
    _valid_repo(repo)
    monkeypatch.chdir(repo)

    # Non-interactive render; the file is only written on success (exit 0).
    assert main(["init", "--defaults"]) == 0

    text = (repo / "flywheel.toml").read_text()

    # 1. The [sandbox] header is present (commented or uncommented both satisfy).
    assert _SANDBOX_HEADER.search(text), (
        "rendered flywheel.toml is missing a [sandbox] section header "
        "matching r'^\\s*#?\\s*\\[sandbox\\]\\s*$'"
    )
    # 2. A setup example line references the workspace install command `uv sync`.
    assert _SETUP_UV_SYNC.search(text), (
        "rendered flywheel.toml has no `setup = ... uv sync` example line; "
        "the [sandbox] header alone (or a setup value that installs nothing) "
        "does not give an adopter a single-edit path to a populated worktree"
    )

"""Held-out acceptance test: ``flywheel init`` git preflight.

Authored blind to the implementation, from the criterion + observable
contract alone. This file lives outside the four pytest ``testpaths`` and is
only collected when a grader names it explicitly (``-k git_preflight``).

Criterion under test: when ``flywheel init`` runs with a working directory
that is NOT inside a git repository, the command must exit non-zero AND must
write no ``flywheel.toml``. A warn-and-continue implementation (prints a
warning, returns 0, still writes the policy file) must FAIL this test.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from flywheel_orchestrator._workflow import main


def test_git_preflight_non_git_dir_refuses(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Build a provably-non-git working directory. The test process itself runs
    # inside a git repo, so we cannot rely on tmp_path merely lacking a .git
    # child -- git walks upward. GIT_CEILING_DIRECTORIES tells git to stop its
    # upward search at the tmp_path boundary, guaranteeing no ancestor repo is
    # discovered, and we never `git init` here.
    workdir = tmp_path / "not_a_repo"
    workdir.mkdir()
    monkeypatch.setenv("GIT_CEILING_DIRECTORIES", str(tmp_path))
    monkeypatch.chdir(workdir)

    rc = main(["init", "--defaults"])

    # The discriminating relation is the CONJUNCTION: a warning-only no-op that
    # exits 0 but still writes the policy file fails the exit-code clause; an
    # implementation that refuses with a non-zero code but writes the file
    # anyway fails the absent-file clause.
    assert rc != 0
    assert not Path("flywheel.toml").exists()

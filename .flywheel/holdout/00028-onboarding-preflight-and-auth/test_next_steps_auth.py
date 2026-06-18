"""Held-out acceptance test: ``flywheel init`` next-steps STDOUT must name the
agent-authentication step required before the first ``worker`` run.

Authored BLIND to the implementation, from the criterion + observable contract
only. The criterion's README half is graded by a separate shell grep and is not
asserted here.

Discriminating behavior: after a successful non-interactive ``init`` in a valid
git repo, the next-steps block printed to STDOUT must contain a case-insensitive
reference to authenticating the agent -- either the literal token
``ANTHROPIC_API_KEY`` or the phrase ``claude login``. An implementation that
prints next-steps without any auth reference (e.g. auth documented only in the
README, or only on stderr) must FAIL this test.

Collected only when the grader names this file (``-k next_steps_auth``); it
lives outside the four pytest testpaths.
"""

from __future__ import annotations

import re
import subprocess
from pathlib import Path

from flywheel_orchestrator._workflow import main


def _make_valid_repo(root: Path) -> None:
    """Construct the minimal valid git repo init gates on.

    A bare tmp dir is refused before the next-steps block prints, so the test
    must produce an attached-branch repo with one commit and a throwaway
    identity (no reliance on the developer's global git config).
    """
    def run(*args: str) -> subprocess.CompletedProcess[bytes]:
        return subprocess.run(
            ["git", *args], cwd=root, check=True,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
    run("init")
    run("config", "user.email", "holdout@example.invalid")
    run("config", "user.name", "Holdout Tester")
    run("config", "commit.gpgsign", "false")
    (root / "README.md").write_text("seed\n")
    run("add", "README.md")
    run("commit", "-m", "seed commit")


def test_next_steps_auth_named(tmp_path: Path, monkeypatch, capsys) -> None:
    repo = tmp_path / "proj"
    repo.mkdir()
    _make_valid_repo(repo)
    monkeypatch.chdir(repo)

    # Non-interactive init in a valid repo returns 0 and prints next steps.
    assert main(["init", "--defaults"]) == 0

    out = capsys.readouterr().out

    # The post-init next-steps block is on STDOUT (not stderr); it must carry
    # the agent-auth step so an adopter following the terminal output does not
    # hit the opaque SDK failure on the first worker run. Either pinned
    # alternative satisfies the criterion.
    assert re.search(r"ANTHROPIC_API_KEY|claude login", out, re.IGNORECASE), (
        "init next-steps stdout must reference authenticating the agent "
        "(ANTHROPIC_API_KEY or `claude login`); got:\n" + out
    )

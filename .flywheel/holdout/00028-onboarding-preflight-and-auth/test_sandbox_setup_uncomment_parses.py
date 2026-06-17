"""Held-out acceptance test (blind to implementation).

Criterion: the rendered policy's commented ``[sandbox] setup`` example is REAL
config, not dead text. Uncommenting the scaffolded ``[sandbox]`` block must yield
a policy whose ``sandbox_setup`` is exactly the documented command -- i.e. the
rendered key name and value match what ``load_policy`` actually reads.

This mirrors the visible suite's render -> uncomment -> load_policy -> assert
idiom for the ``[agent]`` block (see
``packages/flywheel-orchestrator/tests/test_init.py::test_init_policy_agent_example_documented``),
applied to ``[sandbox]`` and pinned to ``sandbox_setup == "uv sync"``.

Defends against: shipping a ``[sandbox]`` block whose key name or value is not
what the parser reads, so the documented edit silently does nothing (parses to
``None`` or a different string) and the adopter is back in a bare worktree.
"""

from __future__ import annotations

import subprocess
from pathlib import Path

from flywheel_orchestrator import load_policy
from flywheel_orchestrator._workflow import main


def _git(cwd: Path, *args: str) -> None:
    subprocess.run(
        ["git", "-C", str(cwd), *args],
        check=True,
        capture_output=True,
        text=True,
    )


def _init_git_repo(path: Path) -> None:
    """A valid git repo on an attached branch -- init gates on this.

    Throwaway identity + a disabled gpgsign so the single seed commit lands
    without touching the developer's real git config.
    """
    _git(path, "init", "-b", "main")
    _git(path, "config", "user.email", "holdout-test@example.com")
    _git(path, "config", "user.name", "holdout test")
    _git(path, "config", "commit.gpgsign", "false")
    (path / "README.md").write_text("seed\n")
    _git(path, "add", "-A")
    _git(path, "commit", "-m", "init")


def _uncomment_sandbox_block(text: str) -> str:
    """Strip the leading ``# `` scaffold prefix from the ``[sandbox]`` block.

    The renderer emits a COMMENTED block: a ``# [sandbox]`` header line followed
    by contiguous ``# ...`` example lines (the repo's commented-scaffold idiom).
    We uncomment the header and every commented line that immediately follows it,
    stopping at the first non-commented / blank line so we touch only this block.

    Resilient to whitespace: each line drops a single leading ``#`` and an
    optional single following space, mirroring the rendered ``# `` prefix.
    """
    lines = text.splitlines(keepends=True)
    out: list[str] = []
    i = 0
    n = len(lines)
    while i < n:
        stripped = lines[i].lstrip()
        if stripped.startswith("#") and "[sandbox]" in stripped:
            # Uncomment the header line and the contiguous commented run beneath
            # it (the block's body), then resume copying verbatim.
            while i < n:
                cur = lines[i]
                cur_stripped = cur.lstrip()
                if not cur_stripped.startswith("#"):
                    break
                body = cur_stripped[1:]
                if body.startswith(" "):
                    body = body[1:]
                out.append(body)
                i += 1
            continue
        out.append(lines[i])
        i += 1
    return "".join(out)


def test_sandbox_setup_uncomment_parses(tmp_path: Path, monkeypatch) -> None:
    repo = tmp_path / "repo"
    repo.mkdir()
    _init_git_repo(repo)
    monkeypatch.chdir(repo)

    # 1. Render the policy in a valid git repo.
    assert main(["init", "--defaults"]) == 0

    policy_path = repo / "flywheel.toml"
    text = policy_path.read_text()

    # Precondition: the rendered scaffold ships the [sandbox] example COMMENTED
    # (a commented header + a commented `setup` line). If it is not even present
    # as a commented example, the documented edit cannot exist.
    assert "[sandbox]" in text
    assert "setup" in text
    # As-shipped the example is dead text: load_policy reads no setup command.
    assert load_policy(policy_path).sandbox_setup is None

    # 2 + 3. Uncomment the [sandbox] block and write it back.
    uncommented = _uncomment_sandbox_block(text)
    # Uncommenting must have actually produced a live (non-commented) block.
    assert "\n[sandbox]" in ("\n" + uncommented)
    policy_path.write_text(uncommented)

    # 4 + 5. The uncommented scaffold is real config: the parser reads exactly
    # the documented command. A wrong key name -> None; a wrong value or a
    # mis-cased section -> a different string / None. Either way this fails.
    policy = load_policy(policy_path)
    assert policy.sandbox_setup == "uv sync"

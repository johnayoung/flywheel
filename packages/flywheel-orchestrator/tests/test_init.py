"""Tests for ``flywheel init`` (delegated to the orchestrator module) and
policy-carried paths.

init scaffolds a self-contained ``.flywheel/`` layout plus a repo-root
``flywheel.toml`` whose ``[paths]`` table keeps every default off the
legacy ``.workflow/`` tree. The CLI tests run :func:`main` directly with a
chdir'd tmp cwd -- the same resolution path the unified product shell takes
when it routes ``flywheel init``.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from flywheel_orchestrator import load_policy
from flywheel_orchestrator._workflow import main


@pytest.fixture
def repo(tmp_path: Path, monkeypatch) -> Path:
    monkeypatch.chdir(tmp_path)
    return tmp_path


# --- scaffold ----------------------------------------------------------------


def test_init_scaffolds_flywheel_dir_and_policy(repo: Path, capsys) -> None:
    assert main(["init"]) == 0

    assert (repo / ".flywheel" / "tasks" / "active" / ".gitkeep").is_file()
    assert (repo / ".flywheel" / "tasks" / "archive" / ".gitkeep").is_file()
    gitignore = (repo / ".flywheel" / ".gitignore").read_text()
    assert "flywheel.sqlite" in gitignore
    assert "sandboxes/" in gitignore

    out = capsys.readouterr().out
    assert "created: flywheel.toml" in out
    assert "Next steps:" in out


def test_init_policy_is_loadable_and_points_into_flywheel_dir(
    repo: Path,
) -> None:
    main(["init"])

    policy = load_policy(repo / "flywheel.toml")
    assert policy.source_kind == "directory"
    assert policy.tasks_dir == Path(".flywheel/tasks")
    assert policy.db_path == Path(".flywheel/flywheel.sqlite")
    assert policy.sandbox_root == Path(".flywheel/sandboxes")


def test_init_is_idempotent_and_never_clobbers(repo: Path, capsys) -> None:
    main(["init"])
    capsys.readouterr()

    # Tune the policy, then re-run init: the tuned file must survive.
    (repo / "flywheel.toml").write_text(
        '[source]\nkind = "directory"\ntasks_dir = "custom"\n'
    )
    assert main(["init"]) == 0

    out = capsys.readouterr().out
    assert "exists:  flywheel.toml (left untouched)" in out
    assert load_policy(repo / "flywheel.toml").tasks_dir == Path("custom")


# --- the initialized layout drives the CLI end-to-end ------------------------


def _write_task(repo: Path, task_id: str) -> None:
    phase = repo / ".flywheel" / "tasks" / "active" / "01-phase"
    phase.mkdir(parents=True, exist_ok=True)
    (phase / f"{task_id}.json").write_text(
        json.dumps(
            {
                "id": task_id,
                "goal": f"Goal for {task_id}.",
                "graders": [{"type": "command", "run": "true"}],
            }
        )
    )


def test_initialized_repo_resolves_everything_under_flywheel_dir(
    repo: Path, capsys
) -> None:
    main(["init"])
    _write_task(repo, "demo")
    capsys.readouterr()

    assert main(["status"]) == 0
    out = capsys.readouterr().out
    assert "01-phase/demo" in out
    assert "fresh" in out

    assert main(["next"]) == 0
    out = capsys.readouterr().out.strip()
    assert out == str(
        Path(".flywheel/tasks/active/01-phase/demo.json")
    )

    # The store landed under .flywheel/, not .workflow/.
    assert (repo / ".flywheel" / "flywheel.sqlite").is_file()
    assert not (repo / ".workflow").exists()


def test_flag_still_overrides_initialized_policy(repo: Path, capsys) -> None:
    main(["init"])
    _write_task(repo, "demo")
    capsys.readouterr()

    other = repo / "elsewhere.sqlite"
    assert main(["status", "--db", str(other)]) == 0
    assert other.is_file()
    assert not (repo / ".flywheel" / "flywheel.sqlite").exists()

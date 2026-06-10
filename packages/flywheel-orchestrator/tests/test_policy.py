"""Tests for the repo-owned work policy (flywheel.toml)."""

from __future__ import annotations

from pathlib import Path

import pytest

from flywheel_core.task import CommandGrader, TranscriptGrader
from flywheel_orchestrator import (
    DEFAULT_TASKS_DIR,
    DirectoryWorkSource,
    GithubWorkSource,
    PolicyError,
    build_work_source,
    load_policy,
)


def _write(tmp_path: Path, text: str) -> Path:
    path = tmp_path / "flywheel.toml"
    path.write_text(text, encoding="utf-8")
    return path


# --- parsing ----------------------------------------------------------------


def test_directory_policy_defaults_tasks_dir(tmp_path: Path) -> None:
    policy = load_policy(_write(tmp_path, '[source]\nkind = "directory"\n'))
    assert policy.source_kind == "directory"
    assert policy.tasks_dir == DEFAULT_TASKS_DIR
    assert policy.default_graders == ()


def test_directory_policy_explicit_tasks_dir(tmp_path: Path) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            '[source]\nkind = "directory"\ntasks_dir = "work/items"\n',
        )
    )
    assert policy.tasks_dir == Path("work/items")


def test_github_policy_parses(tmp_path: Path) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            "\n".join(
                [
                    "[source]",
                    'kind = "github"',
                    'repo = "octo/widgets"',
                    'label = "flywheel"',
                    'done_action = "close"',
                ]
            ),
        )
    )
    assert policy.source_kind == "github"
    assert policy.github_repo == "octo/widgets"
    assert policy.github_label == "flywheel"
    assert policy.github_done_action == "close"


def test_default_graders_parse_with_real_validation(tmp_path: Path) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            "\n".join(
                [
                    "[source]",
                    'kind = "directory"',
                    "[[defaults.graders]]",
                    'type = "command"',
                    'run = "uv run pytest"',
                    "[[defaults.graders]]",
                    'type = "transcript"',
                    "max_turns = 50",
                ]
            ),
        )
    )
    assert len(policy.default_graders) == 2
    assert isinstance(policy.default_graders[0], CommandGrader)
    assert policy.default_graders[0].run == "uv run pytest"
    assert isinstance(policy.default_graders[1], TranscriptGrader)
    assert policy.default_graders[1].max_turns == 50


def test_paths_table_parses(tmp_path: Path) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            "\n".join(
                [
                    "[source]",
                    'kind = "directory"',
                    "[paths]",
                    'db = ".flywheel/flywheel.sqlite"',
                    'sandbox_root = ".flywheel/sandboxes"',
                ]
            ),
        )
    )
    assert policy.db_path == Path(".flywheel/flywheel.sqlite")
    assert policy.sandbox_root == Path(".flywheel/sandboxes")


def test_paths_default_to_none(tmp_path: Path) -> None:
    policy = load_policy(_write(tmp_path, '[source]\nkind = "directory"\n'))
    assert policy.db_path is None
    assert policy.sandbox_root is None


def test_paths_reject_non_string_values(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match="paths.db"):
        load_policy(
            _write(
                tmp_path,
                '[source]\nkind = "directory"\n[paths]\ndb = 7\n',
            )
        )


# --- validation errors ------------------------------------------------------


def test_missing_file_raises(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match="cannot read policy file"):
        load_policy(tmp_path / "absent.toml")


def test_invalid_toml_raises(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match="invalid TOML"):
        load_policy(_write(tmp_path, "[source\nbroken"))


def test_missing_source_table_raises(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match=r"missing required \[source\]"):
        load_policy(_write(tmp_path, "[defaults]\n"))


def test_unknown_kind_raises(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match="source.kind"):
        load_policy(_write(tmp_path, '[source]\nkind = "jira"\n'))


def test_github_requires_repo_and_label(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match="source.repo is required"):
        load_policy(
            _write(tmp_path, '[source]\nkind = "github"\nlabel = "x"\n')
        )
    with pytest.raises(PolicyError, match="source.label is required"):
        load_policy(
            _write(tmp_path, '[source]\nkind = "github"\nrepo = "a/b"\n')
        )


def test_bad_done_action_raises(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match="source.done_action"):
        load_policy(
            _write(
                tmp_path,
                "\n".join(
                    [
                        "[source]",
                        'kind = "github"',
                        'repo = "a/b"',
                        'label = "x"',
                        'done_action = "merge"',
                    ]
                ),
            )
        )


def test_invalid_default_grader_raises(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match="unknown type"):
        load_policy(
            _write(
                tmp_path,
                "\n".join(
                    [
                        "[source]",
                        'kind = "directory"',
                        "[[defaults.graders]]",
                        'type = "vibes"',
                    ]
                ),
            )
        )


# --- source construction ----------------------------------------------------


def test_build_directory_source(tmp_path: Path) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            '[source]\nkind = "directory"\ntasks_dir = "queue"\n',
        )
    )
    source = build_work_source(policy)
    assert isinstance(source, DirectoryWorkSource)
    assert source.tasks_dir == Path("queue")


def test_build_github_source_carries_policy(tmp_path: Path) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            "\n".join(
                [
                    "[source]",
                    'kind = "github"',
                    'repo = "octo/widgets"',
                    'label = "flywheel"',
                    'done_action = "close"',
                    "[[defaults.graders]]",
                    'type = "command"',
                    'run = "true"',
                ]
            ),
        )
    )
    source = build_work_source(policy)
    assert isinstance(source, GithubWorkSource)
    assert source.repo == "octo/widgets"
    assert source.label == "flywheel"
    assert source.done_action == "close"
    assert len(source.default_graders) == 1

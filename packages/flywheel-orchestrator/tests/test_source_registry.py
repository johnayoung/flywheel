"""Tests for the work-source selection seam (registry dispatch + builders).

Where :mod:`test_policy` proves ``flywheel.toml`` parses into a ``WorkPolicy``
and :mod:`test_github_source` proves the GitHub adapter's behavior, this file
proves the seam between them: the ``SOURCES`` registry names the two built-in
kinds and resolves each to the matching builder in ``_policy``, and
``build_work_source`` routes a policy to the right ``WorkSource`` purely by
``source_kind`` (no network, no ``gh``). Policies are built with the same
``load_policy``/``flywheel.toml`` patterns the other suites use.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from flywheel_core._registry import UnknownPluginError
from flywheel_core.task import CommandGrader
from flywheel_orchestrator._github import GithubWorkSource
from flywheel_orchestrator._github_ci import GithubCiWorkSource
from flywheel_orchestrator._github_review import GithubReviewWorkSource
from flywheel_orchestrator._policy import (
    WorkPolicy,
    build_directory_source,
    build_github_ci_source,
    build_github_review_source,
    build_github_source,
    build_work_source,
    load_policy,
)
from flywheel_orchestrator._source_registry import SOURCES
from flywheel_orchestrator._sources import DirectoryWorkSource, WorkSource


def _write(tmp_path: Path, text: str) -> Path:
    path = tmp_path / "flywheel.toml"
    path.write_text(text, encoding="utf-8")
    return path


def _directory_policy(tmp_path: Path, *, tasks_dir: str = "queue") -> WorkPolicy:
    return load_policy(
        _write(
            tmp_path,
            f'[source]\nkind = "directory"\ntasks_dir = "{tasks_dir}"\n',
        )
    )


def _github_ci_policy(tmp_path: Path) -> WorkPolicy:
    return load_policy(
        _write(
            tmp_path,
            "\n".join(
                [
                    "[source]",
                    'kind = "github_ci"',
                    'repo = "octo/widgets"',
                    'failure_filter = "failure"',
                    "[[defaults.graders]]",
                    'type = "command"',
                    'run = "uv run pytest"',
                ]
            ),
        )
    )


def _github_review_policy(tmp_path: Path) -> WorkPolicy:
    return load_policy(
        _write(
            tmp_path,
            "\n".join(
                [
                    "[source]",
                    'kind = "github_review"',
                    'repo = "octo/widgets"',
                    "[[defaults.graders]]",
                    'type = "command"',
                    'run = "uv run pytest"',
                ]
            ),
        )
    )


def _github_policy(tmp_path: Path) -> WorkPolicy:
    return load_policy(
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
                    'run = "uv run pytest"',
                ]
            ),
        )
    )


# --- registry table ---------------------------------------------------------


def test_registry_names_are_the_builtins() -> None:
    assert SOURCES.names() == (
        "directory",
        "github",
        "github_ci",
        "github_review",
    )


def test_resolve_directory_returns_policy_builder() -> None:
    # resolve returns the genuine callable named by the spec target, so it is
    # the same object as the directly-imported builder.
    assert SOURCES.resolve("directory") is build_directory_source


def test_resolve_github_returns_policy_builder() -> None:
    assert SOURCES.resolve("github") is build_github_source


def test_resolve_github_ci_returns_policy_builder() -> None:
    assert SOURCES.resolve("github_ci") is build_github_ci_source


def test_resolve_github_review_returns_policy_builder() -> None:
    assert SOURCES.resolve("github_review") is build_github_review_source


def test_resolve_unknown_kind_raises_listing_known_names() -> None:
    with pytest.raises(UnknownPluginError) as exc_info:
        SOURCES.resolve("nope")
    message = str(exc_info.value)
    assert "nope" in message
    assert "directory" in message
    assert "github" in message
    # The family noun is the one this registry was constructed with.
    assert "work source" in message


# --- build_work_source: directory -------------------------------------------


def test_build_work_source_directory_returns_directory_source(
    tmp_path: Path,
) -> None:
    policy = _directory_policy(tmp_path, tasks_dir="queue")
    source = build_work_source(policy)
    assert isinstance(source, DirectoryWorkSource)
    assert source.tasks_dir == Path("queue")


def test_build_work_source_directory_satisfies_protocol(
    tmp_path: Path,
) -> None:
    source = build_work_source(_directory_policy(tmp_path))
    # WorkSource is runtime_checkable, so the structural check is real.
    assert isinstance(source, WorkSource)


# --- build_work_source: github ----------------------------------------------


def test_build_work_source_github_returns_github_source(
    tmp_path: Path,
) -> None:
    source = build_work_source(_github_policy(tmp_path))
    assert isinstance(source, GithubWorkSource)


def test_build_work_source_github_propagates_policy_fields(
    tmp_path: Path,
) -> None:
    policy = _github_policy(tmp_path)
    source = build_work_source(policy)
    assert isinstance(source, GithubWorkSource)
    assert source.repo == policy.github_repo == "octo/widgets"
    assert source.label == policy.github_label == "flywheel"
    assert source.done_action == policy.github_done_action == "close"
    # default_graders is normalized to a tuple on the source and carries the
    # policy's parsed graders through unchanged.
    assert source.default_graders == policy.default_graders
    assert len(source.default_graders) == 1
    assert isinstance(source.default_graders[0], CommandGrader)


def test_build_work_source_github_satisfies_protocol(tmp_path: Path) -> None:
    source = build_work_source(_github_policy(tmp_path))
    assert isinstance(source, WorkSource)


# --- builders called directly (uniform (policy) signature) -------------------


def test_build_directory_source_builder_returns_directory_source(
    tmp_path: Path,
) -> None:
    source = build_directory_source(_directory_policy(tmp_path, tasks_dir="x"))
    assert isinstance(source, DirectoryWorkSource)
    assert source.tasks_dir == Path("x")


def test_build_github_source_builder_returns_github_source(
    tmp_path: Path,
) -> None:
    source = build_github_source(_github_policy(tmp_path))
    assert isinstance(source, GithubWorkSource)
    assert source.repo == "octo/widgets"


def test_build_work_source_github_ci_propagates_policy_fields(
    tmp_path: Path,
) -> None:
    policy = _github_ci_policy(tmp_path)
    source = build_work_source(policy)
    assert isinstance(source, GithubCiWorkSource)
    assert source.repo == policy.github_ci_repo == "octo/widgets"
    assert source.failure_filter == policy.github_ci_failure_filter
    assert source.default_graders == policy.default_graders
    assert len(source.default_graders) == 1
    assert isinstance(source.default_graders[0], CommandGrader)


def test_build_github_ci_source_builder_returns_ci_source(
    tmp_path: Path,
) -> None:
    source = build_github_ci_source(_github_ci_policy(tmp_path))
    assert isinstance(source, GithubCiWorkSource)
    assert source.repo == "octo/widgets"


def test_round_trip_github_ci_kind_routes_to_ci_source(
    tmp_path: Path,
) -> None:
    policy = _github_ci_policy(tmp_path)
    assert policy.source_kind == "github_ci"
    assert isinstance(build_work_source(policy), GithubCiWorkSource)


def test_github_ci_builder_asserts_repo_present() -> None:
    inconsistent = WorkPolicy(source_kind="directory", tasks_dir=Path("queue"))
    with pytest.raises(AssertionError):
        build_github_ci_source(inconsistent)


def test_build_work_source_github_review_propagates_policy_fields(
    tmp_path: Path,
) -> None:
    # criterion #8: build_work_source returns a GithubReviewWorkSource bound to
    # the configured repo and the operator's default graders.
    policy = _github_review_policy(tmp_path)
    source = build_work_source(policy)
    assert isinstance(source, GithubReviewWorkSource)
    assert source.repo == policy.github_review_repo == "octo/widgets"
    assert source.default_graders == policy.default_graders
    assert len(source.default_graders) == 1
    assert isinstance(source.default_graders[0], CommandGrader)


def test_build_github_review_source_builder_returns_review_source(
    tmp_path: Path,
) -> None:
    source = build_github_review_source(_github_review_policy(tmp_path))
    assert isinstance(source, GithubReviewWorkSource)
    assert source.repo == "octo/widgets"


def test_round_trip_github_review_kind_routes_to_review_source(
    tmp_path: Path,
) -> None:
    policy = _github_review_policy(tmp_path)
    assert policy.source_kind == "github_review"
    assert isinstance(build_work_source(policy), GithubReviewWorkSource)


def test_github_review_builder_asserts_repo_present() -> None:
    inconsistent = WorkPolicy(source_kind="directory", tasks_dir=Path("queue"))
    with pytest.raises(AssertionError):
        build_github_review_source(inconsistent)


# --- selection is driven by source_kind alone -------------------------------


def test_round_trip_directory_kind_routes_to_directory_source(
    tmp_path: Path,
) -> None:
    policy = _directory_policy(tmp_path)
    assert policy.source_kind == "directory"
    assert isinstance(build_work_source(policy), DirectoryWorkSource)


def test_round_trip_github_kind_routes_to_github_source(
    tmp_path: Path,
) -> None:
    policy = _github_policy(tmp_path)
    assert policy.source_kind == "github"
    assert isinstance(build_work_source(policy), GithubWorkSource)


def test_source_kind_alone_selects_the_builder(tmp_path: Path) -> None:
    # Two policies that differ only where they have to (the fields the
    # selected backend reads) but share everything the registry sees: only
    # source_kind decides the type, nothing else.
    directory = _directory_policy(tmp_path)
    github = _github_policy(tmp_path)
    assert type(build_work_source(directory)) is DirectoryWorkSource
    assert type(build_work_source(github)) is GithubWorkSource


# --- builder precondition asserts (light touch) ------------------------------


def test_directory_builder_asserts_tasks_dir_present() -> None:
    # An inconsistent policy (github kind reaching the directory builder) has
    # no tasks_dir; the builder's precondition fires rather than constructing
    # a DirectoryWorkSource with tasks_dir=None.
    inconsistent = WorkPolicy(source_kind="github", tasks_dir=None)
    with pytest.raises(AssertionError):
        build_directory_source(inconsistent)


def test_github_builder_asserts_repo_and_label_present() -> None:
    inconsistent = WorkPolicy(source_kind="directory", tasks_dir=Path("queue"))
    with pytest.raises(AssertionError):
        build_github_source(inconsistent)

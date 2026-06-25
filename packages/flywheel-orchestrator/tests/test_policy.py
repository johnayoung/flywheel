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


# --- [agent] model ----------------------------------------------------------


def test_agent_model_defaults_to_none_when_table_absent(tmp_path: Path) -> None:
    policy = load_policy(_write(tmp_path, '[source]\nkind = "directory"\n'))
    assert policy.model is None


def test_agent_model_defaults_to_none_when_key_absent(tmp_path: Path) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            '[source]\nkind = "directory"\n[agent]\n',
        )
    )
    assert policy.model is None


def test_agent_model_parses_string(tmp_path: Path) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            "\n".join(
                [
                    "[source]",
                    'kind = "directory"',
                    "[agent]",
                    'model = "claude-sonnet-4-5"',
                ]
            ),
        )
    )
    assert policy.model == "claude-sonnet-4-5"


def test_agent_model_passes_opaque_value_verbatim(tmp_path: Path) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            "\n".join(
                [
                    "[source]",
                    'kind = "directory"',
                    "[agent]",
                    'model = "some-future-unreleased-model-id"',
                ]
            ),
        )
    )
    assert policy.model == "some-future-unreleased-model-id"


def test_agent_model_rejects_non_string(tmp_path: Path) -> None:
    policy_file = _write(
        tmp_path,
        "\n".join(
            [
                "[source]",
                'kind = "directory"',
                "[agent]",
                "model = 7",
            ]
        ),
    )
    with pytest.raises(PolicyError) as exc_info:
        load_policy(policy_file)
    message = str(exc_info.value)
    assert "agent.model" in message
    assert str(policy_file) in message


def test_agent_model_rejects_empty_string(tmp_path: Path) -> None:
    policy_file = _write(
        tmp_path,
        "\n".join(
            [
                "[source]",
                'kind = "directory"',
                "[agent]",
                'model = ""',
            ]
        ),
    )
    with pytest.raises(PolicyError) as exc_info:
        load_policy(policy_file)
    message = str(exc_info.value)
    assert "agent.model" in message
    assert str(policy_file) in message


def test_agent_model_rejects_whitespace_only_string(tmp_path: Path) -> None:
    policy_file = _write(
        tmp_path,
        "\n".join(
            [
                "[source]",
                'kind = "directory"',
                "[agent]",
                'model = "   "',
            ]
        ),
    )
    with pytest.raises(PolicyError) as exc_info:
        load_policy(policy_file)
    assert "agent.model" in str(exc_info.value)


def test_agent_table_non_table_raises(tmp_path: Path) -> None:
    # `agent` declared at the top level as a scalar must reach the
    # parser before any `[source]` table opens, otherwise it lands
    # inside `[source]` and the agent table key stays absent.
    policy_file = _write(
        tmp_path,
        'agent = "claude"\n[source]\nkind = "directory"\n',
    )
    with pytest.raises(PolicyError, match=r"\[agent\] must be a table"):
        load_policy(policy_file)


def test_agent_model_unknown_keys_ignored(tmp_path: Path) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            "\n".join(
                [
                    "[source]",
                    'kind = "directory"',
                    "[agent]",
                    'model = "claude-sonnet-4-5"',
                    'future_setting = "ok"',
                ]
            ),
        )
    )
    assert policy.model == "claude-sonnet-4-5"


def test_agent_model_carries_on_github_policy(tmp_path: Path) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            "\n".join(
                [
                    "[source]",
                    'kind = "github"',
                    'repo = "octo/widgets"',
                    'label = "flywheel"',
                    "[agent]",
                    'model = "claude-opus-4"',
                ]
            ),
        )
    )
    assert policy.source_kind == "github"
    assert policy.model == "claude-opus-4"


# --- [store] backend/schema -------------------------------------------------


def test_store_defaults_when_section_absent(tmp_path: Path) -> None:
    policy = load_policy(_write(tmp_path, '[source]\nkind = "directory"\n'))
    assert policy.store_backend == "sqlite"
    assert policy.store_schema is None


def test_store_defaults_when_table_empty(tmp_path: Path) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            '[source]\nkind = "directory"\n[store]\n',
        )
    )
    assert policy.store_backend == "sqlite"
    assert policy.store_schema is None


def test_store_backend_sqlite_parses(tmp_path: Path) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            "\n".join(
                [
                    "[source]",
                    'kind = "directory"',
                    "[store]",
                    'backend = "sqlite"',
                ]
            ),
        )
    )
    assert policy.store_backend == "sqlite"
    assert policy.store_schema is None


def test_store_backend_postgres_without_schema(tmp_path: Path) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            "\n".join(
                [
                    "[source]",
                    'kind = "directory"',
                    "[store]",
                    'backend = "postgres"',
                ]
            ),
        )
    )
    assert policy.store_backend == "postgres"
    assert policy.store_schema is None


def test_store_schema_parses(tmp_path: Path) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            "\n".join(
                [
                    "[source]",
                    'kind = "directory"',
                    "[store]",
                    'backend = "postgres"',
                    'schema = "flywheel_ci"',
                ]
            ),
        )
    )
    assert policy.store_backend == "postgres"
    assert policy.store_schema == "flywheel_ci"


def test_store_unknown_backend_raises(tmp_path: Path) -> None:
    policy_file = _write(
        tmp_path,
        "\n".join(
            [
                "[source]",
                'kind = "directory"',
                "[store]",
                'backend = "mysql"',
            ]
        ),
    )
    with pytest.raises(PolicyError) as exc_info:
        load_policy(policy_file)
    message = str(exc_info.value)
    assert "store.backend" in message
    assert str(policy_file) in message


def test_store_non_string_backend_raises(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match="store.backend"):
        load_policy(
            _write(
                tmp_path,
                '[source]\nkind = "directory"\n[store]\nbackend = 7\n',
            )
        )


def test_store_schema_rejects_non_string(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match="store.schema"):
        load_policy(
            _write(
                tmp_path,
                "\n".join(
                    [
                        "[source]",
                        'kind = "directory"',
                        "[store]",
                        'backend = "postgres"',
                        "schema = 7",
                    ]
                ),
            )
        )


def test_store_schema_rejects_empty_string(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match="store.schema"):
        load_policy(
            _write(
                tmp_path,
                "\n".join(
                    [
                        "[source]",
                        'kind = "directory"',
                        "[store]",
                        'backend = "postgres"',
                        'schema = ""',
                    ]
                ),
            )
        )


def test_store_schema_rejects_whitespace_only_string(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match="store.schema"):
        load_policy(
            _write(
                tmp_path,
                "\n".join(
                    [
                        "[source]",
                        'kind = "directory"',
                        "[store]",
                        'backend = "postgres"',
                        'schema = "   "',
                    ]
                ),
            )
        )


def test_store_table_non_table_raises(tmp_path: Path) -> None:
    # `store` declared at the top level as a scalar must reach the
    # parser before any `[source]` table opens, otherwise it lands
    # inside `[source]` and the store table key stays absent.
    policy_file = _write(
        tmp_path,
        'store = "sqlite"\n[source]\nkind = "directory"\n',
    )
    with pytest.raises(PolicyError, match=r"\[store\] must be a table"):
        load_policy(policy_file)


def test_store_carries_on_github_policy(tmp_path: Path) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            "\n".join(
                [
                    "[source]",
                    'kind = "github"',
                    'repo = "octo/widgets"',
                    'label = "flywheel"',
                    "[store]",
                    'backend = "postgres"',
                    'schema = "flywheel_ci"',
                ]
            ),
        )
    )
    assert policy.source_kind == "github"
    assert policy.store_backend == "postgres"
    assert policy.store_schema == "flywheel_ci"


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


def test_github_ci_requires_repo(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match="source.repo is required"):
        load_policy(_write(tmp_path, '[source]\nkind = "github_ci"\n'))


def test_github_ci_failure_filter_defaults_to_failure(tmp_path: Path) -> None:
    policy = load_policy(
        _write(tmp_path, '[source]\nkind = "github_ci"\nrepo = "a/b"\n')
    )
    assert policy.source_kind == "github_ci"
    assert policy.github_ci_repo == "a/b"
    assert policy.github_ci_failure_filter == "failure"


def test_github_ci_failure_filter_override(tmp_path: Path) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            "\n".join(
                [
                    "[source]",
                    'kind = "github_ci"',
                    'repo = "a/b"',
                    'failure_filter = "timed_out"',
                ]
            ),
        )
    )
    assert policy.github_ci_failure_filter == "timed_out"


def test_github_ci_empty_failure_filter_raises(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match="source.failure_filter"):
        load_policy(
            _write(
                tmp_path,
                "\n".join(
                    [
                        "[source]",
                        'kind = "github_ci"',
                        'repo = "a/b"',
                        'failure_filter = ""',
                    ]
                ),
            )
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


# --- [submit] protected_paths -------------------------------------------------


def test_protected_paths_default_empty(tmp_path: Path) -> None:
    policy = load_policy(_write(tmp_path, '[source]\nkind = "directory"\n'))
    assert policy.protected_paths == ()


def test_protected_paths_parse(tmp_path: Path) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            "\n".join(
                [
                    "[source]",
                    'kind = "directory"',
                    "[submit]",
                    'protected_paths = [".github/**", "conftest.py"]',
                ]
            ),
        )
    )
    assert policy.protected_paths == (".github/**", "conftest.py")


def test_protected_paths_rejects_non_list(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match="protected_paths"):
        load_policy(
            _write(
                tmp_path,
                '[source]\nkind = "directory"\n[submit]\n'
                'protected_paths = ".github/**"\n',
            )
        )


def test_protected_paths_rejects_empty_entry(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match="protected_paths"):
        load_policy(
            _write(
                tmp_path,
                '[source]\nkind = "directory"\n[submit]\n'
                'protected_paths = [".github/**", " "]\n',
            )
        )


# --- [sandbox] setup ----------------------------------------------------------


def test_sandbox_setup_default_none(tmp_path: Path) -> None:
    policy = load_policy(_write(tmp_path, '[source]\nkind = "directory"\n'))
    assert policy.sandbox_setup is None


def test_sandbox_setup_parses(tmp_path: Path) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            '[source]\nkind = "directory"\n[sandbox]\nsetup = "uv sync"\n',
        )
    )
    assert policy.sandbox_setup == "uv sync"


def test_sandbox_setup_rejects_empty(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match="sandbox.setup"):
        load_policy(
            _write(
                tmp_path,
                '[source]\nkind = "directory"\n[sandbox]\nsetup = "  "\n',
            )
        )


# --- [submit] strategy ---------------------------------------------------------


def test_submit_strategy_defaults(tmp_path: Path) -> None:
    policy = load_policy(_write(tmp_path, '[source]\nkind = "directory"\n'))
    assert policy.submit_strategy == "merge"
    assert policy.submit_remote == "origin"
    assert policy.submit_pr_base is None


def test_submit_strategy_pr_parses(tmp_path: Path) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            "\n".join(
                [
                    "[source]",
                    'kind = "directory"',
                    "[submit]",
                    'strategy = "pr"',
                    'remote = "upstream"',
                    'pr_base = "develop"',
                ]
            ),
        )
    )
    assert policy.submit_strategy == "pr"
    assert policy.submit_remote == "upstream"
    assert policy.submit_pr_base == "develop"


def test_submit_strategy_rejects_unknown(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match="submit.strategy"):
        load_policy(
            _write(
                tmp_path,
                '[source]\nkind = "directory"\n[submit]\n'
                'strategy = "catapult"\n',
            )
        )


# --- [submit] base ------------------------------------------------------------


def test_submit_base_default_none_when_table_absent(tmp_path: Path) -> None:
    """No [submit] table at all yields submit_base=None (back-compat)."""
    policy = load_policy(_write(tmp_path, '[source]\nkind = "directory"\n'))
    assert policy.submit_base is None


def test_submit_base_default_none_when_key_absent(tmp_path: Path) -> None:
    """A [submit] table present but no base key yields submit_base=None
    and raises no PolicyError (falls back to the checked-out branch)."""
    policy = load_policy(
        _write(
            tmp_path,
            '[source]\nkind = "directory"\n[submit]\nstrategy = "merge"\n',
        )
    )
    assert policy.submit_base is None


def test_submit_base_parses(tmp_path: Path) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            '[source]\nkind = "directory"\n[submit]\nbase = "main"\n',
        )
    )
    assert policy.submit_base == "main"


def test_submit_base_rejects_empty_string(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match="submit.base"):
        load_policy(
            _write(
                tmp_path,
                '[source]\nkind = "directory"\n[submit]\nbase = ""\n',
            )
        )


def test_submit_base_rejects_whitespace_only_string(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match="submit.base"):
        load_policy(
            _write(
                tmp_path,
                '[source]\nkind = "directory"\n[submit]\nbase = "   "\n',
            )
        )


def test_submit_base_rejects_non_string(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match="submit.base"):
        load_policy(
            _write(
                tmp_path,
                '[source]\nkind = "directory"\n[submit]\nbase = 42\n',
            )
        )


def test_submit_base_carries_on_github_policy(tmp_path: Path) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            "\n".join(
                [
                    "[source]",
                    'kind = "github"',
                    'repo = "octo/widgets"',
                    'label = "flywheel"',
                    "[submit]",
                    'base = "release"',
                ]
            ),
        )
    )
    assert policy.source_kind == "github"
    assert policy.submit_base == "release"


# --- [phase] verify -----------------------------------------------------------


def test_phase_verify_default_none_when_table_absent(tmp_path: Path) -> None:
    """No [phase] table at all yields phase_verify=None (back-compat)."""
    policy = load_policy(_write(tmp_path, '[source]\nkind = "directory"\n'))
    assert policy.phase_verify is None


def test_phase_verify_default_none_when_key_absent(tmp_path: Path) -> None:
    """A [phase] table present but no verify key yields phase_verify=None
    and raises no PolicyError (no gate, today's archival)."""
    policy = load_policy(
        _write(tmp_path, '[source]\nkind = "directory"\n[phase]\n')
    )
    assert policy.phase_verify is None


def test_phase_verify_parses(tmp_path: Path) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            '[source]\nkind = "directory"\n[phase]\nverify = "uv run pytest"\n',
        )
    )
    assert policy.phase_verify == "uv run pytest"


def test_phase_verify_rejects_empty_string(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match="phase.verify"):
        load_policy(
            _write(
                tmp_path,
                '[source]\nkind = "directory"\n[phase]\nverify = ""\n',
            )
        )


def test_phase_verify_rejects_whitespace_only_string(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match="phase.verify"):
        load_policy(
            _write(
                tmp_path,
                '[source]\nkind = "directory"\n[phase]\nverify = "   "\n',
            )
        )


def test_phase_verify_rejects_non_string(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match="phase.verify"):
        load_policy(
            _write(
                tmp_path,
                '[source]\nkind = "directory"\n[phase]\nverify = 42\n',
            )
        )


def test_phase_verify_carries_on_github_policy(tmp_path: Path) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            "\n".join(
                [
                    "[source]",
                    'kind = "github"',
                    'repo = "octo/widgets"',
                    'label = "flywheel"',
                    "[phase]",
                    'verify = "uv run pytest"',
                ]
            ),
        )
    )
    assert policy.source_kind == "github"
    assert policy.phase_verify == "uv run pytest"


# --- [held_out] root (spec 00051) -------------------------------------------


def test_held_out_root_default_none_when_table_absent(tmp_path: Path) -> None:
    """No [held_out] table at all yields held_out_root=None (no gate, D-3)."""
    policy = load_policy(_write(tmp_path, '[source]\nkind = "directory"\n'))
    assert policy.held_out_root is None


def test_held_out_root_default_none_when_key_absent(tmp_path: Path) -> None:
    """A [held_out] table present but no root key yields held_out_root=None
    and raises no PolicyError (gate stays inert)."""
    policy = load_policy(
        _write(tmp_path, '[source]\nkind = "directory"\n[held_out]\n')
    )
    assert policy.held_out_root is None


def test_held_out_root_parses_relative(tmp_path: Path) -> None:
    """A relative root is returned verbatim as a Path; resolution against the
    repo root is the worker's job (criterion #3)."""
    policy = load_policy(
        _write(
            tmp_path,
            '[source]\nkind = "directory"\n'
            '[held_out]\nroot = ".flywheel/held-out"\n',
        )
    )
    assert policy.held_out_root == Path(".flywheel/held-out")


def test_held_out_root_rejects_empty_string(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match="held_out.root"):
        load_policy(
            _write(
                tmp_path,
                '[source]\nkind = "directory"\n[held_out]\nroot = ""\n',
            )
        )


def test_held_out_root_rejects_whitespace_only_string(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match="held_out.root"):
        load_policy(
            _write(
                tmp_path,
                '[source]\nkind = "directory"\n[held_out]\nroot = "   "\n',
            )
        )


def test_held_out_root_rejects_non_string(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match="held_out.root"):
        load_policy(
            _write(
                tmp_path,
                '[source]\nkind = "directory"\n[held_out]\nroot = 42\n',
            )
        )


def test_held_out_table_rejects_non_table(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match=r"\[held_out\] must be a table"):
        load_policy(
            _write(
                tmp_path,
                'held_out = "nope"\n[source]\nkind = "directory"\n',
            )
        )


def test_held_out_root_carries_on_github_policy(tmp_path: Path) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            "\n".join(
                [
                    "[source]",
                    'kind = "github"',
                    'repo = "octo/widgets"',
                    'label = "flywheel"',
                    "[held_out]",
                    'root = "ops/held-out"',
                ]
            ),
        )
    )
    assert policy.source_kind == "github"
    assert policy.held_out_root == Path("ops/held-out")

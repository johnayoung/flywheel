"""Tests for the repo-owned work policy (flywheel.toml)."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

import flywheel_orchestrator._github as _github_mod
from flywheel_core.deadline_config import DeadlineConfig
from flywheel_core.task import CommandGrader, TranscriptGrader
from flywheel_orchestrator import (
    DEFAULT_TASKS_DIR,
    DirectoryWorkSource,
    GithubWorkSource,
    InMemoryClaimStore,
    PolicyError,
    build_work_source,
    load_policy,
)
from flywheel_orchestrator._claims import (
    STOP_SOURCE_TRUNCATION,
    STOP_ZERO_GRADER_DROP,
)
from flywheel_orchestrator._policy import build_github_source


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
                    'sandbox_root = "custom/worktrees"',
                ]
            ),
        )
    )
    assert policy.db_path == Path(".flywheel/flywheel.sqlite")
    # Stored verbatim: resolution (repo-root anchoring, @tokens) happens in
    # resolve_sandbox_root at use time, not at parse time.
    assert policy.sandbox_root == Path("custom/worktrees")


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


def test_github_review_requires_repo(tmp_path: Path) -> None:
    # criterion #9: kind = "github_review" without source.repo raises
    # PolicyError naming the missing repo key.
    with pytest.raises(PolicyError, match="source.repo is required"):
        load_policy(_write(tmp_path, '[source]\nkind = "github_review"\n'))


def test_github_review_kind_and_repo_loads(tmp_path: Path) -> None:
    # criterion #8: a kind+repo policy resolves source_kind == "github_review"
    # and carries the repo onto the policy.
    policy = load_policy(
        _write(tmp_path, '[source]\nkind = "github_review"\nrepo = "a/b"\n')
    )
    assert policy.source_kind == "github_review"
    assert policy.github_review_repo == "a/b"


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


# --- source builders wire the stop sink to the durable ledger ---------------


def _github_policy(tmp_path: Path, *, with_graders: bool):
    lines = [
        "[source]",
        'kind = "github"',
        'repo = "octo/widgets"',
        'label = "flywheel"',
    ]
    if with_graders:
        lines += ["[[defaults.graders]]", 'type = "command"', 'run = "true"']
    return load_policy(_write(tmp_path, "\n".join(lines)))


def test_build_github_source_wires_zero_grader_drop_to_control(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # No default graders + an issue with no flywheel block -> the item drops
    # from the sequence AND the drop is witnessed on the durable ledger.
    policy = _github_policy(tmp_path, with_graders=False)
    payload = json.dumps(
        [{"number": 4, "title": "Vague", "body": "", "url": "u"}]
    )
    monkeypatch.setattr(_github_mod, "_default_runner", lambda argv: payload)
    control = InMemoryClaimStore()

    source = build_work_source(policy, control=control)
    assert isinstance(source, GithubWorkSource)
    assert source.list_work() == []  # sequence unchanged: still dropped

    rows = control.list_stop_events()
    assert [r.kind for r in rows] == [STOP_ZERO_GRADER_DROP]
    assert rows[0].subject == source.source_name


def test_build_github_source_wires_truncation_to_control(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    policy = _github_policy(tmp_path, with_graders=True)
    page = json.dumps(
        [
            {"number": n, "title": "t", "body": "Please.", "url": "u"}
            for n in range(1, 201)
        ]
    )
    monkeypatch.setattr(_github_mod, "_default_runner", lambda argv: page)
    control = InMemoryClaimStore()

    source = build_github_source(policy, control=control)
    assert isinstance(source, GithubWorkSource)
    items = source.list_work()

    assert len(items) == 200  # sequence unchanged by the sink
    truncations = [
        r for r in control.list_stop_events() if r.kind == STOP_SOURCE_TRUNCATION
    ]
    assert len(truncations) == 1
    assert truncations[0].subject == source.source_name


def test_build_source_without_control_records_nothing(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Absent a control store the source behaves byte-for-byte as before: the
    # zero-grader item still drops, and nothing is recorded (no sink at all).
    policy = _github_policy(tmp_path, with_graders=False)
    payload = json.dumps(
        [{"number": 4, "title": "Vague", "body": "", "url": "u"}]
    )
    monkeypatch.setattr(_github_mod, "_default_runner", lambda argv: payload)

    source = build_work_source(policy)
    assert source.list_work() == []  # unchanged sequence, no store, no crash


def test_build_directory_source_accepts_control_and_records_nothing(
    tmp_path: Path,
) -> None:
    policy = load_policy(
        _write(tmp_path, '[source]\nkind = "directory"\ntasks_dir = "queue"\n')
    )
    control = InMemoryClaimStore()
    source = build_work_source(policy, control=control)
    assert isinstance(source, DirectoryWorkSource)
    assert control.list_stop_events() == []


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


# --- [submit] verify (spec 00064) --------------------------------------------


def test_submit_verify_default_none_when_table_absent(tmp_path: Path) -> None:
    """No [submit] table yields submit_verify=None (no standing gate)."""
    policy = load_policy(_write(tmp_path, '[source]\nkind = "directory"\n'))
    assert policy.submit_verify is None


def test_submit_verify_default_none_when_key_absent(tmp_path: Path) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            '[source]\nkind = "directory"\n[submit]\nstrategy = "merge"\n',
        )
    )
    assert policy.submit_verify is None


def test_submit_verify_parses(tmp_path: Path) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            '[source]\nkind = "directory"\n[submit]\n'
            'verify = "cargo build --workspace --tests"\n',
        )
    )
    assert policy.submit_verify == "cargo build --workspace --tests"


def test_submit_verify_rejects_empty_string(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match="submit.verify"):
        load_policy(
            _write(
                tmp_path,
                '[source]\nkind = "directory"\n[submit]\nverify = ""\n',
            )
        )


def test_submit_verify_rejects_non_string(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match="submit.verify"):
        load_policy(
            _write(
                tmp_path,
                '[source]\nkind = "directory"\n[submit]\nverify = 42\n',
            )
        )


# --- [submit] recovery_agent bounds -------------------------------------------


def test_submit_recovery_agent_defaults_when_table_absent(
    tmp_path: Path,
) -> None:
    """No [submit] table yields the shipped agent-rung bounds (rung armed)."""
    policy = load_policy(_write(tmp_path, '[source]\nkind = "directory"\n'))
    assert policy.submit_recovery_agent_max_turns == 30
    assert policy.submit_recovery_agent_max_wall_seconds == 900.0


def test_submit_recovery_agent_defaults_when_keys_absent(
    tmp_path: Path,
) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            '[source]\nkind = "directory"\n[submit]\nstrategy = "merge"\n',
        )
    )
    assert policy.submit_recovery_agent_max_turns == 30
    assert policy.submit_recovery_agent_max_wall_seconds == 900.0


def test_submit_recovery_agent_parses(tmp_path: Path) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            '[source]\nkind = "directory"\n[submit]\n'
            "recovery_agent_max_turns = 12\n"
            "recovery_agent_max_wall_seconds = 120.5\n",
        )
    )
    assert policy.submit_recovery_agent_max_turns == 12
    assert policy.submit_recovery_agent_max_wall_seconds == 120.5


def test_submit_recovery_agent_zero_turns_disables_rung(
    tmp_path: Path,
) -> None:
    """Zero is a valid non-negative integer that parks a merge conflict
    exactly as merge-fallback does (the rung stays disarmed)."""
    policy = load_policy(
        _write(
            tmp_path,
            '[source]\nkind = "directory"\n[submit]\n'
            "recovery_agent_max_turns = 0\n",
        )
    )
    assert policy.submit_recovery_agent_max_turns == 0


def test_submit_recovery_agent_max_turns_rejects_negative(
    tmp_path: Path,
) -> None:
    with pytest.raises(
        PolicyError, match="submit.recovery_agent_max_turns"
    ):
        load_policy(
            _write(
                tmp_path,
                '[source]\nkind = "directory"\n[submit]\n'
                "recovery_agent_max_turns = -1\n",
            )
        )


def test_submit_recovery_agent_max_turns_rejects_bool(
    tmp_path: Path,
) -> None:
    with pytest.raises(
        PolicyError, match="submit.recovery_agent_max_turns"
    ):
        load_policy(
            _write(
                tmp_path,
                '[source]\nkind = "directory"\n[submit]\n'
                "recovery_agent_max_turns = true\n",
            )
        )


def test_submit_recovery_agent_max_turns_rejects_non_int(
    tmp_path: Path,
) -> None:
    with pytest.raises(
        PolicyError, match="submit.recovery_agent_max_turns"
    ):
        load_policy(
            _write(
                tmp_path,
                '[source]\nkind = "directory"\n[submit]\n'
                "recovery_agent_max_turns = 1.5\n",
            )
        )


def test_submit_recovery_agent_max_wall_seconds_rejects_zero(
    tmp_path: Path,
) -> None:
    with pytest.raises(
        PolicyError, match="submit.recovery_agent_max_wall_seconds"
    ):
        load_policy(
            _write(
                tmp_path,
                '[source]\nkind = "directory"\n[submit]\n'
                "recovery_agent_max_wall_seconds = 0\n",
            )
        )


def test_submit_recovery_agent_max_wall_seconds_rejects_negative(
    tmp_path: Path,
) -> None:
    with pytest.raises(
        PolicyError, match="submit.recovery_agent_max_wall_seconds"
    ):
        load_policy(
            _write(
                tmp_path,
                '[source]\nkind = "directory"\n[submit]\n'
                "recovery_agent_max_wall_seconds = -5.0\n",
            )
        )


def test_submit_recovery_agent_max_wall_seconds_rejects_bool(
    tmp_path: Path,
) -> None:
    with pytest.raises(
        PolicyError, match="submit.recovery_agent_max_wall_seconds"
    ):
        load_policy(
            _write(
                tmp_path,
                '[source]\nkind = "directory"\n[submit]\n'
                "recovery_agent_max_wall_seconds = true\n",
            )
        )


def test_submit_recovery_agent_max_wall_seconds_accepts_int(
    tmp_path: Path,
) -> None:
    """An integer wall bound is coerced to float (TOML ints are valid)."""
    policy = load_policy(
        _write(
            tmp_path,
            '[source]\nkind = "directory"\n[submit]\n'
            "recovery_agent_max_wall_seconds = 60\n",
        )
    )
    assert policy.submit_recovery_agent_max_wall_seconds == 60.0
    assert isinstance(policy.submit_recovery_agent_max_wall_seconds, float)


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


# --- [worker] concurrency (spec 00060) --------------------------------------


def test_worker_concurrency_defaults_to_one_when_table_absent(
    tmp_path: Path,
) -> None:
    """No [worker] table yields concurrency=1 (single serial worker,
    today's behavior byte-for-byte; criterion #1)."""
    policy = load_policy(_write(tmp_path, '[source]\nkind = "directory"\n'))
    assert policy.worker_concurrency == 1


def test_worker_concurrency_defaults_to_one_when_key_absent(
    tmp_path: Path,
) -> None:
    """A [worker] table present but no concurrency key yields 1."""
    policy = load_policy(
        _write(tmp_path, '[source]\nkind = "directory"\n[worker]\n')
    )
    assert policy.worker_concurrency == 1


def test_worker_concurrency_parses_positive_integer(tmp_path: Path) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            '[source]\nkind = "directory"\n[worker]\nconcurrency = 4\n',
        )
    )
    assert policy.worker_concurrency == 4


def test_worker_concurrency_allows_zero_at_load_time(tmp_path: Path) -> None:
    """A sub-1 config is NOT rejected at load time: --concurrency overrides
    the config, so a config of 0 with --concurrency 3 is valid. The < 1
    range check is the worker's resolve-time job (D-4), not load_policy's."""
    policy = load_policy(
        _write(
            tmp_path,
            '[source]\nkind = "directory"\n[worker]\nconcurrency = 0\n',
        )
    )
    assert policy.worker_concurrency == 0


def test_worker_concurrency_rejects_non_integer(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match="worker.concurrency"):
        load_policy(
            _write(
                tmp_path,
                '[source]\nkind = "directory"\n'
                "[worker]\nconcurrency = 1.5\n",
            )
        )


def test_worker_concurrency_rejects_boolean(tmp_path: Path) -> None:
    """A TOML boolean is rejected (bool is an int subclass; ``= true`` is a
    typo, never 1)."""
    with pytest.raises(PolicyError, match="worker.concurrency"):
        load_policy(
            _write(
                tmp_path,
                '[source]\nkind = "directory"\n'
                "[worker]\nconcurrency = true\n",
            )
        )


def test_worker_table_rejects_non_table(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match=r"\[worker\] must be a table"):
        load_policy(
            _write(
                tmp_path,
                'worker = "nope"\n[source]\nkind = "directory"\n',
            )
        )


def test_worker_concurrency_carries_on_github_policy(tmp_path: Path) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            "\n".join(
                [
                    "[source]",
                    'kind = "github"',
                    'repo = "octo/widgets"',
                    'label = "flywheel"',
                    "[worker]",
                    "concurrency = 3",
                ]
            ),
        )
    )
    assert policy.source_kind == "github"
    assert policy.worker_concurrency == 3


# --- [worker] checkpoint_nudge_seconds (checkpoint-nudge wiring) -------------


def test_worker_checkpoint_nudge_defaults_when_table_absent(
    tmp_path: Path,
) -> None:
    """No [worker] table yields the default-on 300.0 threshold (the retro loss
    came from this nudge not existing, so it stays on by default)."""
    policy = load_policy(_write(tmp_path, '[source]\nkind = "directory"\n'))
    assert policy.worker_checkpoint_nudge_seconds == 300.0


def test_worker_checkpoint_nudge_defaults_when_key_absent(
    tmp_path: Path,
) -> None:
    """A [worker] table present but no checkpoint_nudge_seconds key yields the
    default 300.0."""
    policy = load_policy(
        _write(tmp_path, '[source]\nkind = "directory"\n[worker]\n')
    )
    assert policy.worker_checkpoint_nudge_seconds == 300.0


def test_worker_checkpoint_nudge_parses_float(tmp_path: Path) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            '[source]\nkind = "directory"\n'
            "[worker]\ncheckpoint_nudge_seconds = 120.5\n",
        )
    )
    assert policy.worker_checkpoint_nudge_seconds == 120.5


def test_worker_checkpoint_nudge_accepts_integer(tmp_path: Path) -> None:
    """A TOML integer coerces to float (900 -> 900.0), like other float keys."""
    policy = load_policy(
        _write(
            tmp_path,
            '[source]\nkind = "directory"\n'
            "[worker]\ncheckpoint_nudge_seconds = 900\n",
        )
    )
    assert policy.worker_checkpoint_nudge_seconds == 900.0


def test_worker_checkpoint_nudge_zero_disables(tmp_path: Path) -> None:
    """``0`` is a valid disable value that flows through as 0.0 (not the
    default): the disabled threshold is what the worker threads to the harness.
    """
    policy = load_policy(
        _write(
            tmp_path,
            '[source]\nkind = "directory"\n'
            "[worker]\ncheckpoint_nudge_seconds = 0\n",
        )
    )
    assert policy.worker_checkpoint_nudge_seconds == 0.0


def test_worker_checkpoint_nudge_rejects_non_number(tmp_path: Path) -> None:
    with pytest.raises(
        PolicyError, match="worker.checkpoint_nudge_seconds"
    ):
        load_policy(
            _write(
                tmp_path,
                '[source]\nkind = "directory"\n'
                '[worker]\ncheckpoint_nudge_seconds = "soon"\n',
            )
        )


def test_worker_checkpoint_nudge_rejects_boolean(tmp_path: Path) -> None:
    """A TOML boolean is rejected (bool is an int subclass; ``= true`` is a
    typo, never 1.0)."""
    with pytest.raises(
        PolicyError, match="worker.checkpoint_nudge_seconds"
    ):
        load_policy(
            _write(
                tmp_path,
                '[source]\nkind = "directory"\n'
                "[worker]\ncheckpoint_nudge_seconds = true\n",
            )
        )


def test_worker_checkpoint_nudge_carries_on_github_policy(
    tmp_path: Path,
) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            "\n".join(
                [
                    "[source]",
                    'kind = "github"',
                    'repo = "octo/widgets"',
                    'label = "flywheel"',
                    "[worker]",
                    "checkpoint_nudge_seconds = 42.0",
                ]
            ),
        )
    )
    assert policy.source_kind == "github"
    assert policy.worker_checkpoint_nudge_seconds == 42.0


# --- attempt budgets: [deadlines] + rubric_judge_max_turns (spec 00066) ------


def test_deadlines_absent_defaults_to_default_config(tmp_path: Path) -> None:
    # Every pre-existing policy file (no [deadlines] table) keeps the harness's
    # finite default-on ceilings, byte-identical to today.
    policy = load_policy(_write(tmp_path, '[source]\nkind = "directory"\n'))
    assert policy.deadlines == DeadlineConfig()


def test_deadlines_table_parses_and_opts_out(tmp_path: Path) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            "\n".join(
                [
                    "[source]",
                    'kind = "directory"',
                    "[deadlines]",
                    "agent_iteration_seconds = 120",
                    "rubric_judge_seconds = 0",
                ]
            ),
        )
    )
    assert policy.deadlines.agent_iteration_seconds == 120.0
    # ``0`` is the on-disk unbounded opt-out -> None, not 0.0 nor the default.
    assert policy.deadlines.rubric_judge_seconds is None
    # An omitted class keeps its finite default.
    assert policy.deadlines.command_grader_seconds == 900.0


def test_deadlines_non_numeric_value_raises(tmp_path: Path) -> None:
    # A non-numeric per-class value is a configuration error: the resolver's
    # ValueError is wrapped into a PolicyError naming the offending key.
    with pytest.raises(PolicyError, match="deadlines.agent_iteration_seconds"):
        load_policy(
            _write(
                tmp_path,
                '[source]\nkind = "directory"\n'
                '[deadlines]\nagent_iteration_seconds = "soon"\n',
            )
        )


def test_deadlines_boolean_value_raises(tmp_path: Path) -> None:
    # TOML ``true`` must not slip through as ``1.0`` (bool is an int subclass);
    # it raises, naming the key.
    with pytest.raises(PolicyError, match="deadlines.agent_iteration_seconds"):
        load_policy(
            _write(
                tmp_path,
                '[source]\nkind = "directory"\n'
                "[deadlines]\nagent_iteration_seconds = true\n",
            )
        )


def test_deadlines_non_table_raises(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match=r"\[deadlines\] must be a table"):
        load_policy(
            _write(
                tmp_path,
                'deadlines = "nope"\n[source]\nkind = "directory"\n',
            )
        )


def test_rubric_judge_max_turns_absent_defaults_none(tmp_path: Path) -> None:
    policy = load_policy(_write(tmp_path, '[source]\nkind = "directory"\n'))
    assert policy.sandbox.limits.rubric_judge_max_turns is None


def test_rubric_judge_max_turns_parses(tmp_path: Path) -> None:
    policy = load_policy(
        _write(
            tmp_path,
            '[source]\nkind = "directory"\n'
            "[sandbox.limits]\nrubric_judge_max_turns = 8\n",
        )
    )
    assert policy.sandbox.limits.rubric_judge_max_turns == 8


def test_rubric_judge_max_turns_non_positive_raises(tmp_path: Path) -> None:
    with pytest.raises(
        PolicyError, match="sandbox.limits.rubric_judge_max_turns"
    ):
        load_policy(
            _write(
                tmp_path,
                '[source]\nkind = "directory"\n'
                "[sandbox.limits]\nrubric_judge_max_turns = 0\n",
            )
        )


def test_rubric_judge_max_turns_boolean_raises(tmp_path: Path) -> None:
    # bool is an int subclass; ``true`` must not read as ``1``.
    with pytest.raises(
        PolicyError, match="sandbox.limits.rubric_judge_max_turns"
    ):
        load_policy(
            _write(
                tmp_path,
                '[source]\nkind = "directory"\n'
                "[sandbox.limits]\nrubric_judge_max_turns = true\n",
            )
        )


def test_rubric_judge_max_turns_non_int_raises(tmp_path: Path) -> None:
    with pytest.raises(
        PolicyError, match="sandbox.limits.rubric_judge_max_turns"
    ):
        load_policy(
            _write(
                tmp_path,
                '[source]\nkind = "directory"\n'
                '[sandbox.limits]\nrubric_judge_max_turns = "lots"\n',
            )
        )

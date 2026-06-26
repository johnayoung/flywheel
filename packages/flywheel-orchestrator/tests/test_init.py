"""Tests for ``flywheel init`` (delegated to the orchestrator module) and
policy-carried paths.

init scaffolds a self-contained ``.flywheel/`` layout plus a repo-root
``flywheel.toml`` whose ``[paths]`` table keeps every default off the
legacy ``.workflow/`` tree. The CLI tests run :func:`main` directly with a
chdir'd tmp cwd -- the same resolution path the unified product shell takes
when it routes ``flywheel init``.
"""

from __future__ import annotations

import io
import json
import subprocess
import sys
from pathlib import Path

import pytest

from flywheel_orchestrator import load_policy
from flywheel_orchestrator import _workflow
from flywheel_orchestrator._workflow import main


def _git_init_attached(path: Path) -> None:
    """Make ``path`` a git repo on an attached branch with one commit.

    init's git preflight (spec 00028) refuses a non-git working directory
    or a detached HEAD, so every test that runs ``main(["init"])`` must do
    so inside a valid attached-branch repo -- the state a legitimate
    adopter is in. One commit gives HEAD a non-detached attached branch.
    """
    run = lambda *args: subprocess.run(  # noqa: E731
        ["git", *args], cwd=path, check=True, capture_output=True
    )
    run("init", "-b", "main")
    run("config", "user.email", "test@flywheel.invalid")
    run("config", "user.name", "Flywheel Test")
    run("commit", "--allow-empty", "-m", "root")


@pytest.fixture
def repo(tmp_path: Path, monkeypatch) -> Path:
    _git_init_attached(tmp_path)
    monkeypatch.chdir(tmp_path)
    return tmp_path


@pytest.fixture
def no_pg_env(monkeypatch) -> None:
    """Postgres-path tests must not inherit a developer's DSN env vars."""
    monkeypatch.delenv("FLYWHEEL_PG_DSN", raising=False)
    monkeypatch.delenv("DATABASE_URL", raising=False)


class _FakeTty(io.StringIO):
    """Scripted stdin that claims to be a TTY so init prompts."""

    def isatty(self) -> bool:
        return True


def _interactive(monkeypatch, *lines: str) -> None:
    monkeypatch.setattr(
        sys, "stdin", _FakeTty("".join(f"{line}\n" for line in lines))
    )


# --- git preflight (spec 00028, criteria 1-4) --------------------------------


def test_init_git_preflight_refuses_non_repo(tmp_path: Path, monkeypatch, capsys) -> None:
    """Outside a git repository init exits non-zero and writes nothing."""
    monkeypatch.chdir(tmp_path)
    assert main(["init", "--defaults"]) == 2
    assert not (tmp_path / "flywheel.toml").exists()
    assert not (tmp_path / ".flywheel").exists()


def test_init_git_preflight_message_names_precondition(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """The non-git refusal names the missing precondition in operator
    terms: case-insensitive 'git' plus 'repository'/'repo' (criterion 2)."""
    monkeypatch.chdir(tmp_path)
    assert main(["init", "--defaults"]) == 2
    err = capsys.readouterr().err.lower()
    assert "git" in err
    assert "repository" in err or "repo" in err


def test_init_detached_head_refuses(tmp_path: Path, monkeypatch, capsys) -> None:
    """A detached HEAD refuses with a message naming 'detached'
    (criterion 3), distinct from the non-git refusal."""
    subprocess.run(["git", "init", "-b", "main"], cwd=tmp_path, check=True, capture_output=True)
    subprocess.run(
        ["git", "config", "user.email", "t@flywheel.invalid"],
        cwd=tmp_path, check=True, capture_output=True,
    )
    subprocess.run(
        ["git", "config", "user.name", "T"], cwd=tmp_path, check=True, capture_output=True
    )
    subprocess.run(
        ["git", "commit", "--allow-empty", "-m", "root"],
        cwd=tmp_path, check=True, capture_output=True,
    )
    head = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=tmp_path, check=True, capture_output=True, text=True,
    ).stdout.strip()
    subprocess.run(
        ["git", "checkout", head], cwd=tmp_path, check=True, capture_output=True
    )
    monkeypatch.chdir(tmp_path)
    assert main(["init", "--defaults"]) == 2
    assert not (tmp_path / "flywheel.toml").exists()
    assert "detached" in capsys.readouterr().err.lower()


def test_init_happy_path_attached_branch_scaffolds(repo: Path) -> None:
    """A valid attached-branch repo still scaffolds normally (criterion
    4): the gate must not block a legitimate adopter."""
    assert main(["init", "--defaults"]) == 0
    assert (repo / "flywheel.toml").is_file()
    assert load_policy(repo / "flywheel.toml").source_kind == "directory"


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
    # The scaffolded [agent] block is commented out by default, so model
    # stays unset; the worker falls back to its CLI / built-in default.
    assert policy.model is None
    # The default scaffold pins the store backend explicitly.
    assert policy.store_backend == "sqlite"
    assert policy.store_schema is None
    # init leaves the landing base UNSET so the worker FF-merges in-tree onto
    # the checked-out branch; an active base equal to the checkout would be
    # refused by the landing guard and exit the worker on startup.
    assert policy.submit_base is None


def test_init_suggests_current_branch_as_commented_base(repo: Path) -> None:
    subprocess.run(
        ["git", "switch", "-c", "integration"], cwd=repo, check=True,
        capture_output=True,
    )
    main(["init"])
    text = (repo / "flywheel.toml").read_text()
    # The detected branch is surfaced as a COMMENTED suggestion (with the
    # branch name in the explanatory comment), never an active key.
    assert "# base = \"integration\"" in text
    assert "\nbase = \"integration\"" not in text  # not active
    assert "integration" in text  # named in the guidance comment
    # Unset -> the worker falls back to the checked-out branch (the default
    # that actually works right after init).
    assert load_policy(repo / "flywheel.toml").submit_base is None


def test_init_reports_agent_auth_present(repo: Path, monkeypatch, capsys) -> None:
    monkeypatch.setenv("ANTHROPIC_API_KEY", "sk-ant-probe")
    main(["init"])
    assert "agent auth: ANTHROPIC_API_KEY is set" in capsys.readouterr().out


def test_init_warns_when_no_agent_auth(
    repo: Path, monkeypatch, tmp_path, capsys
) -> None:
    monkeypatch.delenv("ANTHROPIC_API_KEY", raising=False)
    # Point HOME at an empty dir so a real ~/.claude credential file on the
    # dev box cannot make the check pass.
    monkeypatch.setenv("HOME", str(tmp_path / "empty-home"))
    main(["init"])
    assert "no agent credentials detected" in capsys.readouterr().out


def test_init_policy_agent_example_documented(repo: Path) -> None:
    """The scaffold must show operators how to pin a model id.

    Uncommenting the example block (verbatim) must yield a loadable
    policy whose ``model`` matches the scaffolded example -- a typo in
    the example would leak silently otherwise.
    """
    main(["init"])
    text = (repo / "flywheel.toml").read_text()
    assert "# [agent]" in text
    assert '# model = "claude-sonnet-4-5"' in text

    uncommented = text.replace(
        '# [agent]\n# model = "claude-sonnet-4-5"',
        '[agent]\nmodel = "claude-sonnet-4-5"',
    )
    (repo / "flywheel.toml").write_text(uncommented)
    policy = load_policy(repo / "flywheel.toml")
    assert policy.model == "claude-sonnet-4-5"


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


# --- flags and the non-TTY defaults path (spec FR-2 / FR-3) ------------------


def test_init_non_tty_no_flags_equals_defaults_flag(
    tmp_path: Path, monkeypatch
) -> None:
    """A non-TTY stdin with no flags writes byte-identical config to
    ``--defaults`` (spec FR-3)."""
    plain = tmp_path / "plain"
    flagged = tmp_path / "flagged"
    plain.mkdir()
    flagged.mkdir()
    _git_init_attached(plain)
    _git_init_attached(flagged)

    monkeypatch.chdir(plain)
    assert main(["init"]) == 0
    monkeypatch.chdir(flagged)
    assert main(["init", "--defaults"]) == 0

    text = (plain / "flywheel.toml").read_text()
    assert text == (flagged / "flywheel.toml").read_text()
    assert "[store]" in text
    assert 'backend = "sqlite"' in text


def test_init_store_postgres_defaults_non_tty(
    repo: Path, capsys, no_pg_env
) -> None:
    """``--store postgres --defaults`` writes the backend without
    prompting and prints the env var contract (spec FR-2 / FR-4)."""
    assert main(["init", "--store", "postgres", "--defaults"]) == 0

    policy = load_policy(repo / "flywheel.toml")
    assert policy.store_backend == "postgres"
    assert policy.store_schema is None

    out = capsys.readouterr().out
    assert "FLYWHEEL_PG_DSN" in out
    assert "DATABASE_URL" in out


def test_init_pg_schema_flag_is_recorded(repo: Path, no_pg_env) -> None:
    assert (
        main(
            [
                "init",
                "--store",
                "postgres",
                "--pg-schema",
                "flywheel_ci",
                "--defaults",
            ]
        )
        == 0
    )
    policy = load_policy(repo / "flywheel.toml")
    assert policy.store_backend == "postgres"
    assert policy.store_schema == "flywheel_ci"


def test_init_pg_schema_with_sqlite_backend_errors(
    repo: Path, capsys
) -> None:
    assert main(["init", "--pg-schema", "x", "--defaults"]) == 2
    assert not (repo / "flywheel.toml").exists()
    assert "--pg-schema" in capsys.readouterr().err


def test_init_repo_flag_with_implied_directory_source_errors(
    repo: Path, capsys
) -> None:
    """``--repo``/``--label`` against a (defaulted) directory source is a
    usage error and writes no policy file."""
    assert main(["init", "--repo", "octo/widgets", "--defaults"]) == 2
    assert not (repo / "flywheel.toml").exists()
    err = capsys.readouterr().err
    assert "--repo/--label" in err
    # The scaffold dirs are still ensured (idempotent either way).
    assert (repo / ".flywheel" / "tasks" / "active" / ".gitkeep").is_file()


def test_init_github_source_via_flags(repo: Path) -> None:
    assert (
        main(
            [
                "init",
                "--source",
                "github",
                "--repo",
                "octo/widgets",
                "--label",
                "work",
                "--defaults",
            ]
        )
        == 0
    )
    policy = load_policy(repo / "flywheel.toml")
    assert policy.source_kind == "github"
    assert policy.github_repo == "octo/widgets"
    assert policy.github_label == "work"
    assert policy.github_done_action == "comment"


def test_init_invalid_repo_flag_errors(repo: Path, capsys) -> None:
    assert (
        main(
            [
                "init",
                "--source",
                "github",
                "--repo",
                "not-a-repo",
                "--defaults",
            ]
        )
        == 2
    )
    assert not (repo / "flywheel.toml").exists()
    assert "invalid repo" in capsys.readouterr().err


def test_init_github_defaults_repo_from_origin(
    repo: Path, monkeypatch
) -> None:
    monkeypatch.setattr(
        _workflow, "_github_repo_from_origin", lambda: "octo/widgets"
    )
    assert main(["init", "--source", "github", "--defaults"]) == 0
    assert load_policy(repo / "flywheel.toml").github_repo == "octo/widgets"


def test_init_github_defaults_without_origin_errors(
    repo: Path, monkeypatch, capsys
) -> None:
    monkeypatch.setattr(
        _workflow, "_github_repo_from_origin", lambda: None
    )
    assert main(["init", "--source", "github", "--defaults"]) == 2
    assert not (repo / "flywheel.toml").exists()
    assert "--repo" in capsys.readouterr().err


# --- interactive prompts (spec FR-1 / FR-10) ---------------------------------


def test_init_interactive_all_defaults_is_three_enters(
    repo: Path, monkeypatch, capsys
) -> None:
    _interactive(monkeypatch, "", "", "")
    assert main(["init"]) == 0

    out = capsys.readouterr().out
    assert "store backend ([sqlite]/postgres): " in out
    assert "work source ([directory]/github): " in out
    assert "install Claude Code skills" in out

    policy = load_policy(repo / "flywheel.toml")
    assert policy.store_backend == "sqlite"
    assert policy.source_kind == "directory"
    # The skills prompt defaults to yes, so the all-enters path installs.
    assert (repo / ".claude" / "skills" / "fw-plan" / "SKILL.md").is_file()


def test_init_interactive_invalid_choice_reprompts(
    repo: Path, monkeypatch, capsys
) -> None:
    _interactive(monkeypatch, "mysql", "sqlite", "", "n")
    assert main(["init"]) == 0
    out = capsys.readouterr().out
    assert "invalid value 'mysql'" in out
    assert load_policy(repo / "flywheel.toml").store_backend == "sqlite"


def test_init_interactive_postgres_github_flow(
    repo: Path, monkeypatch, capsys, no_pg_env
) -> None:
    monkeypatch.setattr(
        _workflow, "_github_repo_from_origin", lambda: None
    )
    _interactive(
        monkeypatch,
        "postgres",  # store backend
        "flywheel_ci",  # postgres schema
        "github",  # work source
        "octo/widgets",  # repo (no origin default)
        "",  # label -> flywheel
        "close",  # done action
        "n",  # skills -> no
    )
    assert main(["init"]) == 0

    out = capsys.readouterr().out
    assert "postgres schema [none]: " in out
    assert "FLYWHEEL_PG_DSN" in out

    policy = load_policy(repo / "flywheel.toml")
    assert policy.store_backend == "postgres"
    assert policy.store_schema == "flywheel_ci"
    assert policy.source_kind == "github"
    assert policy.github_repo == "octo/widgets"
    assert policy.github_label == "flywheel"
    assert policy.github_done_action == "close"


def test_init_interactive_repo_prompt_prefills_origin(
    repo: Path, monkeypatch, capsys
) -> None:
    monkeypatch.setattr(
        _workflow, "_github_repo_from_origin", lambda: "octo/widgets"
    )
    _interactive(monkeypatch, "", "github", "", "", "", "n")
    assert main(["init"]) == 0
    out = capsys.readouterr().out
    assert "github repo (owner/name) [octo/widgets]: " in out
    assert load_policy(repo / "flywheel.toml").github_repo == "octo/widgets"


def test_init_interactive_invalid_repo_reprompts(
    repo: Path, monkeypatch, capsys
) -> None:
    monkeypatch.setattr(
        _workflow, "_github_repo_from_origin", lambda: None
    )
    _interactive(
        monkeypatch, "", "github", "not-a-repo", "octo/widgets", "", "", "n"
    )
    assert main(["init"]) == 0
    out = capsys.readouterr().out
    assert "invalid repo 'not-a-repo'" in out
    assert load_policy(repo / "flywheel.toml").github_repo == "octo/widgets"


def test_init_interactive_flag_suppresses_its_prompt(
    repo: Path, monkeypatch, capsys, no_pg_env
) -> None:
    """``--store postgres`` on a TTY skips the backend prompt but still
    asks the remaining questions."""
    # schema -> none, source -> directory, skills -> no
    _interactive(monkeypatch, "", "", "n")
    assert main(["init", "--store", "postgres"]) == 0
    out = capsys.readouterr().out
    assert "store backend" not in out
    assert "postgres schema [none]: " in out
    assert load_policy(repo / "flywheel.toml").store_backend == "postgres"


# --- reconfiguring an existing flywheel.toml (spec FR-9) ----------------------


_HAND_TUNED_POLICY = """\
# Hand-tuned policy: every key here must survive a reconfigure.
[source]
kind = "directory"
tasks_dir = "custom/tasks"

[paths]
db = "custom/state.sqlite"
sandbox_root = "custom/sandboxes"

[[defaults.graders]]
type = "command"
run = "uv run pytest -q"

[agent]
model = "claude-opus-4-7"
"""


def test_init_reconfigure_decline_leaves_file_byte_identical(
    repo: Path, monkeypatch, capsys
) -> None:
    """Interactive init shows the current settings; declining (the
    default, plain enter) leaves the file byte-identical."""
    (repo / "flywheel.toml").write_text(_HAND_TUNED_POLICY)
    before = (repo / "flywheel.toml").read_bytes()
    _interactive(monkeypatch, "")  # enter -> default no

    assert main(["init"]) == 0

    assert (repo / "flywheel.toml").read_bytes() == before
    out = capsys.readouterr().out
    assert "store backend = sqlite, work source = directory" in out
    assert "reconfigure? (y/[n]): " in out
    assert "exists:  flywheel.toml (left untouched)" in out
    # The scaffold dirs are still ensured.
    assert (repo / ".flywheel" / "tasks" / "active" / ".gitkeep").is_file()


def test_init_reconfigure_round_trip_preserves_hand_tuned_keys(
    repo: Path, monkeypatch, capsys, no_pg_env
) -> None:
    """FR-9 acceptance: only the answered keys change; [agent],
    [[defaults.graders]], [paths], and the unanswered [source] tasks_dir
    all survive the round trip."""
    (repo / "flywheel.toml").write_text(_HAND_TUNED_POLICY)
    _interactive(
        monkeypatch,
        "y",  # reconfigure
        "postgres",  # store backend
        "flywheel_ci",  # postgres schema
        "",  # work source -> directory (default)
        "n",  # skills -> no
    )

    assert main(["init"]) == 0
    assert "updated: flywheel.toml" in capsys.readouterr().out

    text = (repo / "flywheel.toml").read_text()
    # Sections the prompts do not own survive verbatim, comments included.
    assert "# Hand-tuned policy" in text
    assert 'model = "claude-opus-4-7"' in text
    assert 'run = "uv run pytest -q"' in text
    assert 'db = "custom/state.sqlite"' in text
    assert 'sandbox_root = "custom/sandboxes"' in text
    # The unanswered [source] key survives too.
    assert 'tasks_dir = "custom/tasks"' in text

    policy = load_policy(repo / "flywheel.toml")
    assert policy.store_backend == "postgres"
    assert policy.store_schema == "flywheel_ci"
    assert policy.source_kind == "directory"
    assert policy.tasks_dir == Path("custom/tasks")
    assert policy.model == "claude-opus-4-7"
    assert len(policy.default_graders) == 1


def test_init_reconfigure_pre_store_config_gains_store_section(
    repo: Path, monkeypatch
) -> None:
    """A flywheel.toml predating [store] gains the section on reconfigure
    without disturbing any other key."""
    (repo / "flywheel.toml").write_text(
        '[source]\n'
        'kind = "directory"\n'
        'tasks_dir = ".flywheel/tasks"\n'
        '\n'
        '[agent]\n'
        'model = "claude-opus-4-7"\n'
    )
    # reconfigure, sqlite, directory, no skills
    _interactive(monkeypatch, "y", "", "", "n")

    assert main(["init"]) == 0

    text = (repo / "flywheel.toml").read_text()
    assert "[store]" in text
    assert 'model = "claude-opus-4-7"' in text
    policy = load_policy(repo / "flywheel.toml")
    assert policy.store_backend == "sqlite"
    assert policy.model == "claude-opus-4-7"


def test_init_reconfigure_github_to_directory_drops_github_keys(
    repo: Path, monkeypatch
) -> None:
    """Switching source kind github -> directory drops repo/label/
    done_action (invalid for directory) from [source]."""
    (repo / "flywheel.toml").write_text(
        '[source]\n'
        'kind = "github"\n'
        'repo = "octo/widgets"\n'
        'label = "flywheel"\n'
        'done_action = "comment"\n'
        '\n'
        '[store]\n'
        'backend = "sqlite"\n'
    )
    _interactive(monkeypatch, "y", "", "directory", "n")

    assert main(["init"]) == 0

    text = (repo / "flywheel.toml").read_text()
    assert "repo =" not in text
    assert "label =" not in text
    assert "done_action =" not in text
    policy = load_policy(repo / "flywheel.toml")
    assert policy.source_kind == "directory"
    assert policy.tasks_dir == Path(".flywheel/tasks")


def test_init_reconfigure_malformed_toml_reports_and_leaves_alone(
    repo: Path, monkeypatch, capsys
) -> None:
    (repo / "flywheel.toml").write_text("[source\nkind =\n")
    before = (repo / "flywheel.toml").read_bytes()
    _interactive(monkeypatch, "y")

    assert main(["init"]) == 2

    assert (repo / "flywheel.toml").read_bytes() == before
    assert "not valid TOML" in capsys.readouterr().err


def test_init_defaults_flag_never_touches_existing_policy(
    repo: Path, monkeypatch, capsys
) -> None:
    """--defaults (and equally a non-TTY stdin) keeps the historical
    never-touch behavior: no reconfigure prompt, file untouched."""
    (repo / "flywheel.toml").write_text(_HAND_TUNED_POLICY)
    before = (repo / "flywheel.toml").read_bytes()
    _interactive(monkeypatch, "y", "y", "y")  # would answer prompts if asked

    assert main(["init", "--defaults"]) == 0

    assert (repo / "flywheel.toml").read_bytes() == before
    out = capsys.readouterr().out
    assert "exists:  flywheel.toml (left untouched)" in out
    assert "reconfigure?" not in out


# --- aborting mid-prompts never leaves a partial policy file -----------------


def test_init_eof_mid_prompts_writes_no_policy_file(
    repo: Path, monkeypatch, capsys
) -> None:
    _interactive(monkeypatch, "postgres")  # stdin ends before later prompts
    assert main(["init"]) == 1
    assert not (repo / "flywheel.toml").exists()
    assert "not written" in capsys.readouterr().err
    assert (repo / ".flywheel" / "tasks" / "active" / ".gitkeep").is_file()


def test_init_ctrl_c_mid_prompts_writes_no_policy_file(
    repo: Path, monkeypatch, capsys
) -> None:
    class _InterruptingTty(io.StringIO):
        def isatty(self) -> bool:
            return True

        def readline(self, *args: object) -> str:
            raise KeyboardInterrupt

    monkeypatch.setattr(sys, "stdin", _InterruptingTty())
    assert main(["init"]) == 130
    assert not (repo / "flywheel.toml").exists()
    assert "not written" in capsys.readouterr().err


# --- postgres DSN validation (spec FR-4 / FR-5) -------------------------------


def test_init_postgres_unreachable_dsn_blocks_and_writes_nothing(
    repo: Path, monkeypatch, capsys
) -> None:
    """An unreachable DSN is a blocking preflight failure: init exits
    non-zero and writes no policy, so a broken target never yields a
    config that looks ready. The DSN and password stay redacted."""
    pytest.importorskip("psycopg")
    dsn = "postgresql://flywheel:secretpw@127.0.0.1:1/flywheel"
    monkeypatch.setenv("FLYWHEEL_PG_DSN", dsn)
    monkeypatch.delenv("DATABASE_URL", raising=False)

    assert main(["init", "--store", "postgres", "--defaults"]) == 2

    captured = capsys.readouterr()
    assert "[BLOCK] connection" in captured.out
    assert "preflight found blocking issues" in captured.err
    assert not (repo / "flywheel.toml").exists()
    # The DSN and its password never appear anywhere (spec FR-4).
    for stream in (captured.out, captured.err):
        assert dsn not in stream
        assert "secretpw" not in stream


def test_init_postgres_unreachable_dsn_allow_unverified_writes(
    repo: Path, monkeypatch, capsys
) -> None:
    """--allow-unverified downgrades the block to a warning: the report
    still prints (redacted), but the policy is scaffolded and exit is 0."""
    pytest.importorskip("psycopg")
    dsn = "postgresql://flywheel:secretpw@127.0.0.1:1/flywheel"
    monkeypatch.setenv("FLYWHEEL_PG_DSN", dsn)
    monkeypatch.delenv("DATABASE_URL", raising=False)

    assert (
        main(["init", "--store", "postgres", "--defaults", "--allow-unverified"])
        == 0
    )

    captured = capsys.readouterr()
    assert "[BLOCK] connection" in captured.out
    for stream in (captured.out, captured.err):
        assert dsn not in stream
        assert "secretpw" not in stream
    text = (repo / "flywheel.toml").read_text()
    assert dsn not in text
    assert "secretpw" not in text
    assert load_policy(repo / "flywheel.toml").store_backend == "postgres"


def test_init_postgres_dsn_priority_flywheel_var_wins(
    repo: Path, monkeypatch
) -> None:
    pytest.importorskip("psycopg")
    monkeypatch.setenv("FLYWHEEL_PG_DSN", "postgresql://primary/db")
    monkeypatch.setenv("DATABASE_URL", "postgresql://fallback/db")
    from flywheel_orchestrator._pg_preflight import PreflightOutcome

    seen: list[str] = []

    def fake(dsn: str, schema: str, **kwargs: object) -> PreflightOutcome:
        seen.append(dsn)
        return PreflightOutcome(checks=[])

    monkeypatch.setattr(_workflow, "run_postgres_preflight", fake)
    assert main(["init", "--store", "postgres", "--defaults"]) == 0
    assert seen == ["postgresql://primary/db"]

"""Repo-owned work policy: ``flywheel.toml``.

The policy file is the consumer repo's versioned contract with the
orchestrator — *where work comes from* and *what "runnable" means by
default* — so switching a project between a task directory and an issue
tracker is a committed config change, not a flywheel code change.

Format (TOML, stdlib ``tomllib``)::

    [source]
    kind = "directory"              # or "github"
    tasks_dir = ".flywheel/tasks"   # directory kind (optional, default shown)

    # github kind:
    # repo = "owner/name"           # required
    # label = "flywheel"            # required: only issues with this label
    # done_action = "comment"       # or "close" (default "comment")

    # Where runtime state lives (optional). CLI flags still win; without
    # these the built-in .flywheel/ defaults apply.
    [paths]
    db = ".flywheel/flywheel.sqlite"
    sandbox_root = ".flywheel/sandboxes"

    # Default grader policy: applied by tracker-backed sources to items
    # that do not declare their own graders. Directory tasks always carry
    # graders (the task schema requires them), so this section is only
    # meaningful for tracker kinds.
    [[defaults.graders]]
    type = "command"
    run = "uv run pytest"

    # Agent runtime settings (optional). The model id is an opaque string
    # passed verbatim to the SDK; flywheel does not maintain an allowlist.
    [agent]
    model = "claude-sonnet-4-5"

    # Store backend (optional). Absent section means sqlite, so every
    # pre-existing flywheel.toml keeps loading unchanged.
    [store]
    backend = "sqlite"              # or "postgres"
    # schema = "flywheel_ci"        # postgres only; optional

    # Sandbox provisioning (optional). setup runs (shell) inside every
    # newly created sandbox before the agent enters — dependency install,
    # codegen — so tasks never pay discovery cost for a bare worktree.
    # Reused parked sandboxes skip it (their environment survived).
    [sandbox]
    setup = "uv sync"

    # Landing policy (optional). strategy picks how DONE work leaves the
    # loop: "merge" FF-merges the task branch into the worker's base (full
    # autonomy, the default); "pr" pushes the branch and opens a pull
    # request with grader receipts in the body — review/CI own the merge.
    # protected_paths are glob patterns (PurePath.full_match semantics,
    # ** crosses directories) matched against every repo-relative path a
    # finished task's branch touches; any match refuses the landing and
    # parks the work. Protects the verification surface itself — grader
    # configs, CI, harness state — from being rewritten by the work it is
    # supposed to judge.
    [submit]
    strategy = "merge"              # or "pr"
    protected_paths = [".github/**", "flywheel.toml"]
    # remote = "origin"             # pr strategy: push target
    # pr_base = "main"              # pr strategy: PR base branch
    #                               # (default: the worker's base branch)

The defaults keep flywheel's readiness gate mechanical without forcing
every ticket author to write graders: an issue with no graders and no
default policy is not runnable and never reaches the scheduler.
"""

from __future__ import annotations

import tomllib
from dataclasses import dataclass
from pathlib import Path

from flywheel_core.loaders import TaskLoadError, load_graders
from flywheel_core.task import Grader

from flywheel_orchestrator._github import GithubWorkSource
from flywheel_orchestrator._source_registry import SOURCES
from flywheel_orchestrator._sources import DirectoryWorkSource, WorkSource

DEFAULT_POLICY_FILENAME = "flywheel.toml"

# Mirrors the names registered in ``_source_registry.SOURCES``; kept here for
# load-time policy validation (``_optional_source`` rejects an unknown kind
# before any work source is built). The registry owns construction dispatch.
_SOURCE_KINDS: tuple[str, ...] = ("directory", "github")

_DONE_ACTIONS: tuple[str, ...] = ("comment", "close")

_STORE_BACKENDS: tuple[str, ...] = ("sqlite", "postgres")


class PolicyError(ValueError):
    """Raised when a policy file is missing, unparseable, or invalid.

    The message always identifies the offending file (and key, where
    applicable) so the operator's fix is unambiguous.
    """


@dataclass(frozen=True, kw_only=True)
class WorkPolicy:
    """Parsed, validated ``flywheel.toml`` contents.

    ``tasks_dir`` is populated for ``kind = "directory"``;
    ``github_repo``/``github_label``/``github_done_action`` for
    ``kind = "github"``. ``default_graders`` is empty when the file
    declares none. ``db_path``/``sandbox_root`` mirror the optional
    ``[paths]`` table and are ``None`` when unset (the CLI then falls
    back to its built-in defaults). ``model`` mirrors the optional
    ``[agent] model`` key -- an opaque, repo-pinned model id passed
    verbatim to the SDK; ``None`` when unset (the worker then falls back
    to its CLI flag / built-in default). ``store_backend``/``store_schema``
    mirror the optional ``[store]`` table; an absent section means sqlite
    with no schema, so every pre-existing policy file keeps loading
    unchanged. ``protected_paths`` mirrors the optional
    ``[submit] protected_paths`` list; empty when unset (no merge-time
    path gate). ``submit_strategy``/``submit_remote``/``submit_pr_base``
    mirror the rest of the optional ``[submit]`` table; an absent table
    means the historical merge landing. ``sandbox_setup`` mirrors the
    optional ``[sandbox] setup`` command; ``None`` when unset (new
    sandboxes are used bare).
    """

    source_kind: str
    tasks_dir: Path | None = None
    github_repo: str | None = None
    github_label: str | None = None
    github_done_action: str = "comment"
    default_graders: tuple[Grader, ...] = ()
    db_path: Path | None = None
    sandbox_root: Path | None = None
    model: str | None = None
    store_backend: str = "sqlite"
    store_schema: str | None = None
    protected_paths: tuple[str, ...] = ()
    submit_strategy: str = "merge"
    submit_remote: str = "origin"
    submit_pr_base: str | None = None
    sandbox_setup: str | None = None


def load_policy(path: Path) -> WorkPolicy:
    """Parse and validate the policy file at ``path``."""
    try:
        text = path.read_bytes()
    except OSError as exc:
        raise PolicyError(f"{path}: cannot read policy file: {exc}") from exc
    try:
        data = tomllib.loads(text.decode("utf-8"))
    except (tomllib.TOMLDecodeError, UnicodeDecodeError) as exc:
        raise PolicyError(f"{path}: invalid TOML: {exc}") from exc

    source = data.get("source")
    if not isinstance(source, dict):
        raise PolicyError(f"{path}: missing required [source] table")
    kind = source.get("kind")
    if kind not in _SOURCE_KINDS:
        raise PolicyError(
            f"{path}: source.kind must be one of {_SOURCE_KINDS}, "
            f"got {kind!r}"
        )

    defaults = data.get("defaults") or {}
    if not isinstance(defaults, dict):
        raise PolicyError(f"{path}: [defaults] must be a table")
    raw_graders = defaults.get("graders", [])
    try:
        default_graders = tuple(
            load_graders(list(raw_graders), source=f"{path}: defaults")
        )
    except TaskLoadError as exc:
        raise PolicyError(str(exc)) from exc

    paths = data.get("paths") or {}
    if not isinstance(paths, dict):
        raise PolicyError(f"{path}: [paths] must be a table")
    db_path = _optional_path(paths, "db", policy_file=path)
    sandbox_root = _optional_path(paths, "sandbox_root", policy_file=path)

    agent = data.get("agent") or {}
    if not isinstance(agent, dict):
        raise PolicyError(f"{path}: [agent] must be a table")
    model = _optional_agent_model(agent, policy_file=path)

    store = data.get("store") or {}
    if not isinstance(store, dict):
        raise PolicyError(f"{path}: [store] must be a table")
    store_backend, store_schema = _optional_store(store, policy_file=path)

    submit = data.get("submit") or {}
    if not isinstance(submit, dict):
        raise PolicyError(f"{path}: [submit] must be a table")
    protected_paths = _optional_protected_paths(submit, policy_file=path)
    submit_strategy, submit_remote, submit_pr_base = _optional_submit_strategy(
        submit, policy_file=path
    )

    sandbox = data.get("sandbox") or {}
    if not isinstance(sandbox, dict):
        raise PolicyError(f"{path}: [sandbox] must be a table")
    sandbox_setup = _optional_sandbox_setup(sandbox, policy_file=path)

    if kind == "directory":
        raw_dir = source.get("tasks_dir")
        if raw_dir is not None and not isinstance(raw_dir, str):
            raise PolicyError(f"{path}: source.tasks_dir must be a string")
        # The historical default lives in _workflow; duplicated here as a
        # late import to keep this module's import graph minimal.
        from flywheel_orchestrator._workflow import DEFAULT_TASKS_DIR

        return WorkPolicy(
            source_kind="directory",
            tasks_dir=Path(raw_dir) if raw_dir else DEFAULT_TASKS_DIR,
            default_graders=default_graders,
            db_path=db_path,
            sandbox_root=sandbox_root,
            model=model,
            store_backend=store_backend,
            store_schema=store_schema,
            protected_paths=protected_paths,
            submit_strategy=submit_strategy,
            submit_remote=submit_remote,
            submit_pr_base=submit_pr_base,
            sandbox_setup=sandbox_setup,
        )

    repo = source.get("repo")
    label = source.get("label")
    if not isinstance(repo, str) or not repo:
        raise PolicyError(
            f"{path}: source.repo is required for kind = \"github\""
        )
    if not isinstance(label, str) or not label:
        raise PolicyError(
            f"{path}: source.label is required for kind = \"github\""
        )
    done_action = source.get("done_action", "comment")
    if done_action not in _DONE_ACTIONS:
        raise PolicyError(
            f"{path}: source.done_action must be one of {_DONE_ACTIONS}, "
            f"got {done_action!r}"
        )
    return WorkPolicy(
        source_kind="github",
        github_repo=repo,
        github_label=label,
        github_done_action=done_action,
        default_graders=default_graders,
        db_path=db_path,
        sandbox_root=sandbox_root,
        model=model,
        store_backend=store_backend,
        store_schema=store_schema,
        protected_paths=protected_paths,
        submit_strategy=submit_strategy,
        submit_remote=submit_remote,
        submit_pr_base=submit_pr_base,
        sandbox_setup=sandbox_setup,
    )


def _optional_path(
    table: dict, key: str, *, policy_file: Path
) -> Path | None:
    value = table.get(key)
    if value is None:
        return None
    if not isinstance(value, str) or not value:
        raise PolicyError(
            f"{policy_file}: paths.{key} must be a non-empty string"
        )
    return Path(value)


def _optional_agent_model(
    table: dict, *, policy_file: Path
) -> str | None:
    """Validate and return the optional ``agent.model`` string.

    Returns ``None`` when the key is absent so an unconfigured policy
    leaves :attr:`WorkPolicy.model` at its default. A non-string value, or
    a string that is empty or whitespace-only, raises :class:`PolicyError`
    so a typo never silently degrades into "no model pinned." No
    allowlist is enforced -- the value is an opaque identifier passed
    through verbatim to the SDK.
    """
    value = table.get("model")
    if value is None:
        return None
    if not isinstance(value, str) or not value.strip():
        raise PolicyError(
            f"{policy_file}: agent.model must be a non-empty string"
        )
    return value


def _optional_store(
    table: dict, *, policy_file: Path
) -> tuple[str, str | None]:
    """Validate and return the optional ``[store]`` table contents.

    Returns ``("sqlite", None)`` when the section (or the ``backend``
    key) is absent so every pre-existing policy file keeps loading
    unchanged. An unknown backend, or a ``schema`` that is not a
    non-empty string, raises :class:`PolicyError` so a typo never
    silently degrades into the sqlite default.
    """
    backend = table.get("backend", "sqlite")
    if backend not in _STORE_BACKENDS:
        raise PolicyError(
            f"{policy_file}: store.backend must be one of "
            f"{_STORE_BACKENDS}, got {backend!r}"
        )
    schema = table.get("schema")
    if schema is None:
        return backend, None
    if not isinstance(schema, str) or not schema.strip():
        raise PolicyError(
            f"{policy_file}: store.schema must be a non-empty string"
        )
    return backend, schema


def _optional_protected_paths(
    table: dict, *, policy_file: Path
) -> tuple[str, ...]:
    """Validate and return the optional ``submit.protected_paths`` list.

    Returns ``()`` when the key is absent so an unconfigured policy has no
    merge-time path gate. A non-list value, or any entry that is not a
    non-empty string, raises :class:`PolicyError` so a typo never silently
    degrades into "nothing protected."
    """
    value = table.get("protected_paths")
    if value is None:
        return ()
    if not isinstance(value, list) or not all(
        isinstance(item, str) and item.strip() for item in value
    ):
        raise PolicyError(
            f"{policy_file}: submit.protected_paths must be a list of "
            f"non-empty strings"
        )
    return tuple(value)


_SUBMIT_STRATEGIES: tuple[str, ...] = ("merge", "pr")


def _optional_submit_strategy(
    table: dict, *, policy_file: Path
) -> tuple[str, str, str | None]:
    """Validate and return ``(strategy, remote, pr_base)`` from ``[submit]``.

    Absent keys mean the historical merge landing (``("merge", "origin",
    None)``) so every pre-existing policy file keeps loading unchanged.
    An unknown strategy, or a remote/pr_base that is not a non-empty
    string, raises :class:`PolicyError`.
    """
    strategy = table.get("strategy", "merge")
    if strategy not in _SUBMIT_STRATEGIES:
        raise PolicyError(
            f"{policy_file}: submit.strategy must be one of "
            f"{_SUBMIT_STRATEGIES}, got {strategy!r}"
        )
    remote = table.get("remote", "origin")
    if not isinstance(remote, str) or not remote.strip():
        raise PolicyError(
            f"{policy_file}: submit.remote must be a non-empty string"
        )
    pr_base = table.get("pr_base")
    if pr_base is not None and (
        not isinstance(pr_base, str) or not pr_base.strip()
    ):
        raise PolicyError(
            f"{policy_file}: submit.pr_base must be a non-empty string"
        )
    return strategy, remote, pr_base


def _optional_sandbox_setup(
    table: dict, *, policy_file: Path
) -> str | None:
    """Validate and return the optional ``sandbox.setup`` command.

    Returns ``None`` when the key is absent so an unconfigured policy
    provisions sandboxes bare. A non-string or empty/whitespace-only value
    raises :class:`PolicyError` so a typo never silently degrades into
    "no setup."
    """
    value = table.get("setup")
    if value is None:
        return None
    if not isinstance(value, str) or not value.strip():
        raise PolicyError(
            f"{policy_file}: sandbox.setup must be a non-empty string"
        )
    return value


def build_directory_source(policy: WorkPolicy) -> WorkSource:
    """Build the directory backend (the registry's ``directory`` target)."""
    assert policy.tasks_dir is not None  # load_policy guarantees it
    return DirectoryWorkSource(policy.tasks_dir)


def build_github_source(policy: WorkPolicy) -> WorkSource:
    """Build the GitHub-issues backend (the registry's ``github`` target)."""
    assert policy.github_repo is not None and policy.github_label is not None
    return GithubWorkSource(
        repo=policy.github_repo,
        label=policy.github_label,
        default_graders=policy.default_graders,
        done_action=policy.github_done_action,
    )


def build_work_source(policy: WorkPolicy) -> WorkSource:
    """Construct the :class:`WorkSource` a policy describes.

    Routes ``policy.source_kind`` through the
    :data:`~flywheel_orchestrator._source_registry.SOURCES` registry; the
    per-kind construction lives in :func:`build_directory_source` /
    :func:`build_github_source`.
    """
    return SOURCES.resolve(policy.source_kind)(policy)


__all__ = [
    "DEFAULT_POLICY_FILENAME",
    "PolicyError",
    "WorkPolicy",
    "build_directory_source",
    "build_github_source",
    "build_work_source",
    "load_policy",
]

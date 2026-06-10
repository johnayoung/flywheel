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
from flywheel_orchestrator._sources import DirectoryWorkSource, WorkSource

DEFAULT_POLICY_FILENAME = "flywheel.toml"

_SOURCE_KINDS: tuple[str, ...] = ("directory", "github")

_DONE_ACTIONS: tuple[str, ...] = ("comment", "close")


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
    back to its built-in defaults).
    """

    source_kind: str
    tasks_dir: Path | None = None
    github_repo: str | None = None
    github_label: str | None = None
    github_done_action: str = "comment"
    default_graders: tuple[Grader, ...] = ()
    db_path: Path | None = None
    sandbox_root: Path | None = None


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


def build_work_source(policy: WorkPolicy) -> WorkSource:
    """Construct the :class:`WorkSource` a policy describes."""
    if policy.source_kind == "directory":
        assert policy.tasks_dir is not None  # load_policy guarantees it
        return DirectoryWorkSource(policy.tasks_dir)
    assert policy.github_repo is not None and policy.github_label is not None
    return GithubWorkSource(
        repo=policy.github_repo,
        label=policy.github_label,
        default_graders=policy.default_graders,
        done_action=policy.github_done_action,
    )


__all__ = [
    "DEFAULT_POLICY_FILENAME",
    "PolicyError",
    "WorkPolicy",
    "build_work_source",
    "load_policy",
]

"""Static task-definition validation (spec 00034).

A grader is the spine of a task; an un-runnable grader is a silent hole in
the verification surface that masquerades as "not done yet." This module
flags a ``command`` grader that the harness could not even *run* — an empty
or unparseable shell command, or one referencing a repo-relative filesystem
path that does not exist — *before* a worker burns a cycle on it.

The check is STATIC (D-1): it never executes a grader. Shell-parseability is
checked with ``bash -n`` (parse only, no run); path tokens are checked with a
filesystem ``stat``. This lives here, not in the purity-enforced
``flywheel_core.task`` module, because it touches the filesystem and
``subprocess`` — the path/shell checks belong where IO is allowed.

It deliberately does NOT judge a grader that parses and runs but is
semantically wrong for its tool (e.g. a bad CLI flag): that is undecidable
statically and is named out of scope by the spec.
"""

from __future__ import annotations

import shlex
import subprocess
from dataclasses import dataclass
from pathlib import Path

from flywheel_core.task import CommandGrader, Task

# Glob metacharacters: a token still carrying one of these after the
# conservative ``/*`` / ``/`` normalization is left unchecked rather than
# risk a false positive on a pattern whose expansion we cannot resolve.
_GLOB_CHARS = ("*", "?", "[", "]")


@dataclass(frozen=True)
class TaskDefect:
    """A single static defect found in a task's definition.

    ``task_id`` names the owning task; ``detail`` is a human-readable
    description naming the specific problem (the empty/unparseable command or
    the missing path). The verb and the orchestrator render this one shape
    (spec shared invariant).
    """

    task_id: str
    detail: str


def validate_task(task: Task, *, repo_root: Path) -> list[TaskDefect]:
    """Return the static defects in ``task``'s graders (empty if none).

    Only ``command`` graders gain the new checks; transcript/rubric/manual
    graders are validated solely by their existing schema rules. For each
    command grader, in order:

    * an empty ``run`` is a defect;
    * a ``run`` that ``bash -n`` cannot parse is a defect;
    * any repo-relative path token in ``run`` that does not exist under
      ``repo_root`` is a defect (D-2's conservative heuristic).

    Never executes a grader (D-1).
    """
    defects: list[TaskDefect] = []
    for grader in task.graders:
        if not isinstance(grader, CommandGrader):
            continue
        run = grader.run
        if not run or not run.strip():
            defects.append(
                TaskDefect(
                    task_id=task.id,
                    detail="command grader 'run' is empty",
                )
            )
            continue
        if not _parses_as_shell(run):
            defects.append(
                TaskDefect(
                    task_id=task.id,
                    detail=(
                        f"command grader 'run' does not parse as a shell "
                        f"command: {run!r}"
                    ),
                )
            )
            continue
        for missing in _missing_path_tokens(run, repo_root):
            defects.append(
                TaskDefect(
                    task_id=task.id,
                    detail=(
                        f"command grader 'run' references a path that does "
                        f"not exist: {missing}"
                    ),
                )
            )
    return defects


def _parses_as_shell(run: str) -> bool:
    """Return ``True`` when ``bash -n`` accepts ``run`` (parse, never run)."""
    result = subprocess.run(
        ["bash", "-n", "-c", run],
        capture_output=True,
    )
    return result.returncode == 0


def _missing_path_tokens(run: str, repo_root: Path) -> list[str]:
    """Return the normalized repo-relative path tokens in ``run`` that are
    absent under ``repo_root`` (D-2's conservative heuristic).

    A token is a path reference only when it contains ``/``, does not start
    with ``-`` (a flag), contains no ``://`` (a URL), and is not absolute. A
    trailing ``/*`` glob segment and a trailing ``/`` are stripped; a token
    still carrying glob metacharacters after that is left unchecked. Only
    such tokens are existence-checked, so flags, URLs, and bare words are
    never flagged.
    """
    missing: list[str] = []
    try:
        tokens = shlex.split(run)
    except ValueError:
        # An unbalanced quote slipped past bash -n in some shells; treat the
        # command as having no resolvable path tokens rather than crash.
        return missing
    for token in tokens:
        if token.startswith("-"):
            continue
        if "/" not in token:
            continue
        if "://" in token:
            continue
        if Path(token).is_absolute():
            continue
        normalized = _normalize_path_token(token)
        if normalized is None:
            continue
        if not (repo_root / normalized).exists():
            missing.append(normalized)
    return missing


def _normalize_path_token(token: str) -> str | None:
    """Strip a trailing ``/*`` then a trailing ``/`` from ``token``.

    Returns ``None`` when the result still carries glob metacharacters (the
    conservative no-false-positive rule) or is empty.
    """
    if token.endswith("/*"):
        token = token[: -len("/*")]
    token = token.rstrip("/")
    if not token:
        return None
    if any(ch in token for ch in _GLOB_CHARS):
        return None
    return token

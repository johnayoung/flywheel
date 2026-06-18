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

# The shell operators ``shlex`` splits off as standalone tokens when
# ``punctuation_chars`` is enabled. Tokenizing with these makes operators their
# own tokens (a separator/pipe/redirection/substitution-paren) instead of
# gluing onto an adjacent word -- so a real path abutting an operator
# (``dir;echo``) is no longer hidden inside one token, and a process
# substitution's closing paren (``a/b)``) is no longer mistaken for part of the
# path. Matches ``shlex``'s default punctuation set.
_PUNCTUATION_CHARS = frozenset("();<>|&")

# Substitution / expansion markers: a word token carrying one of these is not a
# literal path but a command substitution (backtick) or a variable expansion
# (``${...}`` / ``$VAR``). Its real value is only known at run time, exactly
# like an unresolvable glob, so it is left unchecked rather than ``stat``-ed.
# ``$(...)`` command substitutions are handled structurally (their body is
# skipped by paren depth), so this only needs the in-word markers.
_SUBST_CHARS = ("$", "`")


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

    ``run`` is tokenized with ``shlex``'s ``punctuation_chars`` mode so shell
    operators become their own tokens rather than gluing onto an adjacent word.
    A word token is then existence-checked only when it contains ``/``, does
    not start with ``-`` (a flag), contains no ``://`` (a URL), carries no
    substitution/expansion marker (:data:`_SUBST_CHARS`), is not absolute, and
    is neither inside a ``$(...)`` / ``<(...)`` substitution body (skipped by
    paren depth) nor the target of a redirection (skipped after a ``<``/``>``
    operator). A trailing ``/*`` glob segment and a trailing ``/`` are
    stripped; a token still carrying glob metacharacters is left unchecked.

    The operator-aware tokenization is what lets a real path abutting a
    separator (``dir;echo``) still be flagged while a process substitution's
    closing paren (``a/b)``) and a redirection's output target are not
    mistaken for missing paths.
    """
    missing: list[str] = []
    lexer = shlex.shlex(run, posix=True, punctuation_chars=True)
    lexer.whitespace_split = True
    try:
        tokens = list(lexer)
    except ValueError:
        # An unbalanced quote slipped past bash -n in some shells; treat the
        # command as having no resolvable path tokens rather than crash.
        return missing

    subst_depth = 0  # inside a $(...) / <(...) / (...) group
    pending_redirect = False  # previous token was a < / > redirection operator
    for token in tokens:
        if token and all(ch in _PUNCTUATION_CHARS for ch in token):
            # A standalone shell operator. A ``(`` (bare, ``$(``, or ``<(``)
            # opens a substitution/group whose body is run-time-only; a ``)``
            # closes it. A ``<``/``>`` (but not ``<(``/``>(``) introduces a
            # redirection whose next word is an output target, not a path arg.
            if "(" in token:
                subst_depth += 1
            if ")" in token:
                subst_depth = max(0, subst_depth - 1)
            pending_redirect = ("<" in token or ">" in token) and "(" not in token
            continue
        if subst_depth > 0:
            continue
        if pending_redirect:
            pending_redirect = False
            continue
        if token.startswith("-"):
            continue
        if "/" not in token:
            continue
        if "://" in token:
            continue
        if any(ch in token for ch in _SUBST_CHARS):
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

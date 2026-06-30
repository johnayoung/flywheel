"""Static task-definition validation (spec 00034).

A grader is the spine of a task; an un-runnable grader is a silent hole in
the verification surface that masquerades as "not done yet." This module
flags a ``command`` grader the harness could not even *run* — an empty or
unparseable shell command — *before* a worker burns a cycle on it.

The check is STATIC (D-1): it never executes a grader. Shell-parseability is
checked with ``bash -n`` (parse only, no run). It lives here, not in the
purity-enforced ``flywheel_core.task`` module, because the shell check touches
``subprocess`` — IO belongs where IO is allowed.

It deliberately does NOT judge a grader that parses and runs but is
semantically wrong for its tool (e.g. a bad CLI flag): that is undecidable
statically and is named out of scope by the spec.

TABLED — the missing-path check (spec 00034 "check #3"). An earlier version
also flagged a ``command`` grader referencing a repo-relative path that did
not exist under ``repo_root``. It is removed for now because it ran
*pre-execution* and so could not tell an INPUT path (``pytest tests/typo.py``
— a real defect) from an OUTPUT path the task is about to create (``test -f
ci.yml`` for a "generate ci.yml and verify it" task — a legitimate
postcondition). The asymmetry favors not blocking: a false positive silently
strands a valid task forever, while a genuinely un-runnable path-grader merely
wastes one self-correcting cycle (it runs, fails honestly, and surfaces).
``repo_root`` is retained on :func:`validate_task` for a future revival that
discriminates input from output (e.g. validating against the post-execution
tree). The prior heuristic — a ``shlex`` punctuation-mode tokenizer that
skipped substitutions, redirections, and globs — is preserved in git history.
"""

from __future__ import annotations

import subprocess
from dataclasses import dataclass
from pathlib import Path

from flywheel_core.task import CommandGrader, Task


@dataclass(frozen=True)
class TaskDefect:
    """A single static defect found in a task's definition.

    ``task_id`` names the owning task; ``detail`` is a human-readable
    description naming the specific problem (the empty or unparseable
    command). The verb and the orchestrator render this one shape
    (spec shared invariant).
    """

    task_id: str
    detail: str


def validate_task(task: Task, *, repo_root: Path) -> list[TaskDefect]:
    """Return the static defects in ``task``'s graders (empty if none).

    Only ``command`` graders gain these checks; transcript/rubric/manual
    graders are validated solely by their existing schema rules. For each
    command grader, in order:

    * an empty ``run`` is a defect;
    * a ``run`` that ``bash -n`` cannot parse is a defect.

    Never executes a grader (D-1). ``repo_root`` is currently unused — it is
    retained for the tabled missing-path check (see the module docstring) so a
    future revival re-wires without churning callers.
    """
    del repo_root  # reserved for the tabled missing-path check (spec 00034 #3)
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
    return defects


def _parses_as_shell(run: str) -> bool:
    """Return ``True`` when ``bash -n`` accepts ``run`` (parse, never run)."""
    result = subprocess.run(
        ["bash", "-n", "-c", run],
        capture_output=True,
    )
    return result.returncode == 0

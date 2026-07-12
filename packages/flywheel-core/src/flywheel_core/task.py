import re
from dataclasses import dataclass, field
from typing import Literal
from uuid import uuid4


class ValidationError(ValueError):
    """Raised when a task or grader violates docs/task-schema.md."""


# A single path/ref segment of a task id: consumers join ``id`` into
# filesystem paths (worktree/sandbox roots) and git ref names, so each
# ``/``-separated segment is constrained to a conservative path-safe set.
_ID_SEGMENT_RE = re.compile(r"[A-Za-z0-9._-]+")


def _validate_id_path_safe(task_id: str) -> None:
    """Reject a task id that is not a relative, traversal-free path.

    ``id`` is joined into filesystem paths and git ref names downstream
    (``worktrees_dir / id``, ``sandbox_root / id``, ``flywheel/<phase>/<id>``).
    An absolute or ``..``-bearing id would let those joins escape their base
    (pathlib discards the base on an absolute right operand and never collapses
    ``..``). This is pure string validation -- no filesystem access -- so
    ``flywheel_core.task`` stays import-side-effect free.

    ``/``-separated segments are allowed (a nested id maps to a nested branch
    inside the worktree root); each segment must be non-empty, must not be the
    parent-directory marker ``..``, and must contain only ``[A-Za-z0-9._-]``
    (which also rejects backslashes, colons, and null bytes).
    """
    for segment in task_id.split("/"):
        if not segment:
            raise ValidationError(
                f"id {task_id!r} must be a relative path with no empty "
                "segments (no leading, trailing, or doubled '/')"
            )
        if segment == "..":
            raise ValidationError(
                f"id {task_id!r} must not contain a '..' path segment"
            )
        if not _ID_SEGMENT_RE.fullmatch(segment):
            raise ValidationError(
                f"id {task_id!r} segment {segment!r} must contain only "
                "[A-Za-z0-9._-]"
            )


@dataclass(kw_only=True)
class CommandGrader:
    run: str
    name: str | None = None
    type: Literal["command"] = "command"

    def __post_init__(self) -> None:
        if not isinstance(self.run, str) or not self.run:
            raise ValidationError("command grader requires a non-empty 'run'")


@dataclass(kw_only=True)
class RubricGrader:
    assertions: list[str]
    rubric: str | None = None
    name: str | None = None
    judge_model: str | None = None
    retry_on_fail: bool = True
    type: Literal["rubric"] = "rubric"

    def __post_init__(self) -> None:
        if not isinstance(self.assertions, list) or len(self.assertions) == 0:
            raise ValidationError(
                "rubric grader requires a non-empty 'assertions' list"
            )
        if self.judge_model is not None and not isinstance(self.judge_model, str):
            raise ValidationError(
                "rubric grader 'judge_model' must be a string or None"
            )
        if not isinstance(self.retry_on_fail, bool):
            raise ValidationError(
                "rubric grader 'retry_on_fail' must be a bool"
            )


@dataclass(kw_only=True)
class ManualGrader:
    instruction: str
    name: str | None = None
    type: Literal["manual"] = "manual"

    def __post_init__(self) -> None:
        if not isinstance(self.instruction, str) or not self.instruction:
            raise ValidationError(
                "manual grader requires a non-empty 'instruction'"
            )


@dataclass(kw_only=True)
class TranscriptGrader:
    max_turns: int | None = None
    max_total_tokens: int | None = None
    max_wall_seconds: float | None = None
    name: str | None = None
    type: Literal["transcript"] = "transcript"

    def __post_init__(self) -> None:
        if (
            self.max_turns is None
            and self.max_total_tokens is None
            and self.max_wall_seconds is None
        ):
            raise ValidationError(
                "transcript grader requires at least one of "
                "max_turns, max_total_tokens, max_wall_seconds"
            )


Grader = CommandGrader | RubricGrader | ManualGrader | TranscriptGrader

KNOWN_GRADER_TYPES: tuple[str, ...] = ("command", "rubric", "manual", "transcript")


@dataclass
class Context:
    relevant: list[str] = field(default_factory=list)
    references: list[str] = field(default_factory=list)
    constraints: list[str] = field(default_factory=list)
    non_goals: list[str] = field(default_factory=list)
    edge_cases: list[str] = field(default_factory=list)
    notes: str = ""


@dataclass
class TaskBudgets:
    """Optional per-task execution-budget overrides for the heavyweight tail.

    Every field defaults to ``None`` -- inherit the harness/policy value --
    so a default-constructed instance changes nothing. The seconds fields
    mirror the ``[deadlines]`` semantics: a positive number is the ceiling,
    ``0`` is the explicit unbounded opt-out for THIS task. These are harness
    knobs, not agent-facing context: a golden-record task that legitimately
    needs more than the repo-wide iteration ceiling declares it here instead
    of the operator loosening the ceiling for every task.

    ``agent_iteration_seconds`` overrides the ``AGENT_ITERATION`` wall-clock
    ceiling; ``rubric_judge_seconds`` overrides the ``RUBRIC_JUDGE`` ceiling;
    ``rubric_judge_max_turns`` overrides the judge session's turn budget.
    """

    agent_iteration_seconds: float | None = None
    rubric_judge_seconds: float | None = None
    rubric_judge_max_turns: int | None = None

    def validate(self) -> None:
        for name in ("agent_iteration_seconds", "rubric_judge_seconds"):
            value = getattr(self, name)
            if value is None:
                continue
            if isinstance(value, bool) or not isinstance(
                value, (int, float)
            ):
                raise ValidationError(
                    f"budgets.{name} must be a number of seconds"
                )
            if value < 0:
                raise ValidationError(
                    f"budgets.{name} must be >= 0 (0 opts this task out of "
                    f"the ceiling entirely)"
                )
        turns = self.rubric_judge_max_turns
        if turns is not None and (
            isinstance(turns, bool) or not isinstance(turns, int) or turns < 1
        ):
            raise ValidationError(
                "budgets.rubric_judge_max_turns must be a positive integer"
            )


def _default_id() -> str:
    return f"task-{uuid4().hex}"


@dataclass
class Task:
    goal: str
    graders: list[Grader]
    id: str = field(default_factory=_default_id)
    tags: list[str] = field(default_factory=list)
    context: Context = field(default_factory=Context)
    budgets: TaskBudgets = field(default_factory=TaskBudgets)

    def validate(self) -> None:
        if not isinstance(self.id, str) or not self.id:
            raise ValidationError("id must be a non-empty string")
        if any(ch.isspace() for ch in self.id):
            raise ValidationError(f"id {self.id!r} must not contain whitespace")
        _validate_id_path_safe(self.id)

        if not isinstance(self.goal, str) or not self.goal.strip():
            raise ValidationError("goal must be a non-empty string")

        if not isinstance(self.graders, list):
            raise ValidationError("graders must be a list")

        if not isinstance(self.budgets, TaskBudgets):
            raise ValidationError("budgets must be a TaskBudgets")
        self.budgets.validate()

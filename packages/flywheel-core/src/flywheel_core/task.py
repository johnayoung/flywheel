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


def _default_id() -> str:
    return f"task-{uuid4().hex}"


@dataclass
class Task:
    goal: str
    graders: list[Grader]
    id: str = field(default_factory=_default_id)
    tags: list[str] = field(default_factory=list)
    context: Context = field(default_factory=Context)

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

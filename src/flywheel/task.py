from dataclasses import dataclass, field
from typing import Literal
from uuid import uuid4


class ValidationError(ValueError):
    """Raised when a task or grader violates docs/task-schema.md."""


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
    type: Literal["rubric"] = "rubric"

    def __post_init__(self) -> None:
        if not isinstance(self.assertions, list) or len(self.assertions) == 0:
            raise ValidationError(
                "rubric grader requires a non-empty 'assertions' list"
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
    prerequisites: list[str] = field(default_factory=list)
    tags: list[str] = field(default_factory=list)
    context: Context = field(default_factory=Context)

    def validate(self) -> None:
        if not isinstance(self.id, str) or not self.id:
            raise ValidationError("id must be a non-empty string")
        if any(ch.isspace() for ch in self.id):
            raise ValidationError(f"id {self.id!r} must not contain whitespace")

        if not isinstance(self.goal, str) or not self.goal.strip():
            raise ValidationError("goal must be a non-empty string")

        if not isinstance(self.graders, list) or len(self.graders) == 0:
            raise ValidationError("graders must contain at least one entry")

        if self.id in self.prerequisites:
            raise ValidationError(
                f"prerequisites must not reference the task's own id {self.id!r}"
            )

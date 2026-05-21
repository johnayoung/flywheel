from dataclasses import dataclass, field
from uuid import uuid4

KNOWN_GRADER_TYPES: tuple[str, ...] = ("command", "rubric", "manual", "transcript")


class ValidationError(ValueError):
    """Raised by Task.validate() when a task violates docs/task-schema.md."""


@dataclass
class Grader:
    type: str
    name: str | None = None
    run: str | None = None
    assertions: list[str] | None = None
    rubric: str | None = None
    instruction: str | None = None
    max_turns: int | None = None
    max_total_tokens: int | None = None
    max_wall_seconds: float | None = None


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

        for idx, grader in enumerate(self.graders):
            _validate_grader(grader, idx)


def _validate_grader(grader: Grader, idx: int) -> None:
    if grader.type not in KNOWN_GRADER_TYPES:
        raise ValidationError(
            f"graders[{idx}] has unknown type {grader.type!r}; "
            f"expected one of {KNOWN_GRADER_TYPES}"
        )

    if grader.type == "command":
        if not grader.run:
            raise ValidationError(
                f"graders[{idx}] (command) requires a non-empty 'run'"
            )
    elif grader.type == "rubric":
        if not grader.assertions:
            raise ValidationError(
                f"graders[{idx}] (rubric) requires a non-empty 'assertions' list"
            )
    elif grader.type == "manual":
        if not grader.instruction:
            raise ValidationError(
                f"graders[{idx}] (manual) requires a non-empty 'instruction'"
            )
    elif grader.type == "transcript":
        if (
            grader.max_turns is None
            and grader.max_total_tokens is None
            and grader.max_wall_seconds is None
        ):
            raise ValidationError(
                f"graders[{idx}] (transcript) requires at least one of "
                f"max_turns, max_total_tokens, max_wall_seconds"
            )

from flywheel.task import (
    KNOWN_GRADER_TYPES,
    Context,
    Grader,
    Task,
    ValidationError,
)


def hello() -> str:
    return "Hello from flywheel!"


__all__ = [
    "Context",
    "Grader",
    "KNOWN_GRADER_TYPES",
    "Task",
    "ValidationError",
    "hello",
]

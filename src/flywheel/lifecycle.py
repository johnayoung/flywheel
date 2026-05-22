from dataclasses import dataclass, field
from datetime import datetime
from enum import Enum
from uuid import uuid4


class Status(str, Enum):
    PENDING = "pending"
    READY = "ready"
    RUNNING = "running"
    VALIDATING = "validating"
    FAILED_VALIDATION = "failed_validation"
    DONE = "done"
    FAILED = "failed"
    INTERRUPTED = "interrupted"


class Outcome(str, Enum):
    SUCCEEDED = "succeeded"
    VALIDATION_FAILED = "validation_failed"
    AGENT_ERROR = "agent_error"
    CANCELLED = "cancelled"
    INTERNAL_ERROR = "internal_error"


class LifecycleTransitionError(ValueError):
    """Raised when a Lifecycle transition violates docs/task-lifecycle.md."""


def _default_run_id() -> str:
    return f"run-{uuid4().hex}"


@dataclass
class Attempt:
    number: int
    started_at: datetime
    run_id: str
    ended_at: datetime | None = None
    outcome: Outcome | None = None
    agent_output: str = ""
    error: str = ""
    agent_context: dict[str, str] = field(default_factory=dict)


@dataclass
class Lifecycle:
    task_id: str
    run_id: str = field(default_factory=_default_run_id)
    worker_id: str = ""
    status: Status = Status.PENDING
    timestamps: dict[Status, datetime] = field(default_factory=dict)
    version: int = 1
    retries: int = 0
    error: str = ""
    agent_output: str = ""
    attempts: list[Attempt] = field(default_factory=list)
    session_id: str = ""
    artifacts_dir: str = ""

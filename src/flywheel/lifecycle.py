from dataclasses import dataclass, field
from datetime import datetime, timezone
from enum import Enum
from uuid import uuid4


class Status(str, Enum):
    PENDING = "pending"
    READY = "ready"
    RUNNING = "running"
    VALIDATING = "validating"
    FAILED_VALIDATION = "failed_validation"
    INTERNAL_ERROR = "internal_error"
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


_VALID_EDGES: dict[Status, frozenset[Status]] = {
    Status.PENDING: frozenset({Status.READY}),
    Status.READY: frozenset({Status.RUNNING}),
    Status.RUNNING: frozenset(
        {
            Status.VALIDATING,
            Status.FAILED,
            Status.INTERRUPTED,
            Status.INTERNAL_ERROR,
        }
    ),
    Status.VALIDATING: frozenset({Status.DONE, Status.FAILED_VALIDATION}),
    Status.FAILED_VALIDATION: frozenset({Status.READY, Status.FAILED}),
    Status.INTERNAL_ERROR: frozenset({Status.READY, Status.FAILED}),
    Status.INTERRUPTED: frozenset({Status.READY}),
    Status.DONE: frozenset(),
    Status.FAILED: frozenset(),
}

_REQUIRES_ERROR: frozenset[Status] = frozenset(
    {Status.FAILED, Status.FAILED_VALIDATION, Status.INTERNAL_ERROR}
)

_RETRY_SOURCE_STATUSES: frozenset[Status] = frozenset(
    {Status.FAILED_VALIDATION, Status.INTERNAL_ERROR}
)

_FAILED_OUTCOMES: frozenset[Outcome] = frozenset(
    {Outcome.VALIDATION_FAILED, Outcome.AGENT_ERROR, Outcome.INTERNAL_ERROR}
)


def _utcnow() -> datetime:
    return datetime.now(timezone.utc)


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

    def transition_to(
        self,
        target: Status,
        *,
        error: str = "",
        now: datetime | None = None,
    ) -> None:
        allowed = _VALID_EDGES.get(self.status, frozenset())
        if target not in allowed:
            raise LifecycleTransitionError(
                f"illegal transition {self.status.value} -> {target.value}"
            )
        if target in _REQUIRES_ERROR and not error:
            raise LifecycleTransitionError(
                f"transition to {target.value} requires a non-empty error"
            )
        is_retry_edge = (
            self.status in _RETRY_SOURCE_STATUSES and target == Status.READY
        )
        self.status = target
        self.timestamps[target] = now if now is not None else _utcnow()
        self.version += 1
        if is_retry_edge:
            self.retries += 1
            self.error = ""
        elif error:
            self.error = error

    def is_retry_eligible(self, max_retries: int) -> bool:
        return (
            self.status in _RETRY_SOURCE_STATUSES
            and self.retries < max_retries
        )

    def consecutive_failed_runs(self) -> int:
        if not self.attempts:
            return 0
        tail = self.attempts[-1]
        if tail.outcome not in _FAILED_OUTCOMES:
            return 0
        target_run_id = tail.run_id
        count = 0
        for attempt in reversed(self.attempts):
            if attempt.run_id != target_run_id:
                break
            if attempt.outcome not in _FAILED_OUTCOMES:
                break
            count += 1
        return count

from typing import TYPE_CHECKING

from flywheel_core.audit import (
    AuditLoggerHandle,
    EventHandler,
    Subscription,
    attach_logger,
    stream,
    subscribe,
)
from flywheel_core.deadline_config import (
    DEFAULT_AGENT_ITERATION_SECONDS,
    DEFAULT_AUTOPILOT_AGENT_SECONDS,
    DEFAULT_COMMAND_GRADER_SECONDS,
    DEFAULT_DEADLINE_SECONDS,
    DEFAULT_DOCKER_MANAGEMENT_SECONDS,
    DEFAULT_RUBRIC_JUDGE_SECONDS,
    DeadlineClass,
    DeadlineConfig,
    deadline_config_from_mapping,
    resolve_deadlines,
)
from flywheel_core.envelope import (
    CLOSING_FENCE,
    OPENING_FENCE,
    VALID_INTENTS,
    VALID_REQUIREMENT_TYPES,
    BlockedRequirement,
    CommandGraderRequirement,
    DuplicateEnvelope,
    EnvVarSetRequirement,
    EnvelopeResult,
    FileExistsRequirement,
    Intent,
    MalformedEnvelope,
    MissingEnvelope,
    TruncatedEnvelope,
    ValidEnvelope,
    parse_envelope,
)
from flywheel_core.events import (
    GATE_EXCERPT_MAX_BYTES,
    AttemptFinalized,
    AttemptStarted,
    Blocked,
    CommandApplied,
    DomainEvent,
    DomainEventKind,
    EventReplayError,
    GateGraderReceipt,
    GraderEvaluated,
    HeldOutGateEvaluated,
    LandingParked,
    LifecycleInitialized,
    RetryScheduled,
    SessionRecorded,
    TransitionedTo,
    Unblocked,
    apply,
    replay,
)
from flywheel_core.faults import (
    BackoffPolicy,
    DEFAULT_BACKOFF,
    FaultClass,
    SessionLimitReset,
    classify_fault,
    derive_session_limit_reset,
    wait_backoff,
)
from flywheel_core.grader_command import (
    DEFAULT_TAIL_BYTES,
    run_command_graders,
)
from flywheel_core.grader_transcript import (
    BreachedField,
    TranscriptCounter,
    TranscriptObservation,
    enforce_transcript_limits,
    first_breach,
    run_transcript_graders,
    total_tokens_from_usage,
)
from flywheel_core.harness import (
    HarnessConfig,
    HarnessOutcome,
    HarnessStore,
    InvocationRequest,
    InvokeFunc,
    RecheckOutcome,
    ResolveApprovalOutcome,
    recheck_blocked_lifecycle,
    resolve_manual_approval,
    run_task,
)
from flywheel_core.invoker import (
    InvocationFailure,
    InvocationSignals,
    IterationResult,
    ToolInteraction,
    ToolResultObservation,
    invoke_iteration,
)
from flywheel_core.lifecycle import (
    Attempt,
    Lifecycle,
    LifecycleTransitionError,
    Outcome,
    Status,
)
from flywheel_core.loaders import (
    TaskLoadError,
    load_graders,
    load_task_data,
    load_task_directory,
    load_task_file,
    load_tasks_jsonl,
)
from flywheel_core.notifier import RunNotifier
from flywheel_core.prompt import IterationInputs, build_iteration_prompt
from flywheel_core.store_memory import InMemoryStore
from flywheel_core.store_sqlite import SqliteStore
from flywheel_core.strategy import (
    NoOpStrategy,
    Strategy,
    StrategyContext,
    StrategyResult,
    derive_slug,
)
from flywheel_core.store_protocols import (
    CURRENT_SCHEMA_VERSION,
    AttemptStore,
    AuditRecord,
    ControlCommandRecord,
    ControlCommandStore,
    DegradedSpaceRecord,
    DomainEventStore,
    EventRecord,
    GraderResultRecord,
    GraderResultStore,
    GraderType,
    LifecycleAlreadyExistsError,
    LifecycleNotFoundError,
    LifecycleStore,
    OptimisticConcurrencyError,
    SchemaMismatchError,
    SdkMessageRecord,
    SpendSummary,
    StoreConflictError,
    StoreSchemaError,
    TaskStore,
    TelemetryRecord,
    TelemetrySink,
)
from flywheel_core.telemetry_file import (
    DEFAULT_LOGS_ROOT,
    FileTelemetrySink,
)
from flywheel_core.task import (
    KNOWN_GRADER_TYPES,
    CommandGrader,
    Context,
    Grader,
    ManualGrader,
    RubricGrader,
    Task,
    TranscriptGrader,
    ValidationError,
)
from flywheel_core.validation import (
    TaskDefect,
    validate_task,
)

if TYPE_CHECKING:
    # Surfaces ``PostgresStore`` to type-checkers without triggering the
    # ``flywheel[postgres]`` import guard at package import time. The
    # runtime hook is ``__getattr__`` below; it only loads psycopg when a
    # caller actually asks for ``PostgresStore``.
    from flywheel_core.store_postgres import PostgresStore as PostgresStore


def __getattr__(name: str) -> object:
    """Lazy re-export hook.

    Resolves attribute lookups that aren't bound at module import time.
    Currently only ``PostgresStore`` uses this path so the ``postgres``
    extra stays optional: ``from flywheel_core import PostgresStore`` triggers
    the import of ``flywheel_core.store_postgres``, which raises its own
    ``ImportError`` (naming the extra) when ``psycopg`` is absent.
    """
    if name == "PostgresStore":
        from flywheel_core.store_postgres import PostgresStore as _PG

        return _PG
    raise AttributeError(f"module 'flywheel_core' has no attribute {name!r}")


def hello() -> str:
    return "Hello from flywheel!"


__all__ = [
    "Attempt",
    "GATE_EXCERPT_MAX_BYTES",
    "GateGraderReceipt",
    "GraderEvaluated",
    "HeldOutGateEvaluated",
    "LandingParked",
    "AttemptFinalized",
    "AttemptStarted",
    "AttemptStore",
    "AuditLoggerHandle",
    "AuditRecord",
    "BackoffPolicy",
    "Blocked",
    "BlockedRequirement",
    "BreachedField",
    "CLOSING_FENCE",
    "CURRENT_SCHEMA_VERSION",
    "CommandApplied",
    "CommandGrader",
    "CommandGraderRequirement",
    "Context",
    "ControlCommandRecord",
    "ControlCommandStore",
    "DEFAULT_AGENT_ITERATION_SECONDS",
    "DEFAULT_BACKOFF",
    "DEFAULT_AUTOPILOT_AGENT_SECONDS",
    "DEFAULT_COMMAND_GRADER_SECONDS",
    "DEFAULT_DEADLINE_SECONDS",
    "DEFAULT_DOCKER_MANAGEMENT_SECONDS",
    "DEFAULT_LOGS_ROOT",
    "DEFAULT_RUBRIC_JUDGE_SECONDS",
    "DEFAULT_TAIL_BYTES",
    "DeadlineClass",
    "DeadlineConfig",
    "DegradedSpaceRecord",
    "DomainEvent",
    "DomainEventKind",
    "DomainEventStore",
    "DuplicateEnvelope",
    "EnvVarSetRequirement",
    "EnvelopeResult",
    "EventHandler",
    "EventRecord",
    "EventReplayError",
    "FaultClass",
    "FileExistsRequirement",
    "FileTelemetrySink",
    "Grader",
    "GraderResultRecord",
    "GraderResultStore",
    "GraderType",
    "HarnessConfig",
    "HarnessOutcome",
    "HarnessStore",
    "InMemoryStore",
    "Intent",
    "InvocationFailure",
    "InvocationRequest",
    "InvocationSignals",
    "InvokeFunc",
    "IterationInputs",
    "IterationResult",
    "KNOWN_GRADER_TYPES",
    "Lifecycle",
    "LifecycleAlreadyExistsError",
    "LifecycleInitialized",
    "LifecycleNotFoundError",
    "LifecycleStore",
    "LifecycleTransitionError",
    "MalformedEnvelope",
    "ManualGrader",
    "MissingEnvelope",
    "NoOpStrategy",
    "OPENING_FENCE",
    "OptimisticConcurrencyError",
    "Outcome",
    "PostgresStore",
    "RecheckOutcome",
    "ResolveApprovalOutcome",
    "RetryScheduled",
    "RubricGrader",
    "RunNotifier",
    "SchemaMismatchError",
    "SdkMessageRecord",
    "SessionLimitReset",
    "SessionRecorded",
    "SpendSummary",
    "SqliteStore",
    "Status",
    "StoreConflictError",
    "StoreSchemaError",
    "Strategy",
    "StrategyContext",
    "StrategyResult",
    "Subscription",
    "Task",
    "TaskDefect",
    "TaskLoadError",
    "TaskStore",
    "TelemetryRecord",
    "TelemetrySink",
    "ToolInteraction",
    "ToolResultObservation",
    "TranscriptCounter",
    "TranscriptGrader",
    "TranscriptObservation",
    "TransitionedTo",
    "TruncatedEnvelope",
    "Unblocked",
    "VALID_INTENTS",
    "VALID_REQUIREMENT_TYPES",
    "ValidEnvelope",
    "ValidationError",
    "apply",
    "attach_logger",
    "build_iteration_prompt",
    "classify_fault",
    "deadline_config_from_mapping",
    "derive_session_limit_reset",
    "derive_slug",
    "enforce_transcript_limits",
    "first_breach",
    "hello",
    "invoke_iteration",
    "load_graders",
    "load_task_data",
    "load_task_directory",
    "load_task_file",
    "load_tasks_jsonl",
    "parse_envelope",
    "recheck_blocked_lifecycle",
    "replay",
    "resolve_deadlines",
    "resolve_manual_approval",
    "run_command_graders",
    "run_task",
    "run_transcript_graders",
    "stream",
    "subscribe",
    "total_tokens_from_usage",
    "validate_task",
    "wait_backoff",
]

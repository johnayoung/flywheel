from typing import TYPE_CHECKING

from flywheel.audit import (
    AuditLoggerHandle,
    EventHandler,
    Subscription,
    attach_logger,
    stream,
    subscribe,
)
from flywheel.envelope import (
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
from flywheel.events import (
    AttemptFinalized,
    AttemptStarted,
    Blocked,
    DomainEvent,
    DomainEventKind,
    EventReplayError,
    GraderEvaluated,
    LifecycleInitialized,
    RetryScheduled,
    SessionRecorded,
    TransitionedTo,
    Unblocked,
    apply,
    replay,
)
from flywheel.grader_command import (
    DEFAULT_TAIL_BYTES,
    run_command_graders,
)
from flywheel.grader_transcript import (
    BreachedField,
    TranscriptCounter,
    TranscriptObservation,
    enforce_transcript_limits,
    first_breach,
    run_transcript_graders,
    total_tokens_from_usage,
)
from flywheel.harness import (
    HarnessConfig,
    HarnessOutcome,
    HarnessStore,
    InvocationRequest,
    InvokeFunc,
    RecheckOutcome,
    recheck_blocked_lifecycle,
    run_task,
)
from flywheel.invoker import (
    InvocationFailure,
    InvocationSignals,
    IterationResult,
    ToolInteraction,
    ToolResultObservation,
    invoke_iteration,
)
from flywheel.lifecycle import (
    Attempt,
    Lifecycle,
    LifecycleTransitionError,
    Outcome,
    Status,
)
from flywheel.loaders import (
    TaskLoadError,
    load_task_directory,
    load_task_file,
    load_tasks_jsonl,
)
from flywheel.notifier import RunNotifier
from flywheel.orchestrator import (
    OrchestratorReport,
    RunRecord,
    SandboxProvider,
    SandboxRequest,
    SubmitRequest,
    Submitter,
    orchestrate,
)
from flywheel.prompt import IterationInputs, build_iteration_prompt
from flywheel.store_memory import InMemoryStore
from flywheel.store_sqlite import SqliteStore
from flywheel.strategy import (
    NoOpStrategy,
    Strategy,
    StrategyContext,
    StrategyResult,
    derive_slug,
)
from flywheel.store_protocols import (
    CURRENT_SCHEMA_VERSION,
    AgentSessionStore,
    AttemptStore,
    AuditRecord,
    AuditStore,
    ClaimLostError,
    ClaimStore,
    ClaudeSessionEntry,
    ControlCommandRecord,
    ControlCommandStore,
    DomainEventStore,
    EventRecord,
    EventStore,
    GraderResultRecord,
    GraderResultStore,
    GraderType,
    LifecycleAlreadyExistsError,
    LifecycleNotFoundError,
    LifecycleStore,
    OptimisticConcurrencyError,
    SdkMessageRecord,
    SdkMessageStore,
    StoreConflictError,
    StoreSchemaError,
    TaskClaim,
    TaskStore,
)
from flywheel.task import (
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

if TYPE_CHECKING:
    # Surfaces ``PostgresStore`` to type-checkers without triggering the
    # ``flywheel[postgres]`` import guard at package import time. The
    # runtime hook is ``__getattr__`` below; it only loads psycopg when a
    # caller actually asks for ``PostgresStore``.
    from flywheel.store_postgres import PostgresStore as PostgresStore


def __getattr__(name: str) -> object:
    """Lazy re-export hook.

    Resolves attribute lookups that aren't bound at module import time.
    Currently only ``PostgresStore`` uses this path so the ``postgres``
    extra stays optional: ``from flywheel import PostgresStore`` triggers
    the import of ``flywheel.store_postgres``, which raises its own
    ``ImportError`` (naming the extra) when ``psycopg`` is absent.
    """
    if name == "PostgresStore":
        from flywheel.store_postgres import PostgresStore as _PG

        return _PG
    raise AttributeError(f"module 'flywheel' has no attribute {name!r}")


def hello() -> str:
    return "Hello from flywheel!"


__all__ = [
    "AgentSessionStore",
    "Attempt",
    "AttemptFinalized",
    "AttemptStarted",
    "AttemptStore",
    "AuditLoggerHandle",
    "AuditRecord",
    "AuditStore",
    "Blocked",
    "BlockedRequirement",
    "BreachedField",
    "CLOSING_FENCE",
    "CURRENT_SCHEMA_VERSION",
    "ClaudeSessionEntry",
    "ClaimLostError",
    "ClaimStore",
    "CommandGrader",
    "CommandGraderRequirement",
    "Context",
    "ControlCommandRecord",
    "ControlCommandStore",
    "DEFAULT_TAIL_BYTES",
    "DomainEvent",
    "DomainEventKind",
    "DomainEventStore",
    "DuplicateEnvelope",
    "EnvVarSetRequirement",
    "EnvelopeResult",
    "EventHandler",
    "EventRecord",
    "EventReplayError",
    "EventStore",
    "FileExistsRequirement",
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
    "OrchestratorReport",
    "Outcome",
    "PostgresStore",
    "RecheckOutcome",
    "RetryScheduled",
    "RubricGrader",
    "RunNotifier",
    "RunRecord",
    "SandboxProvider",
    "SandboxRequest",
    "SdkMessageRecord",
    "SdkMessageStore",
    "SessionRecorded",
    "SqliteStore",
    "Status",
    "StoreConflictError",
    "StoreSchemaError",
    "Strategy",
    "StrategyContext",
    "StrategyResult",
    "SubmitRequest",
    "Submitter",
    "Subscription",
    "Task",
    "TaskClaim",
    "TaskLoadError",
    "TaskStore",
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
    "derive_slug",
    "enforce_transcript_limits",
    "first_breach",
    "hello",
    "invoke_iteration",
    "load_task_directory",
    "load_task_file",
    "load_tasks_jsonl",
    "orchestrate",
    "parse_envelope",
    "recheck_blocked_lifecycle",
    "replay",
    "run_command_graders",
    "run_task",
    "run_transcript_graders",
    "stream",
    "subscribe",
    "total_tokens_from_usage",
]

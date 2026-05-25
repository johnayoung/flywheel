from typing import TYPE_CHECKING

from flywheel.envelope import (
    CLOSING_FENCE,
    OPENING_FENCE,
    VALID_INTENTS,
    DuplicateEnvelope,
    EnvelopeResult,
    Intent,
    MalformedEnvelope,
    MissingEnvelope,
    TruncatedEnvelope,
    ValidEnvelope,
    parse_envelope,
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
    AgentSessionStore,
    AttemptStore,
    ClaudeSessionEntry,
    EventRecord,
    EventStore,
    GraderResultRecord,
    GraderResultStore,
    GraderType,
    LifecycleAlreadyExistsError,
    LifecycleNotFoundError,
    LifecycleStore,
    OptimisticConcurrencyError,
    StoreConflictError,
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
    "AttemptStore",
    "BreachedField",
    "CLOSING_FENCE",
    "ClaudeSessionEntry",
    "CommandGrader",
    "Context",
    "DEFAULT_TAIL_BYTES",
    "DuplicateEnvelope",
    "EnvelopeResult",
    "EventRecord",
    "EventStore",
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
    "RubricGrader",
    "SqliteStore",
    "Status",
    "StoreConflictError",
    "Strategy",
    "StrategyContext",
    "StrategyResult",
    "Task",
    "TaskLoadError",
    "ToolInteraction",
    "ToolResultObservation",
    "TranscriptCounter",
    "TranscriptGrader",
    "TranscriptObservation",
    "TruncatedEnvelope",
    "VALID_INTENTS",
    "ValidEnvelope",
    "ValidationError",
    "build_iteration_prompt",
    "derive_slug",
    "enforce_transcript_limits",
    "first_breach",
    "hello",
    "invoke_iteration",
    "load_task_directory",
    "load_task_file",
    "load_tasks_jsonl",
    "parse_envelope",
    "run_command_graders",
    "run_task",
    "run_transcript_graders",
    "total_tokens_from_usage",
]

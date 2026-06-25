"""Multi-task orchestration built on flywheel.

Flywheel core owns the lifecycle of a single task. This package is the layer
above it: deciding *which* task runs next (selection over a prerequisite DAG),
coordinating *several workers* over a shared store (claims + leases), managing
the on-disk task queue / phases, and driving each chosen task through
``flywheel.run_task``. It depends on ``flywheel-core`` and never the other
way around. Library only: verbs are surfaced through the unified product
shell (``flywheel``), and module-level plumbing remains runnable as
``python -m flywheel_orchestrator._workflow``.
"""

from typing import TYPE_CHECKING

from flywheel_orchestrator._claims import (
    ClaimLostError,
    ClaimStore,
    InMemoryClaimStore,
    SourceSyncRecord,
    SqliteClaimStore,
    TaskClaim,
    WorkItemRecord,
)
from flywheel_orchestrator._orchestrate import (
    DEFAULT_LEASE_SECONDS,
    DEFAULT_RECONCILE_SECONDS,
    OrchestratorReport,
    RunRecord,
    orchestrate,
    reconcile_live_runs,
    sync_work_source,
)
from flywheel_orchestrator._strategy import (
    SandboxHandle,
    SandboxProvider,
    SandboxRequest,
    SubmitRequest,
    SubmitStrategy,
    Submitter,
)

if TYPE_CHECKING:
    from flywheel_orchestrator._claims_postgres import (
        PostgresClaimStore as PostgresClaimStore,
    )


from flywheel_orchestrator._held_out_gate import (
    FilesystemHeldOutGraderSource,
    GateOutcome,
    GateVerdict,
    HeldOutGraderError,
    HeldOutGraderSource,
    build_oracle_registration,
    evaluate_held_out_gate,
    write_oracle_registration,
)
from flywheel_orchestrator._github import GhRunner, GithubWorkSource
from flywheel_orchestrator._github_ci import GithubCiWorkSource
from flywheel_orchestrator._github_review import GithubReviewWorkSource
from flywheel_orchestrator._policy import (
    DEFAULT_POLICY_FILENAME,
    PolicyError,
    WorkPolicy,
    build_github_ci_source,
    build_github_review_source,
    build_work_source,
    load_policy,
)
from flywheel_orchestrator._sources import (
    DirectoryWorkSource,
    GraderReceipt,
    WorkItem,
    WorkReport,
    WorkSource,
    WorkSourceError,
    iter_active_phase_dirs,
    iter_active_task_files,
    load_active_tasks,
)
from flywheel_orchestrator._work_graph import (
    GraphValidationIssue,
    GraphValidationResult,
    WorkGraph,
    WorkGraphBuilder,
    WorkGraphValidationError,
)
from flywheel_orchestrator._history import (
    TERMINAL_STATUSES,
    AttemptSummary,
    HistoryRow,
    HistoryRun,
    RunDetail,
    build_task_phase_index,
    collect_history_rows,
    collect_run_detail,
    phase_from_source,
    resolve_run_id,
)
from flywheel_orchestrator._store_factory import (
    PG_DSN_ENV,
    PG_DSN_FALLBACK_ENV,
    StoreConfigError,
    build_store,
    open_sqlite_bound_store,
    resolve_postgres_dsn,
)
from flywheel_orchestrator._workflow import (
    DEFAULT_TASKS_DIR,
    INIT_ROOT,
    LOOP_BASE_FILENAME,
    LOOP_BASE_REF_PREFIX,
    LOOP_PATH_OPTOUT_FILENAME,
    LiveRunRow,
    LoopPathOptOut,
    LoopPathOptOutError,
    TaskState,
    TaskStatusRow,
    archive_completed_phases,
    build_status_rows,
    collect_live_rows,
    load_effective_policy,
    load_loop_path_optout,
    loop_base_ref,
    phase_diff_vs_base,
    read_phase_base,
    resolve_db_path,
    select_next_task,
    status_rows_for_items,
    task_state,
    write_phase_base_if_missing,
)


def __getattr__(name: str) -> object:
    """Lazy re-export so the ``postgres`` extra stays optional."""
    if name == "PostgresClaimStore":
        from flywheel_orchestrator._claims_postgres import PostgresClaimStore

        return PostgresClaimStore
    raise AttributeError(
        f"module 'flywheel_orchestrator' has no attribute {name!r}"
    )


__all__ = [
    "AttemptSummary",
    "ClaimLostError",
    "ClaimStore",
    "HistoryRow",
    "HistoryRun",
    "RunDetail",
    "TERMINAL_STATUSES",
    "DEFAULT_LEASE_SECONDS",
    "DEFAULT_RECONCILE_SECONDS",
    "DEFAULT_POLICY_FILENAME",
    "DirectoryWorkSource",
    "FilesystemHeldOutGraderSource",
    "GateOutcome",
    "GateVerdict",
    "GhRunner",
    "GithubCiWorkSource",
    "GithubReviewWorkSource",
    "GithubWorkSource",
    "GraderReceipt",
    "HeldOutGraderError",
    "HeldOutGraderSource",
    "GraphValidationIssue",
    "GraphValidationResult",
    "InMemoryClaimStore",
    "PolicyError",
    "PostgresClaimStore",
    "SqliteClaimStore",
    "StoreConfigError",
    "TaskClaim",
    "DEFAULT_TASKS_DIR",
    "INIT_ROOT",
    "LOOP_BASE_FILENAME",
    "LOOP_BASE_REF_PREFIX",
    "LOOP_PATH_OPTOUT_FILENAME",
    "LiveRunRow",
    "LoopPathOptOut",
    "LoopPathOptOutError",
    "OrchestratorReport",
    "PG_DSN_ENV",
    "PG_DSN_FALLBACK_ENV",
    "RunRecord",
    "SandboxHandle",
    "SandboxProvider",
    "SandboxRequest",
    "SourceSyncRecord",
    "SubmitRequest",
    "SubmitStrategy",
    "Submitter",
    "TaskState",
    "TaskStatusRow",
    "WorkGraph",
    "WorkGraphBuilder",
    "WorkGraphValidationError",
    "WorkItem",
    "WorkItemRecord",
    "WorkPolicy",
    "WorkReport",
    "WorkSource",
    "WorkSourceError",
    "archive_completed_phases",
    "build_github_ci_source",
    "build_github_review_source",
    "build_oracle_registration",
    "build_status_rows",
    "build_store",
    "build_task_phase_index",
    "build_work_source",
    "collect_history_rows",
    "collect_live_rows",
    "collect_run_detail",
    "evaluate_held_out_gate",
    "phase_from_source",
    "resolve_run_id",
    "iter_active_phase_dirs",
    "iter_active_task_files",
    "load_active_tasks",
    "load_policy",
    "load_effective_policy",
    "load_loop_path_optout",
    "loop_base_ref",
    "open_sqlite_bound_store",
    "orchestrate",
    "phase_diff_vs_base",
    "read_phase_base",
    "reconcile_live_runs",
    "resolve_db_path",
    "resolve_postgres_dsn",
    "select_next_task",
    "status_rows_for_items",
    "sync_work_source",
    "task_state",
    "write_oracle_registration",
    "write_phase_base_if_missing",
]

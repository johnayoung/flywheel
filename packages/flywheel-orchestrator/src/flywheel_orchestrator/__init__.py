"""Multi-task orchestration built on flywheel.

Flywheel core owns the lifecycle of a single task. This package is the layer
above it: deciding *which* task runs next (selection over a prerequisite DAG),
coordinating *several workers* over a shared store (claims + leases), managing
the on-disk task queue / phases, and driving each chosen task through
``flywheel.run_task``. It depends on ``flywheel`` and never the other way
around. The ``flywheel-orchestrate`` console command is its CLI.
"""

from typing import TYPE_CHECKING

from flywheel_orchestrator._claims import (
    ClaimLostError,
    ClaimStore,
    InMemoryClaimStore,
    SqliteClaimStore,
    TaskClaim,
)
from flywheel_orchestrator._orchestrate import (
    DEFAULT_LEASE_SECONDS,
    DEFAULT_RECONCILE_SECONDS,
    OrchestratorReport,
    RunRecord,
    SandboxProvider,
    SandboxRequest,
    SubmitRequest,
    Submitter,
    orchestrate,
    reconcile_live_runs,
)

if TYPE_CHECKING:
    from flywheel_orchestrator._claims_postgres import (
        PostgresClaimStore as PostgresClaimStore,
    )


def __getattr__(name: str) -> object:
    """Lazy re-export so the ``postgres`` extra stays optional."""
    if name == "PostgresClaimStore":
        from flywheel_orchestrator._claims_postgres import PostgresClaimStore

        return PostgresClaimStore
    raise AttributeError(
        f"module 'flywheel_orchestrator' has no attribute {name!r}"
    )
from flywheel_orchestrator._github import GithubWorkSource
from flywheel_orchestrator._policy import (
    DEFAULT_POLICY_FILENAME,
    PolicyError,
    WorkPolicy,
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
from flywheel_orchestrator._workflow import (
    DEFAULT_LOG_DIR,
    DEFAULT_TASKS_DIR,
    INIT_ROOT,
    LOOP_BASE_FILENAME,
    LOOP_PATH_OPTOUT_FILENAME,
    LiveRunRow,
    LoopPathOptOut,
    LoopPathOptOutError,
    TaskState,
    TaskStatusRow,
    archive_completed_phases,
    build_status_rows,
    collect_live_rows,
    load_loop_path_optout,
    phase_diff_vs_base,
    read_phase_base,
    select_next_task,
    status_rows_for_items,
    task_state,
    write_phase_base_if_missing,
)

__all__ = [
    "ClaimLostError",
    "ClaimStore",
    "DEFAULT_LEASE_SECONDS",
    "DEFAULT_LOG_DIR",
    "DEFAULT_RECONCILE_SECONDS",
    "DEFAULT_POLICY_FILENAME",
    "DirectoryWorkSource",
    "GithubWorkSource",
    "GraderReceipt",
    "InMemoryClaimStore",
    "PolicyError",
    "PostgresClaimStore",
    "SqliteClaimStore",
    "TaskClaim",
    "DEFAULT_TASKS_DIR",
    "INIT_ROOT",
    "LOOP_BASE_FILENAME",
    "LOOP_PATH_OPTOUT_FILENAME",
    "LiveRunRow",
    "LoopPathOptOut",
    "LoopPathOptOutError",
    "OrchestratorReport",
    "RunRecord",
    "SandboxProvider",
    "SandboxRequest",
    "SubmitRequest",
    "Submitter",
    "TaskState",
    "TaskStatusRow",
    "WorkItem",
    "WorkPolicy",
    "WorkReport",
    "WorkSource",
    "WorkSourceError",
    "archive_completed_phases",
    "build_status_rows",
    "build_work_source",
    "collect_live_rows",
    "iter_active_phase_dirs",
    "iter_active_task_files",
    "load_active_tasks",
    "load_loop_path_optout",
    "orchestrate",
    "phase_diff_vs_base",
    "read_phase_base",
    "reconcile_live_runs",
    "select_next_task",
    "status_rows_for_items",
    "task_state",
    "write_phase_base_if_missing",
]

"""Multi-task orchestration built on flywheel.

Flywheel core owns the lifecycle of a single task. This package is the layer
above it: deciding *which* task runs next (selection over a prerequisite DAG),
coordinating *several workers* over a shared store (claims + leases), managing
the on-disk task queue / phases, and driving each chosen task through
``flywheel.run_task``. It depends on ``flywheel`` and never the other way
around. The ``flywheel-orchestrate`` console command is its CLI.
"""

from flywheel_orchestrator._orchestrate import (
    DEFAULT_LEASE_SECONDS,
    OrchestratorReport,
    RunRecord,
    SandboxProvider,
    SandboxRequest,
    SubmitRequest,
    Submitter,
    orchestrate,
)
from flywheel_orchestrator._workflow import (
    DEFAULT_LOG_DIR,
    DEFAULT_TASKS_DIR,
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
    iter_active_phase_dirs,
    iter_active_task_files,
    load_active_tasks,
    load_loop_path_optout,
    phase_diff_vs_base,
    read_phase_base,
    select_next_task,
    task_state,
    write_phase_base_if_missing,
)

__all__ = [
    "DEFAULT_LEASE_SECONDS",
    "DEFAULT_LOG_DIR",
    "DEFAULT_TASKS_DIR",
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
    "archive_completed_phases",
    "build_status_rows",
    "collect_live_rows",
    "iter_active_phase_dirs",
    "iter_active_task_files",
    "load_active_tasks",
    "load_loop_path_optout",
    "orchestrate",
    "phase_diff_vs_base",
    "read_phase_base",
    "select_next_task",
    "task_state",
    "write_phase_base_if_missing",
]

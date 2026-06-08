"""Multi-task orchestration built on flywheel.

Flywheel core owns the lifecycle of a single task. This package is the layer
above it: deciding *which* task runs next (selection over a prerequisite DAG),
coordinating *several workers* over a shared store (claims + leases), and
driving each chosen task through ``flywheel.run_task``. It depends on
``flywheel`` and never the other way around.
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

__all__ = [
    "DEFAULT_LEASE_SECONDS",
    "OrchestratorReport",
    "RunRecord",
    "SandboxProvider",
    "SandboxRequest",
    "SubmitRequest",
    "Submitter",
    "orchestrate",
]

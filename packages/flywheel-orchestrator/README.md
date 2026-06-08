# flywheel-orchestrator

The multi-task layer built on [`flywheel`](../flywheel). Flywheel runs **one**
task; this drives **many**: it decides which task runs next over a prerequisite
DAG, coordinates several workers over a shared store (claims + leases), and
drives each chosen task through `flywheel.run_task`.

Depends on `flywheel`; `flywheel` never depends on this.

## Install

```bash
uv add flywheel-orchestrator
```

## CLI

```bash
flywheel-orchestrate next               # path to the next eligible task
flywheel-orchestrate orchestrate        # drain every eligible task to quiescence
flywheel-orchestrate status             # state of every active task
flywheel-orchestrate live               # one line per in-flight run
flywheel-orchestrate recover            # finalize stranded lifecycles
```

## The submit seam

`orchestrate()` provisions a sandbox and acts on each task's terminal status
through two injectable callbacks, so no consumer-specific code (git, merges)
enters the loop:

```python
from flywheel_orchestrator import orchestrate, SandboxRequest, SubmitRequest

await orchestrate(
    tasks_dir=tasks_dir, db_path=db, sandbox_root=root,
    prepare_sandbox=my_prepare,   # SandboxRequest -> Path
    submit=my_submit,             # SubmitRequest -> None
)
```

`flywheel-worktree` is a worked example of that seam. Bring your own for a
different VCS/sandbox/merge strategy.

## Persistence

Owns its scheduling state (`task_claims`) in its **own** store
(`SqliteClaimStore` / `PostgresClaimStore` / `InMemoryClaimStore`), which can
share a backend with flywheel's store but manages only its own tables.

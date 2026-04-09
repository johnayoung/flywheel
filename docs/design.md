# Flywheel — Design Document

## Overview

Worktree-based parallel agent orchestrator with dependency resolution, merge conflict handling, and a review pipeline. Agents execute coding tasks in isolated git worktrees, coordinated by a DAG scheduler that understands task dependencies, merge ordering, and failure recovery.

**Language:** Go 1.23+
**Interface:** CLI (primary), Web UI (future)
**Agent:** Claude Code (MVP), pluggable via `Agent` interface
**Scope:** Local-only to start

---

## Core Concepts

### Separation of Concerns: Task vs Lifecycle

The system separates **what to do** (task definition) from **what's happening** (execution lifecycle). These are two distinct data models with different ownership:

- **Task** — authored by humans or scripts. Immutable during execution. Defines the work: description, steps, acceptance criteria, commit template, dependencies. Think of it as a row in a `tasks` table.
- **Lifecycle** — owned entirely by Flywheel. Mutable. Tracks execution state: status, branch, worktree path, timestamps, errors, agent output, review results. Think of it as a row in a `task_runs` table keyed by task ID.

This separation means:
- Tasks can be defined in any format (JSON, JSONL, YAML, pulled from an API, rows in Postgres)
- The lifecycle layer is always structured the same way regardless of task source
- Workers pick up tasks and manage lifecycle records — the task definition is read-only to them
- You can re-run a task (new lifecycle record) without modifying the task itself

### Storage Backend Abstraction

The backing store is pluggable. The interface is what matters:

```go
type TaskStore interface {
    // Task definitions (read-only during execution)
    ListTasks(ctx context.Context, filter TaskFilter) ([]task.Task, error)
    GetTask(ctx context.Context, id string) (*task.Task, error)

    // Lifecycle management (read-write, with locking)
    CreateLifecycle(ctx context.Context, lc *lifecycle.Lifecycle) error
    ClaimNextReady(ctx context.Context, workerID string) (*lifecycle.Lifecycle, error)
    GetLifecycle(ctx context.Context, taskID string) (*lifecycle.Lifecycle, error)
    UpdateLifecycle(ctx context.Context, lc *lifecycle.Lifecycle) error
    ListLifecycles(ctx context.Context, filter LifecycleFilter) ([]lifecycle.Lifecycle, error)
}
```

`ClaimNextReady` is the critical method — it atomically finds the highest-priority task in `ready` state and assigns it to a worker. In Postgres this is `SELECT ... FOR UPDATE SKIP LOCKED`. In a file-based store it's an advisory lock file. The interface is the same either way.

**Planned backends:**
- **JSONL/JSON files** — MVP. Tasks in `tasks/` dir, lifecycle in `.flywheel/lifecycle/`. File locking for concurrency.
- **SQLite** — single-file database, good for local CLI use with better query support.
- **Postgres** — for team/CI use cases where multiple workers coordinate.

---

## Task Schema

A task is the human-authored work definition. Everything here is **read-only** during execution.

```json
{
  "id": "INF-0252",
  "description": "Create Uniswap V2 PoolsListUpdater that wraps KyberSwap's updater for pool discovery.",
  "category": "feat",
  "priority": 4,
  "prerequisites": ["INF-0250"],

  "commit": "feat(uniswap/v2): integrate KyberSwap PoolsListUpdater for pool discovery",
  "steps": [
    "Step 1: Create internal/protocol/uniswap/v2/updater.go with PoolsListUpdater struct",
    "Step 2: Implement NewPoolsListUpdater constructor",
    "Step 3: Implement GetNewPools with entity.Pool to v2.Pool conversion",
    "Step 4: Create convertEntityPool helper",
    "Step 5: Implement Protocol() and ChainID() methods",
    "Step 6: Add updater_test.go with integration test"
  ],
  "acceptance_criteria": [
    "PoolsListUpdater implements liquidity.PoolsListUpdater interface",
    "GetNewPools returns v2.Pool instances with correct token metadata",
    "Pagination works via metadata parameter",
    "Integration test demonstrates full discovery -> store flow",
    "Handles conversion errors gracefully (skip invalid pools, log warning)"
  ],

  "review": "agent"
}
```

### Field Reference

| Field                 | Type     | Description                                                                                                                                                                                                                                                               |
| --------------------- | -------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `id`                  | string   | Unique task identifier. Convention: `{PROJECT}-{NUMBER}`.                                                                                                                                                                                                                 |
| `description`         | string   | Full description of the work. Passed to the agent as primary context.                                                                                                                                                                                                     |
| `category`            | string   | Task category: `feat`, `fix`, `refactor`, `test`, `docs`, `chore`.                                                                                                                                                                                                        |
| `priority`            | int      | Scheduling priority. Lower = higher priority. Used to break ties when multiple tasks are ready.                                                                                                                                                                           |
| `prerequisites`       | []string | Task IDs that must be `merged` before this task can become `ready`.                                                                                                                                                                                                       |
| `commit`              | string   | Commit message template. Flywheel enforces the agent's commit starts with this.                                                                                                                                                                                           |
| `steps`               | []string | Ordered implementation steps. Gives the agent structure and enables per-step checkpointing.                                                                                                                                                                               |
| `acceptance_criteria` | []string | Conditions that must be true for the task to pass validation.                                                                                                                                                                                                             |
| `review`              | string   | Review mode for this task: `"agent"` (AI review), `"human"` (HITL gate), `"none"` (skip). Per-task because the decision of whether a human needs to look at a specific change is a property of the task, not the system. Falls back to the run config default if omitted. |

Operational settings (`merge_strategy`, `timeout`, `max_retries`) live in the run configuration, not on individual tasks. These are system-level concerns, not task-level concerns.

---

## Task Lifecycle

The lifecycle is a separate record, owned and mutated exclusively by Flywheel. One lifecycle record per task execution.

```json
{
  "task_id": "INF-0252",
  "run_id": "run-20250408-100100-b2e7",
  "worker_id": "worker-1",

  "status": "running",
  "branch": "flywheel/INF-0252",
  "worktree_path": "/tmp/flywheel/INF-0252",
  "base_ref": "main",
  "base_sha": "abc123f",

  "current_step": 3,
  "steps_completed": ["Step 1", "Step 2", "Step 3"],

  "timestamps": {
    "created_at": "2025-04-08T10:00:00Z",
    "ready_at": "2025-04-08T10:00:05Z",
    "started_at": "2025-04-08T10:01:00Z"
  },

  "version": 3,
  "retries": 0,
  "resolve_attempts": 0
}
```

### State Machine

```
pending ──→ ready ──→ running ──→ validating ──→ reviewing ──→ merging ──→ merged
              ↑          │            │              │            │
              │          ↓            ↓              ↓            ↓
              │       failed*   failed_valid.     rejected    conflict
              │                   │     │          │   │         │
              │                   │     ↓          │   ↓         ↓
              │                   │  failed*       │ failed*  resolving
              │                   │                │          │      │
              └───────────────────┴────────────────┘    ──→ merging  │
                       (retry if retries < max_retries)             ↓
                                                                 failed*

* failed = terminal, no further transitions
```

### State Transitions

| From                                        | To                  | Trigger                                                                                   |
| ------------------------------------------- | ------------------- | ----------------------------------------------------------------------------------------- |
| `pending`                                   | `ready`             | All prerequisites have status `merged`                                                    |
| `ready`                                     | `running`           | Worker claims task via `ClaimNextReady`, creates worktree, starts agent                   |
| `running`                                   | `validating`        | Agent reports completion                                                                  |
| `running`                                   | `failed`            | Agent error or timeout                                                                    |
| `validating`                                | `reviewing`         | Validation passes (clean worktree, commit matches, build passes, acceptance criteria met) |
| `validating`                                | `failed_validation` | Validation fails (uncommitted files, bad commit msg, build failure, criteria not met)     |
| `reviewing`                                 | `merging`           | Reviewer approves (agent or human)                                                        |
| `reviewing`                                 | `rejected`          | Reviewer rejects — can be retried or escalated                                            |
| `merging`                                   | `merged`            | Merge succeeds, worktree cleaned up, dependents notified                                  |
| `merging`                                   | `conflict`          | Merge has conflicts that need resolution                                                  |
| `conflict`                                  | `resolving`         | Conflict resolution agent starts                                                          |
| `resolving`                                 | `merging`           | Conflicts resolved, retry merge                                                           |
| `resolving`                                 | `failed`            | Conflict resolution failed or max resolution attempts exceeded                            |
| `failed_validation` / `rejected`             | `ready`             | Retry (if retries < max_retries)                                                          |
| `failed_validation` / `rejected`             | `failed`            | Max retries exceeded — escalate to terminal failure                                       |
| `running` / `validating` / `reviewing` / `merging` / `resolving` | `ready` | Crash recovery — engine restart resets incomplete tasks (see resume support) |
| `failed`                                     | —                   | Terminal. No transitions. Hard failures (agent crash, timeout) are not retryable.          |

### Validation Checklist (running → validating → reviewing)

When a task transitions from `running` to `validating`, Flywheel runs:

1. **Clean worktree**: `git status --porcelain` must be empty
2. **Commit exists**: `git log --oneline {base_sha}..HEAD` must have ≥1 commit (uses the SHA recorded at branch creation, not the ref name, since `base_ref` moves forward as other tasks merge)
3. **Commit message**: The earliest commit on the branch (first in `git log --reverse {base_sha}..HEAD`) must start with the task's `commit` field. Subsequent commits are unconstrained.
4. **Build passes**: Run configurable build command (e.g., `go build ./...`)
5. **Acceptance criteria**: Feed criteria + diff to a validation agent for verification

All must pass to proceed to `reviewing`. Any failure → `failed_validation` with details captured in `lifecycle.error`.

---

## Dependency-Aware Branching

Tasks branch based on their position in the dependency graph:

- **Independent tasks** (no prerequisites, or all prerequisites merged): branch off `base_ref` (typically `main`).
- **Dependent tasks**: branch off the current `base_ref` which already contains all prerequisite merges.

This falls out naturally from the wave-based execution model:

```
Wave 1: A, B (independent) → branch off main, run in parallel
         A merges into main
         B merges into main (conflict resolution if needed)

Wave 2: C (depends on A, B) → branch off new main (has both A and B)
         C runs, reviews, merges
```

The scheduler doesn't wait for entire waves — it re-evaluates after every merge. If D depends only on A, it can start as soon as A merges, even if B is still running.

---

## Merge Strategies

### `sequential` (default)
Tasks merge into `base_ref` one at a time in completion order. A merge queue with a lock ensures only one merge at a time. The conflict resolution agent handles any drift between the branch point and current `base_ref`.

Merges happen in a dedicated temporary worktree checked out to `base_ref`, not in the user's main working directory. After the merge commit is created in the temporary worktree, `base_ref` is updated via `git update-ref`. This isolates the merge from the user's working tree entirely.

### `pr`
Each task creates a GitHub/GitLab PR. Review happens on the PR (AI reviewer posts comments, or HITL via PR approval). Merge happens through the platform's merge mechanism. Best for audit trails and CI integration.

### `accumulate`
Tasks don't merge immediately. All tasks in a group/wave complete, then they're merged as a batch. Useful when you want to review the full set of changes holistically before anything touches `base_ref`.

---

## Run Configuration

```json
{
  "version": "1",
  "repo": ".",
  "base_ref": "main",
  "branch_prefix": "flywheel/",
  "max_parallel": 3,
  "build_command": "go build ./...",
  "store": {
    "backend": "jsonl",
    "tasks_path": "./tasks",
    "lifecycle_path": "./.flywheel/lifecycle"
  },
  "merge_strategy": "sequential",
  "review": "agent",
  "agent": "claude-code",
  "timeout": "30m",
  "max_retries": 2,
  "max_resolve_attempts": 2
}
```

The only setting that can be overridden per-task is `review`, because whether a human needs to look at a specific change is a property of the task, not the system. All other operational settings are config-level only.

---

## Go Package Architecture

```
cmd/flywheel/              → CLI entrypoint (cobra)
internal/
  config/                  → Run config parsing + validation
  task/                    → Task model + parsing (JSON, JSONL, YAML)
  lifecycle/               → Lifecycle model + state machine + transitions
  store/                   → Storage interface + implementations
    store.go               → TaskStore interface
    jsonl/                 → JSONL/file-based implementation
    sqlite/                → SQLite implementation (post-MVP)
    postgres/              → Postgres implementation (post-MVP)
  dag/                     → DAG construction, topo sort, readiness checks, wave computation
  worktree/                → Git worktree lifecycle (create/remove/list)
  agent/                   → Agent interface + implementations
    agent.go               → Agent interface
    claudecode/            → Claude Code implementation
  merge/                   → Merge strategies (sequential, pr, accumulate)
  conflict/                → Conflict detection + resolution agent
  review/                  → Review pipeline (agent reviewer, HITL gate)
  validate/                → Post-agent validation (clean tree, commit, build, criteria)
  engine/                  → Main orchestration loop, worker coordination
```

### Key Interface: Agent

```go
type Agent interface {
    Execute(ctx context.Context, req ExecutionRequest) (*ExecutionResult, error)
}

type ExecutionRequest struct {
    WorktreePath string
    TaskID       string
    Description  string
    Steps        []string
    ResumeFrom   int      // 0 = start from beginning, N = resume from step N
}

type ExecutionResult struct {
    Success             bool
    StepsCompleted      int
    Output              string   // full agent output/logs
    ImplementationNotes string   // agent's summary of what it did
    Error               string   // error message if failed
}
```

### Key Interface: TaskStore

Defined in the [Storage Backend Abstraction](#storage-backend-abstraction) section above.

`ClaimNextReady` semantics:
- Postgres: `SELECT ... FOR UPDATE SKIP LOCKED` on the lifecycle table
- SQLite: `BEGIN IMMEDIATE` transaction with status check
- JSONL: Advisory lock file per task in the lifecycle directory

---

## Open Design Questions

1. **Concurrency limits** — `max_parallel` caps total concurrent agents. Should there also be per-group or per-resource limits?

2. **UI** — A web dashboard showing task status, lifecycle progression, and pending HITL reviews would be high-value. Consider embedding a simple HTTP server in the CLI (`flywheel dashboard`).
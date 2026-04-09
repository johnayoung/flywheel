# Flywheel

Worktree-based parallel agent orchestrator. Flywheel coordinates multiple AI coding agents executing tasks concurrently in isolated git worktrees, with dependency-aware scheduling, automated review, merge conflict resolution, and crash recovery.

## How it works

1. Define tasks as JSON files with descriptions, steps, acceptance criteria, and dependencies
2. Flywheel builds a DAG from task prerequisites and schedules work in parallel waves
3. Each task runs in an isolated git worktree -- agents cannot interfere with each other
4. Completed work is validated, reviewed (by agent or human), and merged sequentially
5. After each merge, the DAG re-evaluates which tasks are now unblocked

## Install

```
go install github.com/johnayoung/flywheel/cmd/flywheel@latest
```

Or build from source:

```
git clone https://github.com/johnayoung/flywheel.git
cd flywheel
make build        # produces ./flywheel binary with version from git tag
```

## Quick start

Create a task file in `tasks/`:

```json
{
  "id": "add-logging",
  "description": "Add structured logging to the HTTP handler",
  "category": "feat",
  "priority": 1,
  "prerequisites": [],
  "commit": "feat: add structured logging to HTTP handler",
  "steps": [
    "Add slog-based structured logging to all handler functions",
    "Include request ID, method, path, and duration in each log entry",
    "Add a middleware that injects a logger into the request context"
  ],
  "acceptance_criteria": [
    "All handler functions log request metadata on entry and exit",
    "Log output is JSON-formatted",
    "go build ./... succeeds"
  ]
}
```

Initialize and run:

```
flywheel init              # validate tasks, show execution plan
flywheel run               # execute the orchestration loop
flywheel status            # check progress
flywheel review list       # see tasks awaiting review
flywheel review approve <id>
```

## Configuration

Create `flywheel.json` in your repo root:

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

| Field | Default | Description |
|-------|---------|-------------|
| `repo` | `.` | Path to git repository |
| `base_ref` | `main` | Branch to create worktrees from and merge into |
| `branch_prefix` | `flywheel/` | Prefix for task branches |
| `max_parallel` | `3` | Max concurrent agent workers |
| `build_command` | | Command to validate builds (e.g. `go build ./...`) |
| `merge_strategy` | `sequential` | How to merge completed work |
| `review` | `agent` | Default review mode: `agent`, `human`, or `none` |
| `agent` | `claude-code` | Agent backend for task execution |
| `timeout` | `30m` | Per-task timeout |
| `max_retries` | `2` | Retries on task failure |
| `max_resolve_attempts` | `2` | Retries for merge conflict resolution |

## Task schema

| Field | Required | Description |
|-------|----------|-------------|
| `id` | yes | Unique identifier, no whitespace |
| `description` | yes | What the task accomplishes |
| `category` | yes | `feat`, `fix`, `refactor`, `test`, `docs`, or `chore` |
| `priority` | no | Numeric priority (lower = higher priority) |
| `prerequisites` | no | Task IDs that must merge before this task starts |
| `commit` | yes | Commit message template |
| `steps` | yes | Ordered list of implementation steps for the agent |
| `acceptance_criteria` | no | Conditions that must be true when complete |
| `review` | no | Per-task override: `agent`, `human`, or `none` |

## Commands

```
flywheel init                  # Validate tasks, build DAG, show execution plan
flywheel run                   # Execute the orchestration loop
flywheel run --dry-run         # Show plan without executing
flywheel run --max-parallel 5  # Override parallelism
flywheel status                # Show all task statuses
flywheel status <id>           # Show single task detail
flywheel status <id> --lifecycle  # Include timestamps
flywheel review list           # List tasks awaiting review
flywheel review approve <id>   # Approve a task
flywheel review reject <id> --reason "..."  # Reject with reason
flywheel validate tasks        # Validate task definitions
flywheel validate dag          # Validate dependency graph
flywheel clean                 # Remove worktrees and .flywheel/ state
flywheel clean --worktrees-only  # Keep .flywheel/, remove only worktrees
```

## Architecture

```
cmd/flywheel/       CLI entry point (Cobra)
internal/
  agent/            Agent interface + Claude Code implementation
  config/           Run configuration model and loader
  conflict/         Merge conflict detection and agent-based resolution
  dag/              DAG construction, cycle detection, readiness scheduling
  engine/           Orchestration engine with DAG-reactive scheduling
  lifecycle/        State machine (11 states, 22 transitions)
  merge/            Merge strategies (sequential MVP)
  review/           Agent and human-in-the-loop reviewers
  store/            TaskStore interface + JSONL file backend
  task/             Task model, validation, parsers
  validate/         Post-agent validation pipeline
  worktree/         Git worktree lifecycle management
```

## Prerequisites

- Go 1.26+
- Git
- [Claude Code](https://docs.anthropic.com/en/docs/claude-code) CLI (for the `claude-code` agent backend)

## License

[MIT](LICENSE)

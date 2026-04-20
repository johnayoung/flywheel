# Task Lifecycle

The lifecycle tracks a task's execution state from creation to terminal outcome. The schema says *what* to do, the lifecycle tracks *what happened*.

## States

| Status              | Meaning                                             |
| ------------------- | --------------------------------------------------- |
| `pending`           | Task exists but prerequisites not met               |
| `ready`             | All prerequisites satisfied, eligible for execution |
| `running`           | Agent is actively working                           |
| `validating`        | Agent finished, validation checks running           |
| `failed_validation` | Validation failed (retryable)                       |
| `done`              | Terminal: work completed successfully               |
| `failed`            | Terminal: unrecoverable failure                     |
| `interrupted`       | Execution halted externally (resumable)             |

## State machine

```
pending -> ready -> running -> validating -> done
                      |           |
                      |      failed_validation -> ready (retry) or failed
                      |
                      +-> failed
                      +-> interrupted -> ready
```

Key rules:
- `done` and `failed` are terminal — no transitions out
- `failed_validation` can transition back to `ready` (consuming retry budget)
- `interrupted` always resumes via `ready`
- Transitioning to `failed` or `failed_validation` requires the `Error` field to be set

## Lifecycle struct

| Field             | Purpose                                   |
| ----------------- | ----------------------------------------- |
| `task_id`         | Links back to the task definition         |
| `run_id`          | Identifies this execution run             |
| `worker_id`       | Which worker is executing                 |
| `status`          | Current state                             |
| `current_step`    | Index into the task's steps array         |
| `steps_completed` | Steps the agent has finished              |
| `timestamps`      | When each major transition occurred       |
| `version`         | Optimistic concurrency control            |
| `retries`         | How many times this task has been retried |
| `error`           | Current error (cleared on retry)          |
| `agent_output`    | Last output from the agent                |
| `attempts`        | Full history of all attempts              |
| `session_id`      | Agent session for resumption              |
| `artifacts_dir`   | Where attempt artifacts are stored        |

## Attempts

Each execution attempt is recorded as an `Attempt` with:

- `number` — sequential attempt index
- `started_at` / `ended_at` — wall clock duration
- `outcome` — one of: `succeeded`, `validation_failed`, `agent_error`, `cancelled`, `internal_error`
- `agent_output`, `error`, `validation_failures` — context for debugging
- `run_id` — groups attempts within the same run

## Retries

A task is eligible for retry when:
1. Status is `failed_validation`
2. `retries < max_retries` (configurable)

On retry, the lifecycle transitions back to `ready`, increments `retries`, and clears transient error state. The full attempt history is preserved.

`ConsecutiveFailedRuns` counts sequential failed runs (grouped by `run_id`) from the tail of the attempts list. This drives circuit-breaker logic.

## Strategy

Git workflow concerns (branching, merging, conflict resolution, worktrees) live outside the lifecycle in a pluggable `Strategy` interface. The loop calls `Strategy.Setup()` before running and `Strategy.Submit()` as a side-effect of transitioning to `done`. The lifecycle does not model or track git state.

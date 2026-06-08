# flywheel

Production-grade orchestration loop for AI coding agents. Owns the execution
lifecycle of a **single task**: invoke the agent, validate its iteration
envelopes, verify with graders, record attempts, retry. It knows nothing about
who calls it.

Scheduling across many tasks (a dependency DAG, claims, multi-worker, git
submit) is **not** here — that's the consumer layer (`flywheel-orchestrator`,
`flywheel-worktree`).

## Install

```bash
uv add flywheel            # core
uv add 'flywheel[postgres]'  # + Postgres store backend
```

## Run one task

```bash
flywheel run "Add exponential backoff to the HTTP client." \
    --check "uv run pytest tests/http"
flywheel run path/to/task.json
```

`flywheel run` streams events to stdout and exits 0 only on `done`. Other
subcommands: `is-done`, `interrupt`, `steer`, `set-model`, `approve`, `reject`.

## Library

```python
from flywheel import Task, CommandGrader, run_task, Lifecycle, SqliteStore

task = Task(
    goal="Add exponential backoff to the HTTP client.",
    graders=[CommandGrader(run="uv run pytest tests/http")],
)
```

A `Task` is `id` / `goal` / `graders` / `tags` / `context`. Direct construction
is first-class; loaders (`load_task_file`, …) are optional conveniences.

See `docs/` in the repo for the authoritative specs (`task-schema`,
`task-lifecycle`, `loop`).

# flywheel-core

Production-grade orchestration loop for AI coding agents. Owns the execution
lifecycle of a **single task**: invoke the agent, validate its iteration
envelopes, verify with graders, record attempts, retry. It knows nothing about
who calls it.

The dist is `flywheel-core`; the import module is `flywheel_core`.

Scheduling across many tasks (a dependency DAG, claims, multi-worker, git
submit) is **not** here — that's the consumer layer (`flywheel-orchestrator`,
`flywheel-worktree`); operator verbs are owned by the top-of-stack `flywheel`
product package.

## Install

```bash
uv add flywheel-core                # core
uv add 'flywheel-core[postgres]'    # + Postgres store backend
```

## Run one task

```bash
uv run python -m flywheel_core.workflow run "Add exponential backoff to the HTTP client." \
    --check "uv run pytest tests/http"
uv run python -m flywheel_core.workflow run path/to/task.json
```

`flywheel_core.workflow run` streams events to stdout and exits 0 only on `done`.
Other subcommands: `is-done`, `interrupt`, `steer`, `set-model`, `approve`,
`reject`. The product shell (`flywheel` / `fw`) re-exposes the operator-facing
verbs (`say`, `interrupt`, `approve`, `reject`) on top of these primitives.

## Library

```python
from flywheel_core import Task, CommandGrader, run_task, Lifecycle, SqliteStore

task = Task(
    goal="Add exponential backoff to the HTTP client.",
    graders=[CommandGrader(run="uv run pytest tests/http")],
)
```

A `Task` is `id` / `goal` / `graders` / `tags` / `context`. Direct construction
is first-class; loaders (`load_task_file`, …) are optional conveniences.

See `docs/` in the repo for the authoritative specs (`task-schema`,
`task-lifecycle`, `loop`).

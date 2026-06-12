# flywheel

An orchestration loop for AI coding agents: the agent is the brain, flywheel is the control plane — it invokes the agent against a structured task, verifies completion claims with graders, and records the full execution history.

## Quickstart

Requires Python 3.13 and [uv](https://docs.astral.sh/uv/).

```bash
uv sync                        # install all workspace packages
uv run flywheel init           # scaffold .flywheel/ and a work policy
uv run flywheel worker --once  # claim one task, run it, exit
uv run flywheel                # operator console (alias: fw)
uv run pytest                  # full test suite
```

`flywheel <verb> --help` lists each verb's flags (`status`, `live`, `history`, `show`, `say`, `interrupt`, `approve`, `reject`, `audit`, ...).

## Packages

A uv workspace of four packages under `packages/`. Dependencies point one way only — core imports nothing downstream.

| Package | Role |
| --- | --- |
| `flywheel-core` | Lifecycle of a single task: invoke, validate envelopes, verify via graders, record attempts, retry. |
| `flywheel-orchestrator` | Drives many tasks on top of core: prerequisite DAG, claims/leases, multi-worker, phases. |
| `flywheel-worktree` | Git-worktree submit strategy plus the worker daemon. |
| `flywheel` | The `flywheel` / `fw` shell: verb router and operator console. |

## Docs

Authoritative specs in `docs/` override any inferred behavior:

- [vision.md](docs/vision.md) — what the loop is and is not
- [task-schema.md](docs/task-schema.md) — `Task`/`Grader`/`Context` shape and validation
- [task-lifecycle.md](docs/task-lifecycle.md) — lifecycle states, attempts, transition rules
- [loop.md](docs/loop.md) — iteration envelope and harness behavior
- [workflow.md](docs/workflow.md) — end-to-end run flow
- [data-taxonomy.md](docs/data-taxonomy.md) — state vs. telemetry split

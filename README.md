# flywheel

A production-grade orchestration loop for AI coding agents: the agent is the brain, flywheel is the control plane that invokes it, verifies its claims, and records everything.

## Quickstart

Requires Python 3.13 and [uv](https://docs.astral.sh/uv/).

```bash
uv sync                          # install all workspace packages
uv run flywheel init             # scaffold .flywheel/ and a work policy
uv run flywheel worker --once    # claim and run one task, then exit
uv run flywheel                  # open the operator console (alias: fw)
uv run pytest                    # full test suite
```

## Packages

A uv workspace of four packages under `packages/`. Dependencies point one way only — core imports nothing downstream.

| Package | What it does |
| --- | --- |
| `flywheel-core` | The lifecycle of a single task: invoke, validate envelopes, verify via graders, record attempts, retry. |
| `flywheel-orchestrator` | Drives many tasks on top of core: prerequisite DAG, claims/leases, multi-worker, phases. |
| `flywheel-worktree` | Git-worktree submit strategy plus the reference worker daemon. |
| `flywheel` | The `flywheel` / `fw` CLI: verb router and operator console. |

## Docs

Authoritative specs live in `docs/` and override any inferred behavior:

- [vision.md](docs/vision.md) — what the loop is and is not
- [task-schema.md](docs/task-schema.md) — `Task`/`Grader`/`Context` shape and validation
- [task-lifecycle.md](docs/task-lifecycle.md) — `Lifecycle`/`Attempt`/`Status`/`Outcome` transitions
- [loop.md](docs/loop.md) — iteration envelope and harness behavior
- [strategy.md](docs/strategy.md) — submission strategy

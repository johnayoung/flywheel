# Sandbox (the agent execution environment)

The sandbox is where and how the agent runs a single task: working directory, tools and MCP surface, network, credentials, budget, and workspace disposal. There is no `[sandbox.*]` config: two worker flags pick the backend, everything else is a built-in default (`SandboxPolicy::default`, `crates/flywheel-orchestrator/src/policy.rs`). Old keys and their fates: [configuration.md](configuration.md#where-every-old-setting-went).

## The sandbox-as-deploy model

**A sandbox is a deploy of a task**: a fresh process, a provisioned working directory, attached backing services, disposal at the end -- the `prepare_sandbox -> invoke -> submit -> teardown` lifecycle ([strategy.md](strategy.md)). **There is no human-approval axis**: graders plus the landing trust ladder ([strategy.md](strategy.md)) do the job an approval prompt would, so the agent always runs with `permission_mode = "bypassPermissions"` (a worker loop has no approver).

## The aspects and their homes

| Aspect | Home | Value |
| --- | --- | --- |
| Isolation (backend) | `fw worker --sandbox worktree\|container` | `worktree` |
| Container image | `fw worker --image IMAGE` | required with `--sandbox container` |
| Agent adapter / model | `--agent`, `model` key | `claude-code`, the CLI's own model |
| Capabilities (skills/tools/MCP) | default | all skills, no tool allow/deny lists, MCP servers from the repo's committed `.mcp.json` |
| Bash exec isolation | default | off |
| Network | default | `allow` (full egress) |
| Credentials | operator environment | inherited as-is |
| Limits | default | 500 turns, 1 retry, 300s lease; cost/token/wall-clock ceilings off |
| Landing | `land`, `verify`, `protected_paths` keys | [strategy.md](strategy.md) |
| Provisioning | none (the setup hook was removed) | -- |
| Retention | default | DONE worktree destroyed, non-DONE parked; `--worktree-retention-days` sweeps parked ones (default 7) |
| Location | fixed | `$FLYWHEEL_HOME/projects/<id>/worktrees` (`--sandbox-root PATH` overrides) |

Concurrency is not a sandbox axis: it is the `concurrency` key (worker pool size).

## Budget ceilings

`max_cost_usd`, `max_tokens`, and `wall_clock_seconds` are enforced in the harness iteration loop via `HarnessConfig` (`crates/flywheel-core/src/harness/mod.rs`; breach handling in `finalize_budget_breach`, `harness/attempt.rs`):

- **Per-run cumulative.** Cost sums `total_cost_usd` across all attempts; tokens sum `Attempt.total_tokens`; wall-clock measures elapsed from the earliest attempt's `started_at`.
- **Checked after the per-iteration rollup, before grading** — a breach pre-empts the grade, so a run that blew its budget never gets to pass.
- **Terminal `Status.FAILED`, non-retryable** (the ABORT path, not the retryable THRASH path).
- **Order: cost, then tokens, then wall-clock.** The first breach ends the run.
- Each breach emits a `harness.budget_ceiling_breached` event with `payload["ceiling"]` in `{"cost_usd", "tokens", "wall_clock_seconds"}`, distinguishing a budget kill from an agent error in audit (see [data-taxonomy.md](data-taxonomy.md)).
- **`0` is unenforced** -- the built-in default for all three; the ceilings stay a harness capability (`HarnessConfig`) with no config key.

Container runs feed this path too: the folded `CompletedRun.usage` reaches `IterationResult.usage` on every backend, so `max_tokens` still trips.

## Backend

`backend = "worktree"` (the default) runs the agent CLI as a local process (flywheel-agents `LocalHost`) against a git worktree; `backend = "container"` runs the agent as its own CLI inside a Docker image against the bind-mounted worktree. Select it with `fw worker --sandbox container --image IMAGE` (see [cli.md](cli.md)). The container backend, its image contract, auth modes, network teeth, and v1 limitations are documented in [container-backend.md](container-backend.md).

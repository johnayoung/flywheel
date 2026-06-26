# flywheel

An orchestration loop for AI coding agents: the agent is the brain, flywheel is the control plane — it invokes the agent against a structured task, verifies completion claims with graders, and records the full execution history.

## Quickstart

Requires Python 3.13 and [uv](https://docs.astral.sh/uv/).

```bash
uv sync                        # install all workspace packages
uv run flywheel init           # scaffold .flywheel/, a work policy, and the SDD skills
export ANTHROPIC_API_KEY=...   # or run `claude login` — authenticate the agent first
uv run flywheel worker --once  # claim one task, run it, exit
uv run flywheel                # operator console (alias: fw)
scripts/check.sh               # full CI gate: ruff, pyright, pytest (quiet on success)
```

The worker drives the Claude agent, so authenticate it before the first run: set `ANTHROPIC_API_KEY` or run `claude login`. Without it the first worker run fails when the agent SDK cannot authenticate.

`flywheel <verb> --help` lists each verb's flags (`status`, `live`, `history`, `show`, `say`, `interrupt`, `approve`, `reject`, `audit`, ...).

## Authoring skills

Tasks are the input to the loop; flywheel ships the spec-driven pipeline that produces them as four Claude Code skills. `flywheel init` installs them into the repo's `.claude/skills/` (prompted by default in an interactive run; `--skills` / `--no-skills` answer non-interactively), rendered for the repo's work policy (`flywheel.toml` — `/fw-plan` swaps a task-directory vs GitHub-issues delivery section by work source):

- **`/fw-spec`** — interview an idea into ungameable, end-state success criteria, written as a numbered spec.
- **`/fw-plan`** — compile a spec or request into right-sized tasks, each spined on the strongest reward-hack-resistant grader the worker can run out-of-band.
- **`/fw-retro`** — forensic audit of how the loop executed a phase; every finding carries a re-runnable CLI pointer and stops at diagnosis.
- **`/fw-improve`** — turn cited retro findings into ranked, scoped proposals, each ending in a handoff (`/fw-spec`, `/fw-plan`, or accept).

They are project-agnostic templates: re-running `flywheel init` regenerates the managed files and leaves any you have edited untouched. Each skill's cited design rationale lives in [`docs/research/`](docs/research/).

## How work lands

Each task runs in its own git worktree on a `flywheel/<phase>/<task-id>` branch. When the graders pass, the work lands through the configured landing strategy (`flywheel.toml` `[submit] strategy`):

- `merge` (default) — fast-forward the branch into the worker's base branch. If the base advanced underneath a finished task, the branch is rebased and its command graders re-run before the merge, so nothing lands that was not verified against the exact base it lands on.
- `pr` — push the branch and open a pull request with the grader receipts in the body; review and CI own the merge.

Either way the work itself is never trusted blind: agent claims feed verification, graders gate the landing, and a `[submit] protected_paths` list keeps a task from rewriting the verification surface (grader config, CI) it is judged by. The worker never commits to your branch directly.

## Packages

A uv workspace of five packages under `packages/`. Dependencies point one way only — core imports nothing downstream.

| Package | Role |
| --- | --- |
| `flywheel-core` | Lifecycle of a single task: invoke, validate envelopes, verify via graders, record attempts, retry. The agent SDK is an optional extra (`flywheel-core[claude]`); the data and lifecycle surface need no SDK. |
| `flywheel-orchestrator` | Drives many tasks on top of core: prerequisite DAG, work sources, claims/leases, multi-worker, phases, the autopilot intake daemon, and the `SubmitStrategy` landing seam. |
| `flywheel-worktree` | Git-worktree landing strategies — FF-merge (default) or pull request — plus the worker daemon. |
| `flywheel-container` | Docker sandbox execution backend: runs the agent CLI inside a container against a bind-mounted worktree. SDK-free; activated via `[sandbox] backend = "container"`. |
| `flywheel` | The `flywheel` / `fw` shell: verb router and operator console. Bundles the Claude agent SDK and the container backend. |

## Docs

Full index: **[docs/README.md](docs/README.md)**. The `docs/` specs are authoritative — they override any inferred behavior. Start with [vision.md](docs/vision.md), then read down your layer:

- **Core (a single task):** [loop.md](docs/loop.md) · [task-schema.md](docs/task-schema.md) · [task-lifecycle.md](docs/task-lifecycle.md) · [persistence-tables.md](docs/persistence-tables.md) · [data-taxonomy.md](docs/data-taxonomy.md)
- **Orchestration (many tasks):** [orchestration.md](docs/orchestration.md) · [work-sources.md](docs/work-sources.md) · [strategy.md](docs/strategy.md) · [held-out-gate.md](docs/held-out-gate.md) · [sandbox.md](docs/sandbox.md) · [container-backend.md](docs/container-backend.md)
- **Operating flywheel:** [cli.md](docs/cli.md) · [configuration.md](docs/configuration.md) · [autopilot.md](docs/autopilot.md) · [workflow.md](docs/workflow.md)

## License

[Apache-2.0](LICENSE).

# flywheel

An orchestration loop for AI coding agents: the agent is the brain, flywheel is the control plane — it invokes the agent against a structured task, verifies completion claims with graders, and records the full execution history.

## Quickstart

Install (gives `flywheel` and its alias `fw`):

```bash
cargo install --git https://github.com/johnayoung/flywheel flywheel   # Rust 1.91+
cargo install --path crates/flywheel                                   # or from a checkout
```

Prebuilt binaries (linux x86_64/aarch64, macOS x86_64/aarch64) are attached to each [GitHub Release](https://github.com/johnayoung/flywheel/releases).

```bash
flywheel init           # scaffold .flywheel/, a work policy, and the SDD skills
claude login            # or export ANTHROPIC_API_KEY=... -- authenticate the agent CLI first
flywheel worker --once  # claim one task, run it, exit
flywheel                # operator console (alias: fw)
```

Any git repo works without `flywheel init`: flywheel keeps its state under `$FLYWHEEL_HOME/projects/<id>/` (default `~/.flywheel`), keyed by an id it stores in the repo's `.git/config`. `fw project ls` lists the known projects.

Settings: `~/.flywheel/config.toml` (yours), an optional committed `.flywheel.toml` (`verify`, `protected_paths`, `land`, `project_id`; honored after `fw trust`), and a per-project `config.toml` in the home. `fw config` shows the effective values and where each came from ([docs/configuration.md](docs/configuration.md)).

The worker drives the agent through its CLI (`claude`, or `codex` via `[agent]`), so the CLI must be on `PATH` and authenticated before the first run.

`flywheel <verb> --help` lists each verb's flags (`status`, `live`, `history`, `show`, `say`, `interrupt`, `approve`, `reject`, `audit`, ...).

## Authoring skills

Tasks are the input to the loop; flywheel ships the spec-driven pipeline that produces them as Claude Code skills (five pipeline skills plus an operator runbook). `flywheel init` installs them into the repo's `.claude/skills/` (prompted by default in an interactive run; `--skills` / `--no-skills` answer non-interactively), rendered for the repo's work policy (`flywheel.toml` — `/fw-plan` swaps a task-directory vs GitHub-issues delivery section by work source):

- **`/fw-spec`** — interview an idea into ungameable, end-state success criteria, written as a numbered spec.
- **`/fw-plan`** — compile a spec or request into right-sized tasks, each spined on the strongest reward-hack-resistant grader the worker can run out-of-band.
- **`/fw-verify`** — blind-author the discriminating held-out test oracle for the planned tasks before execute, so the agent being graded never writes the test that grades it.
- **`/fw-retro`** — forensic audit of how the loop executed a phase; every finding carries a re-runnable CLI pointer and stops at diagnosis.
- **`/fw-improve`** — turn cited retro findings into ranked, scoped proposals, each ending in a handoff (`/fw-spec`, `/fw-plan`, or accept).
- **`/flywheel-ops`** — operate the running loop: start and drain the worker, read run state from grader receipts, recover parked or stranded work.

They are project-agnostic templates: re-running `flywheel init` regenerates the managed files and leaves any you have edited untouched. Each skill's cited design rationale lives in [`docs/research/`](docs/research/).

## How work lands

Each task runs in its own git worktree on a `flywheel/<phase>/<task-id>` branch. When the graders pass, the work lands through the configured landing strategy (`land = "merge" | "pr"`; `phase` and tiers through a legacy `--policy` file):

- `merge` (default) — fast-forward the branch into the worker's base branch. If the base advanced underneath a finished task, the branch is rebased and its command graders re-run before the merge, so nothing lands that was not verified against the exact base it lands on.
- `pr` — push the branch and open a pull request with the grader receipts in the body; review and CI own the merge.
- `phase` — fast-forward each task onto a per-phase integration branch, then publish the completed phase as one aggregate pull request.

`[[submit.tiers]]` routes each change to one of these by the risk tier of the files it touches.

Either way the work itself is never trusted blind: agent claims feed verification, graders gate the landing, and a `[submit] protected_paths` list keeps a task from rewriting the verification surface (grader config, CI) it is judged by. The worker never commits to your branch directly.

## Crates

A Cargo workspace of seven crates under `crates/`. Dependencies point one way only — core imports nothing downstream.

| Crate | Role |
| --- | --- |
| `flywheel-agents` | Multi-agent execution layer below core: adapters (Claude Code stream-json, Codex), normalized events, execution hosts. Depends on no other flywheel crate. |
| `flywheel-core` | Lifecycle of a single task: invoke, validate envelopes, verify via graders, record attempts, retry. |
| `flywheel-orchestrator` | Drives many tasks on top of core: prerequisite DAG, work sources, claims/leases, multi-worker, phases, the autopilot intake daemon, and the `SubmitStrategy` landing seam. |
| `flywheel-worktree` | Git-worktree landing strategies — FF-merge (default) or pull request — plus the worker daemon. |
| `flywheel-container` | Docker sandbox execution backend: runs the agent CLI inside a container against a bind-mounted worktree. Activated via `[sandbox] backend = "container"`. |
| `flywheel-hub` | `fw hub`: web workspace of server-owned agent terminals (axum server, REST + WebSocket API, embedded web UI). Depends on no other flywheel crate. |
| `flywheel` | The `flywheel` / `fw` binaries: verb router and operator console. Bundles the container backend. |

Development: `scripts/check.sh` runs the full CI gate (rustfmt, clippy, cargo test; quiet on success). See [CONTRIBUTING.md](CONTRIBUTING.md).

## Docs

Full index: **[docs/README.md](docs/README.md)**. The `docs/` specs are authoritative — they override any inferred behavior. Start with [vision.md](docs/vision.md), then read down your layer:

- **Core (a single task):** [loop.md](docs/loop.md) · [task-schema.md](docs/task-schema.md) · [task-lifecycle.md](docs/task-lifecycle.md) · [persistence-tables.md](docs/persistence-tables.md) · [data-taxonomy.md](docs/data-taxonomy.md)
- **Orchestration (many tasks):** [orchestration.md](docs/orchestration.md) · [work-sources.md](docs/work-sources.md) · [strategy.md](docs/strategy.md) · [held-out-gate.md](docs/held-out-gate.md) · [sandbox.md](docs/sandbox.md) · [container-backend.md](docs/container-backend.md)
- **Operating flywheel:** [cli.md](docs/cli.md) · [configuration.md](docs/configuration.md) · [autopilot.md](docs/autopilot.md) · [workflow.md](docs/workflow.md) · [team-mode.md](docs/team-mode.md) · [hub.md](docs/hub.md)
- **Agents:** [agent-harness.md](docs/agent-harness.md)

## License

[Apache-2.0](LICENSE).

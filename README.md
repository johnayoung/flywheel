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

A uv workspace of four packages under `packages/`. Dependencies point one way only — core imports nothing downstream.

| Package | Role |
| --- | --- |
| `flywheel-core` | Lifecycle of a single task: invoke, validate envelopes, verify via graders, record attempts, retry. The agent SDK is an optional extra (`flywheel-core[claude]`); the data and lifecycle surface need no SDK. |
| `flywheel-orchestrator` | Drives many tasks on top of core: prerequisite DAG, claims/leases, multi-worker, phases, the `SubmitStrategy` landing seam. |
| `flywheel-worktree` | Git-worktree landing strategies — FF-merge (default) or pull request — plus the worker daemon. |
| `flywheel` | The `flywheel` / `fw` shell: verb router and operator console. Bundles the Claude agent SDK. |

## Docs

Authoritative specs in `docs/` override any inferred behavior:

- [vision.md](docs/vision.md) — what the loop is and is not
- [task-schema.md](docs/task-schema.md) — `Task`/`Grader`/`Context` shape and validation
- [task-lifecycle.md](docs/task-lifecycle.md) — lifecycle states, attempts, transition rules
- [loop.md](docs/loop.md) — iteration envelope and harness behavior
- [strategy.md](docs/strategy.md) — the `SubmitStrategy` landing seam and shipped strategies
- [workflow.md](docs/workflow.md) — end-to-end run flow
- [data-taxonomy.md](docs/data-taxonomy.md) — state vs. telemetry split

## License

[Apache-2.0](LICENSE).

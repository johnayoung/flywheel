# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

Flywheel is a Python orchestration loop for AI coding agents — it owns the execution lifecycle of a single task (invoke, validate envelopes, verify via graders, record attempts, retry).

## Workspace layout (the hard line)

A uv workspace of six packages under `packages/`. Every dist name matches its import name (dashes to underscores) — `packages/<dist>/src/<import>`. The dependency arrow points one way only — core imports nothing downstream:

- **`flywheel-agents`** (import `flywheel_agents`) — bottom of the stack, below core: the multi-agent execution layer (adapters, normalized events, execution hosts; Claude Code first). Imports nothing from any flywheel package; stdlib-only at runtime, vendor SDKs are optional extras. Consumed by core via the `flywheel-core[agents]` extra. Design doc: `docs/agent-harness.md`.
- **`flywheel-core`** (import `flywheel_core`) — the lifecycle of a **single task**. Knows nothing about who calls it. No console script; invoke via `python -m flywheel_core.workflow`. This is the only package most contributors touch.
- **`flywheel-orchestrator`** — built on core: drives **many** tasks (selection over a prerequisite DAG, work sources, claims/leases, multi-worker, phases, the autopilot intake daemon). Owns its own store (`task_claims`). Library only; no console script.
- **`flywheel-worktree`** — built on the orchestrator: the git-worktree submit strategy + daemon, one worked example of a consumer. Library only; spawned via `flywheel worker`.
- **`flywheel-container`** — built on the orchestrator: the Docker sandbox execution backend (the agent CLI in headless stream-json via `docker exec` against a bind-mounted worktree, driven through the `flywheel-agents` claude-code adapter under `DockerExecHost`). SDK-free, a sibling consumer of the orchestrator like `flywheel-worktree`. Library only; activated via `[sandbox] backend = "container"`.
- **`flywheel`** (import `flywheel`) — top of stack: the unified product shell. Console scripts `flywheel` and `fw` (one implementation, two entries) route every operator verb and own the operator console. Bundles the Claude agent SDK and the container backend.

Cross-task concepts (prerequisites/DAG, scheduling, claims) live in the orchestrator, never in core. When adding to core, ask: would a single `run_task(task, lifecycle, store)` invocation — one task, no scheduler — need it? If not, it belongs above the line.

Authoritative specs in `docs/` override any inferred behavior. Full index: `docs/README.md`.

- `docs/vision.md` — what the loop is and is not
- `docs/task-schema.md` — `Task`/`Grader`/`Context` shape and validation rules
- `docs/task-lifecycle.md` — `Lifecycle`/`Attempt`/`Status`/`Outcome` and transition rules
- `docs/loop.md` — iteration envelope (`<!-- LOOP_STATUS -->`) and harness behavior
- `docs/persistence-tables.md` — the core store catalog; canonical DDL is `packages/flywheel-core/src/flywheel_core/_schema/persistence-schema.sql` (SQLite, WAL, foreign keys on, optimistic concurrency on `version`; Postgres mirror alongside)
- `docs/data-taxonomy.md` — authoritative state vs. telemetry split
- `docs/orchestration.md` — multi-task scheduling, claims/leases, the orchestrator store and ledgers
- `docs/work-sources.md` — the `WorkSource` seam and shipped sources (directory, github, github_ci, github_review)
- `docs/strategy.md` — the `SubmitStrategy`/`SandboxHandle` landing seam and shipped strategies (merge, PR, container)
- `docs/sandbox.md` + `docs/container-backend.md` — the sandbox-as-deploy model, the `[sandbox.*]` reference, and the Docker backend
- `docs/held-out-gate.md` — the execute-time held-out landing gate
- `docs/configuration.md` — the complete `flywheel.toml` reference
- `docs/cli.md` — the `flywheel`/`fw` verbs and operator console
- `docs/autopilot.md` — the autopilot intake daemon (tiers, scoring, CLI, console)
- `docs/workflow.md` — how flywheel develops itself: the spec-driven authoring pipeline and the runtime loop

New feature work is specced under `.flywheel/specs/` (there is no `docs/roadmap.md`).

## Commands

```bash
uv sync                                                   # install all workspace packages
scripts/check.sh                                          # full gate: ruff -> pyright -> pytest, quiet on success
uv run pytest packages/flywheel-core/tests/test_task.py   # one file (iterating)
uv run pytest -k transcript                                # one keyword (iterating)
uv run pytest -x -vv                                       # stop on first failure, verbose (iterating)
```

Python 3.13 required. `uv` is the package manager and task runner — do not invoke `pip` or call `python` directly. Each package keeps its own `tests/` under `packages/<pkg>/tests/`; a root `conftest.py` hosts shared fixtures (e.g. the Postgres test container).

**Run the full suite through `scripts/check.sh`, not bare `uv run pytest`.** It wraps each gate in `scripts/run_silent.sh` (context-efficient backpressure): output is suppressed on success — one `ok` line per gate — and dumped in full only on the first failure, so passing runs do not flood the transcript. Use the bare `uv run pytest -k ...` forms above only while iterating on a single test; set `RUN_SILENT_VERBOSE=1` to stream output live. Run any other verification command the same way: `scripts/run_silent.sh "<desc>" <command>`.

## Reading large modules

The `serena` MCP server (auto-loaded from the committed `.mcp.json`) provides language-server-backed symbol retrieval. For any file over ~800 lines — e.g. `_orchestrate.py`, `_claims.py`, `worker.py`, `harness.py` — use `get_symbols_overview` to map it, then `find_symbol` to pull only the target symbol and `find_referencing_symbols` for its callers. Do not `Read` a whole large module to make a localized edit: whole-file reads of these modules are the dominant per-task context cost, and reading one repeatedly across an iteration is what triggers SDK auto-compaction.

## Non-negotiable invariants

- **IMPORTANT: `flywheel_core.task` and `flywheel_core.lifecycle` are pure.** No `json`/`pathlib`/`io` imports, no `open()`. Enforced by `packages/flywheel-core/tests/test_task_module_purity.py` and `test_lifecycle_module_purity.py`. Do not weaken these tests to make a feature fit — move file/JSON code into `flywheel_core.loaders` instead.
- **IMPORTANT: `Task` is input-source agnostic.** Direct dataclass construction is a first-class API; loaders are optional conveniences. No `Task` field may name a path/file/payload/raw/source. See memory `feedback_task_schema_input_agnostic.md`.
- **Agent claims are untrusted.** Agent-reported status feeds verification, never authoritative lifecycle state. The harness owns transitions.
- **Original task definition is immutable.** Execution-time clarifications belong in lifecycle records, not in the `Task`.
- **The agent SDK is an optional extra.** `claude-agent-sdk` is `flywheel-core[claude]`, not a hard dependency: `import flywheel_core` must work without it. Every SDK touch goes through the single lazy boundary `flywheel_core._sdk` (annotations under `TYPE_CHECKING`, a local `from flywheel_core._sdk import ...` inside agent-driving functions). The product `flywheel` dist and the dev group pin the extra. Never add a top-level `import claude_agent_sdk` to a module that `flywheel_core/__init__` imports.
- **The worker never commits to the operator's branch.** Landing is the `SubmitStrategy`'s job (FF-merge or PR); phase bookkeeping lives in the `refs/flywheel/loop-base/<phase>` ref namespace, materialized into a `.loop-base` dotfile only when a phase archives. Nothing lands that was not verified against the exact base it lands on (submit-time rebase re-runs command graders).

## Conventions

- Commits use Conventional Commits (`feat:`, `fix:`, `docs:`, `chore:`, `refactor:`, `test:`) — matches existing history.
- No emojis anywhere — code, commits, docs, comments.

## After editing Python

Call `mcp__ide__getDiagnostics` on every `.py` file you touch before reporting the task done. Treat Pylance errors as build failures; fix them in the same turn rather than handing back red squiggles. Per-file diagnostics are necessary but not sufficient — they do not surface project-wide type errors or test failures. Before reporting the task done, run the full gate with `scripts/check.sh` (ruff -> pyright -> pytest, the same checks CI runs) and confirm it is green.

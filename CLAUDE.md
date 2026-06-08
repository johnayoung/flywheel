# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

Flywheel is a Python orchestration loop for AI coding agents — it owns the execution lifecycle of a single task (invoke, validate envelopes, verify via graders, record attempts, retry).

## Workspace layout (the hard line)

A uv workspace of three packages under `packages/`. The dependency arrow points one way only — core imports nothing downstream:

- **`flywheel`** (core) — the lifecycle of a **single task**. Knows nothing about who calls it. CLI: `flywheel`. This is the only package most contributors touch.
- **`flywheel-orchestrator`** — built on core: drives **many** tasks (selection over a prerequisite DAG, claims/leases, multi-worker, phases). Owns its own store (`task_claims`). CLI: `flywheel-orchestrate`.
- **`flywheel-worktree`** — built on the orchestrator: the git-worktree submit strategy + daemon, one worked example of a consumer. CLI: `flywheel-worktree`.

Cross-task concepts (prerequisites/DAG, scheduling, claims) live in the orchestrator, never in core. When adding to core, ask: would a single `run_task(task, lifecycle, store)` invocation — one task, no scheduler — need it? If not, it belongs above the line.

Authoritative specs in `docs/` override any inferred behavior:

- `docs/vision.md` — what the loop is and is not
- `docs/task-schema.md` — `Task`/`Grader`/`Context` shape and validation rules
- `docs/task-lifecycle.md` — `Lifecycle`/`Attempt`/`Status`/`Outcome` and transition rules
- `docs/loop.md` — iteration envelope (`<!-- LOOP_STATUS -->`) and harness behavior
- `packages/flywheel/src/flywheel/_schema/persistence-schema.sql` — SQLite store (WAL, foreign keys on, optimistic concurrency on `version`); Postgres mirror lives alongside it
- `docs/strategy.md` — submission strategy (deferred)
- `docs/roadmap.md` — ordered build plan; each item depends only on prior items

## Commands

```bash
uv sync                                              # install all workspace packages
uv run pytest                                        # full suite (all packages)
uv run pytest packages/flywheel/tests/test_task.py   # one file
uv run pytest -k transcript                           # one keyword
uv run pytest -x -vv                                  # stop on first failure, verbose
```

Python 3.13 required. `uv` is the package manager and task runner — do not invoke `pip` or call `python` directly. Each package keeps its own `tests/` under `packages/<pkg>/tests/`; a root `conftest.py` hosts shared fixtures (e.g. the Postgres test container).

## Non-negotiable invariants

- **IMPORTANT: `flywheel.task` and `flywheel.lifecycle` are pure.** No `json`/`pathlib`/`io` imports, no `open()`. Enforced by `packages/flywheel/tests/test_task_module_purity.py` and `test_lifecycle_module_purity.py`. Do not weaken these tests to make a feature fit — move file/JSON code into `flywheel.loaders` instead.
- **IMPORTANT: `Task` is input-source agnostic.** Direct dataclass construction is a first-class API; loaders are optional conveniences. No `Task` field may name a path/file/payload/raw/source. See memory `feedback_task_schema_input_agnostic.md`.
- **Agent claims are untrusted.** Agent-reported status feeds verification, never authoritative lifecycle state. The harness owns transitions.
- **Original task definition is immutable.** Execution-time clarifications belong in lifecycle records, not in the `Task`.

## Conventions

- Commits use Conventional Commits (`feat:`, `fix:`, `docs:`, `chore:`, `refactor:`, `test:`) — matches existing history.
- No emojis anywhere — code, commits, docs, comments.

## After editing Python

Call `mcp__ide__getDiagnostics` on every `.py` file you touch before reporting the task done. Treat Pylance errors as build failures; fix them in the same turn rather than handing back red squiggles. `uv run pytest` is necessary but not sufficient — the test suite does not surface type errors.

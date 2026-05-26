# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

Flywheel is a Python orchestration loop for AI coding agents — it owns the execution lifecycle of a single task (invoke, validate envelopes, verify via graders, record attempts, retry).

Authoritative specs in `docs/` override any inferred behavior:

- `docs/vision.md` — what the loop is and is not
- `docs/task-schema.md` — `Task`/`Grader`/`Context` shape and validation rules
- `docs/task-lifecycle.md` — `Lifecycle`/`Attempt`/`Status`/`Outcome` and transition rules
- `docs/loop.md` — iteration envelope (`<!-- LOOP_STATUS -->`) and harness behavior
- `src/flywheel/_schema/persistence-schema.sql` — SQLite store (WAL, foreign keys on, optimistic concurrency on `version`); Postgres mirror lives alongside it
- `docs/strategy.md` — submission strategy (deferred)
- `docs/roadmap.md` — ordered build plan; each item depends only on prior items
- `docs/lkg.md` — dogfooding loop: the worker runs `.workflow/lkg/`, not the live tree; promote explicitly

## Commands

```bash
uv sync                              # install deps
uv run pytest                        # full suite
uv run pytest tests/test_task.py     # one file
uv run pytest -k transcript          # one keyword
uv run pytest -x -vv                 # stop on first failure, verbose
```

Python 3.13 required. `uv` is the package manager and task runner — do not invoke `pip` or call `python` directly.

## Non-negotiable invariants

- **IMPORTANT: `flywheel.task` and `flywheel.lifecycle` are pure.** No `json`/`pathlib`/`io` imports, no `open()`. Enforced by `tests/test_task_module_purity.py` and `tests/test_lifecycle_module_purity.py`. Do not weaken these tests to make a feature fit — move file/JSON code into `flywheel.loaders` instead.
- **IMPORTANT: `Task` is input-source agnostic.** Direct dataclass construction is a first-class API; loaders are optional conveniences. No `Task` field may name a path/file/payload/raw/source. See memory `feedback_task_schema_input_agnostic.md`.
- **Agent claims are untrusted.** Agent-reported status feeds verification, never authoritative lifecycle state. The harness owns transitions.
- **Original task definition is immutable.** Execution-time clarifications belong in lifecycle records, not in the `Task`.

## Working from the roadmap

`docs/roadmap.md` is the build order. Each item is decomposed into JSON task definitions under `tasks/roadmap-NN/` that conform to `docs/task-schema.md` — these are the unit of agentic work. Before implementing a roadmap item, read its task JSON(s) and the spec doc they reference; the `graders` block defines "done."

## Conventions

- Commits use Conventional Commits (`feat:`, `fix:`, `docs:`, `chore:`, `refactor:`, `test:`) — matches existing history.
- No emojis anywhere — code, commits, docs, comments.

## After editing Python

Call `mcp__ide__getDiagnostics` on every `.py` file you touch before reporting the task done. Treat Pylance errors as build failures; fix them in the same turn rather than handing back red squiggles. `uv run pytest` is necessary but not sufficient — the test suite does not surface type errors.

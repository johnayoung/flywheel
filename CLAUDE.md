# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

Flywheel is a Rust orchestration loop for AI coding agents — it owns the execution lifecycle of a single task (invoke, validate envelopes, verify via graders, record attempts, retry). The product is the `flywheel` / `fw` binary pair (crate `crates/flywheel`).

## Workspace layout (the hard line)

A Cargo workspace of six crates under `crates/`, each with its source in `crates/<name>/src/`. The dependency arrow points one way only — core imports nothing downstream:

- **`flywheel-agents`** — bottom of the stack, below core: the multi-agent execution layer (adapters, normalized events, execution hosts; Claude Code and Codex adapters shipped). Depends on no other flywheel crate. Design doc: `docs/agent-harness.md`.
- **`flywheel-core`** — the lifecycle of a **single task**. Knows nothing about who calls it. Its workflow entry points are hidden verbs on the binary (`flywheel workflow run|is-done|set-model`). This is the only crate most contributors touch.
- **`flywheel-orchestrator`** — built on core: drives **many** tasks (selection over a prerequisite DAG, work sources, claims/leases, multi-worker, phases, the autopilot intake daemon). Owns its own store (`task_claims`). Library only.
- **`flywheel-worktree`** — built on the orchestrator: the git-worktree submit strategy + daemon, one worked example of a consumer. Library only; spawned via `flywheel worker`.
- **`flywheel-container`** — built on the orchestrator: the Docker sandbox execution backend (the agent CLI in headless stream-json via `docker exec` against a bind-mounted worktree, driven through the `flywheel-agents` claude-code adapter under `DockerExecHost`). A sibling consumer of the orchestrator like `flywheel-worktree`. Library only; activated via `[sandbox] backend = "container"`.
- **`flywheel`** — top of stack: the unified product shell. Binaries `flywheel` and `fw` (one implementation, two entries) route every operator verb and own the operator console.

Cross-task concepts (prerequisites/DAG, scheduling, claims) live in the orchestrator, never in core. When adding to core, ask: would a single `run_task(task, lifecycle, store)` invocation — one task, no scheduler — need it? If not, it belongs above the line.

Plugins (stores, work sources, submit strategies, agent adapters) are compile-time registries (`crates/*/src/*registry.rs`); there is no runtime plugin discovery.

Authoritative specs in `docs/` override any inferred behavior. Full index: `docs/README.md`.

- `docs/vision.md` — what the loop is and is not
- `docs/task-schema.md` — `Task`/`Grader`/`Context` shape and validation rules
- `docs/task-lifecycle.md` — `Lifecycle`/`Attempt`/`Status`/`Outcome` and transition rules
- `docs/loop.md` — iteration envelope (`<!-- LOOP_STATUS -->`) and harness behavior
- `docs/persistence-tables.md` — the core store catalog; canonical DDL is `crates/flywheel-core/schema/persistence-schema.sql` (SQLite, WAL, foreign keys on, optimistic concurrency on `version`; Postgres mirror `persistence-schema-postgres.sql` alongside)
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

Curated docs (`cli`, `configuration`, `task-schema`, `loop`, `workflow`, `strategy`, `sandbox`, `held-out-gate`, `autopilot`, `team-mode`) are embedded in the binary for `fw docs` (`crates/flywheel/src/docs.rs`); a rebuild picks up edits.

New feature work is specced under `.flywheel/specs/` (there is no `docs/roadmap.md`).

## Commands

```bash
scripts/check.sh                                  # full gate: rustfmt -> clippy -D warnings -> cargo test, quiet on success
cargo test -p flywheel-core --test module_purity  # one integration-test file (iterating)
cargo test -p flywheel-core harness               # one crate, one name filter (iterating)
cargo test -p flywheel-core -- --nocapture        # show test output (iterating)
cargo build --release -p flywheel                 # target/release/{flywheel,fw}
cargo install --path crates/flywheel              # install flywheel + fw from this checkout
```

Stable Rust, `rust-version` 1.91, edition 2024. Unit tests live in-module (`#[cfg(test)]`); integration tests in `crates/<name>/tests/`. Docker-backed tests (Postgres, container backend) skip themselves without a daemon.

**Run the full suite through `scripts/check.sh`, not bare `cargo test`.** It wraps each gate in `scripts/run_silent.sh` (context-efficient backpressure): output is suppressed on success — one `ok` line per gate — and dumped in full only on the first failure. Use the bare `cargo test` forms above only while iterating; set `RUN_SILENT_VERBOSE=1` to stream output live. Run any other verification command the same way: `scripts/run_silent.sh "<desc>" <command>`.

## Reading large modules

The `serena` MCP server (auto-loaded from the committed `.mcp.json`) provides language-server-backed symbol retrieval. For any file over ~800 lines — e.g. `crates/flywheel-orchestrator/src/autopilot.rs`, `crates/flywheel-core/src/workflow.rs`, `crates/flywheel-worktree/src/submitter.rs`, `crates/flywheel-worktree/src/daemon.rs`, `crates/flywheel-orchestrator/src/policy.rs`, `crates/flywheel-orchestrator/src/claims.rs` — use `get_symbols_overview` to map it, then `find_symbol` to pull only the target symbol and `find_referencing_symbols` for its callers. Do not `Read` a whole large module to make a localized edit: whole-file reads of these modules are the dominant per-task context cost, and reading one repeatedly across an iteration is what triggers auto-compaction.

## Non-negotiable invariants

- **IMPORTANT: `task.rs`, `lifecycle.rs`, and `envelope.rs` in `flywheel-core` are pure.** No `std::fs`/`std::io`/`std::env`/`std::process`/`std::net`/`tokio::`/`File::`/`read_to_string`. Enforced by `crates/flywheel-core/tests/module_purity.rs`. Do not weaken the test to make a feature fit — move file/JSON code into `crates/flywheel-core/src/loaders.rs` instead.
- **IMPORTANT: `Task` is input-source agnostic.** Direct struct construction (`Task::new`) is a first-class API; loaders are optional conveniences. No `Task` field may name a path/file/payload/raw/source. See memory `feedback_task_schema_input_agnostic.md`.
- **Agent claims are untrusted.** Agent-reported status feeds verification, never authoritative lifecycle state. The harness owns transitions.
- **Original task definition is immutable.** Execution-time clarifications belong in lifecycle records, not in the `Task`.
- **Agents are external CLIs.** Flywheel drives the `claude` (stream-json) and `codex` CLIs as subprocesses through `flywheel-agents`; there is no in-process agent SDK, and `[agent] transport = "sdk"` is rejected at config load. `flywheel-agents` must not depend on any other flywheel crate.
- **The worker never commits to the operator's branch.** Landing is the `SubmitStrategy`'s job (FF-merge or PR); phase bookkeeping lives in the `refs/flywheel/loop-base/<phase>` ref namespace, materialized into a `.loop-base` dotfile only when a phase archives. Nothing lands that was not verified against the exact base it lands on (submit-time rebase re-runs command graders).

## Conventions

- Commits use Conventional Commits (`feat:`, `fix:`, `docs:`, `chore:`, `refactor:`, `test:`) — matches existing history.
- No emojis anywhere — code, commits, docs, comments.

## After editing Rust

Before reporting the task done, run the full gate with `scripts/check.sh` (rustfmt -> clippy -D warnings -> cargo test, the same checks CI runs) and confirm it is green. Treat clippy warnings as build failures; fix them in the same turn.

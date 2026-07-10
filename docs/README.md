# flywheel documentation

flywheel is an orchestration loop for AI coding agents: the agent is the brain, flywheel is the control plane — it invokes the agent against a structured task, verifies completion claims with graders, and records the full execution history.

These docs are **authoritative specs** — they override any behavior inferred from reading the code. New to the project? Start with [vision.md](vision.md), then read down the layer that matches what you are doing.

## Start here

- [vision.md](vision.md) — what the loop is and is not, the core principles, and the North Star.

## Core — a single task (`flywheel-core`)

The lifecycle of one task: invoke, validate envelopes, verify via graders, record attempts, retry. Knows nothing about who calls it.

- [loop.md](loop.md) — the single-task control plane: the iteration envelope (`<!-- LOOP_STATUS -->`), state detection, and harness behavior.
- [task-schema.md](task-schema.md) — the `Task` / `Grader` / `Context` shape and validation rules.
- [task-lifecycle.md](task-lifecycle.md) — `Status` / `Attempt` / `Outcome` and the legal transitions.
- [persistence-tables.md](persistence-tables.md) — the core store catalog (SQLite, Postgres mirror; schema_version pinned).
- [data-taxonomy.md](data-taxonomy.md) — the authoritative-state vs. telemetry split.

## Orchestration — many tasks (`flywheel-orchestrator`, `-worktree`, `-container`)

Driving many tasks over a prerequisite DAG, landing the results, and isolating the agent. Cross-task concepts live here, never in core.

- [orchestration.md](orchestration.md) — the WorkGraph/DAG, scheduling, claims and leases, the orchestrator ledgers, `status --rollup`, and distributed mode.
- [work-sources.md](work-sources.md) — the `WorkSource` seam and the shipped sources: `directory`, `github`, `github_ci`, `github_review`.
- [strategy.md](strategy.md) — the `SubmitStrategy` / `SandboxHandle` landing seam and the shipped strategies (merge, PR, container).
- [held-out-gate.md](held-out-gate.md) — the execute-time held-out landing gate that defends against reward-hacking.
- [sandbox.md](sandbox.md) — the sandbox-as-deploy model and the complete `[sandbox.*]` reference.
- [container-backend.md](container-backend.md) — the Docker execution backend: image contract, auth modes, network teeth, limitations.

## Operating flywheel — the product shell (`flywheel` / `fw`)

- [cli.md](cli.md) — every verb, the interactive operator console, slash commands, and `init`.
- [configuration.md](configuration.md) — the complete `flywheel.toml` reference.
- [team-mode.md](team-mode.md) — the multi-operator runbook: GitHub App worker identity, ruleset requirements, and the merge queue as the distributed merge lock.
- [autopilot.md](autopilot.md) — the autopilot intake daemon: tier model, scoring, CLI, and console activation.
- [workflow.md](workflow.md) — how flywheel develops itself: the spec-driven authoring pipeline (the `fw-*` skills) and the runtime loop.

## Design rationale

- [agent-harness.md](agent-harness.md) — the `flywheel-agents` multi-agent execution layer: architecture, per-section status markers, and the remaining backlog. Phases 1-5 shipped (claude-code + codex adapters, opt-in via `[agent] id`); the legacy SDK path stays the default.
- [research/](research/) — cited design rationale for the `fw-*` authoring skills and the adoption-readiness audits. Not part of the normative spec set.

---

The five workspace packages (`flywheel-core`, `flywheel-orchestrator`, `flywheel-worktree`, `flywheel-container`, `flywheel`) each carry a package-level `README.md`; the dependency arrow points one way only, with core importing nothing downstream. See the root [README.md](../README.md) for the package map and quickstart.

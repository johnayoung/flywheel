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
- [sandbox.md](sandbox.md) — the sandbox-as-deploy model: backends and built-in defaults.
- [container-backend.md](container-backend.md) — the Docker execution backend: image contract, auth modes, network teeth, limitations.

## Operating flywheel — the product shell (`flywheel` / `fw`)

- [cli.md](cli.md) — every verb, the interactive operator console, slash commands, and `init`.
- [configuration.md](configuration.md) — the eight config keys (`config.toml`, the trust-gated `.flywheel.toml`), verb flags, and where every removed setting went.
- [team-mode.md](team-mode.md) — the multi-operator runbook: GitHub App worker identity, ruleset requirements, and the merge queue as the distributed merge lock.
- [autopilot.md](autopilot.md) — the autopilot intake daemon: tier model, scoring, CLI, and console activation.
- [workflow.md](workflow.md) — how flywheel develops itself: the spec-driven authoring pipeline (the `fw-*` skills) and the runtime loop.
- [hub.md](hub.md) — `fw hub`: the web workspace of server-owned agent terminals. Front door: security, quickstart, feature map, verbs. Details:
  - [hub/reference.md](hub/reference.md) — every verb and flag, serve flags, `hub.toml`, `notify.toml`, `plugin.toml`, keys, state-dir files.
  - [hub/api.md](hub/api.md) — REST routes, terminal and events WebSockets, plugin payloads.
  - [hub/security.md](hub/security.md) — auth, remote access, built-in TLS, machines and peers, share links.
  - [hub/workflows.md](hub/workflows.md) — scripting, launcher, suggested commands, usage, worktrees, review, handoff, fan-out, Flywheel panel.
  - [hub/internals.md](hub/internals.md) — model, lifecycle and live handoff, status detection, layout, web UI, phone, search, recording, push, doctor.

## Design rationale

- [agent-harness.md](agent-harness.md) — the `flywheel-agents` multi-agent execution layer: architecture, per-section status markers, and the remaining backlog. claude-code and codex adapters shipped; `--agent` selects one (default `claude-code`). Agents run only through their CLIs.
- [research/](research/) — cited design rationale for the `fw-*` authoring skills. Not part of the normative spec set.

---

The seven workspace crates under `crates/` (`flywheel-agents`, `flywheel-core`, `flywheel-orchestrator`, `flywheel-worktree`, `flywheel-container`, `flywheel-hub`, `flywheel`) form a one-way dependency arrow: core depends on nothing downstream, and `flywheel-agents` and `flywheel-hub` depend on no other flywheel crate. See the root [README.md](../README.md) for the crate map and quickstart.

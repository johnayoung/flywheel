# 00039 — Sandbox budget ceilings (increment D of 00036)

Status: spec. Increment D of [00036](00036-sandbox-deploy-model.md): enforce a
**per-run cost (USD) ceiling** in the harness loop — the first of the
`[sandbox.limits]` ceilings that A parsed but left inert. Establishes the breach
mechanism (terminal path + telemetry event) that the token and wall-clock
ceilings reuse later.

## Why

`[sandbox.limits]` carries `max_cost_usd`/`max_tokens`/`wall_clock_seconds` but
nothing enforces them: a runaway agent has no dollar bound. The harness already
captures `total_cost_usd` per iteration and rolls it onto the attempt — D adds
the cumulative check and a hard stop.

## Scope (decided)

- **Cost ceiling only, this increment.** `max_tokens` and `wall_clock_seconds`
  stay parsed-but-inert; they reuse D's breach mechanism in a fast-follow (tokens
  need usage-rollup semantics pinned; wall-clock needs an injectable clock). Cost
  is the headline dollar-cap and is fully deterministic via
  `signals.total_cost_usd`.
- **Per-run cumulative, terminal FAILED, non-retryable** (the resolved design
  decision). The ceiling sums `total_cost_usd` across *all* attempts of the run;
  once the run total reaches the ceiling, the run ends `Status.FAILED` directly
  (mirroring the ABORT path, not the retryable THRASH path), emitting a distinct
  `harness.budget_ceiling_breached` event so audit tells a budget kill from an
  agent error.
- **Zero is unenforced.** `max_cost_usd = 0.0` (the `fast` default) means no
  ceiling — byte-identical to today.
- **Check after the per-iteration rollup, before grading.** A breach pre-empts
  the grade: a run that blew its budget does not get to pass.

## Success criteria (each lowers to a grader)

**SC-1 — Breach is terminal and non-retryable.** With `max_cost_usd` set and a
single iteration whose cost exceeds it, the run reaches `Status.FAILED` in one
attempt (no retry despite remaining retry budget), and a
`harness.budget_ceiling_breached` event with `payload["ceiling"] == "cost_usd"`
is emitted. *Grader:* `test_budget_ceilings.py`.

**SC-2 — Cost accumulates across attempts (the per-run proof).** A run whose
attempt 1 fails validation (cost under ceiling) and retries, then whose attempt 2
pushes the *run cumulative* over the ceiling, breaches in attempt 2 — emitting the
breach event and ending `Status.FAILED` with two attempts. A per-attempt
implementation (which resets each attempt) would never breach here; the breach
event is the discriminator. *Grader:* `test_budget_ceilings.py`.

**SC-3 — Under-ceiling and zero-ceiling runs are unaffected.** A run under its
ceiling reaches its normal terminal (`DONE`) with no breach event; a zero ceiling
never enforces even at huge cost (back-compat). *Grader:* `test_budget_ceilings.py`.

**SC-4 — Config threads from policy.** `policy.sandbox.limits.max_cost_usd`
decomposes into the primitive the run consumes via `_sandbox_limit_primitives`
(mirroring `_sandbox_agent_primitives`), defaulting to `0.0` for an absent
section or a `None` policy. *Grader:* `test_sandbox_limit_threading.py`. The
decomposition is wired through `run_task_object` into `HarnessConfig.max_cost_usd`
(covered by the full suite).

**SC-5 — Back-compat preserved.** `fast` (zero ceiling) is unchanged; the full
suite, increment A/B oracles, and purity all stay green. *Grader:* full suite.

## Out of scope

`max_tokens` and `wall_clock_seconds` enforcement; lifting
`max_turns`/`max_retries`/`lease_seconds` into policy (they keep their
CLI/default path); a new `Outcome` variant (reuse `AGENT_ERROR` for the breached
attempt, as the ABORT path does).

## Tasks

- `sandbox-budget-ceilings` (core) — SC-1/2/3. `HarnessConfig.max_cost_usd`;
  per-run cumulative check after the rollup; breach -> terminal `Status.FAILED` +
  `harness.budget_ceiling_breached` event.
- `sandbox-limit-threading` (orchestrator, prereq `sandbox-budget-ceilings`) —
  SC-4. `_sandbox_limit_primitives` + wiring into `run_task_object` ->
  `HarnessConfig`.

## Anchor files

- `packages/flywheel-core/src/flywheel_core/harness.py` — `HarnessConfig`
  (~line 310); the per-iteration cost rollup `attempt.total_cost_usd += cost`
  (~3196-3198) where the cumulative check belongs; the ABORT terminal path
  (~2111-2125, direct to `Status.FAILED`) the breach mirrors.
- `packages/flywheel-core/src/flywheel_core/workflow.py` — `run_task_object`
  builds `HarnessConfig(max_retries=..., ...)` (~866); add `max_cost_usd`.
- `packages/flywheel-orchestrator/src/flywheel_orchestrator/_orchestrate.py` —
  `_sandbox_agent_primitives` (~418) to mirror; `orchestrate` threads the value.
- `packages/flywheel-orchestrator/src/flywheel_orchestrator/_policy.py` —
  `SandboxLimits.max_cost_usd` (~182), already parsed.

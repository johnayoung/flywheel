# 00042 — Token + wall-clock budget ceilings (completes increment D of 00036)

Status: spec. Increment D of [00036](00036-sandbox-deploy-model.md) shipped the
per-run **cost** ceiling ([00039](00039-sandbox-budget-ceilings.md)) but left
`[sandbox.limits] max_tokens` and `wall_clock_seconds` as parsed-but-inert
policy fields. This finishes D by enforcing both, reusing the cost ceiling's
exact mechanism.

## Why

`SandboxLimits` already carries `max_tokens` and `wall_clock_seconds`
(`_policy.py`), and `HarnessConfig` already enforces `max_cost_usd` in the
iteration loop. The two remaining ceilings have identical semantics; leaving
them unenforced means `preset = "hardened"`'s "tight cost/token/wall-clock"
promise (00036 §4) is two-thirds unmet. A hung run with no wall-clock ceiling
also cannot dispose itself (00036 factor IX).

## Scope (decided)

- **Per-run cumulative + terminal `FAILED`, non-retryable** — identical to the
  cost ceiling (00036 §7-open-1 resolved: consistency with the shipped cost
  semantics; a retryable token ceiling would re-breach immediately on a per-run
  budget).
- **`max_tokens`** sums `Attempt.total_tokens` across all attempts of the run.
- **`wall_clock_seconds`** measures elapsed wall time from the run's earliest
  attempt `started_at` to the harness `clock()`, checked at the same point.
- **Checked after the per-iteration rollup, before grading** — a breach
  pre-empts the grade, same as cost. Order: cost, then tokens, then wall-clock.
- **Same telemetry** — `harness.budget_ceiling_breached` with
  `payload["ceiling"]` of `"tokens"` / `"wall_clock_seconds"`.
- **Threaded policy-only** (no CLI flag), mirroring cost:
  `_sandbox_limit_primitives` → `_drive_under_lease` → `run_task_object` →
  `HarnessConfig`.
- **Defaults `0` = unenforced = `fast` = today** — non-breaking.

## Success criteria (each lowers to a grader)

**SC-1 — token breach is terminal + non-retryable.** A run whose cumulative
`total_tokens` reaches `max_tokens` ends `Status.FAILED` before grading, emits
one breach event with `ceiling == "tokens"`. *Grader:*
`test_budget_ceilings_tokens_walltime.py`.

**SC-2 — tokens accumulate across attempts.** A per-run proof: two attempts each
under the ceiling whose sum breaches it. A per-attempt impl would never breach.
*Grader:* same file.

**SC-3 — wall-clock breach is terminal + non-retryable.** A run whose elapsed
time reaches `wall_clock_seconds` ends `Status.FAILED` before grading, emits one
breach event with `ceiling == "wall_clock_seconds"`. *Grader:* same file.

**SC-4 — zero ceilings unenforced; back-compat.** `max_tokens = 0` and
`wall_clock_seconds = 0` enforce nothing regardless of usage/time; the full
suite stays green. *Grader:* same file + full suite.

**SC-5 — threaded from policy.** `policy.sandbox.limits.{max_tokens,
wall_clock_seconds}` decompose via `_sandbox_limit_primitives`; absent
section / `None` policy default to `0`. *Grader:*
`test_sandbox_limit_threading.py`.

## Out of scope

`max_tokens`/`wall_clock_seconds` CLI flags (policy-only, as cost is); a
soft-warn tier; per-attempt token caps.

## Task

- `sandbox-budget-tokens-walltime` (core) — SC-1..5. `HarnessConfig` gains
  `max_tokens`/`wall_clock_seconds`; the loop's budget block gains the two
  guards (shared `_finalize_budget_breach` helper); `_sandbox_limit_primitives`,
  `_drive_under_lease`, `run_task_object` thread the two primitives.

## Anchor files

- `packages/flywheel-core/src/flywheel_core/harness.py` — `HarnessConfig`
  (fields + docstring); `_run_attempt_body` budget block; `_finalize_budget_breach`.
- `packages/flywheel-core/src/flywheel_core/workflow.py` — `run_task_object`
  signature + `HarnessConfig` construction.
- `packages/flywheel-orchestrator/src/flywheel_orchestrator/_orchestrate.py` —
  `_sandbox_limit_primitives`, `_drive_under_lease`.

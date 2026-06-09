# Phase audit: 20-context-recovery-policy

**Source:** `.workflow/flywheel.sqlite`, `logs/worker/`, `.workflow/tasks/archive/20-context-recovery-policy`
**Audited:** 2026-06-04
**Wall-clock window:** 2026-06-04T17:55:03Z -> 2026-06-04T18:31:35Z (~36.5 min)

## Summary

| Metric                              | Value |
| ----------------------------------- | ----- |
| Tasks in phase                      | 6     |
| Tasks reaching DONE                 | 6     |
| Total lifecycles                    | 6     |
| Tasks requiring >1 lifecycle        | 0     |
| In-lifecycle retries                | 0     |
| `harness.crash` events              | 0     |
| `harness.budget_exceeded` events    | 0     |
| Median attempt wall-clock (seconds) | 308.75 |

One-line health verdict: **Phase ran clean** — every task reached DONE on a single attempt with all graders passing first try; the only thing worth recording is the token cost of two single-iteration attempts that no iteration-boundary detector could observe.

## Per-task findings

### clean
- `outcome-recovered` — 1 lifecycle, 1 attempt, intent=verify, both command graders pass. (`run-719841a2570e4c6898378301d54ae248`)
- `recovery-handoff-prompt` — 1 lifecycle, 1 attempt, command grader passes. (`run-03b1a6085ea54c649361d9c950bd099b`)
- `recovery-summarizer` — 1 lifecycle, 1 attempt, command grader passes. (`run-da81e9f4ff734176aba8799787f908f2`)
- `context-recovery-config` — 1 lifecycle, 1 attempt, command grader passes. (`run-c2b0ba621a0b4ef1b217980d18cea555`)
- `in-loop-verify-context-recovery` — 1 lifecycle, 1 attempt, `in-loop-verify` grader passes. (`run-01594b0aad664eabb348e4c50f1c53a2`) — see token note below.
- `context-recovery-action` — 1 lifecycle, 1 attempt, both command graders pass. (`run-d93addfcdb384dd4915d22c1cf7098c2`) — see token note below.

No multi-lifecycle, no retries, no grader flaps, no agent-vs-grader misses, no blocked/budget/protocol events for any run in the phase.

### `context-recovery-action` and `in-loop-verify-context-recovery` — long token burn (single-iteration blind spot)

**What happened**
Both tasks completed in one attempt / one iteration but consumed far more tokens and turns than the other four. From `harness.iteration_completed` payloads:

| task | turns | total_tokens | total_cost_usd | intent |
| ---- | ----- | ------------ | -------------- | ------ |
| outcome-recovered | 13 | 647,033 | 0.4751 | verify |
| recovery-handoff-prompt | 15 | 1,345,590 | 0.8580 | verify |
| recovery-summarizer | 28 | 2,811,915 | 1.8610 | verify |
| context-recovery-config | 24 | 1,758,178 | 1.0506 | verify |
| **context-recovery-action** | **72** | **14,564,848** | **6.5374** | verify |
| **in-loop-verify-context-recovery** | **67** | **14,677,387** | **6.7788** | verify |

**Evidence**
- `events` (`kind='harness.iteration_completed'`, `run-d93addfcdb384dd4915d22c1cf7098c2`): `num_turns=72`, `usage.total_tokens=14564848`, `total_cost_usd=6.5374`, `envelope.intent='verify'`, `stop_reason=end_turn`.
- `events` (`kind='harness.iteration_completed'`, `run-01594b0aad664eabb348e4c50f1c53a2`): `num_turns=67`, `usage.total_tokens=14677387`, `total_cost_usd=6.7788`.
- Exactly one `harness.iteration_completed` event per run (`SELECT COUNT(*) ... GROUP BY task_id` -> `1` for all six), so each total is a single harness iteration, not a sum across iterations.
- `logs/worker/context-recovery-action_d93addfcdb38_20260604T183135.log`: a single `attempt_started` (18:09:29) -> `iteration_completed` (18:22:01) -> `attempt_finalized` (18:22:04) sequence — ~12.5 min inside one iteration with no intermediate harness decision point.

**Diagnosis**
These two iterations each sent ~14.6M tokens across ~70 SDK turns within a single harness iteration. The phase ran with `max_iterations_per_attempt=1` (the harness default) and `context_window_tokens=None` (recovery off by default), so the iteration-boundary safety net — including the context-recovery trigger these very tasks shipped — had zero in-attempt decision points to observe or act on this burn. The loop did not misfire; it has no mid-turn visibility into a long single iteration, which is the exact "intra-iteration recovery depends on streaming usage" gap the implemented spec lists as out of scope (`00018` Scope, audit finding 3). The work succeeded on first try, so this is expense and a coverage blind spot, not waste from looping or retries.

## Loop-path marker re-check (FR-6)

- **Base SHA:** `4726ff7c6ec53ef4cafe5d5e9aa1a0357a2d4a67`
- **Re-derived signals:** `[status_or_transition]` — the new `Outcome.RECOVERED` member added in `outcome-recovered` (mechanically derived via `detect_loop_path_signals(phase_diff_vs_base(...))`).
- **`in-loop-verification` task:** `in-loop-verify-context-recovery` lifecycle `run-01594b0aad664eabb348e4c50f1c53a2` status `done`.
- **Opt-out:** absent (`load_loop_path_optout` returned `null`, no error).

**Finding:** none. A watched signal was tripped (`status_or_transition`) and is covered by a DONE `in-loop-verification` task, so the archive gate was correctly satisfied — neither FR-6a (uncovered diff) nor FR-6b (opt-out over a watched diff) applies.

## Cross-task patterns

- **Token outliers cluster on the two harness-loop tasks.** The four pure-module / config tasks stayed under 2.9M tokens; the two tasks that touch the orchestration loop (`context-recovery-action`, `in-loop-verify-context-recovery`) each exceeded 14.5M tokens — ~5-8x the phase floor. Same blind spot, two runs: `run-d93addfcdb384dd4915d22c1cf7098c2`, `run-01594b0aad664eabb348e4c50f1c53a2`. Larger blast radius (the harness file + a full real-loop integration fixture) correlated with the long single iterations.
- **No recurring error strings, no shared crash payloads, no prerequisite cascades, no worker idle gaps.** The six lifecycles ran consecutively to DONE; the only non-phase anomalies in the store (`harness.protocol_failure|1`, `harness.retry_scheduled|1`) belong to other runs, not these six (`SELECT ... WHERE run_id IN (<phase runs>) AND kind IN (...)` returned zero rows).

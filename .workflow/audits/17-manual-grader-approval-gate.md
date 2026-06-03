# Phase audit: 17-manual-grader-approval-gate

**Source:** `.workflow/flywheel.sqlite`, `logs/worker/`, `.workflow/tasks/archive/17-manual-grader-approval-gate`
**Audited:** 2026-06-03
**Wall-clock window:** 2026-06-03T14:28:06Z -> 2026-06-03T16:13:10Z (~1h45m)

## Summary

| Metric                              | Value |
| ----------------------------------- | ----- |
| Tasks in phase                      | 10    |
| Tasks reaching DONE                 | 10    |
| Total lifecycles                    | 10    |
| Tasks requiring >1 lifecycle        | 0     |
| In-lifecycle retries                | 0     |
| Attempts (all `succeeded`)          | 10    |
| `harness.crash` events              | 0     |
| `harness.budget_exceeded` events    | 0     |
| `harness.blocked` events            | 0     |
| `harness.protocol_failure` events   | 0     |
| Failed grader receipts              | 0     |
| Median attempt wall-clock (seconds) | 596.8 |
| Total agent cost (SDK-reported)     | ~$49.52 |

One-line health verdict: **Phase ran clean — every task reached DONE on a single first-pass attempt with no retries, crashes, or grader failures.**

## Per-task findings

### clean (1 lifecycle, 1 attempt, all graders pass)
- `grader-manual-module` — `run-f9503053f44d4c14be5ac35c4fb25b65` (graders: grader-manual-tests, purity-guard)
- `lifecycle-awaiting-approval-state` — `run-d1066d0fcd2248989a8514be10e9767d` (grader: lifecycle-tests)
- `control-verbs-approve-reject` — `run-21223fc3c1b847daba95810a9e56e214` (grader: verbs-and-producers-tests)
- `persist-awaiting-manual-ordinal` — `run-aa7c10c954064103af5a8356b7f3ade6` (grader: store-tests)
- `harness-manual-gate-entry` — `run-dc5c685359c146c8a6742e27e489f926` (graders: harness-tests, full-suite)
- `operator-surface-awaiting-approval` — `run-98de14a64de6493397fc3d3b506a7068` (grader: surfacing-tests)
- `resolve-manual-approval` — `run-f5380f0e6eaf439292a44aecac3955d9` (graders: resolver-tests, full-suite)
- `orchestrator-reactive-resolve` — `run-b8bad1cac58e48fc9992825a1d1cabf9` (graders: orchestrator-durability-tests, full-suite)
- `reject-feedback-into-prompt` — `run-73ccd6bca7c548cdad1907d2e6df7b8d` (grader: prompt-feedback-tests)
- `docs-manual-approval-gate` — `run-1d03221a55ad4225adcb582652f49454` (graders: docs-mention-state, full-suite)

No task triggered any finding bucket (multi-lifecycle, retry, crash, budget squeeze, grader flap, agent-vs-grader miss, blocked, protocol failure). Every declared grader produced exactly one `passed=1` receipt; no grader was skipped.

## Cross-task observations

These are observations about loop behavior, recorded with evidence. None is a misfire; they are context for the clean run.

### A. Every run was rate-limited; long wall-clocks are rate-limit-stretched, not budget-driven

All 10 `harness.iteration_completed` events carry `"rate_limited": true`, yet every one finished with `"stop_reason": "end_turn"` and no `harness.budget_exceeded` event fired. The wall-clock spread (146s to 1580s) tracks turn count and rate-limit waiting, not a cap or loop stall.

**Evidence**
- `run-dc5c685359c146c8a6742e27e489f926` (`harness-manual-gate-entry`, longest at 1580.0s) iteration payload: `"rate_limited": true, "stop_reason": "end_turn", ... "num_turns": 103, "usage": {... "total_tokens": 16708396}`.
- `SELECT rate_limited FROM events WHERE kind='harness.iteration_completed'` -> `1` for all 10 runs.
- No row in `events` has `kind='harness.budget_exceeded'` for any phase run.
- Median attempt wall-clock 596.8s; 3x threshold = 1790s; max observed 1580s falls under it (no long-wall-clock outlier by the phase-median rule).

### B. No worker logs were written to disk for this phase

`logs/worker/` contains 117 files, but the newest is dated `20260528T153302` (`worker-circuit-breaker-counts-post-spawn-crashes_406ba0_...`); the phase ran on 2026-06-03. No file matches any phase-17 task id, and `find logs .workflow -name '*.log' -newermt '2026-06-03'` returns nothing.

**Evidence**
- `ls -t logs/worker/ | head -1` -> `worker-circuit-breaker-counts-post-spawn-crashes_406ba0_20260528T153302.log`
- `find logs .workflow -name '*.log' -newermt '2026-06-03'` -> (empty)
- `ls logs/worker/ | grep -E '<phase task ids>'` -> (empty)

**Diagnosis** — This phase was driven by the worktree-merging worker (heartbeat lines `[worker] Merged flywheel/17-manual-grader-approval-gate/<task> into tasks/00016-...`). For this phase the agent transcript exists only as the ephemeral stdout heartbeat and the SQLite `events` rows; no per-task transcript was persisted to `logs/worker/`. Consequently the spec-ambiguity bucket (which reads transcripts for clarifying loops / false starts) cannot be evaluated for this phase from disk — only the lifecycle/attempt/grader telemetry in SQLite is available.

### C. The manual-approval gate was never exercised in-loop during the phase that built it

The phase implemented `AWAITING_APPROVAL`, the resolver, the `approve`/`reject` verbs, and the surfacing — but none of the 10 phase tasks declared a `manual` grader, so the loop never entered the path it was building. The new state has zero telemetry in the entire store.

**Evidence**
- `SELECT COUNT(*) FROM lifecycles WHERE status='awaiting_approval'` -> `0` (whole store, not just this phase).
- `SELECT COUNT(*) FROM events WHERE kind LIKE '%awaiting_approval%' OR kind LIKE '%manual_%'` -> `0`.
- All 15 grader receipts for the phase are `grader_type='command'`; no `manual` receipt exists.

**Diagnosis** — Verification of the gate behavior came entirely from the agent's own pytest graders (e.g. `resolver-tests`, `harness-tests`), which exercise `resolve_manual_approval` in-process. The orchestrated loop itself never produced an `AWAITING_APPROVAL` lifecycle, so there is no end-to-end in-loop confirmation that the gate-entry -> park -> resolve path behaves under the real worker as it does under the unit tests.

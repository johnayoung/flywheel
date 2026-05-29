# Phase audit: 09-loop-observability

**Source:** `.workflow/flywheel.sqlite`, `logs/worker/`, `.workflow/tasks/archive/09-loop-observability`
**Audited:** 2026-05-28
**Wall-clock window:** 2026-05-28T19:21:51Z -> 2026-05-28T19:38:58Z

## Summary

| Metric                              | Value |
| ----------------------------------- | ----- |
| Tasks in phase                      | 2     |
| Tasks reaching DONE                 | 2     |
| Total lifecycles                    | 2     |
| Tasks requiring >1 lifecycle        | 0     |
| In-lifecycle retries                | 0     |
| `harness.crash` events              | 0     |
| `harness.budget_exceeded` events    | 0     |
| Median attempt wall-clock (seconds) | 510.5 |

One-line health verdict: _Phase ran clean — both tasks landed on the first attempt with every grader green; the only observed friction was SDK rate-limiting, which the loop absorbed transparently._

## Per-task findings

### clean
- `harness-records-pre-lifecycle-crashes` — 1 lifecycle, 1 attempt, all 4 graders pass, `verify` intent honored, DONE. (`run_id: run-f5f03a51372e49d5bbf79a2ca0a7a8e0`)
- `worker-circuit-breaker-counts-post-spawn-crashes` — 1 lifecycle, 1 attempt, all 5 graders pass, `verify` intent honored, DONE. (`run_id: run-7bf56590b51b4f36b605c095de3208e9`)

No task triggered a finding bucket: no multi-lifecycle, no in-lifecycle retry, no crash, no budget squeeze, no grader flap, no agent-vs-grader miss, no blocked, no protocol failure. Both lifecycles transitioned `ready -> running -> validating -> done` with no error column set.

## Cross-task patterns

- **SDK rate-limiting on both runs (environmental, absorbed).** Every `harness.iteration_completed` event in the phase carries `"rate_limited": true`:
  - `events.id=66` (`run-f5f03a51372e49d5bbf79a2ca0a7a8e0`, attempt 1): payload `{"iteration": 1, "envelope": {"kind": "valid", "intent": "verify", ...}, "failure": null, "stop_reason": "end_turn", "rate_limited": true}`
  - `events.id=69` (`run-7bf56590b51b4f36b605c095de3208e9`, attempt 1): payload `{"iteration": 1, "envelope": {"kind": "valid", "intent": "verify", ...}, "failure": null, "stop_reason": "end_turn", "rate_limited": true}`

  **Diagnosis:** The flag is set by `harness.py:1538` when `iteration_result.signals.rate_limit_events` is non-empty for the iteration — i.e. the underlying SDK reported one or more rate-limit backoffs during the single attempt. Neither run failed, retried, or crashed as a result, so the rate-limiting was retried inside the SDK invocation and never propagated to a lifecycle transition. The boolean is the only granularity persisted; the per-iteration `rate_limit_events` count is not written to `events` or `sdk_messages`, so the audit cannot distinguish one backoff from many. This is environmental friction, not a loop misfire.

- **Wall-clock spread tracks attempt length, not loop overhead.** `harness-records-pre-lifecycle-crashes` ran 666.8s vs 354.2s for the circuit-breaker task (`attempts` table). The longer attempt is the larger task (157 vs 90 persisted `sdk_messages`), and both sit well under the median×3 long-wall-clock threshold (1531.5s). The gap between `running` and `validating` timestamps (≈10.6 min and ≈5.4 min respectively, from `timestamps_json`) is agent-execution time, not harness stall — graders consumed only ~29s and ~29s of post-iteration wall-clock combined. No idle gaps between the two lifecycles (task 2 started 6s after task 1 finished).

# Improvement proposals: 17-manual-grader-approval-gate

**Source audit(s):** `.workflow/audits/17-manual-grader-approval-gate.md` (siblings consulted for recurrence: `02-harness-resilience.md`, `08-recoverable-blocked-lifecycles.md`, `09-loop-observability.md`, `15-audit-redaction-layer.md`)
**Proposed:** 2026-06-03

## Summary

| Metric                         | Value |
| ------------------------------ | ----- |
| Findings reviewed              | 3 (phase-17 observations A, B, C) |
| Proposals (clustered)          | 3     |
| Advancing via `/define`        | 1     |
| Advancing via `/task`          | 1     |
| Accepted (no fix)              | 1     |

## Proposals (ranked by leverage)

### P1 — End-to-end in-loop verification is absent from the grader vocabulary  [New capability]

**Problem**
- Phase 17 implemented `AWAITING_APPROVAL`, the resolver, the `approve`/`reject` verbs, and the surfacing, but the loop never entered the path it built: `17:73` `SELECT COUNT(*) FROM lifecycles WHERE status='awaiting_approval'` -> `0` (whole store); `17:74` `SELECT COUNT(*) FROM events WHERE kind LIKE '%awaiting_approval%' OR kind LIKE '%manual_%'` -> `0`; `17:75` all 15 grader receipts are `grader_type='command'`, no `manual` receipt. Verification came entirely from the agent's own in-process pytest graders (`17:77`).
- Same root cause one phase deep: `08:45` — "No grader ever queries the live, long-lived `.workflow/flywheel.sqlite`... From the loop's perspective the task passed; from the next task's perspective the prerequisite was hollow." The verification surface and the deliverable were the same surface (ephemeral-store pytest + file grep).
- Cost of inaction is demonstrated, not hypothetical: in phase 08 the hollow-prerequisite pattern detonated as a 76-run `OperationalError` crash storm — `08:80` "Crash happens before the lifecycle row exists... no row, no event, no attempt, no `harness.crash` event"; `08:92` 76 crashed `run_id`s in `logs/worker/harness-recheck-primitive_cf45b5_*` with zero rows in `lifecycles`/`attempts`/`events`/`grader_results`.
- Recurrence: phases 08 and 17 (plus the dogfooding rollout recommendation at `02:136`).

**Proposed outcome**
- A feature that adds a new loop path (a lifecycle state, a grader type, a resolver, a store column the live store binds to) has that path exercised end-to-end against the real orchestrated loop and the live store at least once before its phase is declared done — so "graders pass" cannot mean "the shipped path was never run."

**Handoff:** `/define in-loop-verification-gate`
**Leverage:** Recurs across 2 audits; the one time it was left unaddressed (phase 08) it cost a 76-run crash storm and a silent 8-minute telemetry gap (`08:53`). Design question (new grader type vs dogfood-task convention vs live-store grader), so it needs discovery, not a scoped edit.
**Operator decision:** advance via `/define`.

### P2 — The worker's per-task file-log channel produces no artifacts  [Observability gap]

**Problem**
- Phase 17 wrote no worker logs: `17:62` `ls -t logs/worker/ | head -1` -> `worker-circuit-breaker-counts-post-spawn-crashes_406ba0_20260528T153302.log`; `17:63` `find logs .workflow -name '*.log' -newermt '2026-06-03'` -> (empty); `17:64` `ls logs/worker/ | grep -E '<phase task ids>'` -> (empty).
- Same gap one phase earlier: `15:48` — "No file-channel worker logs for this phase... the newest files in `logs/worker/` are dated 2026-05-28 and carry unrelated task IDs... the per-task file log channel produced no artifacts for the run."
- Why it matters, from a prior diagnosis: `08:92` — "Crashes before `create_lifecycle` are invisible to every loop subsystem except the worker log... this failure shape exists entirely off the DB." With the log channel silent, that crash class now leaves zero trace anywhere, and the audit's spec-ambiguity bucket (which reads transcripts) cannot be evaluated (`17:66`).
- Recurrence: phases 15 and 17. The channel was populated through 2026-05-28 (phases 02, 08, 09 cite live worker logs) and went silent after — consistent with a regression rather than an absent feature.

**Proposed outcome**
- Every task run by the worker leaves a durable, on-disk transcript the auditor can read, restoring the pre-2026-05-28 behavior where each `run_id` had a corresponding `logs/worker/` file.

**Handoff:** `/task` — diagnose why the worktree-merging worker stopped writing per-task logs after 2026-05-28 and restore the channel.
**Leverage:** Recurs across 2 audits; blinds an entire diagnosed failure class (`08:92`) and one of the audit buckets. Treated as a regression first (scoped diagnosis + restore); escalate to `/define` only if diagnosis reveals the worktree worker needs a redesigned transcript channel rather than a fix.
**Operator decision:** advance via `/task`.

## Not proposed (findings reviewed, no action)

- **Rate-limiting / long wall-clock (phase-17 observation A).** Every run carried `"rate_limited": true` (`17:53`) but finished `end_turn` with no `budget_exceeded` (`17:54`); max wall-clock 1580s sat under the median×3 outlier threshold (`17:55`). The same was true and accepted in phase 09 (`09:32`, `09:36`). Environmental and absorbed by the SDK — accept, do not fix. The one sliver (phase 09 noted only the boolean is persisted, not the backoff count, `09:36`) is a low-leverage telemetry nicety that never caused a misfire; not advanced.

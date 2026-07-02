# Loop retro: 05-work-redriver

**Audited:** 2026-07-02
**Wall-clock window:** 2026-07-01T17:55:00Z -> 2026-07-01T21:33:52Z
**Run set examined:** run-221d70cf9f2d4534935d44764b41df03, run-353a07590e744ba8a0a7d621f94a978f, run-65a1c4f48cee4368ac4e837e1c2d41ec, run-4aca25fdd3a34a479182c553d36a3f32, run-7ebd0559f2f840fcb9b9d72d0caf3efb, run-f4d9f1341f994b9692fb8abc714ae298, run-ca20561ae9f24948b605b5c2c2df2e38, run-ae7fbe7c20ac4509940871590c4dca2d (one run per task; no prior runs in the store for any in-scope task id).

All commands below run from the repo root against the store at `.flywheel/flywheel.sqlite`. `flywheel audit` requires the db path explicitly (`--db .flywheel/flywheel.sqlite`); `flywheel history`/`flywheel show` resolve it without a flag.

## Bucket distribution

| Bucket | Class | Findings | Candidates (single-run) | Distinct run_ids |
| --- | --- | --- | --- | --- |
| retry-storm / exhaustion | loop friction | 1 (systemic) | 0 | 8 |
| missing-observability | loop friction | 1 | 0 | 8 |
| agent-mistake | agent mistake | 0 | 0 | 0 |

Every task landed, and every task consumed its entire retry budget: all 8 first attempts were discarded on a LOOP_STATUS protocol failure and the work was re-verified by a short second attempt.

## Task ledger

- `redriver-queue-surface` — see F1, F2. `flywheel show redriver-queue-surface --json`
- `redriver-lease-sweep` — see F1, F2. `flywheel show redriver-lease-sweep --json`
- `redriver-landing-redriver` — see F1, F2. `flywheel show redriver-landing-redriver --json`
- `redriver-retry-escalation` — see F1, F2. `flywheel show redriver-retry-escalation --json`
- `redriver-prereq-redriver` — see F1, F2. `flywheel show redriver-prereq-redriver --json`
- `redriver-no-progress-backoff` — see F1, F2. `flywheel show redriver-no-progress-backoff --json`
- `redriver-human-gate-routing` — see F1, F2. `flywheel show redriver-human-gate-routing --json`
- `redriver-discipline-and-aggregate` — see F1, F2. `flywheel show redriver-discipline-and-aggregate --json`

## Findings

### F1 — retry-storm / exhaustion — loop friction — confidence: high (n=8 distinct run_ids, identical signature)

**What the loop did** (decisions/transitions, not agent prose)

In all 8 runs, attempt 1 ran long (31-89 turns, 5.9M-19.1M tokens), ended with `stop_reason: end_turn`, and the harness recorded `harness.protocol_failure` on the iteration envelope — `kind: missing` in 4 runs, `kind: truncated` ("opening fence found without a matching closing fence") in 4 runs. The harness finalized attempt 1 as `outcome: agent_error`, recorded `harness.retry_scheduled` (`max_retries: 1, retries_used: 0`), and attempt 2 emitted a valid `verify` envelope and succeeded in 6-16 turns. The per-run retry budget (1) was fully consumed on 8 of 8 runs. Per the bucket definition, the retry curve is flagged as symptom, not cause.

| run_id | task | a1 outcome / turns / tokens | envelope failure | a2 turns / tokens |
| --- | --- | --- | --- | --- |
| run-221d70cf...4df03 | redriver-queue-surface | agent_error / 72 / 14,883,758 | missing (seq 241) | 16 / 1,993,907 |
| run-353a0759...4978f | redriver-lease-sweep | agent_error / 78 / 14,569,919 | missing (seq 247) | 6 / 339,481 |
| run-65a1c4f4...d41ec | redriver-landing-redriver | agent_error / 89 / 19,117,695 | truncated (seq 292) | 16 / 2,613,704 |
| run-4aca25fd...6a3f32 | redriver-retry-escalation | agent_error / 86 / 14,534,999 | truncated (seq 271) | 12 / 917,369 |
| run-7ebd0559...caf3efb | redriver-prereq-redriver | agent_error / 54 / 12,571,037 | truncated (seq 187) | 15 / 2,559,566 |
| run-f4d9f134...4ae298 | redriver-no-progress-backoff | agent_error / 77 / 12,262,453 | missing (seq 243) | 13 / 1,578,140 |
| run-ca20561a...df2e38 | redriver-human-gate-routing | agent_error / 68 / 14,349,000 | truncated (seq 228) | 16 / 1,235,383 |
| run-ae7fbe7c...4dca2d | redriver-discipline-and-aggregate | agent_error / 31 / 5,909,643 | missing (seq 114) | 12 / 932,785 |

Retry overhead recorded in the store: 12,170,335 tokens and roughly 66 minutes of wall-clock across the 8 second attempts. Grader receipts exist only for `attempt_number: 2` in all 8 runs — no grader ran against any first attempt; the protocol failure finalized the attempt before grading.

The same signature does not recur in the 11 runs of phases 06-07 (zero `harness.protocol_failure` records in those audit streams; see the 06/07 retros).

**Contributing factors** (jointly sufficient; no single root cause)

- The `harness.protocol_failure` records themselves, in two variants: `missing` (4 runs) and `truncated` with an unmatched opening fence (4 runs).
- Context pressure during every attempt 1: `harness.context_threshold_crossed` at tier 0.5 in all 8 runs, and additionally tier 0.75 in 7 of 8 (capacity_source `sdk`, capacity 200,000 tokens). No crossing was recorded during any attempt 2.
- The failing iteration's terminal record shows `stop_reason: "end_turn"` and small final output (e.g. `output_tokens: 439` on run-ae7fbe7c, `output_tokens: 1602` on run-65a1c4f4). `rate_limited: true` appears on these records but also on every succeeding attempt-2 record, so it does not differentiate.

**Re-verifiable pointers** (paste to re-open; verbatim excerpts)

- `flywheel audit run-ae7fbe7c20ac4509940871590c4dca2d --db .flywheel/flywheel.sqlite` -> seq=114 `harness.protocol_failure` `{"kind": "missing"}`; seq=116 `harness.attempt_finalized` `{"error": "protocol failure: missing iteration envelope", "number": 1, "outcome": "agent_error"}`; seq=118 `harness.retry_scheduled` `{"max_retries": 1, "retries_used": 0}`
- `flywheel audit run-65a1c4f48cee4368ac4e837e1c2d41ec --db .flywheel/flywheel.sqlite` -> seq=292 `harness.protocol_failure` `{"detail": "opening fence found without a matching closing fence", "kind": "truncated"}`; seq=294 `harness.attempt_finalized` `{"error": "protocol failure: truncated envelope (opening fence found without a matching closing fence)", "number": 1, "outcome": "agent_error"}`
- Same-signature records in the remaining six runs: run-221d70cf (seq 241), run-353a0759 (seq 247), run-4aca25fd (seq 271), run-7ebd0559 (seq 187), run-f4d9f134 (seq 243), run-ca20561a (seq 228) — `flywheel audit <run-id> --db .flywheel/flywheel.sqlite`
- `flywheel audit run-65a1c4f48cee4368ac4e837e1c2d41ec --db .flywheel/flywheel.sqlite --json` -> seq=36 `harness.context_threshold_crossed` `{"capacity_source": "sdk", "capacity_tokens": 200000, "iteration": 1, "occupancy_tokens": 109931, "percentage": 54.9655, "tier": 0.5}`; seq=70 same kind, `"occupancy_tokens": 155530, "tier": 0.75`
- `flywheel audit run-ae7fbe7c20ac4509940871590c4dca2d --db .flywheel/flywheel.sqlite --json` -> seq=112 `harness.iteration_completed` `{"envelope": {"kind": "missing"}, ..., "num_turns": 31, "rate_limited": true, "stop_reason": "end_turn", ..., "output_tokens": 439, ...}`
- `flywheel show redriver-discipline-and-aggregate --json` -> grader receipts all `attempt_number: 2` (`behavior`, `core-purity-holdout`, `full-gate`, all `passed: true`); attempts array shows a1 `agent_error` / a2 `succeeded`. Same shape in the other seven `flywheel show <task-id> --json` outputs.
- Agent narrative, quoted as what the loop observed (untrusted, per corollary 2, never this finding's sole evidence): attempt-2 envelope reason on run-ae7fbe7c seq=176 -> "... work already committed at acf48bd with a clean tree. Prior attempt only lacked the envelope."

> Every pointer above must re-open to the same artifact. A finding whose
> pointers a skeptic cannot re-verify does not belong in this document.

### F2 — missing-observability — loop friction — confidence: high (n=8, deterministic across the run set)

**What the loop did**

For every multi-attempt run in this phase, the run-level `started_at` surfaced by `flywheel history` and `flywheel show` equals the *final* attempt's start; no run-level field covers the first attempt's start. The first-attempt start exists only in the attempt records and the audit stream. `tokens_total` on the same run record aggregates *all* attempts. Read together, the run record reports a window that excludes most of the run's recorded work: e.g. run-221d70cf reports `started_at` 18:13:58 and `finished_at` 18:21:41 (7m43s) while its own attempt records span 17:55:00 -> 18:21:41 (26m41s) and its `tokens_total` (16,877,665) includes attempt 1's 14,883,758.

**Contributing factors**

- The run record's `started_at` field value (matches attempt 2's start in all 8 runs).
- The absence of any run-level field carrying the earliest attempt start — the gap is the finding; the attempt-level records are where the true span lives.

**Re-verifiable pointers**

- `flywheel show run-221d70cf9f2d4534935d44764b41df03 --json` -> `run.started_at: "2026-07-01T18:13:58.988961+00:00"`; `attempts[0].started_at: "2026-07-01T17:55:00.968485+00:00"`; `run.tokens_total: 16877665`; `attempts[0].tokens: 14883758`
- `flywheel history --phase 05-work-redriver --json` -> every entry's `latest.started_at` equals its attempt-2 start (cross-check any entry against `flywheel audit <run-id> --db .flywheel/flywheel.sqlite` seq=5 `harness.attempt_started` ts).

## Agent-mistake candidates (not loop findings)

None. No clean-loop failure exists in this run set; all 8 tasks passed every grader on attempt 2.

# Loop retro: 06-process-supervision

**Audited:** 2026-07-02
**Wall-clock window:** 2026-07-02T15:17:15Z -> 2026-07-02T17:05:55Z
**Run set examined:** run-0b820b10cff548a882c61a345ae42f96, run-e586792803684a58a63be392414cc988, run-097faa65b8a848a1bf4cbf5fb615cf99, run-f96eaa05aabe4f09a6f883fcd485fe9e, run-37a52ae3d05c499486483bc4a506507b (one run per task; no prior runs in the store for any in-scope task id).

Commands run from the repo root; `flywheel audit` needs `--db .flywheel/flywheel.sqlite`.

## Bucket distribution

| Bucket | Class | Findings | Candidates (single-run) | Distinct run_ids |
| --- | --- | --- | --- | --- |
| retry-storm / exhaustion | loop friction | 0 | 1 | 1 |
| agent-mistake | agent mistake | 0 | 0 | 0 |

Ran essentially clean: five tasks, five single-attempt runs, every grader passed on attempt 1, valid envelope first time in all 5. One in-attempt transient retry on one run, absorbed without an attempt loss.

## Task ledger

- `supervision-policy-core` — verified clean (run-0b820b10cff548a882c61a345ae42f96; no threshold crossings, no retries). `flywheel show supervision-policy-core --json`
- `console-supervisor-respawn` — see F1.
- `autopilot-liveness-record` — verified clean (run-097faa65b8a848a1bf4cbf5fb615cf99; one tier-0.5 context threshold record, no downstream effect). `flywheel show autopilot-liveness-record --json`
- `headless-supervised-autopilot` — verified clean (run-f96eaa05aabe4f09a6f883fcd485fe9e; tier-0.5 and 0.75 threshold records, no downstream effect). `flywheel show headless-supervised-autopilot --json`
- `pool-slot-retirement` — verified clean (run-37a52ae3d05c499486483bc4a506507b; one tier-0.5 threshold record, no downstream effect). `flywheel show pool-slot-retirement --json`

## Findings

### F1 — retry-storm / exhaustion — loop friction — confidence: low (n=1; single record, single run; candidate, not systemic)

**What the loop did**

During attempt 1 of run-e586792803684a58a63be392414cc988 (`console-supervisor-respawn`), the harness classified a five-hour rate-limit rejection as transient and retried in place: `harness.transient_retry` with `retry: 1` of `max_transient_retries: 6` and `delay_seconds: 0.5`. The attempt continued, emitted a valid `verify` envelope, and finalized `succeeded`; all three graders passed on attempt 1. No attempt was lost and no run-level retry was consumed. This is a single observation — not a storm, and no recurrence exists in this phase's run set.

**Contributing factors**

- The transient-retry record itself (the only retry-shaped record in the phase's five audit streams).

**Re-verifiable pointers**

- `flywheel audit run-e586792803684a58a63be392414cc988 --db .flywheel/flywheel.sqlite --json` -> seq=281 `harness.transient_retry` `{"classification": "transient", "delay_seconds": 0.5, "iteration": 1, "max_transient_retries": 6, "reason": "rate_limit_rejected:five_hour", "retry": 1}`
- Same stream -> seq=359 `harness.iteration_completed` (envelope `valid`, intent `verify`); seq=362 `harness.attempt_finalized` `{"error": "", "number": 1, "outcome": "succeeded"}`
- `flywheel show console-supervisor-respawn --json` -> grader receipts `a1:behavior:PASS | a1:seam-holdout:PASS | a1:full-gate:PASS`

> Every pointer above must re-open to the same artifact. A finding whose
> pointers a skeptic cannot re-verify does not belong in this document.

## Agent-mistake candidates (not loop findings)

None.

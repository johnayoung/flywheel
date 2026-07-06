# Loop retro: 08-agent-usage-surface

**Audited:** 2026-07-06
**Wall-clock window:** 2026-07-06T19:10:10Z -> 2026-07-06T19:41:15Z
**Run set examined:** run-8ba2d1882d1a40718d708c0bff971a95 (fw-docs-verb),
run-487153532c5f4d1fbe49c541c7563ea9 (flywheel-ops-skill),
run-4b5726c8122745338bbf3a7eeddbd0ae (init-claudemd-breadcrumb)

## Bucket distribution

| Bucket | Class | Findings | Candidates (single-run) | Distinct run_ids |
| --- | --- | --- | --- | --- |
| missing-observability | loop friction | 1 (F1) | 1 (F3) | 3 |
| budget/context-squeeze | loop friction | 0 | 1 (F2) | 1 |
| agent-mistake | agent mistake | 0 | 0 | 0 |

Health verdict: _Ran to done across three single-attempt, zero-retry runs; the
landing stage operated — including one block-and-redrive cycle on
flywheel-ops-skill — without leaving any CLI-retrievable record of its
decisions; one context-pressure record mid-phase._

## Task ledger

- `fw-docs-verb` — verified clean at run level (1 run, 1 attempt, 0 retries,
  all five receipts pass). `flywheel show fw-docs-verb --json`. Referenced by
  F1/F3.
- `flywheel-ops-skill` — friction present; see F1, F2.
- `init-claudemd-breadcrumb` — verified clean at run level (1 run, 1 attempt,
  0 retries). `flywheel show init-claudemd-breadcrumb --json`. Referenced by
  F1/F3.

## Findings

### F1 — missing-observability — loop friction — medium (n=3 streams)

**What the loop did**
- All three landings were subject to the active held-out gate
  (`[held_out] root` configured; registrations present for two of the three
  task ids). The gate demonstrably decided landings: flywheel-ops-skill's
  landing was first blocked and then re-driven to a merge (observed in
  worker stdout during the session — context only, not store evidence).
- No run's CLI-retrievable record contains ANY landing-stage record: no
  held-out gate verdict, no park, no redrive, no merge. Every audit stream
  terminates at `harness.attempt_finalized`.
- Grader receipts surfaced by `flywheel show` are the in-task graders only;
  the held-out verdicts that gated the landings left no receipt there.

**Contributing factors** (jointly sufficient; no single root cause)
- Landing-stage events are not part of the per-run audit surface the CLI
  exposes (the streams end at attempt finalization in all three runs).
- The park record that blocks a landing carries only a summary
  (grader name + exit code) wherever it lives; the oracle stdout that would
  identify WHICH check failed is not retrievable through any CLI view, so
  the block-and-redrive on run-487153... is diagnosable only from ephemeral
  worker stdout.

**Re-verifiable pointers**
- `flywheel audit run-487153532c5f4d1fbe49c541c7563ea9 --json` -> record
  kinds present: `harness.attempt_started`, `harness.context_threshold_crossed`,
  `harness.iteration_completed`, `harness.rubric_invoked`,
  `harness.rubric_verdict`, `harness.attempt_finalized`, plus sdk:* — zero
  landing-stage kinds; terminal record verbatim:
  `2026-07-06T19:36:41.109302+00:00 seq=165 attempt=1 iter=- kind=event:harness.attempt_finalized | {"error":"","number":1,"outcome":"succeeded"}`
- Same extraction for `run-8ba2d1882d1a40718d708c0bff971a95` (terminal:
  seq=109, `attempt_finalized`, 19:20:40) and
  `run-4b5726c8122745338bbf3a7eeddbd0ae` (terminal: seq=85,
  `attempt_finalized`, 19:41:15) — zero landing-stage kinds in each.
- `flywheel show fw-docs-verb --json` -> `grader_results` names exactly
  `parity-from-anywhere, bare-listing, unknown-topic,
  parity-harness-discriminates, package-suite` — no held-out receipt,
  although `ls .flywheel/verification/held-out/` shows `fw-docs-verb.json`
  and `flywheel-ops-skill.json` registrations that gate those landings.
- Context only (not store evidence): the landed merges exist on main
  (`git log --oneline -3` -> 0b63e9c, 0b1f8df, bdf08ff).

### F2 — budget/context-squeeze — loop friction — candidate, low (n=1)

**What the loop did**
- Run `run-487153532c5f4d1fbe49c541c7563ea9` crossed the 50% context tier
  mid-iteration (occupancy 105,277 of 200,000 tokens) at 19:28:46, roughly
  halfway through a 15m05s single iteration that finished successfully
  (45 turns, 8,890,821 total tokens). No exhaustion, no retry; recorded
  pressure only.

**Re-verifiable pointers**
- `flywheel audit run-487153532c5f4d1fbe49c541c7563ea9 --json` -> seq=92:
  `{"capacity_source": "sdk", "capacity_tokens": 200000, "iteration": 1,
  "occupancy_tokens": 105277, "percentage": 52.6385, "tier": 0.5}`
- `flywheel history --phase 08-agent-usage-surface --json` -> flywheel-ops-skill
  `tokens_total: 8890821, turns_total: 45, attempts: 1, retries: 0`.

### F3 — missing-observability — loop friction — candidate, low (n=3)

**What the loop did**
- In all three runs, no metric-bearing harness record exists between
  `harness.attempt_started` and `harness.iteration_completed`, while sdk:*
  records stream continuously in that window. First-iteration spans:
  9m45s (run-8ba2..., seq 5 -> 106), 15m05s (run-4871..., seq 5 -> 160),
  4m21s (run-4b57..., seq 5 -> 82).
- Context only: the operator live surface derives its STALE marker from
  90s of idle telemetry (docs/cli.md:52); during this phase's execution the
  session operator observed a healthy first iteration displayed as
  `idle=530s STALE` (session observation, not store evidence).

**Re-verifiable pointers**
- `flywheel audit run-8ba2d1882d1a40718d708c0bff971a95` -> seq=5
  `harness.attempt_started` at 19:10:10; next harness record seq=106
  `harness.iteration_completed` at 19:19:55; sdk records fill seq 6-105.
- Same shape: `flywheel audit run-487153532c5f4d1fbe49c541c7563ea9`
  (19:21:02 -> 19:36:07) and `flywheel audit run-4b5726c8122745338bbf3a7eeddbd0ae`
  (19:36:50 -> 19:41:11).

## Agent-mistake candidates (not loop findings)

None. All three runs landed verified work through their graders in single
attempts; no clean-loop wrong-work candidate exists in the run set.

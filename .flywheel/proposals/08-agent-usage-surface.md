# Improvement proposals: 08-agent-usage-surface

**Source retro(s):** `.flywheel/audits/08-agent-usage-surface.md` (siblings consulted for recurrence: `05-work-redriver-retro.md`, `06-process-supervision-retro.md`, `07-resource-hardening-retro.md`)
**Proposed:** 2026-07-06

Note on prior action state: no `.flywheel/proposals/` doc existed before this
one, so the sibling retros' findings had neither been advanced nor formally
accepted -- the recurrence cited below was live and undecided at proposal time.

## Summary

| Metric                  | N |
| ----------------------- | - |
| Findings reviewed       | 3 |
| Proposals (clustered)   | 2 |
| Advancing via /fw-spec  | 1 |
| Advancing via /fw-plan  | 0 |
| Accepted -- do not fix  | 1 |

## Proposals (ranked by leverage)

### P1 -- The per-run CLI record omits loop decisions (landing stage; multi-attempt window)

**Problem**
- 08-agent-usage-surface F1 (missing-observability, medium, n=3 streams): no
  run's CLI-retrievable record contains any landing-stage record -- no
  held-out gate verdict, no park, no redrive, no merge -- although the active
  gate demonstrably decided landings. Verbatim pointers from the retro:
  `flywheel audit run-487153532c5f4d1fbe49c541c7563ea9 --json` -> zero
  landing-stage kinds; terminal record
  `2026-07-06T19:36:41.109302+00:00 seq=165 attempt=1 iter=- kind=event:harness.attempt_finalized | {"error":"","number":1,"outcome":"succeeded"}`;
  `flywheel show fw-docs-verb --json` -> `grader_results` names exactly
  `parity-from-anywhere, bare-listing, unknown-topic, parity-harness-discriminates, package-suite`
  (no held-out receipt) while `ls .flywheel/verification/held-out/` shows the
  registrations that gated those landings.
- Recurrence (same bucket, sibling scope): 05-work-redriver F2
  (missing-observability, high, n=8): "run-221d70cf reports `started_at`
  18:13:58 and `finished_at` 18:21:41 (7m43s) while its own attempt records
  span 17:55:00 -> 18:21:41 (26m41s) and its `tokens_total` (16,877,665)
  includes attempt 1's 14,883,758."
- Recurrence: 2 of 4 retro scopes on record (n=3 + n=8 distinct run_ids).

**Outcome**
- For any run, the loop decisions the store acted on are retrievable through
  the per-run CLI record: landing-gate verdicts (with the grader output that
  decided them), parks, redrives, and merges, plus a run window that
  faithfully covers all attempts. Checkable against the same evidence shape
  the retro used: re-running F1's kind-extraction on a future gated run
  yields landing-stage records instead of a stream that ends at
  `attempt_finalized`, and a retro of a blocked landing no longer depends on
  ephemeral worker stdout.

**Handoff:** `/fw-spec every decision the loop takes on a run -- attempts, grader verdicts, held-out gate results with their output, parks, redrives, merges -- is retrievable per-run through the CLI, with a faithful multi-attempt window`
**Leverage:** recurs in 2 of 4 scopes across 11 runs; blast radius scales with every gated landing and every multi-attempt run; cost of inaction is forensic blindness exactly where trust decisions happen (this retro had to demote a real block-and-redrive to un-citable context); fix-cost is moderate and is why the route is discovery, not a ready plan.
**Operator decision:** advance (recommended default; the operator prompt timed out -- override by editing this line and re-routing)

### P2 -- Intra-attempt telemetry cadence (accept -- do not fix)

**Problem**
- 08-agent-usage-surface F3 (missing-observability, candidate, n=3): no
  metric-bearing harness record exists between `harness.attempt_started` and
  `harness.iteration_completed` while sdk records stream continuously;
  first-iteration spans 9m45s / 15m05s / 4m21s across the three runs.
  Verbatim pointer: `flywheel audit run-8ba2d1882d1a40718d708c0bff971a95` ->
  seq=5 `harness.attempt_started` at 19:10:10; next harness record seq=106
  `harness.iteration_completed` at 19:19:55; sdk records fill seq 6-105.

**Outcome**
- None sought at this time.

**Handoff:** accept -- do not fix (all three runs completed in single attempts with zero operator interventions -- `flywheel history --phase 08-agent-usage-surface --json` -> `attempts: 1, retries: 0` on every run; the only cost in evidence is display-side, and reworking intra-attempt telemetry cadence exceeds the friction on record. Revisit if a retro ever cites an operator interrupting a healthy run it displayed as stalled.)
**Leverage:** recurs structurally every run but with zero recorded harm; fix-cost exceeds evidenced cost of inaction.
**Operator decision:** accept (as drafted; prompt timed out with no override)

## Considered, not proposed (the auditable null result)

- 08/F2 budget/context-squeeze (seq=92: `{"capacity_source": "sdk", "capacity_tokens": 200000, "iteration": 1, "occupancy_tokens": 105277, "percentage": 52.6385, "tier": 0.5}`) -- below the leverage bar: single record, single scope (n=1), the run succeeded in one attempt; the threshold record existing is the telemetry functioning, not friction to fix.
- 05/F1 (protocol-failure retry consumption, n=8) -- outside this doc's scope (its own retro; consulted only for bucket recurrence). Noted so the null result is auditable: it has never been through /fw-improve either.

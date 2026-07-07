# fw-verify record: 00074-FEATURE-redrive-gate-integrity

**Verified:** 2026-07-07
**Spec:** `.flywheel/specs/00074-FEATURE-redrive-gate-integrity.md`
**Tasks:** `.flywheel/tasks/active/10-redrive-gate-integrity/` (2 tasks, parallel)
**Oracles (git-ignored scratch):** `.flywheel/verification/00074-redrive-gate-integrity/V*/oracle.py`; execute-time adapters live in the shared `.flywheel/verification/00073-run-decision-record/driver.py`
**Registrations:** `redrive-gate-reevaluation.json` (V1, V2, V3), `park-output-excerpts.json` (V4, V5)

## Routing

- AUTHOR: criteria 1 (V1), 2+3 (V2, one scenario), 4 (V3), 8 (V4), 9 (V5).
- SKIP (already un-gameable): criteria 11-13 (operator merge gate + fenced committed suites + this stage). Visible criteria 5, 6, 7, 10 stay with the tasks' own graders.
- REPLACEMENT (spec criterion 13): unit U4 (00073 #5) re-authored blind to the new semantics — under the fix its original two-evaluation nonce-witness contract becomes realizable, so it returns to its strongest form (fourth amendment; ADMITTED, 3/3 kills, stable). Its registration remains on the landed 09 task and becomes live for any future redrive of it after phase 10 lands.

## Discrimination proofs (verified independently, not agent claims)

All gates re-run by the orchestrating session: correct reference PASSED, kills below, flake screen stable, exit 0.

| Unit | Criteria | Kills |
| --- | --- | --- |
| V1 blocked-stays-blocked | #1 | submit_bypass (the confirmed live bug); redrive_without_reevaluation; park_overwritten_in_place; force_land_after_retry_exhaustion — 4/4 |
| V2 cleared-gate-lands | #2, #3 | land_without_reevaluating (synthetic/copied verdict); dead_end; park_erased; stale_verdict_copy — 4/4 |
| V3 every-reattempt-gated | #4 | gate_parks_only_scoping; stale_pass_reuse; reevaluated_but_unrecorded; record_overwritten_on_redrive — 4/4 |
| V4 standing-verify-output | #8 | fixed_message_only; exit_code_synthesis; stale_cached_record — 3/3 |
| V5 reverify-output | #9 | receipts_in_memory_only; name_only; landed_anyway — 3/3 |
| U4 (re-authored) | 00073 #5 under new semantics | mark_redriven_without_reevaluating; overwrite_park_in_place; drop_redrive_outcome — 3/3 |

## Null-reference kill on the real system

All five V drivers RED against current main with zero DRIVER-ERRORs. V2's failure output is itself the finest evidence on record of finding 1: the run's own decision stream shows `held_out_gate_evaluated: fail` -> `landing_parked: held-out-gate` -> `landed` -> `landing_redriven: result=landed` — the 00073 records witnessing the bypass end-to-end in the store.

## Realizability engineering (applied up front this time)

- Stateful gate-registration scripts (per-evaluation output/exit schedules) realize multi-evaluation scenarios, which the 00074 semantics make possible for the first time.
- V5 forces the rebase + re-verification path by advancing the base immediately after worktree creation (prepare hook); its grader passes in-run and fails post-rebase via an invocation-scheduled script.
- The driver bridges the redrive entry point across the signature change by introspection: when a held-out-source-shaped parameter exists it is threaded; on current main the old signature is called as-is. A future incompatible change surfaces as DRIVER-ERROR (exit 3), fail-closed and distinguishable from an oracle red.
- Adapters for U4 and U6 (park-then-land case) were updated to stateful fail-then-pass oracles so they remain valid on BOTH sides of the semantics change; on current main U6 re-ran GREEN semantics-neutral, U4's new form goes green only once phase 10 lands (its registration is inert until a redrive of the landed 09 task occurs).

## Honest limits

1. This phase's own landings are performed by worker processes running current main's orchestrator (the bypass still present); if a gate FAIL parks and redrives during execution, the verdict is adjudicated manually by the operator session before acceptance.
2. V3 deliberately does not assert the standing check's output text (that is V4's criterion, owned by the parallel task) — the two registrations stay independent of each other's landing order.
3. The V oracles' landed_reference relations rest on the out-of-band git observer in the driver, not on any record field the implementation controls.

## Fences

Both task briefs already carry "Do not read or write under .flywheel/verification/" in non_goals (applied at authoring).

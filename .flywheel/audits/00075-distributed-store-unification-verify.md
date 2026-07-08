# fw-verify record: 00075-FEATURE-distributed-store-unification

**Verified:** 2026-07-08
**Spec:** `.flywheel/specs/00075-FEATURE-distributed-store-unification.md`
**Tasks:** `.flywheel/tasks/active/11-distributed-store-unification/` (5 tasks: 2 roots, 1 join, 2 leaves)
**Oracles (git-ignored scratch):** `.flywheel/verification/00075-distributed-store-unification/V*/oracle.py`; execute-time adapters `V*/sut_real.py` (per-unit, introspection-bound, fail-closed DRIVER-ERROR)
**Registrations:** `postgres-data-plane-routing.json` (V1, V2), `postgres-bootstrap-serialization.json` (V3), `distributed-read-surfaces.json` (V4)

## Routing

- AUTHOR: criteria 1+2 (V1, one unit — positive and negative space of one run), 3 (V2), 4 (V3), 6 (V4).
- SKIP (visible, tasks' own graders): criteria 5 (fail-loud) and 7 (sqlite default unchanged; also guarded by keep-green suites in every task and the operator merge gate).
- SKIP (process gates, not oracle-able): DoD-1 (suite unweakened — operator merge gate + fenced assertions), DoD-2 (blind authoring — this stage itself).
- No registration for `core-run-store-injection` (its behavior is subsumed end-to-end by V1 on the downstream routing task; it lands on its visible graders only) or `postgres-fail-loud` (visible criterion).

## Discrimination proofs (verified independently, not agent claims)

All gates re-run by the orchestrating session after blind authoring: correct reference PASSED (run-twice stable), kills below.

| Unit | Criteria | Kills |
| --- | --- | --- |
| V1 pg-complete-record | #1, #2 | terminal_summary_only; sqlite_side_writer (both tests); dual_writer (killed by the sqlite-state test alone while the postgres test passed — the split-brain is caught by negative space); schema_knob_inert — 4/4 |
| V2 claim-contention | #3 | read_then_mark TOCTOU (50 double-claims under barrier); greedy_starver (peer returned 0 claims); duplicate_reconciler — 3/3 |
| V3 bootstrap-serialization | #4 | non_idempotent_probe (UniqueViolation, 3/3 runs); opposite_lock_order deadlock (DeadlockDetected, 5/5 runs); retry_masking — killed 5/5 ONLY by the server-side pg_stat_database.deadlocks delta, no client-visible failure: the anti-masking teeth work exactly as specified — 3/3 |
| V4 distributed-reads | #6 | partial_fix (runs from pg, stop events local — died precisely on the stop-reason assertion); local_only_reader; marker_caching_writer — 3/3 |

## Post-gate contract-pinning amendments (blind-legal, gates re-run)

Both fence packs overstated criterion 2/6 into whole-workdir invariance; a hidden oracle demanding unstated behavior is a defect, so the oracles were narrowed to the declared contract with no implementation in view, and every gate re-run in full:

- V1 test 2: whole-workdir snapshot -> sqlite-state-only (content-magic + WAL/SHM/journal sidecars; seeded pre-existing db must stay byte-identical). All 4 kills preserved; correct 2-passed twice.
- V4 test 3: "workdir stays empty" -> "no file under the working copy contains the shared-store markers". All 3 kills preserved (the cache writer embeds rendered markers); correct 3-passed twice.

Consequence: a correct implementation may write non-sqlite local artifacts (logs/telemetry) without false-failing the gate.

## Null-reference kill on the real system (current main, via sut_real adapters)

- V1 RED/RED: the driven run completes (`status: done`) with the full record in `workdir/.flywheel/flywheel.db` and all five postgres tables missing — the exact split-brain observed in the infrared PR #193 smoke.
- V2 RED: two instances constructed exactly as production does today (per-instance `SqliteClaimStore`, `_orchestrate.py:2277`) each claimed all 50 tasks — exactly-once violated 50 times.
- V3 RED: 7 of 8 concurrent first-opens raised `UniqueViolation` on the unserialized `CREATE SCHEMA` race in `PostgresStore._bootstrap` (dies before lock-order deadlocks are even reached; the counter assertion stays armed against retry-masking). Serial re-open passes today.
- V4 RED on the stop-reason assertion only: run reads already route to postgres via the factory on main; `_stop_events_by_subject` (`_workflow.py:1383`) still reads local sqlite. Current main IS the partial shape criterion 6 defends against.

Null verdicts independently re-run by the orchestrating session for V1 and V4.

## Adapter-maintenance notes (for the implementing phase and future re-binds)

1. V1 detects the store-injection parameter on `run_task_object` by name containing `store` (or `backend`); a differently named parameter falls through to the legacy db_path-only call and V1 stays red — update the detection list then. db_path is deliberately kept pointing inside the working copy alongside an injected store.
2. V2 binds the claim builder in `_store_factory` (preference `build_claim_store` > `open_claim_store` > `build*` among names containing "claim"), mirroring `build_store`'s shape; if claim construction lands elsewhere the adapter degenerates to the sqlite fallback and stays red — itself a signal the pinned factory discipline was deviated from. The ready set lives in a driver-owned table; claims flow only through the real `acquire_claim` seam.
3. `load_policy` requires a `[source]` table; adapters emit `[source] kind = "directory"` plus `[store]`. DSN flows through the factory's env contract (`FLYWHEEL_PG_DSN`).
4. V4 writes the finished run via `PostgresStore.create_lifecycle` (DONE) and the stop event as `kind=STOP_PREPARE_SKIP, subject=task_id, detail=stop_reason`; rendering goes through the production argv surface (`_workflow.main(["status"|"history", ...])`) in-process, so it flips green automatically once the stop-event read routes through the factory.
5. V3 refuses to bootstrap into `public` if either postgres store loses its `schema` constructor kwarg (DRIVER-ERROR rather than vacuously green).

## Honest limits

1. This stage proves blind that discriminating oracles exist and records that proof; the execute-time gate on the agents' real work is the tasks' own command graders plus these registrations, which the orchestrator's held-out landing gate runs out-of-worktree (`[held_out] root` is configured). Adapters bind by introspection and fail closed; a DRIVER-ERROR at landing parks for operator adjudication rather than passing.
2. V3's null-kill on current main manifests as the CREATE SCHEMA race, not counter increments; the deadlock-counter assertion was proven against the synthesized retry-masking reference (5/5), not against main.
3. V1 grades the single-run datapath end-to-end through the injection seam; it does not by itself prove `_drive_under_lease` threads the store — V2 plus the task's visible graders cover the production loop construction, and the adapters bind through the production factory precisely to narrow that gap.
4. Oracles self-provision throwaway postgres containers; a Docker-less landing host would fail these graders red (fail-closed), never green.

## Fences

All five task briefs already carry "Do not read or write under .flywheel/verification/" in non_goals (applied at plan time). The registrations and oracles live only in the git-ignored verification root; nothing runnable landed in the tracked tree.

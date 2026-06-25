# fw-verify discrimination proofs — 00054 orchestrator events ledger

Date: 2026-06-25. Stage: fw-verify (held-out oracle authoring), run POST-execute
(spec 00054 shipped to `main` at commits `b8a61ac` memory+sqlite ledger /
`decb6bf` postgres mirror). 00054 adds an append-only `orchestrator_events`
ledger to the orchestrator claim store: one immutable event per COMMITTED
claim-lease transition (`acquired` / `stolen` / `renewed` / `released` /
`expired`), written in the SAME store transaction as the state change, readable
as a global stream (`list_events`) and a per-task timeline (`list_task_events`).
This is the heartbeat/reclaim event log 00049 deliberately omitted. This audit
blind-grades the event-emission behaviors.

Each oracle was authored BLIND by an independent subagent from a fenced contract
(the criterion + the `ClaimStore` lease API + the NEW read API + the
`OrchestratorEventRecord` field set + the five-member `event_type` taxonomy);
the `_claims.py` / `_claims_postgres.py` emit bodies were fenced out — authors
confirmed signatures via `inspect.signature` only, never reading the source or
the agent's own `test_orchestrator_claims` additions. Oracles live git-ignored
under `.flywheel/verification/00054/<unit>/`; only this proof is durable. Every
oracle was REAL-GRADED against the shipped store and additionally KILLS a
concrete MUTANT (`.flywheel/verification/00054/mutants_check.py`).

Discovered surface (blind, via `inspect.signature`): `InMemoryClaimStore` with
`acquire_claim(task_id, worker_id, *, now, lease_seconds, conflict_keys=frozenset())`,
`renew_claim(claim, *, now, lease_seconds)` (raises `ClaimLostError`),
`release_claim(claim, *, now=None)`, `sweep_expired_claims(*, now)`,
`list_events()`, `list_task_events(task_id)`; `OrchestratorEventRecord(id,
task_id, worker_id, event_type, version, lease_expires_at, occurred_at)`;
`event_type in {acquired, stolen, renewed, released, expired}`.

Legend: ADMITTED = discrimination-proven + flake-stable. REAL-GRADE = run against
the shipped implementation. KILL = oracle red against a deliberately-wrong
variant of the shipped store, green against shipped.

## Routing (12 criteria; held-out = #1,2,3,5,7,8,9,10,11)

- AUTHOR held-out oracle:
  - A — #1 + #2 + #5 + #7 (atomic emit & anti-phantom: a committed acquire emits
    exactly one `acquired` event; a refused acquire (live-lease OR conflict-key),
    a stale renew raising `ClaimLostError`, and a no-op release each emit NONE).
  - B — #3 (a steal — acquire over a DIFFERENT worker's lapsed lease — is recorded
    as `stolen`, distinct from `acquired`, carrying the new holder; a fresh acquire
    and a same-worker re-acquire are `acquired`).
  - C — #9 (append-only: acquire->renew->renew->release yields exactly
    [acquired, renewed, renewed, released] in id order; re-acquire APPENDS, never
    collapses/dedups/overwrites).
  - D — #8 (sweep emits exactly one `expired` event PER reaped lapsed claim with
    correct per-task worker attribution; a still-valid claim emits none).
- SKIP / real-grade probe (un-gameable, graded by the parametrized real suite;
  `.flywheel/verification/00054/probes_real_grade.py`, all GREEN vs shipped):
  - #11 (additive, non-destructive v3->v4 migration) — probe: roll a store back to
    schema v3 (drop `orchestrator_events`, sentinel=3) + a `work_items` row, reopen
    with shipped code -> claim row preserved, work-item preserved, ledger empty,
    sentinel converged to 4, no error. PASS. A drop-and-recreate migration would
    lose the rows the probe asserts survive.
  - #10 (cross-backend parity) — probe: drive an identical acquire/renew/steal/
    sweep/release sequence against `InMemoryClaimStore` and `SqliteClaimStore`;
    the 7-event ledgers are equal (type, task, worker, version, order). PASS.
  - #4 (renewed event carries post-renew version + lease expiry) — probe GREEN.
  - #6 (released event recorded on an actual release) — probe GREEN.
  - #12 (read API: global stream + per-task timeline; no event mutate/delete
    method) — probe GREEN.
- Routed to manual: none.

## Admitted oracles (blind, discrimination-proven, real-graded)

### A — atomic emit & anti-phantom (#1, #2, #5, #7)
`.flywheel/verification/00054/A-atomic-emit/test_atomic_emit.py`
- A fresh acquire by worker A -> exactly one `acquired` event (worker A, version
  matches the returned claim). A live-lease refusal (B acquires inside A's lease,
  returns None) and a conflict-key refusal (overlapping keys, returns None) each
  leave the event count UNCHANGED. A stale renew after a steal raises
  `ClaimLostError` and adds no `renewed` event. A no-op release on a stale token
  adds no `released` event.
- REAL-GRADE: GREEN against the shipped store (events emitted inside the same
  transaction as the claim mutation; refusal/stale-renew/no-op-release paths
  return/raise before any `_append_event`).
- KILL: a `PhantomOnRefusalStore` mutant (emits an `acquired` event when acquire
  returns None) -> oracle RED (`a refused acquire must emit no event ... 1 -> 2`).
  This is the D-1 anti-hack: an event written outside the committed state change.

### B — steal distinct from acquired (#3)
`.flywheel/verification/00054/B-steal-taxonomy/test_steal_taxonomy.py`
- A acquires t; B acquires t after A's lease lapses -> timeline
  [acquired(A), stolen(B)]; event 2 is `stolen` (not `acquired`), worker B. A
  fresh acquire and a same-worker re-acquire of a lapsed lease are both `acquired`
  (pins steal to the different-worker-lapsed case; neither "always acquired" nor
  "always stolen" passes).
- REAL-GRADE: GREEN against the shipped store (`stolen` only when an existing
  different-worker row is reclaimed; the live-different-worker case already
  returned None).
- KILL: a `CollapseStealStore` mutant (relabels the steal `acquired`) -> oracle
  RED on `events[1].event_type == "stolen"`. This is the headline reclaim-
  invisibility hack the criterion defends against.

### C — append-only ledger (#9)
`.flywheel/verification/00054/C-append-only/test_append_only.py`
- acquire->renew->renew->release on one task -> exactly
  [acquired, renewed, renewed, released] (length 4), strictly increasing ids, the
  two renews distinct rows; re-acquiring after release APPENDS a 5th `acquired`
  event with the prior four ids unchanged.
- REAL-GRADE: GREEN against the shipped store (every committed transition inserts
  a fresh monotonic-id row; no dedup/upsert/clear).
- KILL: a `DedupConsecutiveStore` mutant (collapses consecutive same-type events,
  so the two renews become one) -> oracle RED on the length/sequence assertion.
  This is the lossy-ledger hack: a clean-looking but forensically useless history.

### D — sweep expired attribution (#8)
`.flywheel/verification/00054/D-sweep-expired/test_sweep_expired.py`
- t1/A and t2/B lapsed, t3/C still valid at sweep time; `sweep_expired_claims`
  -> exactly TWO `expired` events {(t1,A),(t2,B)}, none for t3, each carrying the
  reaped task's own worker.
- REAL-GRADE: GREEN against the shipped store (one `expired` event per reaped
  claim, still-valid claim untouched).
- KILL: a `BulkSweepStore` mutant (one bulk `expired` event for the whole sweep)
  -> oracle RED (`len(expired) == 2`). This is the lost-attribution hack
  (single-bulk-event, or marking a live claim) the criterion defends against.

## Discrimination gate result

`uv run python .flywheel/verification/00054/mutants_check.py` -> **Killed 4/4
mutants; correct reference passed all 4.** `uv run pytest .flywheel/verification/00054/`
-> 4 passed, stable across two runs (flake screen). `uv run python
.flywheel/verification/00054/probes_real_grade.py` -> 3/3 probe groups PASS.
Every oracle both passes the shipped code and kills a concrete plausible-wrong
variant; none is a green-test-that-grades-nothing. Integrated main: ruff 0,
pyright 0, 2016 passed.

## Honest limits

- **#10 Postgres parity verified for memory<->sqlite directly; Postgres is by the
  parametrized suite.** The probe drives `InMemoryClaimStore` vs `SqliteClaimStore`
  (durable, on-disk) and proves equal ledgers. The `PostgresClaimStore` mirror
  (`decb6bf`) is covered by `test_orchestrator_claims.py`, which parametrizes the
  same assertions over memory/sqlite/postgres and skips the postgres backend
  cleanly when no `postgres_dsn` is configured. A no-DSN local run does not
  exercise the postgres path; CI with a DSN does.
- **#11 migration is a real-grade probe, not a blind oracle.** Preservation of
  rows across an additive schema bump is a state/schema check (the fw-verify SKIP
  bucket); the probe discriminates a destructive migration by asserting the
  claim/work-item rows survive and the sentinel converges, but it was authored by
  the orchestrator, not blind. Its durable guard is the tasks' own command graders
  plus this probe.
- **Visible criteria #4/#6/#12 are not held-out.** They are graded by the agent's
  committed tests in the normal suite; the probe re-confirms them against shipped
  code for the record, but they were never blind-authored (the spec tagged them
  `visible`, so the agent iterates against them).
- **Registration deferred.** No `[held_out] root` is configured in `flywheel.toml`,
  so there is nowhere to write a `<held-out-root>/<task_id>.json` registration; the
  durable proof is this audit + the tasks' own committed command graders
  (orchestrator + core suites) + CI. When a held-out root is set, oracles A-D can
  be registered to gate the real run out-of-band (the 00051 channel).

## Fence (operator applies if re-running these tasks)

`non_goals`: "Do not read or write under `.flywheel/verification/`" — keeps a
future blind oracle for the same criterion out of the implementing agent's view.
(The 00054 tasks are already archived; recorded here for the record.)

This stage proves BLIND that a discriminating oracle exists for each held-out
event-emission behavior and records that proof. The execute-time gate on the
agent's real work stays the task's own command graders and tests (the durable,
CI-run guard). A held-out suite is a filter, not a correctness guarantee.

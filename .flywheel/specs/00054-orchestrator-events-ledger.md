# Feature: Orchestrator Events Ledger

## Outcome
The orchestrator store gains a durable, append-only `orchestrator_events`
ledger that records every committed claim-lease transition (acquire, steal,
renew, release, expiry-sweep) as its own immutable row. After a worker holds,
renews, loses, and another worker reclaims a task, an operator can read back the
full holder timeline for that task — who held it, when each lease was renewed,
when it was stolen or swept — from the store alone, with no live process
running. This is the heartbeat/reclaim event log that spec 00049 deliberately
omitted (claims are transient coordination state today and write no history).

## Background
The `ClaimStore` (`_claims.py`, `_claims_postgres.py`) drives the multi-worker
lease lifecycle — `acquire_claim` / `renew_claim` / `release_claim` /
`sweep_expired_claims`, plus the implicit steal-on-expiry inside `acquire_claim`
— but each transition only mutates the live `task_claims` row and leaves no
trace. When a worker crashes and its task is reclaimed, there is no durable
record of when it last proved liveness (its last renew), when the lease lapsed,
or who took the task over. The orchestrate loop's whole heartbeat/reclaim story
(`_orchestrate.py`) is therefore unobservable after the fact. The tacit
requirement an optimizing agent would miss: the value is not "log some events"
but a *forensically complete and trustworthy* trace — every committed
transition present, no transition for a no-op or refused operation, never
collapsed or rewritten — so the ledger can be trusted to reconstruct what
actually happened. A ledger with phantom or missing events is worse than none.

## Scope
### In scope
- A new additive `orchestrator_events` table on the orchestrator store (both
  SQLite and Postgres backends, plus the in-memory test substrate), with a
  v3->v4 `orchestrator_schema_version` bump via additive forward migration.
- One immutable event row written, in the same store transaction as the claim
  state change, for each committed transition: `acquired`, `stolen`, `renewed`,
  `released`, `expired`.
- A read API exposing the global event stream (insertion order) and a single
  task's event timeline.

### Out of scope
- Wiring `sweep_expired_claims` into the orchestrate loop. It is a pre-existing
  un-wired 00049 method; the ledger records `expired` wherever the sweep is
  invoked, and proving that is a store-level concern.
- Pruning, retention, rotation, or any cap on ledger growth (a separate future
  spec).
- Source-sync events. Source observability already has a durable home in the
  `source_syncs` table (00048); it stays there.
- Any operator CLI/TUI/HTTP surface that renders the ledger. The read API is
  library-only this spec.
- A `lost` event on a failed `renew_claim`. A rejected renew changes no committed
  state, so there is no row mutation to attach an event to; the reclaim is
  captured by the new holder's `stolen` event and the sweep's `expired` event.
- Any change to `flywheel-core`'s schema or the `persistence-schema*.sql` files
  (those are core's; the orchestrator store schema is inline in `_claims*.py`).

### Must not regress
- Existing `ClaimStore` semantics: `acquire_claim` / `renew_claim` /
  `release_claim` / `sweep_expired_claims` return values, conflict-key refusal
  (00049), and optimistic-concurrency rejection are unchanged.
- A pre-existing v1/v2/v3 orchestrator store still opens (now converging its
  sentinel to v4) with its `task_claims` and `work_items` rows intact.
- Existing `work_items` / `work_item_dependencies` / `source_syncs` behavior.
- `flywheel-core` purity and schema are untouched (orchestrator-only change).

## Success Criteria
Each criterion is atomic, grades an observable end-state, and carries its grader
type, visibility, the exact check, and the gaming move it forecloses. `/fw-plan`
lowers each one to a command / transcript / rubric / manual grader.

1. When a worker acquires a previously-unheld task, the ledger gains exactly one
   event for that `task_id` of type `acquired`, carrying that worker id and the
   claim's post-acquire version. [command | held-out]
   verify: on a fresh store, `acquire_claim(t, A)`; the per-task event list for
   `t` has length 1, type `acquired`, `worker_id == A`, `version` equal to the
   returned claim's version.
   defends against: a silent ledger (logging nothing), or logging a typeless/
   workerless event that cannot be traced back to a holder.

2. If `acquire_claim` returns `None` (refused — a live lease held by a different
   worker, or a conflict-key overlap), then the ledger gains no event for that
   task. [command | held-out]
   verify: with a live claim by A on `t`, `acquire_claim(t, B)` returns `None`;
   the per-task event list for `t` is unchanged in length. Likewise a
   conflict-key refusal adds no event.
   defends against: writing a phantom `acquired` event for a refused acquire —
   i.e. emitting the event outside the state-change transaction.

3. When a worker acquires a task whose prior lease has lapsed and was held by a
   different worker (a steal), the ledger gains exactly one event of type
   `stolen` (distinct from `acquired`) carrying the new holder's worker id.
   [command | held-out]
   verify: A acquires `t`; advance `now` past the lease; B acquires `t`; the
   per-task event list for `t` is `[acquired(A), stolen(B)]` in order, the steal
   event's type is not `acquired`, and its `worker_id == B`.
   defends against: collapsing a steal into a plain `acquired` event, which makes
   reclaim — the ledger's headline purpose — invisible.

4. When a held lease is renewed, the ledger gains exactly one event of type
   `renewed` carrying the post-renew version and lease-expiry. [command | visible]
   verify: `renew_claim(claim)`; a new `renewed` event appears whose `version`
   equals the returned claim's version and whose `lease_expires_at` equals the
   returned claim's lease expiry.
   defends against: dropping renews so the heartbeat/liveness trace is absent
   (the operator explicitly required renew transitions to be recorded).

5. If `renew_claim` raises `ClaimLostError` (the caller's token no longer
   matches and no row is updated), then the ledger gains no event. [command | held-out]
   verify: A acquires `t`; B steals `t`; A's `renew_claim(stale)` raises
   `ClaimLostError`; the per-task event list for `t` gains no event from that
   failed renew.
   defends against: writing a `renewed` event when nothing was renewed — i.e.
   decoupling the event from the actual row update.

6. When `release_claim` deletes the caller's matching live claim row, the ledger
   gains exactly one event of type `released`. [command | visible]
   verify: A acquires then releases `t` with the matching token; the per-task
   event list for `t` ends with a `released` event for worker A.
   defends against: leaving the timeline open-ended by never recording release.

7. If `release_claim` is called with a stale or already-stolen token (it deletes
   no row — a no-op), then the ledger gains no `released` event. [command | held-out]
   verify: A acquires `t`; B steals `t` (A's token now stale); A calls
   `release_claim(stale)`; the per-task event list for `t` is `[acquired(A),
   stolen(B)]` with no trailing `released`.
   defends against: logging a phantom `released` for a no-op release — i.e. an
   event not tied to an actual row deletion.

8. When `sweep_expired_claims` reaps lapsed claims, the ledger gains exactly one
   `expired` event per reaped task, each carrying the reaped task's worker id;
   a still-valid claim produces no `expired` event. [command | held-out]
   verify: A holds `t1` and B holds `t2`, both lapsed at `now`, and C holds `t3`
   still valid; `sweep_expired_claims(now=now)`; the ledger gains exactly two
   `expired` events (one for `t1`/A, one for `t2`/B) and none for `t3`.
   defends against: emitting a single bulk event (losing per-task attribution),
   or marking a still-live claim expired.

9. The ledger is append-only: a sequence of committed state changes on one task
   produces one event per change, in insertion order, never collapsed,
   deduped, or overwritten. [command | held-out]
   verify: `acquire -> renew -> renew -> release` on one task yields a per-task
   event list of exactly `[acquired, renewed, renewed, released]` in id order;
   re-acquiring the same task after release appends a further `acquired` event
   rather than replacing any prior row.
   defends against: upserting/deduping events so the history is lossy — a
   clean-looking but forensically useless ledger.

10. Given the identical sequence of claim transitions, the SQLite and Postgres
    backends produce equal event records — same types, worker ids, versions, and
    ordering (modulo backend-assigned row ids). [command | held-out]
    verify: drive the same acquire/renew/steal/sweep/release sequence against a
    `SqliteClaimStore` and a `PostgresClaimStore`; the read-back event lists are
    equal after dropping the row-id field.
    defends against: implementing the ledger on one backend only, or with
    divergent type spellings, so distributed (Postgres) deployments silently
    lose or mis-record events.

11. When a pre-existing v3 orchestrator store is opened by the v4 code, it opens
    without error, retains all its `task_claims` and `work_items` rows, and
    exposes an initially-empty `orchestrator_events` ledger. [command | held-out]
    verify: build a store at schema v3 with claim and work-item rows; open it
    with the v4 build; no `OrchestratorSchemaError`; the prior rows are intact;
    the global event list is empty. Assert on both backends.
    defends against: a drop-and-recreate migration that wipes existing claims or
    work items — the non-destructive-bump invariant every prior orchestrator
    schema change has held.

12. The store exposes a read API for the global event stream (all events in
    insertion order) and a single task's timeline, and exposes no method that
    updates or deletes an event. [command | visible]
    verify: a global list returns every recorded event in id order; a per-task
    accessor returns only that task's events in id order; the public store API
    has no event-mutating or event-deleting method.
    defends against: writing events that can never be read (a useless ledger),
    or providing an edit/delete path that would let history be rewritten.

Verification surface: unchanged.

## Decomposition Hint (for /fw-plan)
Two layers split along the backend boundary, mirroring 00048/00049.

- Layer A (event model + SQLite/in-memory store): satisfies #1-9, #12, and #11
  for the SQLite/in-memory backends. Introduces the event-type taxonomy and the
  event record, the `orchestrator_events` table in `_SCHEMA_SQL`, the
  same-transaction emit inside `acquire_claim` / `renew_claim` / `release_claim`
  / `sweep_expired_claims` on `InMemoryClaimStore` and `SqliteClaimStore`, the
  read API, and the additive v3->v4 sentinel bump + forward migration. Depends
  on nothing.
- Layer B (Postgres mirror): satisfies #10 and #11 for Postgres. Mirrors the
  table, the same-transaction emit, the read API, and the additive migration in
  `PostgresClaimStore`. Depends on Layer A — it inherits the event-type
  taxonomy and the event-record shape as fixed contracts.

Shared invariants both layers assert against (name them so the slices move
together): the event-type set `{acquired, stolen, renewed, released, expired}`;
the event-record field set (task id, worker id, type, version, lease-expiry,
occurred-at timestamp, insertion id); and `CURRENT_ORCH_SCHEMA_VERSION = 4`.

## Decisions Log (ADR-style: immutable, supersede — never edit)

### D-1: Event emission is a same-transaction structural consequence of each committed claim state change  (Status: Accepted)
- Context: a forensic ledger is only trustworthy if it cannot drift from the
  state it records. | Decision: each event row is INSERTed in the same store
  transaction as the `task_claims` mutation it describes, so a committed
  transition always has its event and a rolled-back/refused/no-op operation
  writes none. Operator-confirmed.
- Rejected: best-effort emission after the state change, outside its transaction
  — a crash or refusal between the two writes leaves the ledger with phantom or
  missing events, defeating the forensic purpose and trivially gameable.
- Consequences: the emit point is the store method itself (not the orchestrate
  loop), so the ledger cannot be bypassed by a new or forgotten callsite.

### D-2: Event taxonomy is five types, one per committed state change; steal is distinct from acquire  (Status: Accepted)
- Context: the events must let an operator reconstruct holder handoff. |
  Decision: `acquired` (fresh insert or own re-acquire), `stolen` (acquire over a
  different worker's lapsed lease), `renewed`, `released` (an actual row
  deletion), `expired` (one per claim reaped by a sweep). Each corresponds to a
  committed `task_claims` insert/update/delete.
- Rejected: collapsing `stolen` into `acquired` (reclaim becomes invisible);
  adding a `lost` event on a failed renew (no committed state change to attach
  it to — the new holder's `stolen` event and the sweep's `expired` event
  already capture reclaim, and the loser's loss is derivable from the timeline).
- Consequences: a refused acquire, a stale renew, and a no-op release each
  produce zero events — the anti-phantom criteria #2/#5/#7.

### D-3: Renew transitions ARE recorded  (Status: Accepted)
- Context: operator chose "include renews" over a lifecycle skeleton. |
  Decision: each successful `renew_claim` writes a `renewed` event, so the ledger
  is the heartbeat trace, not just acquire/release bookends.
- Rejected: skeleton-only (acquire/steal/release/expire) — loses the
  last-known-alive signal that the reclaim forensic depends on.
- Consequences: higher row volume on long runs (~lease/3 renews per task);
  acceptable for append-only infra, and pruning/retention is a separate future
  spec, explicitly out of scope here.

### D-4: Claim-lifecycle only; source-sync observability stays in `source_syncs`  (Status: Accepted)
- Context: source syncs already have a durable table (00048). | Decision: the
  ledger records claim/lease transitions only.
- Rejected: a unified ledger that also absorbs source-sync runs — duplicates and
  migrates existing working state for no new capability (scope creep).
- Consequences: one concern per table; the v3->v4 bump stays cleanly additive.

### D-5: Additive v3->v4 bump, non-destructive on both backends  (Status: Accepted)
- Context: every prior orchestrator schema change (v1->v2->v3) has preserved
  existing rows. | Decision: add `orchestrator_events` via `CREATE TABLE IF NOT
  EXISTS`, converge the sentinel forward, leave all existing tables untouched —
  mirroring the v2/v3 precedent exactly on SQLite and Postgres.
- Rejected: drop-and-recreate (wipes live `task_claims`/`work_items` rows).
- Consequences: a store written by an even-newer build still trips the existing
  mismatch guard; only forward convergence from v1/v2/v3 is silent.

### D-6: Schema and emit live inline in `_claims.py` / `_claims_postgres.py`  (Status: Accepted)
- Context: the orchestrator store has no `.sql` mirror file; its DDL is inline,
  and the `persistence-schema*.sql` files belong to flywheel-core. | Decision:
  the new table DDL and the event-emit logic go inline in the two orchestrator
  store modules; core is not touched.
- Rejected: adding the table to core's schema files (violates the one-way
  dependency arrow and core purity).
- Consequences: keeps the orchestrator's own-persistence boundary intact.

## Open Questions (accepted gaps)
None. Every criterion lowers to a `command` grader (pytest assertions against
the stores, the Postgres-container fixture for #10/#11), with the highest-stakes
ones marked held-out for fw-verify.

## Next Steps
Run `/fw-plan 00054-orchestrator-events-ledger` to compile these criteria into
flywheel tasks and graders.

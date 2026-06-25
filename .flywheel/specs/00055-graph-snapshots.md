# Feature: WorkGraph Snapshots

## Outcome
The orchestrator store gains a durable, append-only record of point-in-time
WorkGraph snapshots. Each snapshot is a coherent cross-section of the whole
graph as a scheduling pass saw it: one row per work item carrying that item's
source provenance, priority, capabilities, conflict keys, lifecycle state,
readiness, current claim holder, and resolved prerequisite edges, plus a header
that stamps the snapshot's position in the 00054 `orchestrator_events` ledger.
After a real orchestrate run, an operator can read back, from the store alone
with no live process, the sequence of graph states the worker scheduled
against — and reconstruct the exact graph state at any moment by replaying the
events that follow a snapshot's recorded cursor. Snapshots are the
materialized-state half of WorkGraph observability; 00054's event ledger is the
delta half.

## Background
The orchestrate loop rebuilds the WorkGraph every pass (`_orchestrate.py`:
`items = source.list_work()` -> `WorkGraph.build(items)` -> a `states` map ->
`ready_set(...)`), decides what to dispatch, and then discards that
cross-section. Nothing durable records what the graph looked like when a
decision was made: which items existed, which were ready, who held a claim,
what each item's lifecycle state was. The 00054 ledger captures the *deltas*
(claim acquired/stolen/renewed/released/expired) but not the *base state* those
deltas mutate, so after the fact you can replay transitions but have no anchor
to replay them from. The tacit requirement an optimizing agent would miss: the
value is not "store some snapshots" but a *faithful and reconstructable*
cross-section. A snapshot of placeholder/empty rows would satisfy a naive
"non-empty" check while recording nothing about the actual graph, and a
snapshot whose event-ledger cursor is stale or absent breaks the
snapshot-plus-events reconstruction that is the whole point of doing this now.

## Scope
### In scope
- A new additive durable record of graph snapshots on the orchestrator store
  (both SQLite and Postgres backends, plus the in-memory test substrate), with
  a v4->v5 `orchestrator_schema_version` bump via additive forward migration.
- A snapshot capturing one entry per work item with its full materialized
  state: task id, source provenance (kind/ref/url/version), priority, required
  capabilities, conflict keys, lifecycle state, readiness flag, current claim
  holder (or none), and resolved prerequisite ids.
- A snapshot header stamping the capture time and the `orchestrator_events`
  high-water mark (last event id) at the moment of the write, computed by the
  store inside the snapshot's own write transaction.
- A store write method that records one snapshot atomically (header plus all
  item rows in a single transaction) and is append-only (no update/delete).
- A read API exposing the snapshot stream (headers in insertion order), a
  single snapshot's item rows, and the latest snapshot.
- Wiring into the orchestrate loop so each scheduling pass records one snapshot
  of the graph it built, during real runs.

### Out of scope
- Pruning, retention, rotation, throttling, or any cap on snapshot growth. A
  per-pass cadence is the shipped behavior; an interval/volume throttle is a
  separate future spec (see D-6).
- Any operator CLI/TUI/HTTP surface that renders snapshots, and any
  reconstruction helper that materializes "graph state at event N" from a
  snapshot plus subsequent events. The read API is library-only this spec; the
  recorded cursor is what *enables* reconstruction, but the replay tool itself
  is future work.
- Source-sync snapshots or any non-graph state. Source observability lives in
  `source_syncs` (00048); claim deltas live in `orchestrator_events` (00054).
- Any change to `flywheel-core`'s schema or the `persistence-schema*.sql`
  files (those are core's; the orchestrator store schema is inline in
  `_claims*.py`).
- Mutating the WorkGraph, scheduling, or claim behavior. Snapshotting is a pure
  read-and-record side channel; it never changes which task is dispatched.

### Must not regress
- Existing `ClaimStore` semantics: `acquire_claim` / `renew_claim` /
  `release_claim` / `sweep_expired_claims` / `list_claims` and the 00054 event
  ledger (`list_events` / `list_task_events`) are unchanged.
- The orchestrate loop's selection and dispatch decisions are byte-for-byte
  unchanged by the presence of snapshotting (it observes, never steers).
- A pre-existing v1/v2/v3/v4 orchestrator store still opens (now converging its
  sentinel to v5) with its `task_claims`, `work_items`, `source_syncs`, and
  `orchestrator_events` rows intact.
- `flywheel-core` purity and schema are untouched (orchestrator-only change).

## Success Criteria
Each criterion is atomic, grades an observable end-state, and carries its grader
type, visibility, the exact check, and the gaming move it forecloses. `/fw-plan`
lowers each one to a command / transcript / rubric / manual grader.

1. When a snapshot is recorded for a graph of N work items, the store gains
   exactly one snapshot whose read-back item set is the N items, each carrying
   the source provenance (kind, ref, url, version), priority, required
   capabilities, conflict keys, lifecycle state, readiness flag, claim holder,
   and resolved prerequisite ids it was given. [command | held-out]
   verify: assemble per-item materialized state for a known graph (mixed
   provenance, priorities, one ready and one blocked item, one item with a
   live claim); record a snapshot; read it back; assert every field of every
   item round-trips, including an item whose claim holder is none and an item
   whose resolved-prerequisites set is non-empty.
   defends against: recording a placeholder/all-null snapshot that passes a
   "a snapshot exists" check while carrying none of the graph's real state.

2. The readiness flag, claim holder, and lifecycle state recorded for each item
   equal that item's actual values in the captured graph. [command | held-out]
   verify: build a graph where item X is in `ready_set(states)` and item Y is
   not (unsatisfied prerequisite), and where X has a live claim by worker A and
   Y has none; record the snapshot; assert the recorded ready flag is true for
   X and false for Y, the recorded holder is A for X and none for Y, and the
   recorded state equals `states[id]` for each.
   defends against: stamping a constant (e.g. all-ready, all-unheld, or a fixed
   state) that satisfies criterion 1's field-presence check while
   misrepresenting which items were schedulable and who held them.

3. Every recorded snapshot is internally complete: the number of item rows
   readable for a snapshot equals the item count its header declares, and no
   snapshot ever exposes a subset of the items it captured. [command | held-out]
   verify: record several snapshots of different sizes (including a 0-item and a
   multi-item graph); for each, assert the header's declared item count equals
   the count of item rows read back, and equals the size of the input graph.
   defends against: a non-atomic write that leaves a partially-populated
   cross-section, so a reader sees a graph state that never actually existed.

4. When a snapshot is recorded, its header `last_event_id` equals the maximum
   `orchestrator_events` id at the instant of the write (0 when the ledger is
   empty), and a snapshot recorded after further claim transitions carries a
   strictly greater cursor. [command | held-out]
   verify: on a fresh store, record a snapshot and assert its cursor is 0; drive
   `acquire_claim` then `renew_claim` (appending two events); record a second
   snapshot and assert its cursor equals the id of the latest event; append one
   more event and record a third snapshot and assert its cursor advanced again.
   defends against: a stale, zero, or caller-supplied cursor that no longer
   points at the ledger's true high-water mark, breaking snapshot-plus-events
   reconstruction.

5. The snapshot record is append-only: recording successive snapshots yields
   distinct, accumulating snapshots in insertion order, and the public store
   API exposes no method that updates or deletes a recorded snapshot or its item
   rows. [command | held-out]
   verify: record three snapshots; assert the snapshot stream returns all three
   in insertion order as distinct entries; assert re-recording does not replace
   or mutate an earlier snapshot; assert the store class exposes no
   snapshot-updating or snapshot-deleting method.
   defends against: upserting/overwriting the "current" snapshot so history is
   lossy, or providing an edit/delete path that lets a recorded state be
   rewritten.

6. The store exposes a read API for the snapshot stream (all snapshot headers in
   insertion order), a single snapshot's item rows, and the latest snapshot, and
   each returns the recorded data. [command | visible]
   verify: a stream accessor returns every recorded snapshot header in id order;
   an item accessor returns exactly one snapshot's item rows; a latest accessor
   returns the most recently recorded snapshot; an empty store returns an empty
   stream and a null latest without error.
   defends against: writing snapshots that can never be read back (a useless
   record).

7. When the orchestrate loop completes a scheduling pass, it records a snapshot
   of the graph that pass built, whose items and states reflect that pass's
   actual graph. [command | held-out]
   verify: drive `orchestrate` against a fake source and fake invoke through to
   its no-progress return; assert the orchestrator store's snapshot stream is
   non-empty and the latest snapshot's item set equals the source's work items
   with their lifecycle states for that pass.
   defends against: shipping the store capability dormant (nothing in a real run
   ever records a snapshot), repeating the dormant-machinery gap.

8. As lifecycle state changes across passes, successively recorded snapshots
   reflect the changed graph (a task that becomes DONE in a later pass appears
   with state done and is absent from that snapshot's ready set). [command | held-out]
   verify: drive `orchestrate` over a two-task chain (B depends on A) so A runs
   and reaches done before B; read back the snapshot stream; assert an earlier
   snapshot shows A not-done with B not ready, and a later snapshot shows A done
   with B ready.
   defends against: capturing once and caching it (or re-recording an identical
   first-pass cross-section), so the record never reflects the graph evolving.

9. Given the identical recorded snapshot inputs, the SQLite and Postgres
   backends produce equal snapshot records — same item fields, states, holders,
   cursors, and ordering (modulo backend-assigned ids). [command | held-out]
   verify: record the same sequence of snapshots against a `SqliteClaimStore`
   and a `PostgresClaimStore`; assert the read-back snapshot streams and their
   item rows are equal after dropping backend-assigned id fields.
   defends against: implementing snapshots on one backend only, or with
   divergent field spellings, so distributed (Postgres) deployments silently
   lose or mis-record graph history.

10. When a pre-existing v4 orchestrator store is opened by the v5 code, it opens
    without error, retains all its `task_claims`, `work_items`, `source_syncs`,
    and `orchestrator_events` rows, and exposes an initially-empty snapshot
    stream. [command | held-out]
    verify: build a store at schema v4 with claim, work-item, source-sync, and
    event rows; open it with the v5 build; assert no `OrchestratorSchemaError`,
    the prior rows are intact, and the snapshot stream is empty. Assert on both
    backends.
    defends against: a drop-and-recreate migration that wipes existing rows —
    the non-destructive-bump invariant every prior orchestrator schema change
    has held.

11. When the graph is empty (the source yields no work items), a recorded
    snapshot is still a valid snapshot with zero item rows and a cursor.
    [command | visible]
    verify: record a snapshot for an empty graph; assert the snapshot stream
    gains one entry whose declared item count is 0, whose item rows are empty,
    and whose cursor is the current ledger high-water mark.
    defends against: skipping the write when the graph is empty, which would
    erase the observable "the graph went empty" signal from the history.

Verification surface: unchanged. This feature adds an orchestrator-store record
and a read-only loop side channel; it does not touch the test/lint/typecheck/CI
machinery, the graders, or any check that decides whether other changes are
correct.

## Decomposition Hint (for /fw-plan)
Three layers. Layer A is the store contract every other layer asserts against;
B and C are independent consumers of A and can run in parallel after it.

- Layer A (snapshot model + SQLite/in-memory store): satisfies #1-6, #11, and
  #3/#10 for the SQLite/in-memory backends. Introduces the snapshot-header and
  snapshot-item record shapes, the `graph_snapshots` / `graph_snapshot_items`
  tables in `_SCHEMA_SQL`, the atomic single-transaction `record_graph_snapshot`
  write that stamps `last_event_id` from the live `orchestrator_events` max id,
  the read API (stream / per-snapshot items / latest), and the additive v4->v5
  sentinel bump + forward migration. Depends on nothing.
- Layer B (orchestrate-loop wiring): satisfies #7, #8. Assembles each pass's
  per-item materialized state (provenance + priority + capabilities +
  conflict_keys from `WorkItem`, lifecycle state from `states`, readiness from
  `graph.ready_set(...)`, holder from `claims.list_claims()`, edges from
  `graph.resolved_prerequisites(...)`) at the top of the pass and records one
  snapshot before dispatch. Depends on Layer A's write method and record shapes.
- Layer C (Postgres mirror): satisfies #9 and #10 for Postgres. Mirrors the
  tables, the atomic write + cursor stamping, the read API, and the additive
  migration in `PostgresClaimStore`. Depends on Layer A — it inherits the record
  shapes and field set as fixed contracts.

Shared invariants all layers assert against (name them so the slices move
together): the snapshot-header field set (snapshot id, captured-at timestamp,
declared item count, `last_event_id` cursor); the snapshot-item field set (task
id, source kind/ref/url/version, priority, required capabilities, conflict keys,
lifecycle state, readiness flag, claim holder, resolved prerequisite ids); the
`record_graph_snapshot` write contract (atomic, store-stamped cursor); and
`CURRENT_ORCH_SCHEMA_VERSION = 5`.

## Decisions Log (ADR-style: immutable, supersede — never edit)

### D-1: A snapshot captures full per-item materialized state, not just DAG shape  (Status: Accepted)
- Context: the observability value is "what was the whole graph at time T,"
  which is dominated by live state (which items were ready, who held a claim,
  each item's lifecycle state), not by the structural edge set, which changes
  rarely. Operator-confirmed. | Decision: one snapshot = a row per work item
  carrying provenance, priority, capabilities, conflict keys, lifecycle state,
  readiness, claim holder, and resolved prerequisites.
- Rejected: structural-only snapshots (items + edges) — they barely change pass
  to pass and cannot be composed with the event ledger to reconstruct state, so
  they carry little observability value.
- Consequences: snapshots are wider rows and depend on state the store does not
  itself own (lifecycle state, readiness, holders), so the loop must assemble
  and pass that state to the store (D-3).

### D-2: Each snapshot stamps the `orchestrator_events` high-water mark, store-computed in the write transaction  (Status: Accepted)
- Context: the snapshot-plus-events reconstruction story (a snapshot is the
  base state; subsequent events replay forward from it) requires each snapshot
  to know exactly where it sits in the ledger. Operator-confirmed. | Decision:
  the snapshot header carries `last_event_id`, computed by the store as the max
  `orchestrator_events` id inside the snapshot's own write transaction (0 when
  the ledger is empty), not supplied by the caller.
- Rejected: a caller-supplied cursor (drifts from the real ledger, trivially
  faked, criterion #4 forecloses it); standalone snapshots with no cursor (loses
  the composition payoff that made this the natural follow-on to 00054).
- Consequences: the cursor cannot drift from the ledger it points at; a
  reconstruction tool (future) can take any snapshot and replay every event with
  a greater id to reach the exact state at that later point.

### D-3: The snapshot is captured by the orchestrate loop, written atomically by the store  (Status: Accepted)
- Context: unlike a claim event (a structural consequence of a store
  transaction, so 00054 emits it inside the store), a snapshot materializes
  state the store does not own — the WorkGraph, lifecycle states, readiness, and
  claim holders the loop holds. | Decision: the loop assembles the per-item
  cross-section and calls a single store write method that inserts the header
  plus all item rows in one transaction; the store stamps the cursor (D-2).
- Rejected: emitting from inside a claim-store transaction (the store has no
  access to the graph/states/readiness); multiple write calls per snapshot
  (a crash between them leaves a partial cross-section — criterion #3 forecloses
  it).
- Consequences: the capture point is one loop callsite, so unlike the 00054
  ledger the snapshot record *can* be bypassed by a future loop that forgets to
  call it; criterion #7 guards the wired callsite, and the atomic write keeps
  each recorded snapshot internally complete.

### D-4: Capture at the top of each scheduling pass, before dispatch  (Status: Accepted)
- Context: a snapshot should record the graph state the pass actually scheduled
  against. | Decision: the loop records the snapshot after it builds the graph
  and `states` for a pass and computes readiness, before it dispatches any task,
  so readiness and holders reflect what the scheduler saw.
- Rejected: capturing after dispatch (the recorded readiness/holder no longer
  matches the decision the pass made); capturing only on the first pass (loses
  the graph-evolving signal — criterion #8 forecloses it).
- Consequences: a multi-task session records several snapshots showing the graph
  evolve as tasks complete; the terminal no-progress pass records the final
  cross-section.

### D-5: Additive v4->v5 bump, non-destructive on both backends; schema inline in `_claims*.py`  (Status: Accepted)
- Context: every prior orchestrator schema change (v1->v2->v3->v4) has preserved
  existing rows, and the orchestrator store has no `.sql` mirror — its DDL is
  inline, and `persistence-schema*.sql` belongs to flywheel-core. | Decision:
  add the snapshot tables via `CREATE TABLE IF NOT EXISTS`, converge the
  sentinel forward to 5, leave all existing tables untouched, inline in
  `_claims.py` / `_claims_postgres.py`; core is not touched.
- Rejected: drop-and-recreate (wipes live rows); adding the tables to core's
  schema files (violates the one-way dependency arrow and core purity).
- Consequences: a store written by an even-newer build still trips the existing
  mismatch guard; only forward convergence from v1/v2/v3/v4 is silent.

### D-6: Per-pass cadence; no throttle or retention this spec  (Status: Accepted)
- Context: `orchestrate` returns once the worker can make no further progress,
  so a single drive records a bounded number of snapshots (~one per task driven
  plus a terminal pass), not an unbounded stream. The operator chose per-pass
  capture. | Decision: record one snapshot per pass, with no interval/volume
  throttle and no retention/pruning in this spec.
- Rejected: an interval-gated capture with a new `[execution]` config knob —
  adds config surface for a volume concern that the bounded per-drive pass count
  does not yet require; deferred to a future retention/throttle spec alongside
  ledger pruning (00054's deferred retention).
- Consequences: a long-lived daemon that calls `orchestrate` repeatedly
  accumulates snapshots across drives; bounding that growth is the future
  retention spec's job, explicitly out of scope here.

## Open Questions (accepted gaps)
None. Every criterion lowers to a `command` grader (pytest assertions against
the stores and a driven `orchestrate` with fakes, the Postgres-container fixture
for #9/#10), with the highest-stakes ones marked held-out for fw-verify.

## Next Steps
Run `/fw-plan 00055-graph-snapshots` to compile these criteria into flywheel
tasks and graders.

# 00049 — Distributed scheduling metadata

Status: spec. Folds the WorkGraph roadmap's distributed-scheduling-metadata
milestone: give a `WorkItem` the four scheduling dimensions a multi-worker
orchestrator needs — `priority` (which ready item goes first),
`required_capabilities` (which workers may run it), `conflict_keys` (which items
may not run concurrently), and a claim **liveness sweep** (how a dead worker's
work returns to the pool) — and make the scheduler and claim store actually act
on them. flywheel-core's schema and the `Task` value type are untouched.

## Outcome

A `WorkItem` carries `priority`, `required_capabilities`, and `conflict_keys`,
populated by `DirectoryWorkSource` from the task file and persisted into the
`work_items` columns 00048 reserved for them. The scheduler consumes them:
ready items are offered highest-priority-first (ties keep today's walk order),
and an item is withheld from a worker whose advertised capability set does not
cover the item's `required_capabilities`. In the claim store (the multi-worker
coordination path), acquiring an item that shares a `conflict_key` with a live
claim is refused so the two never run at once, and a liveness sweep batch-
releases every lapsed claim — returning a dead worker's tasks to the acquirable
pool — while leaving live, recently-renewed claims untouched. With every field
at its default (priority 0, empty capability/conflict sets, no worker
capabilities configured), scheduling order and claim behavior are byte-identical
to today.

## Background

00047 made the prerequisite graph explicit while *preserving* first-eligible
selection; 00048 added `work_items` / `work_item_dependencies` / `source_syncs`
and deliberately created `priority` / `required_capabilities_json` /
`conflict_keys_json` columns with defaults but populated none of them (00048
D-5), leaving the `WorkItem` fields to this spec. The scheduler
(`select_next_task`, `WorkGraph.ready_set`) and the claim store
(`acquire_claim`, `renew_claim`, the un-expiry-filtered `list_claims`) are all
contention-blind today: order is walk order, any worker runs anything, two items
never know they collide, and a dead worker's lease only frees the *one* task a
later worker happens to retry. This spec is where that metadata gains a
consumer. The tacit load-bearing requirement a literal agent will miss: the
*defaults must reproduce today's behavior exactly* — priority 0 everywhere must
not perturb walk order, an empty worker capability set must still run every
existing (zero-requirement) item, and a non-conflicting / non-lapsed claim must
behave precisely as it does now. The point of the feature is the behavior under
*non-default* metadata; the safety of the feature is the behavior under default
metadata.

Per the mode split confirmed with the author: `priority` and
`required_capabilities` are meaningful even single-worker (deterministic ready
ordering; a worker skipping work it cannot run) and apply in **both** execution
modes. `conflict_keys` exclusion and the liveness sweep only bite under
multi-worker contention — they live in the claim store, the path local
pull-mode never exercises — so they are effectively **distributed-only**, gated
by where claims exist rather than by a mode flag check.

## Scope

### In scope
- `WorkItem` gains three optional fields: `priority: int = 0`,
  `required_capabilities: frozenset[str] = frozenset()`,
  `conflict_keys: frozenset[str] = frozenset()` (frozen + kw_only preserved; all
  default so every existing constructor still compiles).
- `DirectoryWorkSource.list_work()` reads `priority` / `required_capabilities` /
  `conflict_keys` from each task file's top-level JSON, mirroring how
  `prerequisites` is read today; absent keys yield the defaults.
- Persistence writes those three values into the existing `work_items`
  `priority` / `required_capabilities_json` / `conflict_keys_json` columns
  (replacing the 00048 forward-compat defaults with the item's actual values).
- Priority-ordered selection: `select_next_task` and `WorkGraph.ready_set`
  offer ready items highest-priority-first, ties broken by existing
  enumeration/walk order — in both execution modes.
- Capability filtering: given a worker's advertised capability set, an item
  whose `required_capabilities` is not a subset of that set is excluded from
  that worker's selectable/ready set — in both execution modes. The worker's
  capability set is an explicit per-worker configuration value, defaulting to
  empty.
- Conflict-key mutual exclusion at the claim store: acquiring an item that
  shares a `conflict_key` with a *different* live claim is refused; the worker
  falls through to the next ready item (the already-shipped "claim fails -> try
  next" fallback).
- A claim-store **liveness sweep** operation that batch-releases every lapsed
  claim, returning those tasks to the acquirable pool and making `list_claims`
  reflect only live (non-lapsed) workers, including reaping all of one dead
  worker's claims in a single pass.

### Out of scope
- `GithubWorkSource` deriving `priority` / `required_capabilities` /
  `conflict_keys` from issue labels or fields — github items schedule at the
  defaults until a later spec. (The fields exist on every `WorkItem`; only
  directory population is in scope here.)
- A persistent reclaim/heartbeat *event* ledger (that is spec 00051,
  `orchestrator_events`). The sweep's effect is graded on claim-store end-state
  (acquirable / no longer live-held), not on a logged event row.
- Queue/block-on-conflict or any waiting machinery — exclusion is refuse-and-
  skip, never block-until-clear.
- Any change to flywheel-core's schema, `schema_version`, `task_versions`, or
  the `Task` value type.
- `graph_snapshots`, Linear/Slack sources.

### Must not regress
- Every existing `WorkItem` construction and every source/orchestrator/core
  test stays green: the three new fields are optional with defaults.
- With all items at default priority (0), scheduler order is exactly today's
  walk order; `select_next_task` still returns the first-eligible item.
- A worker with no configured capabilities still selects exactly the items
  today's scheduler would (every existing item declares no required
  capabilities).
- The claim store's `acquire_claim` / `renew_claim` / `release_claim` /
  `list_claims` behavior for non-conflicting, non-lapsed claims is unchanged,
  including the existing v1 sentinel and `task_claims` rows.

## Success Criteria

Each criterion grades an observable end-state — a constructed object's
attributes, rows read back through the store object the agent does not author,
or the *order/membership* of what the scheduler offers — never a call sequence.
Tests live under `packages/flywheel-orchestrator/tests/`; claim-store criteria
run on both backends (Postgres auto-skips without Docker via the root
`require_postgres` fixture).

1. The `WorkItem` value type exposes `priority`, `required_capabilities`, and
   `conflict_keys`, each constructible by keyword and each defaulting so a
   `WorkItem(task=..., source_ref=...)` with none of them still constructs;
   constructing with all three round-trips the supplied values. [command | held-out]
   verify: `uv run pytest packages/flywheel-orchestrator/tests/` — a test
   constructs a `WorkItem` with none of the three (asserting `priority == 0`,
   both sets empty) and one with `priority=5`, a non-empty capability set, and a
   non-empty conflict set (asserting each round-trips); both pass.
   defends against: adding the fields as required/positional, which breaks every
   existing constructor — caught by the default-construction assertion.

2. When `DirectoryWorkSource.list_work()` reads a task file declaring top-level
   `priority`, `required_capabilities`, and `conflict_keys`, the returned
   `WorkItem` carries those exact values; a task file omitting them yields the
   defaults (priority 0, empty sets). [command | held-out]
   verify: `uv run pytest packages/flywheel-orchestrator/tests/` — a test points
   a `DirectoryWorkSource` at one fixture file declaring all three (e.g.
   `priority: 7`, `required_capabilities: ["gpu"]`, `conflict_keys: ["deploy"]`)
   and one declaring none, and asserts the parsed `WorkItem` fields match
   (declared values on the first, defaults on the second).
   defends against: ignoring the file values and always emitting defaults, or
   crashing when a key is absent — both pinned by the two-fixture assertion.

3. When a sync persists an observed item carrying non-default `priority` /
   `required_capabilities` / `conflict_keys`, that item's `work_items` row
   exposes those values in the `priority` / `required_capabilities_json` /
   `conflict_keys_json` columns (no longer the 00048 forward-compat defaults),
   read back through the store. [command | held-out]
   verify: `uv run pytest packages/flywheel-orchestrator/tests/` — a test syncs
   an item with `priority=7`, `required_capabilities={"gpu"}`,
   `conflict_keys={"deploy"}`, reads the `work_items` row back, and asserts
   `priority == 7` and the two json columns decode to the supplied sets (order-
   insensitive); a second item left at defaults still reads 0 / `[]` / `[]`.
   defends against: persistence ignoring the new fields and writing the 00048
   defaults — the non-default read-back fails a hardcoded 0/`[]`.

4. When several items are ready (eligible state, prerequisites satisfied),
   the scheduler offers them highest-`priority`-first; among equal-priority
   items, in existing enumeration/walk order. [command | held-out]
   verify: `uv run pytest packages/flywheel-orchestrator/tests/` — a test builds
   ready items with priorities `[1, 9, 5]` in walk order and asserts
   `ready_set` (and `select_next_task`'s pick) orders them `9, 5, 1`; a second
   test with priorities `[3, 3, 3]` asserts the result preserves walk order.
   defends against: ignoring priority (walk order regardless) or an unstable
   sort that drops the equal-priority tie-break — both pinned by the two
   ordering assertions.

5. When every ready item carries the default priority (0), the scheduler's
   order and `select_next_task`'s first pick are identical to the pre-feature
   walk-order behavior. [command | held-out] (must-not-regress)
   verify: `uv run pytest packages/flywheel-orchestrator/tests/` — the existing
   selection/ordering tests pass unchanged, plus a test asserting that a set of
   default-priority items yields the same sequence as raw walk order.
   defends against: a sort that reorders equal-priority items (e.g. by id),
   silently changing today's deterministic first-eligible selection.

6. Given a worker advertising capability set C, the scheduler excludes from that
   worker's selectable/ready set every item whose `required_capabilities` is not
   a subset of C; an item with empty `required_capabilities` is selectable by
   any worker, including one with empty C. [command | held-out]
   verify: `uv run pytest packages/flywheel-orchestrator/tests/` — a test offers
   three ready items requiring `{}`, `{"gpu"}`, `{"gpu","cuda"}` to a worker with
   `C = {"gpu"}` and asserts only the first two are selectable; to a worker with
   `C = {"gpu","cuda"}` all three are; to a worker with `C = {}` only the first.
   defends against: ignoring capabilities (running anything) OR over-filtering
   (excluding the empty-requirement item) — both pinned by the subset cases.

7. A worker with no configured capabilities (empty C) selects exactly the items
   today's scheduler would (every existing item declares no required
   capabilities), and an item that declares any required capability is withheld
   from it. [command | held-out] (must-not-regress + new exclusion)
   verify: `uv run pytest packages/flywheel-orchestrator/tests/` — a test with
   all-default items and a default (empty-capability) worker asserts the full
   ready set is offered unchanged; adding one item requiring `{"gpu"}` asserts
   that item alone is withheld while the rest are unaffected.
   defends against: a capability gate that defaults to deny-all (breaking every
   existing zero-requirement item) or to allow-all (never enforcing).

8. When a live claim is held on an item carrying conflict-key `k`, an attempt to
   acquire a *different* item that also carries `k` is refused (no claim
   returned), so the two items never hold concurrent live claims. [command | held-out]
   verify: `uv run pytest packages/flywheel-orchestrator/tests/` — a test
   acquires item A (conflict-keys `{"k"}`) for worker-1, then attempts to acquire
   item B (conflict-keys `{"k"}`) for worker-2 and asserts the acquire returns
   no claim while A's claim remains live and owned by worker-1.
   defends against: the headline reward-hack — letting two conflicting items run
   at once. The "B is refused while A is live" assertion forecloses it.

9. Two items whose conflict-key sets are disjoint (or empty) can hold concurrent
   live claims — exclusion fires only on actual key overlap. [command | held-out]
   verify: `uv run pytest packages/flywheel-orchestrator/tests/` — a test
   acquires item A (`{"k1"}`) for worker-1 then item B (`{"k2"}`, or `{}`) for
   worker-2 and asserts both claims are live concurrently.
   defends against: over-broad exclusion that blocks any second concurrent claim
   regardless of keys — the disjoint-keys concurrency assertion fails it.

10. When the claim that held conflict-key `k` is released (or its lease lapses
    and is swept), a previously-refused conflicting item becomes acquirable
    again. [command | held-out]
    verify: `uv run pytest packages/flywheel-orchestrator/tests/` — extends the
    #8 test: after releasing A's claim, asserts B is now acquirable by worker-2.
    defends against: a conflict registry that never clears, permanently
    blocking an item after the conflicting claim ends.

11. A liveness sweep at instant T releases every claim whose lease lapsed at or
    before T, so those task ids become acquirable by any worker and are no
    longer reported as live-held by `list_claims`; a claim whose lease is still
    valid at T stays held and owned. [command | held-out]
    verify: `uv run pytest packages/flywheel-orchestrator/tests/` — a test
    acquires two claims with different lease windows, advances injected `now`
    past one lease only, runs the sweep, and asserts the lapsed task is absent
    from `list_claims` and freshly acquirable, while the still-valid claim
    remains in `list_claims` owned by its worker and is NOT acquirable by another
    worker.
    defends against: reaping live (recently-renewed) claims, or never reaping
    dead ones — both pinned by the mixed-lease assertion.

12. A single sweep releases ALL lapsed claims held by the same worker in one
    pass (per-worker dead-worker reaping), not one task at a time. [command | held-out]
    verify: `uv run pytest packages/flywheel-orchestrator/tests/` — a test gives
    one worker three claims, advances `now` past all their leases, runs one
    sweep, and asserts all three task ids are acquirable / absent from
    `list_claims` after that single pass.
    defends against: reclaiming only the one task a later acquire happens to
    retry (today's opportunistic steal), leaving a dead worker's other tasks
    stuck held.

13. Criteria #3 and #8-#12 hold against **both** the SQLite and Postgres
    claim-store backends. [command | held-out]
    verify: `uv run pytest packages/flywheel-orchestrator/tests/` — the
    persistence/conflict/sweep tests are parametrized/duplicated across
    `SqliteClaimStore` and `PostgresClaimStore` (Postgres via the root
    `postgres_dsn` fixture, auto-skipped only when Docker is absent).
    defends against: implementing the SQLite path only and leaving Postgres a
    stub — the Postgres-parametrized cases fail (or are visibly skipped only for
    Docker absence, never for missing implementation).

14. The existing orchestrator suite and the core suite still pass after the
    change. [command | held-out] (verification-surface)
    verify: `uv run pytest packages/flywheel-orchestrator/tests/` and
    `uv run pytest packages/flywheel-core/tests/` both pass.
    defends against: satisfying a new criterion by weakening or deleting an
    existing scheduling/claim test.

15. If this feature changes `orchestrator_schema_version`, the bump is additive:
    a pre-existing store (holding `task_claims` and `work_items` rows) opened by
    the new code keeps every row and converges, with no drop-and-recreate and no
    hard version-mismatch error. [command | held-out] (verification-surface)
    verify: `uv run pytest packages/flywheel-orchestrator/tests/` — a test writes
    a claim and a work-item through the store, reopens it under the new code, and
    asserts both pre-existing rows still load and the new conflict/sweep
    operations work. (Passes trivially if no version bump occurs.)
    defends against: a destructive migration or a hard mismatch error if the
    implementer stores conflict metadata on a new column — the surviving-rows
    assertion fails either, mirroring 00048 criterion #9.

16. flywheel-core's `schema_version` and `task_versions` table definition are
    unchanged by this feature. [command | held-out] (verification-surface)
    verify: `uv run pytest packages/flywheel-core/tests/` passes unchanged, and
    `git diff` touches no file under
    `packages/flywheel-core/src/flywheel_core/_schema/` and does not alter
    `CURRENT_SCHEMA_VERSION` in flywheel-core.
    defends against: leaking scheduling columns into core's schema to satisfy a
    persistence or conflict criterion.

17. The claim store's `acquire_claim` / `renew_claim` / `release_claim` /
    `list_claims` behavior for non-conflicting, non-lapsed claims is unchanged.
    [command | held-out] (must-not-regress)
    verify: `uv run pytest packages/flywheel-orchestrator/tests/` — the existing
    claim-store tests (acquire on free task, same-worker re-acquire, renew bumps
    version, release, expiry-steal, list) pass unchanged.
    defends against: the conflict/sweep additions altering the lease/version
    semantics of an ordinary claim.

Verification surface: this feature changes the scheduler (which task is offered/
selected) and the claim store (which claims survive), and MAY add an additive
orchestrator-store column — it is itself a state/grader surface. Definition of
Done (inherited by every task, all held-out): the existing orchestrator suite
and core suite still pass (criterion #14); any orchestrator-store version bump is
additive and non-destructive (#15); no flywheel-core schema or `schema_version`
change (#16); ordinary (non-conflicting, non-lapsed, default-metadata) scheduling
and claiming behave exactly as before (#5, #7, #17). No grading assertion may be
relaxed, skipped, or deleted; a removed assertion with no equal-or-stronger
replacement is a blocking defect.

## Decomposition Hint (for /fw-plan)
- Layer **value type + directory population + persistence**: satisfies #1, #2,
  #3. Adds the three `WorkItem` fields, reads them in `DirectoryWorkSource`
  (mirroring `_read_prerequisites`), and writes them into the existing
  `work_items` columns. No scheduler or claim-store dependency.
- Layer **priority + capability scheduling**: satisfies #4, #5, #6, #7. Makes
  `select_next_task` / `WorkGraph.ready_set` priority-order and capability-filter
  (worker capability set threaded in, defaulting empty). Depends on the field
  names from the value-type layer; both execution modes.
- Layer **conflict-key exclusion (claim store)**: satisfies #8, #9, #10, the
  conflict part of #13, and #15/#17. Enforces refuse-on-overlap in
  `acquire_claim`; clears on release/lapse. Distributed/claim-store path.
- Layer **liveness sweep (claim store)**: satisfies #11, #12, the sweep part of
  #13, and #17. Adds the batch-release sweep operation on both backends.
  Distributed/claim-store path.

Shared invariants multiple layers assert against:
- The three new `WorkItem` field names (`priority`, `required_capabilities`,
  `conflict_keys`) — the value-type layer defines them; the directory source,
  persistence, scheduler, and claim store all read them. Define once.
- The `work_items` column names (`priority` / `required_capabilities_json` /
  `conflict_keys_json`) — already created by 00048; this spec only populates
  them. Do not re-create or rename.
- The conflict-exclusion contract (refuse a claim that overlaps a live claim's
  keys) and the sweep contract (batch-release lapsed claims, leave valid ones)
  are shared by both claim-store layers and the both-backends criterion; specify
  each once and assert it on SQLite and Postgres together.

## Decisions Log (ADR-style: immutable, supersede — never edit)

### D-1: `priority` is a descending hard ordering key; default 0 preserves walk order  (Status: Accepted)
- Context: ready items must be offered in a deterministic, operator-controllable
  order without perturbing today's behavior, where the only ordering is
  enumeration/walk order and `select_next_task` returns the first eligible item.
- Decision: higher `priority` is offered first; ties broken by existing walk
  order (a stable sort by descending priority over the current sequence).
  Default `priority` is 0, so an all-default set sorts to its original walk
  order — today's behavior exactly.
- Rejected: ascending priority (less intuitive for "do this first"); a separate
  priority queue/heap (loses the stable walk-order tie-break and the trivial
  no-regression proof); priority as a mere filter (does not order).
- Consequences: criterion #5 (all-default == walk order) is the no-regression
  proof and must stay green; the sort must be stable.

### D-2: Worker capabilities are an explicit per-worker set, distinct from `[sandbox.capabilities]`  (Status: Accepted)
- Context: capability filtering needs a worker-side set to compare against an
  item's `required_capabilities`. `[sandbox.capabilities]` already exists but
  means the agent's tools/skills/MCP surface — a different concept; reusing the
  name would conflate "what tools the agent has" with "what classes of work this
  worker is allowed to run."
- Decision: an item runs on a worker iff `required_capabilities` is a subset of
  the worker's advertised capability set. The worker's set is an explicit
  per-worker configuration value (its natural home is the `[execution]` table
  that 00046 introduced), defaulting to empty. Empty worker set + empty item
  requirements (today's items) = today's behavior.
- Rejected: deriving worker capabilities from the resolved sandbox preset
  (couples filtering to sandbox internals and makes the grader depend on sandbox
  resolution); one global capability set (cannot express heterogeneous workers,
  the whole point); reusing `SandboxCapabilities` (name collision, wrong
  concept).
- Consequences: criteria grade the subset-filtering *behavior* given a worker
  capability set, never the config path; #7 pins the empty-set no-regression.

### D-3: Conflict-key exclusion is enforced at the claim store, refuse-and-skip  (Status: Accepted)
- Context: WHERE concurrent-execution exclusion can be enforced. The claim store
  is the single mutual-exclusion point across workers; the scheduler's
  `ready_set` cannot see other workers' in-flight claims. Local pull-mode has no
  concurrent claims, so this is effectively distributed-only.
- Decision: acquiring an item that shares a `conflict_key` with a *different*
  live claim is refused (no claim returned); the worker falls through to the
  next ready item, reusing the already-shipped "claim fails -> try next ready
  task" fallback (Milestone 5). Exclusion clears when the conflicting claim is
  released or swept.
- Rejected: queue/block-until-clear (needs new wait/wake machinery the
  orchestrator lacks, risks deadlock); advisory-only recording (contradicts the
  behavioral choice, gives a weak "column populated" criterion); enforcing in
  the scheduler (cannot see cross-worker in-flight state).
- Consequences: criterion #8 grades "two conflicting items never hold concurrent
  claims" as a store end-state; the worker-level fall-through is the existing
  shipped behavior, so no new scheduling loop is needed.

### D-4: The claim store is given each item's conflict keys at acquire time  (Status: Accepted)
- Context: to refuse on overlap, `acquire_claim` must know both the candidate
  item's conflict keys and those of every live claim. The keys could be read by
  joining live `task_claims` to `work_items.conflict_keys_json`, or supplied
  with the claim.
- Decision: the conflict-key information needed to enforce exclusion is supplied
  to the claim store at acquire time (so the check does not depend on
  `work_items` being freshly synced or non-stale). The spec grades only the
  no-concurrent-conflicting-claims end-state, not the storage mechanism; if the
  implementer persists keys on a claim row, the migration is additive (see D-5).
- Rejected: joining live claims to `work_items` at acquire time (couples
  claiming to catalog freshness and sync timing — a mid-sync or stale catalog
  would mis-enforce exclusion).
- Consequences: criterion #15 covers the additive-migration safety if a column
  is added; the conflict behavior is gradeable without prescribing schema.

### D-5: This spec populates 00048's forward-compat columns; any new column is additive  (Status: Accepted)
- Context: 00048 created `work_items.priority` /
  `required_capabilities_json` / `conflict_keys_json` with defaults (00048 D-5)
  expressly for this spec. Conflict enforcement may also need to persist a
  claim's conflict keys.
- Decision: populate the existing `work_items` columns from the new `WorkItem`
  fields (no `work_items` schema change). If conflict enforcement persists keys
  on the claim (D-4), that is an additive `task_claims` column with an additive
  `orchestrator_schema_version` bump, non-destructive to existing stores —
  mirroring 00048's additive 1->2 pattern.
- Rejected: a second `work_items` migration (the columns already exist);
  a destructive re-create (criterion #15 forbids it).
- Consequences: criterion #15 grades the additive/non-destructive property and
  passes trivially if no bump occurs.

### D-6: Heartbeat == lease renewal; the new capability is an explicit liveness sweep  (Status: Accepted)
- Context: `renew_claim` already extends a lease (the heartbeat), and a free or
  expired lease is already stealable by a later `acquire_claim` on that one
  task. What is missing is *proactive, batched* reclamation: today
  `list_claims` does not filter expiry, and a dead worker's other tasks stay
  reported as held until each is individually retried.
- Decision: add a claim-store sweep that, at an injected `now`, batch-releases
  every claim whose lease lapsed — returning those tasks to the acquirable pool
  and making `list_claims` reflect only live workers, reaping all of one dead
  worker's claims in a single pass. No new clock (`now` stays injected); no
  separate `heartbeat_at` column (the lease already encodes liveness).
- Rejected: a `last_heartbeat_at` column redundant with `lease_expires_at`
  (changes no behavior); relying solely on opportunistic per-task expiry-steal
  (leaves a dead worker's other tasks stuck held — criterion #12 forecloses
  this); a persistent reclaim-event ledger (that is 00051).
- Consequences: the sweep's effect is graded on claim-store end-state
  (acquirable / absent from `list_claims`), criteria #11/#12; persistent event
  logging is explicitly deferred to 00051.

### D-7: GithubWorkSource scheduling-metadata derivation is deferred  (Status: Accepted)
- Context: the new `WorkItem` fields exist for every source, but mapping github
  issue labels/fields to priority/capabilities/conflict-keys is a separate
  design surface (label conventions, precedence, validation).
- Decision: only `DirectoryWorkSource` populates the three fields this spec;
  `GithubWorkSource` leaves them at defaults (priority 0, empty sets), so github
  items schedule unchanged.
- Rejected: also deriving from github labels now (widens scope; the directory
  format is the established precedent via `prerequisites`).
- Consequences: no criterion grades github scheduling metadata; github items
  schedule at defaults until a later spec.

## Open Questions (accepted gaps)
None. Every criterion lowers to a `command` grader over the orchestrator (and,
for #16, core) test suite.

## Next Steps
Run `/fw-plan 00049-distributed-scheduling-metadata` to compile these criteria
into flywheel tasks and graders.

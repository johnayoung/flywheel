# Feature: Cross-task query surface on the store protocol (list + filter by status)

## Outcome
Holding only a flywheel store (no on-disk `WorkSource`, no task files), an operator
or tool can ask "list every lifecycle" and "which lifecycles are in status X / belong
to task Y" and get a complete, deterministically-ordered answer through the public
`StoreProtocol` surface alone. The cross-task lifecycle reads that today reach past the
protocol into `store._connection.execute(...)` (`# noqa: SLF001`) in the orchestrator
are gone, served instead by a backend-agnostic protocol method that behaves identically
on the in-memory, SQLite, and Postgres backends.

## Background
The store protocol is 100% single-`run_id` keyed (`store_protocols.py:259-502`): there
is no way to enumerate or filter lifecycles, only to `load_lifecycle(run_id)` one you
already know. So every cross-task question is answered either by re-reading the on-disk
`WorkSource` (`_workflow.py:1227` re-lists task files to build `status`) or by reaching
into the private connection with raw SQL (`_history.py:212,272,351,378`;
`_workflow.py:167,889`). The tacit requirement a literal agent would miss: the
orchestrator already *wrote* the exact filtered query four times over (status filter,
task-id filter, "most recent lifecycle for a task", active-status set) and tagged each
with `# noqa: SLF001`; one author even wrote in the docstring that "no Protocol method
exposes a by-task-id lookup — and adding one would leak workflow concerns into the store
contract" (`_workflow.py:158-160`). That judgement is what this feature reverses: a
single-store *list/filter* primitive is a legitimate store-level read (it answers a
question about rows the store already holds), distinct from cross-task *selection* or DAG
logic, which CLAUDE.md keeps above core in the orchestrator. The DAG itself
(prerequisites, `schema:48-52`) stays sourced from task files and is explicitly not
persisted here.

## Scope
### In scope
- A `StoreProtocol`-level read method that lists `Lifecycle` rows and filters them by
  status and/or task id, returning full typed `Lifecycle` instances (attempts populated,
  same shape `load_lifecycle` returns) in a deterministic, documented order.
- Identical behavior of that method across all three shipped backends (in-memory, SQLite,
  Postgres mirror) — same filtering, same ordering, same empty-result handling.
- Migration of the orchestrator's cross-task *lifecycle* reads off the private connection
  onto the new protocol method: the status/task-id-filtered selects in `_history.py` and
  the lifecycle selects in `_workflow.py` that today carry `# noqa: SLF001`.
- The pure-module purity guarantee of `store_protocols` preserved (no IO/JSON/sqlite
  imports introduced by the new method's signature).

### Out of scope
- Cost/spend aggregation across tasks (audit Q2) — no `SUM`/rollup verb is added.
- Per-worker activity / `list_claims` (audit Q3) — `ClaimStore` is untouched.
- Persisting the prerequisite DAG (audit Q4) — prerequisites stay sourced from task files
  at runtime; no schema column for prerequisites is added.
- Cross-run telemetry query / event-type index (audit Q5) — the telemetry sink and audit
  stream are untouched.
- Any cross-task *selection* / scheduling / DAG-walk logic in core — that stays in the
  orchestrator (CLAUDE.md hard line).
- Migrating the `attempts`-table aggregate reads (token/cost rollups) off the private
  connection — those are per-run reads already served by `list_attempts`; only the
  *lifecycle* listing/filtering raw SQL is in scope here.

### Must not regress
- `flywheel status` continues to enumerate every task its `WorkSource` lists with its
  correct state (it still reads the source for task identity; only the per-task lifecycle
  lookup changes substrate).
- The single-`run_id` protocol methods (`load_lifecycle`, `list_attempts`, etc.) keep
  their current behavior and signatures.
- The store contract suite parametrized over memory/sqlite/postgres still passes, with the
  Postgres backend matching the others.
- `flywheel_core.store_protocols` stays a pure type module (its purity tests still pass).
- The on-disk schema still opens existing v12 databases without forcing a re-create.

## Success Criteria
Each criterion is atomic, grades an observable end-state, and carries its grader type,
visibility, the exact check, and the gaming move it forecloses.

1. When the store holds lifecycles in several statuses and a caller requests the list
   filtered to a single status, the protocol method returns exactly the lifecycles in
   that status and no others. [command | held-out]
   verify: held-out pytest in `packages/flywheel-core/tests/` seeds (via the store API,
   not raw SQL) lifecycles across at least three distinct statuses for at least two task
   ids, calls the new method filtered to one status, and asserts the returned set of
   `run_id`s equals exactly the seeded run_ids in that status — run with
   `uv run pytest packages/flywheel-core/tests/<file> -k <kw>`, exit 0.
   defends against: returning every lifecycle and ignoring the filter; or hardcoding the
   one status value the visible tests happen to use — the held-out case seeds different
   run_ids/statuses than any visible fixture, so a literal status echo fails.

2. When a caller requests the list with no status and no task-id filter, the protocol
   method returns every lifecycle the store holds (one entry per `run_id`), each a fully
   populated `Lifecycle` with its attempts in ascending number order. [command | held-out]
   verify: held-out pytest seeds N lifecycles (N >= 3) with differing attempt counts,
   calls the method with no filters, and asserts (a) the returned `run_id` set equals all
   N seeded run_ids and (b) for one multi-attempt run the returned `Lifecycle.attempts`
   equals what `load_lifecycle(run_id).attempts` returns — exit 0.
   defends against: a narrow implementation that returns only currently-active rows, or
   that returns lifecycle rows with empty/None `attempts` (a stub shape) instead of the
   same fully-folded object `load_lifecycle` yields.

3. When a caller requests the list filtered to a task id, the protocol method returns
   exactly that task's lifecycles in a stable, deterministic order matching the documented
   `(updated_at DESC, run_id DESC)` contract. [command | held-out]
   verify: held-out pytest seeds, through the store API only, multiple lifecycles for one
   task id (alongside lifecycles for a second task), then asserts three relations without
   injecting `updated_at` (which the store sets from its own clock, not the caller):
   (a) metamorphic determinism — calling the method twice with the same task-id filter
   yields the exact same `run_id` sequence (byte-identical ordering, no run-to-run churn);
   (b) recency — for two lifecycles of that task created at distinct store-assigned
   `updated_at`s, the later-stamped row sorts before the earlier one (most-recently-updated
   first), observing whatever `updated_at` the store assigns rather than forcing a tie;
   (c) the `run_id` DESC tiebreak — for two lifecycles sharing a `updated_at` (asserted
   equal as read back from the store, not forced by the caller; if the store's clock
   resolution leaves them distinct the case reads back distinct and is decided by relation
   (b) instead, so the test never depends on a tie it cannot create), the one with the
   greater `run_id` sorts first — run with
   `uv run pytest packages/flywheel-core/tests/<file> -k <kw>`, exit 0.
   defends against: returning the right *set* in arbitrary order (the orchestrator's
   "most recent lifecycle for a task" depends on first-element ordering; an unordered
   result silently picks the wrong run); a non-deterministic order that varies between
   identical calls; and a `run_id`-agnostic sort that would let two same-timestamp rows
   come back in unstable, backend-dependent insertion order.

4. When the same seed and the same filter are applied to the in-memory, SQLite, and
   Postgres backends, each backend's result is identical in membership and order.
   [command | held-out]
   verify: the shared store-contract test (parametrized over the three backends in
   `packages/flywheel-core/tests/test_store_contract.py`) exercises the new method;
   `uv run pytest packages/flywheel-core/tests/test_store_contract.py -k <kw>` exits 0,
   and the Postgres parametrization runs (not skipped) when a container is available.
   defends against: implementing the method only on SQLite and leaving Postgres to fall
   through to a different (or raising) path — the audit's M1 lesson that Postgres is
   "advertised but non-functional"; a backend-specific divergence in ordering or filtering.

5. While holding only the database file (no `WorkSource`, no task files on disk), a caller
   can answer "every lifecycle and its status" entirely through the public protocol.
   [command | held-out]
   verify: held-out pytest opens a store on a pre-seeded db path, with the working
   directory containing no `.flywheel/tasks` tree, calls the new list method, and asserts
   the `(run_id, status)` pairs returned match the seeded rows — exit 0.
   defends against: a "solution" that still requires the on-disk source to enumerate
   (the exact audit-Q1 failure); reading task files behind the protocol.

6. If a caller requests a status filter that matches no stored lifecycle, then the protocol
   method returns an empty list and raises no error. [command | visible]
   verify: pytest seeds lifecycles in status A only, calls the method filtered to status B,
   asserts the result is `== []` and no exception — exit 0.
   defends against: raising on empty results, or returning a sentinel/None that callers
   must special-case (the orchestrator iterates the result directly).

7. The cross-task lifecycle reads in the orchestrator no longer reach past the store
   protocol into the private connection. [command | held-out]
   verify: a grep assertion over the orchestrator package source —
   `grep -rn "_connection" packages/flywheel-orchestrator/src/flywheel_orchestrator/_history.py packages/flywheel-orchestrator/src/flywheel_orchestrator/_workflow.py`
   returns no line that performs a lifecycle `SELECT` (the lifecycle-listing/filtering
   `store._connection.execute` sites and their `# noqa: SLF001` markers at
   `_history.py:212,272,351,378` and `_workflow.py:167,889` are removed); encoded as a
   pytest that fails if any remain — exit 0.
   defends against: adding the protocol method but leaving the raw-SQL call sites in place
   so nothing actually moves onto the public surface (a no-op that passes criteria 1-6 in
   isolation). Scoped to lifecycle SELECTs so it does not falsely flag any legitimately
   per-run/attempts connection use the migration leaves behind.

8. When the orchestrator status/history surfaces run against a seeded store, they produce
   the same task states and the same history rows as before the migration. [command | held-out]
   verify: the existing `test_history.py` and `test_workflow.py` suites in
   `packages/flywheel-orchestrator/tests/` still pass unchanged after the migration —
   `uv run pytest packages/flywheel-orchestrator/tests/test_history.py packages/flywheel-orchestrator/tests/test_workflow.py` exits 0; these tests are not weakened (see verification surface).
   defends against: silently changing which run is "latest" or which rows appear when
   swapping substrate — the migration must be behavior-preserving, proven against tests
   the implementing agent did not author to its own convenience.

9. When a store opened on an existing schema-v12 database is asked for the lifecycle list,
   it answers without requiring a schema re-create. [command | held-out]
   verify: held-out pytest writes a lifecycle through a store, closes it, reopens a fresh
   store on the same db path, calls the new list method, and asserts the prior lifecycle is
   returned and `CURRENT_SCHEMA_VERSION` is unchanged from its pre-feature value — exit 0.
   defends against: smuggling a breaking schema migration (new column / bumped
   `CURRENT_SCHEMA_VERSION`) under a feature that the audit says needs none, which would
   force every existing adopter to re-create their store.

### Verification surface (inherited Definition-of-Done — this feature touches the test/contract machinery)
This feature adds methods to the protocol that the parametrized store-contract suite and
the purity tests assert against, so it changes the verification surface. The following are
held-out and inherited by every task touching that surface:

V1. The full existing suite still passes after the change. [command | held-out] (verification-surface)
   verify: `uv run pytest` exits 0 across all four package test dirs.
   defends against: making the new method pass by deleting or weakening an existing test
   (e.g. the store-contract parity assertions or the orchestrator history/workflow tests).

V2. The `store_protocols` purity guarantee is intact: the module imports no
   IO/JSON/sqlite/file API. [command | held-out] (verification-surface)
   verify: `uv run pytest packages/flywheel-core/tests/test_store_protocols_module_purity.py`
   exits 0 with the new method present in the module.
   defends against: typing the new method's return/params against a serialization helper
   or a sqlite Row that drags a forbidden import into the pure type module.

V3. Any existing check relaxed, removed, or skipped to land this feature is named with an
   equal-or-greater replacement. [manual | held-out] (verification-surface)
   verify: operator confirms the diff removed no assertion from the store-contract,
   purity, or orchestrator history/workflow suites without a named, equal-or-stronger
   replacement; a removed assertion with no replacement is a blocking defect.
   defends against: quietly dropping the Postgres parametrization, an ordering assertion,
   or a `# noqa: SLF001` test guard to make red turn green.

## Decomposition Hint (for /fw-plan)
Splits along the dependency arrow (core upward), one task per slice, chained by
prerequisite so no slice inherits a red suite:

- Layer **protocol contract** (`flywheel_core.store_protocols`): declare the new
  `LifecycleStore` read method signature and its ordering/empty-result/return-shape
  contract in docstrings. Satisfies the *shape* asserted by #1-#6; keeps #V2 green.
- Layer **backends** (`store_memory`, `store_sqlite`, `store_postgres`): implement the
  method identically on all three. Satisfies #1, #2, #3, #4, #5, #6, #9; depends on the
  protocol layer. Postgres parity is the high-risk slice (#4).
- Layer **orchestrator migration** (`_history.py`, `_workflow.py`): replace the
  lifecycle-listing/filtering `store._connection.execute(...)` reads with calls to the new
  method. Satisfies #7, #8; depends on the backend layer.

Shared invariants every layer asserts against (name them so dependent tasks update together):
- **SI-3** — the canonical `LifecycleStore` read method (one signature, all three backends,
  both call-site migrations must agree, and 00032 implements it verbatim on `PostgresStore`):
  `list_lifecycles(self, *, statuses: Collection[Status] | None = None, task_id: str | None = None) -> list[Lifecycle]`.
  Both filters keyword-only and optional (default `None` = no filter); `statuses` is a
  collection so the "active-status set" and "single-status" call sites both fit one method;
  returns fully-folded `Lifecycle` objects (attempts populated, same shape as `load_lifecycle`).
- **SI-5** — the result ordering contract `(updated_at DESC, run_id DESC)` — the orchestrator's
  "latest lifecycle for a task" depends on first-element order; every backend (incl. Postgres
  in 00032) must produce it identically.
- **SI-9** — `CURRENT_SCHEMA_VERSION` stays 12: NO column, NO index migration, NO version bump.
  Shared with spec 00031 (which also adds no migration) and depended on by 00032 (whose parity
  reads assume the same v12 projection on both backends). All three specs land ZERO schema
  changes; if any future need forces a bump it must be a single coordinated migration across
  `store_protocols` + `store_sqlite` + `store_postgres` + `persistence-schema.sql` + its
  Postgres mirror.

## Decisions Log (ADR-style: immutable, supersede — never edit)

### D-1: One list-with-filters method, not separate list / filter-by-status / filter-by-task verbs  (Status: Accepted)
- Context: the audit names two needs (list_lifecycles, filter-by-status) and the raw-SQL
  sites add a third (filter-by-task-id). | Decision: expose a single
  `list_lifecycles(self, *, statuses: Collection[Status] | None = None, task_id: str | None = None) -> list[Lifecycle]`
  read (canonical signature SI-3) with optional keyword-only filters, both defaulting to
  "no filter", subsuming all four current raw-SQL call sites.
- Rejected: three separate methods (more surface for 00031 to reconcile, more backend
  mirrors, combinatorial filter gaps); a generic predicate/query-object (un-gradeable, and
  would smuggle selection logic toward core). | Consequences: one signature to keep in
  parity across three backends; status is a set/collection filter so "active statuses"
  and "terminal statuses" call sites both fit without a second method.

### D-2: Return full `Lifecycle` objects, not a new projection record  (Status: Accepted)
- Context: callers need run_id+status+task_id (status surface), and also error / timestamps
  / attempts (history surface). | Decision: the method returns the same fully-folded
  `Lifecycle` shape `load_lifecycle` returns, attempts populated.
- Rejected: a narrow `LifecycleSummary` dataclass (would need its own schema-coordination
  with 00031, and the history surface already needs attempts so the summary would be
  immediately insufficient — under-specification is a guaranteed exploit surface). |
  Consequences: list of a large run set folds every attempt; acceptable — the audit states
  no perf bar, and every current call site already loads comparable detail.

### D-3: No schema change; reuse the existing `lifecycles.status` / `task_id` columns  (Status: Accepted)
- Context: filtering needs only columns that already exist; `CURRENT_SCHEMA_VERSION` is 12.
  | Decision: add no column, no index migration, no version bump — the method filters over
  existing columns.
- Rejected: adding a covering index or a denormalized column "for speed" (gold-plating; no
  perf NFR in the audit, and a version bump forces every adopter to re-create their store —
  the exact M1/Q6 pain). | Consequences: status filter is a table scan on SQLite; fine at
  current scale. 00031's schema-reconcile inherits "no new column from 00030".

### D-4: Migrate only the lifecycle listing/filtering raw SQL; leave per-run/attempts reads  (Status: Accepted)
- Context: not every `# noqa: SLF001` site is a cross-task lifecycle read — `_history.py`
  and `_workflow.py` also read the `attempts` table per run for token/cost rollups. |
  Decision: this spec lifts only the lifecycle SELECTs (the cross-task gap the audit names);
  per-run attempts reads stay as-is (already covered by `list_attempts`, out of scope).
- Rejected: ripping out every private-connection use at once (scope creep beyond the audit
  finding; risks the aggregate-rollup path). | Consequences: criterion #7 is scoped to
  lifecycle SELECTs so it does not falsely flag legitimate remaining per-run connection use.

### D-5: Prerequisite DAG stays out of the store (audit Q4 deferred)  (Status: Accepted)
- Context: Q4 ("what's blocked on what") wants the DAG in the store, but CLAUDE.md keeps
  cross-task selection/DAG above core, and `schema:48-52` deliberately omits prerequisites.
  | Decision: prerequisites remain sourced from task files at runtime; no persistence here.
- Rejected: persisting prerequisites now (violates the core/orchestrator hard line; the
  audit sequences Q4 after Q1). | Consequences: "what's blocked on what" still needs the
  task source after this spec — explicitly accepted, recorded as out of scope.

### D-6: Reconciliation (2026-06-17) — canonical `list_lifecycles` signature (SI-3), no schema bump (SI-9), and 00032 consumes this surface  (Status: Accepted)
- Context: the store-layer trio 00030/00031/00032 share `StoreProtocol` + `store_sqlite` +
  `store_postgres` + `persistence-schema.sql`. Divergence risk: an under-pinned method
  signature or an accidental schema bump in any one spec would break the parity the other
  two assume.
- Decision: (1) `list_lifecycles` is canonically
  `list_lifecycles(self, *, statuses: Collection[Status] | None = None, task_id: str | None = None) -> list[Lifecycle]`
  (SI-3) — owned and defined here, implemented identically on `store_memory`/`store_sqlite`/
  `store_postgres`, and implemented VERBATIM on `PostgresStore` by spec 00032. (2) No schema
  change, `CURRENT_SCHEMA_VERSION` stays 12 (SI-9), and 00031 agrees (it adds no migration
  either). (3) 00032 HARD-depends on this spec and 00031 (DAG edges `00032 -> 00030` and
  `00032 -> 00031`); 00032 does not redefine the surface, only implements it on Postgres and
  ports the orchestrator reads onto it.
- Rejected: three separate list/filter methods (re-litigated in D-1; multiplies the Postgres
  parity surface 00032 must mirror); any "speed" index or column (D-3); letting 00032 invent
  its own method names if 00030 ships late (the reconcile pins names NOW so 00032 has a fixed
  target).
- Consequences: a change to SI-3's signature ripples to 00031 (which must keep its own new
  method's vocabulary consistent), to the orchestrator call-site migrations here, and to
  00032's PostgresStore implementation and read-path port.

### D-7: Recast criterion 3 as a metamorphic-determinism check, not an injected-`updated_at` tie  (Status: Accepted, 2026-06-17)
- Context: c3's original verify line required seeding "controlled `updated_at` values (and at
  least one `updated_at` tie)". The store sets `updated_at` from its own clock (`_utcnow_iso()`
  on create/update in `store_sqlite`/`store_postgres`; `Lifecycle` carries no caller-settable
  `updated_at` field), so a held-out author cannot inject a value or force a tie deterministically.
  The /fw-verify gate correctly routed c3 to manual on that unauthorable seed step. (Numbered D-7
  because D-6 is taken by the 2026-06-17 reconciliation entry above, which is immutable.)
- Decision: recast c3 to grade the same end-state — the documented `(updated_at DESC, run_id DESC)`
  ordering — through three blind-authorable relations that need no `updated_at` injection:
  (a) metamorphic determinism (identical calls yield byte-identical `run_id` sequences);
  (b) recency observed from store-assigned timestamps (later-created row sorts first);
  (c) the `run_id` DESC tiebreak, asserted only when two rows read back with an equal `updated_at`
  (and falling through to (b) when the clock leaves them distinct, so the test never depends on a
  tie it cannot manufacture). Stays `[command | held-out]`. The SI-5 ordering contract
  `(updated_at DESC, run_id DESC)` is unchanged; only c3's verify/defends-against lines move.
- Rejected: keeping the forced-tie wording behind a `manual` grade (degrades c3 from the
  highest trust tier to a human gate for an outcome that is machine-decidable — a defect per
  fw-spec STEP 3); adding a caller-settable `updated_at` to the store API or `Lifecycle` purely
  to make the tie injectable (changes the surface under test to fit the test — gold-plating, and
  would touch SI-3/SI-9). | Consequences: the tiebreak is exercised opportunistically (only when
  the store's clock resolution yields a real tie), so a backend with sub-`updated_at` resolution
  proves the order via relation (b) rather than (c); both paths still foreclose an unordered or
  non-deterministic result. No change to any other criterion, to SI-3/SI-5/SI-9, or to scope.

## Open Questions (accepted gaps)
None. Every criterion lowers to a `command` grader except V3 (`manual` — relaxed-check
review needs human judgment) and is accounted for.

## Next Steps
Run `/fw-plan 00030-FEATURE-cross-task-query-surface` to compile these criteria into
flywheel tasks and graders.

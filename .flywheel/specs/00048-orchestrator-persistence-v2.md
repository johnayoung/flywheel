# 00048 — Orchestrator-owned persistence for observed work items

Status: spec. Folds Milestones 4, 7, 8 of the WorkGraph roadmap: give the
orchestrator a durable, relational record of the WorkItems it observes from a
WorkSource, their dependency edges, and the source-sync runs that produced them
— plus the source provenance (`source_kind` / `source_version` / `source_url`)
on `WorkItem` itself so each observed item can be persisted and inspected.

## Outcome

After a scheduling pass over a `WorkSource`, the orchestrator's *own* store
holds a queryable catalog of every observed item: `work_items` rows carrying
source provenance and `first_seen_at`/`last_seen_at`, `work_item_dependencies`
rows for the current DAG edges, and a `source_syncs` row recording that pass.
Re-observing an item refreshes `last_seen_at` and clears any prior
`disappeared_at`; an item absent from a *successful* sync is marked
`disappeared_at`. Critically, a *failed* `list_work()` records a `source_syncs`
error row and marks **nothing** disappeared — a tracker hiccup is never read as
"all work vanished." flywheel-core's schema is untouched.

## Background

The orchestrator already owns a store separate from flywheel-core
(`_claims.py`: `task_claims` + an `orchestrator_schema_version` sentinel at
version 1) and already reconciles live runs against a live `list_work()` pass
(`_orchestrate.py:reconcile_live_runs`). That reconcile holds the load-bearing
invariant a literal agent will miss: a *failed* listing interrupts nothing,
because a transient source error must not be read as task disappearance. The
new persistence layer sits beside that reconcile and must preserve the same
posture at the storage level — only a *successful* observed catalog can mark an
item gone. Per `docs/data-taxonomy.md`, `work_items` /
`work_item_dependencies` / `source_syncs` are operational + decision-analytics
*state* (relational, fine); per-tick poll exhaust and SDK logs are *telemetry*
and belong in JSONL sinks, never these tables.

## Scope

### In scope
- `WorkItem` gains three optional provenance fields: `source_kind`,
  `source_version`, `source_url` (frozen + kw_only preserved; all default so
  every existing constructor still compiles).
- `DirectoryWorkSource` and `GithubWorkSource` populate those three fields.
- Three new orchestrator-owned tables (`work_items`,
  `work_item_dependencies`, `source_syncs`) on both the SQLite and Postgres
  claim-store backends, created by bumping `orchestrator_schema_version` 1 → 2
  with an additive forward-migration (`CREATE TABLE IF NOT EXISTS` on open).
- Persistence operations: upsert an observed item, replace the dependency edges
  for the current graph, mark previously-seen-but-now-absent items disappeared,
  and record a source-sync run (start / finish / status / observed_count /
  error).

### Out of scope
- Any change to flywheel-core's schema, its `schema_version`, or its
  `task_versions` table.
- Adding `priority`, `conflict_keys`, or `required_capabilities` *fields* to
  the `WorkItem` dataclass — those are spec 00049. (The `work_items` *table*
  carries forward-compat columns for them with empty/default values; nothing in
  this spec populates them from a `WorkItem` field.)
- Storing telemetry (poll ticks, token streams, SDK logs, spans) in any of the
  new tables.
- Changing the runtime `reconcile_live_runs` / `_source_reconcile_loop`
  interrupt behavior. This spec persists the catalog; it does not alter who gets
  interrupted.

### Must not regress
- Every existing `WorkItem` construction and every source/orchestrator test
  stays green: the new fields are optional with defaults, so direct dataclass
  construction without them keeps working.
- The orchestrator claim store's existing behavior (`task_claims`,
  acquire/renew/release/list, the v1 sentinel) is unchanged.
- The "failed listing marks nothing gone" invariant holds at the storage layer,
  matching the runtime reconcile posture.

## Success Criteria

Each criterion grades an observable end-state — a constructed `WorkItem`
object's attributes or rows in the orchestrator store after an operation — never
a call sequence. The store is read back through the store object the agent does
not author; tests live under
`packages/flywheel-orchestrator/tests/` and run on both backends (Postgres
auto-skips without Docker via the root `require_postgres` fixture).

1. The `WorkItem` value type exposes `source_kind`, `source_version`, and
   `source_url`, each constructible by keyword and each defaulting so a
   `WorkItem(task=..., source_ref=...)` with none of them still constructs.
   [command | held-out]
   verify: `uv run pytest packages/flywheel-orchestrator/tests/` — a test
   constructs a `WorkItem` without the three fields (asserting the defaults) and
   one with all three (asserting they round-trip); both pass.
   defends against: adding the fields as required/positional, which would break
   every existing constructor — caught by the default-construction assertion.

2. When `DirectoryWorkSource.list_work()` enumerates a task file, each returned
   `WorkItem` carries `source_kind == "directory"`, a non-empty `source_url`
   tied to the file path, and a `source_version` derived from the task content
   (the same digest flywheel-core content-addresses by). [command | held-out]
   verify: `uv run pytest packages/flywheel-orchestrator/tests/` — a test points
   a `DirectoryWorkSource` at a fixture task file and asserts the three
   provenance fields on the returned item, with `source_version` equal to
   `flywheel_core.loaders.task_digest(item.task)`.
   defends against: stamping a constant/empty version string — the assertion
   pins `source_version` to the actual task digest, so a hardcoded value fails
   when the task content differs.

3. When `GithubWorkSource.list_work()` compiles an issue, each returned
   `WorkItem` carries `source_kind == "github_issue"`, `source_url` equal to the
   issue URL, and a non-empty `source_version` that changes when the issue body
   changes. [command | held-out]
   verify: `uv run pytest packages/flywheel-orchestrator/tests/` — a test runs
   `GithubWorkSource` over a canned issue (injected `gh` runner) and asserts
   `source_url` is the issue url and that two issues with different bodies yield
   different `source_version` values.
   defends against: returning a constant `source_version` — the differing-body
   assertion fails a constant; a version that does not vary with content is no
   provenance.

4. When a sync runs a successful `list_work()` pass, every observed item is
   upserted into `work_items` with `first_seen_at` and `last_seen_at` set and
   `disappeared_at` NULL, and its dependency edges appear in
   `work_item_dependencies` keyed `(task_id, prerequisite_task_id)`.
   [command | held-out]
   verify: `uv run pytest packages/flywheel-orchestrator/tests/` — a test syncs a
   source with a prerequisite edge, then reads the store back and asserts one
   `work_items` row per observed task (with both timestamps set, `disappeared_at`
   NULL) and the expected `work_item_dependencies` row(s).
   defends against: writing the catalog but dropping edges (or vice versa) — the
   test reads both tables and asserts row content, not just that the call
   returned.

5. When an item already in `work_items` is observed again by a later successful
   sync, its `last_seen_at` advances and any prior `disappeared_at` is cleared,
   while its `first_seen_at` is unchanged. [command | held-out]
   verify: `uv run pytest packages/flywheel-orchestrator/tests/` — a test syncs,
   marks an item disappeared, then syncs again observing it; asserts
   `first_seen_at` unchanged, `last_seen_at` advanced, `disappeared_at` NULL.
   defends against: re-insert-on-conflict that resets `first_seen_at`, or an
   upsert that leaves a stale `disappeared_at` — both are pinned by the
   read-back assertions.

6. When a successful sync observes a catalog that omits a previously-seen item,
   that item's `work_items` row gets `disappeared_at` set (and is not deleted).
   [command | held-out]
   verify: `uv run pytest packages/flywheel-orchestrator/tests/` — a test syncs
   two items, then syncs a catalog with only one, and asserts the omitted item's
   row still exists with `disappeared_at` set while the present item's
   `disappeared_at` stays NULL.
   defends against: hard-deleting absent items (losing history) — the assertion
   requires the row to remain with a timestamp, not vanish.

7. If a `list_work()` pass fails, then the sync records a `source_syncs` row with
   `status == "error"` and a non-empty `error`, and **no** `work_items` row is
   marked `disappeared_at` by that failed pass. [command | held-out]
   verify: `uv run pytest packages/flywheel-orchestrator/tests/` — a test seeds a
   catalog via a successful sync, then runs a sync whose `list_work()` raises;
   asserts a new `source_syncs` row with `status="error"` and non-empty `error`,
   AND that every previously-seen item still has `disappeared_at` NULL.
   defends against: the headline reward-hack — treating a tracker/transport
   failure as task disappearance (marking items gone, or interrupting). The
   "nothing disappeared after a failed pass" assertion forecloses it; a literal
   agent that marks-absent-on-any-non-observation fails this test.

8. When a successful sync finishes, its `source_syncs` row carries
   `status == "ok"`, an `observed_count` equal to the number of items the pass
   observed, and a non-null `finished_at`. [command | held-out]
   verify: `uv run pytest packages/flywheel-orchestrator/tests/` — a test syncs N
   items and asserts the recorded `source_syncs` row has `status="ok"`,
   `observed_count == N`, and `finished_at` set.
   defends against: recording a sync that never reflects what was seen — pinning
   `observed_count` to N stops a constant 0 / unset count from passing.

9. When an existing v1 orchestrator store (a store created before this change,
   holding `task_claims` rows) is opened by the new code, it converges to
   schema version 2, gains the three new tables, and its existing `task_claims`
   rows survive. [command | held-out]
   verify: `uv run pytest packages/flywheel-orchestrator/tests/` — a test writes
   a claim through the store, reopens it, and asserts: the
   `orchestrator_schema_version` sentinel reads 2, the new tables accept a
   work-item upsert, and the pre-existing claim still loads.
   defends against: a destructive migration (drop-and-recreate) or a hard
   version-mismatch error on an additive bump — the surviving-claim assertion
   fails either.

10. The persistence criteria (#4–#9) hold against **both** the SQLite and the
    Postgres claim-store backends. [command | held-out]
    verify: `uv run pytest packages/flywheel-orchestrator/tests/` — the
    persistence tests are parametrized/duplicated across SqliteClaimStore and
    PostgresClaimStore (Postgres via the root `postgres_dsn` fixture, auto-skipped
    when Docker is absent).
    defends against: implementing the SQLite path only and leaving Postgres a
    stub — the Postgres-parametrized cases fail (or are visibly skipped only for
    Docker absence, never for missing implementation).

11. flywheel-core's `schema_version` and `task_versions` table definition are
    unchanged by this feature. [command | held-out] (verification-surface)
    verify: `uv run pytest packages/flywheel-core/tests/` passes unchanged, and
    `git diff` touches no file under
    `packages/flywheel-core/src/flywheel_core/_schema/` and does not alter
    `CURRENT_SCHEMA_VERSION` in flywheel-core.
    defends against: leaking DAG/source columns into core's schema to satisfy a
    persistence criterion — the core suite plus the no-core-schema-change
    assertion forecloses it.

Verification surface: this feature ADDS tables and an additive migration on the
orchestrator store — it is itself a state/grader surface. Definition of Done
(inherited by every task touching it, all held-out): the existing orchestrator
suite (`uv run pytest packages/flywheel-orchestrator/tests/`) and the core suite
(`uv run pytest packages/flywheel-core/tests/`) still pass after the change; the
additive 1→2 migration does not destroy a v1 store (criterion #9); no
flywheel-core schema or `schema_version` change (criterion #11). No grading
assertion may be relaxed, skipped, or deleted; a removed assertion with no
equal-or-stronger replacement is a blocking defect.

## Decomposition Hint (for /fw-plan)
- Layer **value type + sources**: satisfies #1, #2, #3. Adds the three
  `WorkItem` fields and populates them in both adapters. No store dependency.
- Layer **store schema + work-item/edge persistence**: satisfies #4, #5, #6, #9,
  #10 (the work_items / work_item_dependencies parts). Owns the 1→2 migration
  and the upsert/replace-edges/mark-disappeared operations on both backends.
- Layer **sync recording + failed-listing safety**: satisfies #7, #8, and the
  source_syncs part of #10. Records the sync run and enforces the
  failed-pass-marks-nothing posture; depends on the schema/persistence layer.

Shared invariants multiple layers assert against:
- The three new `WorkItem` field names (`source_kind`, `source_version`,
  `source_url`) — the value-type layer defines them; every source constructor
  and any persistence caller that reads them updates in the same change.
- `CURRENT_ORCH_SCHEMA_VERSION` bumps 1 → 2 — both backend bootstraps and the
  v1-upgrade test assert against the same value; bump them together.
- The `work_items` column set (including the unpopulated forward-compat
  `priority` / `required_capabilities_json` / `conflict_keys_json`) is shared by
  the persistence and sync layers; define it once.

## Decisions Log (ADR-style: immutable, supersede — never edit)

### D-1: Reuse flywheel-core's `task_digest` as the work-item content hash  (Status: Accepted)
- Context: `work_items.task_content_hash` must content-address an observed item,
  and a #2 criterion pins `DirectoryWorkSource`'s `source_version` to the task
  content. flywheel-core already content-addresses task definitions in
  `task_versions` keyed `(task_id, content_hash)` where `content_hash =
  flywheel_core.loaders.task_digest(task)` (loaders.py:364; canonical
  JSON of goal/graders/tags/context, SHA-256, id and prerequisites excluded).
- Decision: compute `work_items.task_content_hash` and
  `DirectoryWorkSource`'s `source_version` via `task_digest(item.task)`. Do not
  invent a second hash.
- Rejected: a local/new hash over a different field set — would diverge from the
  value a run pins in core, defeating cross-referencing and adding a redundant
  hashing convention.
- Consequences: the orchestrator imports `task_digest` from
  `flywheel_core.loaders` (already a dependency; sources import from it today).
  GithubWorkSource's `source_version` is NOT the task digest (see D-2).

### D-2: `source_version` semantics differ per source  (Status: Accepted)
- Context: `source_version` should track "did this item change at the source."
  For directories the task content is the source of truth; for GitHub the issue
  body is what the operator edits, and the `gh` list already returns `body` and
  `url` (`_LIST_FIELDS = "number,title,body,url"`), with no extra field needed.
- Decision: directory → `task_digest(item.task)` (D-1); github → a stable hash
  of the issue body (content that changes when the operator edits the issue).
  `source_url`: directory → the file path; github → the issue `url`.
- Rejected: requiring a new `updatedAt` gh field — adds a query-shape change and
  a timestamp that is coarser than body content; the body hash is deterministic
  and already available. (A future spec may add `updatedAt` if needed.)
- Consequences: #3 grades github `source_version` by "differs when body differs,"
  not by equality to any digest — kept source-appropriate and gradeable.

### D-3: A sync wraps one `list_work()` pass; the store end-state is graded  (Status: Accepted)
- Context: WHEN persistence is written relative to the existing scheduling /
  reconcile loops. The runtime already lists work per pass
  (`reconcile_live_runs` consumes `list_work()`); persistence should attach to a
  pass, not a new clock.
- Decision: one *sync* = (record source_syncs start) → run `list_work()` → on
  success: upsert observed items, replace the current edge set, mark
  previously-seen-but-absent items disappeared, record finish `status="ok"` with
  `observed_count`; on failure: record finish `status="error"` with the error and
  mark NOTHING disappeared. Integration point is pinned, but every criterion
  grades the resulting rows (present / updated / marked), never the call order.
- Rejected: grading a fixed call sequence — path-grading; would have to change
  when the wiring changes though behavior does not.
- Consequences: the failed-pass safety (criterion #7) is a property of the
  store end-state, mirroring `_source_reconcile_loop`'s "failed listing
  interrupts nothing" posture.

### D-4: `source_name` = the source's locus  (Status: Accepted)
- Context: `source_syncs.source_name` distinguishes which concrete source a sync
  ran against.
- Decision: directory → the `tasks_dir` path string; github → the `repo`
  (`owner/repo`). Paired with `source_kind` ("directory" / "github_issue").
- Rejected: a synthetic id — the natural locus is human-meaningful and already
  on each adapter.
- Consequences: no new identity scheme; `source_name` is read straight off the
  adapter.

### D-5: Forward-compat columns stay unpopulated this spec  (Status: Accepted)
- Context: `work_items` carries `priority` / `required_capabilities_json` /
  `conflict_keys_json` for spec 00049, but the corresponding `WorkItem` fields
  do not exist until 00049.
- Decision: create the columns with defaults (`priority` 0, the json columns
  `'[]'`) and DO NOT populate them from any `WorkItem` field now. 00049 adds the
  fields and starts populating, inheriting a stable column set.
- Rejected: omitting the columns until 00049 — would force a second migration;
  additive columns now cost nothing and keep 00049 a data change, not a schema
  change.
- Consequences: tests must not assert non-default values for these columns in
  this spec; they only assert the columns exist with defaults if at all.

## Open Questions (accepted gaps)
None. Every criterion lowers to a `command` grader over the orchestrator (and,
for #11, core) test suite.

## Next Steps
Run `/fw-plan 00048-orchestrator-persistence-v2` to compile these criteria into
flywheel tasks and graders.

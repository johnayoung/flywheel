# Orchestration (scheduling and the orchestrator store)

`flywheel-core` owns the lifecycle of a **single task** and knows nothing about who calls it. `flywheel-orchestrator` is built on top to drive **many** tasks: it decides which task runs next, coordinates several workers over a shared store, and persists a queryable catalog and ledger of what it observed and did.

**Cross-task concepts live here and never leak into core.** The prerequisite DAG, priority/capability selection, claims/leases, conflict keys, and the orchestrator's own store are all above the line. The dependency arrow points one way: orchestrator imports core, never the reverse. See [vision.md](vision.md) for what the single-task loop is and is not.

The operator-facing entry point is `flywheel worker` (in the product shell, see [cli.md](cli.md)); the orchestrate loop itself is library plumbing exposed as `flywheel_orchestrator.orchestrate`. It is safe to run several concurrent workers against one store — per-task leases keep them from colliding.

## The WorkGraph

`flywheel_orchestrator.WorkGraph` (`_work_graph.py:113`) lifts the implicit prerequisite edges riding on each `WorkItem` (`WorkItem.prerequisites`, a tuple of task ids) into a first-class, validated prerequisite DAG. It is built and validated once at the top of every scheduling pass (`_orchestrate.py:841`), so structural corruption is caught loudly instead of silently deadlocking.

### Validation: hard fail vs soft issue

Construction validates eagerly. Defects split into two buckets:

| Defect | Outcome | Where |
| --- | --- | --- |
| Duplicate task id | Raises `WorkGraphValidationError` | `_work_graph.py:125` |
| Self-dependency (task lists its own id) | Raises `WorkGraphValidationError` | `_work_graph.py:150` |
| Cycle (multi-node) | Raises `WorkGraphValidationError`, naming every member | `_work_graph.py:165` |
| Missing prerequisite (dangling edge) | Non-fatal `GraphValidationIssue` on `.issues` | `_work_graph.py:155` |

Structural corruption raises `WorkGraphValidationError` (`_work_graph.py:64`) with a message naming the offending id(s); a cycle names every participating member (detected via iterative Tarjan SCC, so long chains do not hit a recursion limit). A **missing prerequisite does not raise**: under multi-source aggregation a referenced item may simply not be loaded by a sibling source this pass, so it is recorded as a `GraphValidationIssue` and the referencing task stays out of the ready set rather than aborting the whole graph.

A prerequisite that is absent from the listing is **satisfied by a `done` lifecycle in the store**. The store is the authoritative record of completion; the source listing is an input surface, not the record ([data-taxonomy.md](data-taxonomy.md)). When a prerequisite's defining task leaves the listing (e.g. its phase archived, moving the task JSON out of `active/`) while its lifecycle already reached `done`, the scheduler resolves the edge off the store — the dependent is dispatched, and **no** dangling witness or `prerequisite-missing` review entry is recorded for that edge. The caller consults the store only for ids no listed row provides (`satisfied_prerequisites_from_store`, `_workflow.py`) and hands the resulting set in as data, so `_work_graph.py` stays store-agnostic and no per-listed-task read is added. Only a `done` lifecycle satisfies — a `failed` / `running` / absent id still dangles and the bounded re-driver applies.

### Eligibility (the scheduling query)

`WorkGraph.ready_set(states, excluded=frozenset(), *, worker_capabilities=frozenset(), satisfied_prerequisites=frozenset())` (`_work_graph.py:257`) returns **every** runnable item this pass. An item is runnable iff all hold:

1. its id is not in `excluded`;
2. its own state is one of `fresh` / `retryable` / `interrupted` (`_ELIGIBLE_STATE_VALUES`, `_work_graph.py:47`);
3. every **declared** prerequisite is satisfied — either it resolves to a listed node whose state is `done`, or its id is in `satisfied_prerequisites` (absent from this pass's listing but `done` in the authoritative store); a prerequisite that is neither fails this gate;
4. its `required_capabilities` is a subset of `worker_capabilities`.

Results are ordered by **descending `priority`**, ties broken by construction/walk order via a stable sort (`_work_graph.py:308`); an all-default (priority 0) set is byte-identical to pure walk order. The loop takes `ready[0]` (`_orchestrate.py:1050`). The `select_next_task` CLI path (`_workflow.py:302`, used by `flywheel next`) applies the identical predicates but returns only the single highest-priority match.

An excluded id drops from candidacy but still satisfies a dependent's prerequisite — exclusion is a "skip this one now," not a "treat it as undone."

### Multi-source aggregation

`WorkGraphBuilder.build(*sources)` (`_work_graph.py:338`) asks each [work source](work-sources.md) for `list_work()`, concatenates into one combined set in argument order, then validates the aggregate (never per source). A prerequisite declared by source A and satisfied by source B resolves as a real edge; a reference unresolved in every source becomes a `GraphValidationIssue`.

## Selection and scheduling

Two scheduling dimensions ride on each task, both read off the `WorkItem`:

| Knob | Type | Default | Effect |
| --- | --- | --- | --- |
| `priority` | int | `0` | Descending hard ordering key; stable sort preserves walk order on ties |
| `required_capabilities` | set of str | empty | Item runs on a worker iff this is a subset of the worker's advertised capabilities |

For a directory source, both are read from the task file's top-level JSON keys (`priority`, `required_capabilities`, `conflict_keys`) by `DirectoryWorkSource`; GitHub-derived sources do not yet populate them. A task with empty `required_capabilities` runs on any worker, including a worker advertising no capabilities.

The worker's advertised set is `WorkPolicy.execution_capabilities`, configured under `[execution] capabilities` in `flywheel.toml` (`_policy.py:458`), defaulting to empty. **`[execution] capabilities` (worker work-classes) is distinct from `[sandbox.capabilities]` (agent tools/skills/MCP)** — see [configuration.md](configuration.md). Priority and capability filtering apply in both execution modes.

## Claims and leases (multi-worker mutual exclusion)

The orchestrator's per-task lease store coordinates concurrent workers. A worker holds a lease while running a task and releases it on completion; the live lease is the liveness signal. A crashed worker's heartbeat stops, the lease lapses, and another worker reclaims it. This is transient coordination state, not audit history, which is why it lives in the orchestrator store rather than core.

`ClaimStore` (`_claims.py:260`) is a `runtime_checkable` protocol with three implementations: `InMemoryClaimStore` (tests), `SqliteClaimStore` (durable default), and `PostgresClaimStore` (behind the `postgres` extra). All share one contract:

| Method | Behavior |
| --- | --- |
| `acquire_claim` | Returns a `TaskClaim` when the task is free, the existing lease has expired (steals it), or the caller already holds it (idempotent). Returns `None` when a **live** lease is held by another worker, or the item's conflict keys overlap a different live claim's keys. Check-and-write is atomic. |
| `renew_claim` | Extends the lease and bumps `version`; raises `ClaimLostError` when the token no longer matches (lapsed-and-stolen, or released). |
| `release_claim` | Drops the claim if the token still matches; no-op if already stolen or released. |
| `load_claim` / `list_claims` | Current claim for a task / every held row (expiry is not filtered). |
| `sweep_expired_claims` | Batch-releases every claim whose lease has lapsed in one pass, returning the freed task ids. |

`TaskClaim` (`_claims.py:103`) is an immutable snapshot of `task_id`, `worker_id`, `claimed_at`, `lease_expires_at`, `version`. `(version, worker_id)` is the optimistic-concurrency token for renew/release — a stale token (wrong version or different worker) is rejected with `ClaimLostError`.

### The lease window and heartbeat

The default lease is `DEFAULT_LEASE_SECONDS = 300.0` (`_orchestrate.py:142`), overridable per worker via `--lease-seconds`. While a run is in flight, `_ClaimHeartbeat` (`_orchestrate.py:498`), a daemon thread, renews the lease every `lease_seconds / 3`. If the process dies the thread dies, the lease lapses, and the task becomes reclaimable.

**Clock-skew gotcha: lease expiry is compared against each worker's own clock, with no shared or monotonic clock.** `lease_seconds` must exceed (max cross-host clock skew + longest inter-heartbeat gap), or a fast-clocked worker can steal a live peer's lease. The steal is contained to that one task — the preempted worker relinquishes cleanly rather than corrupting state — but the preempted work is wasted (`_orchestrate.py:132`).

### Conflict keys

`acquire_claim` refuses a claim whose `conflict_keys` overlap a **different live claim's** keys, so two tasks that touch the same resource never run concurrently. The task's own row is excluded and lapsed claims do not block, so the refusal clears once the conflicting claim is released or lapses. Empty `conflict_keys` (the default) is never refused on that basis. Keys are supplied at acquire time and persisted on the claim row (`conflict_keys_json`).

The loop passes keys wherever a claim gates repo work: the fresh-dispatch and blocked-resume acquires carry the `WorkItem`'s `conflict_keys`, and the claim is held through verify and landing, so two overlapping items never have concurrent edit-to-land windows. Bookkeeping acquires (stranded finalize, retry escalation, approval resolve, landing re-drive) deliberately pass none — record-keeping on one task must not queue behind an unrelated task that merely shares a file key, and a parked branch's textual conflicts are already baked in by re-land time. A conflict-refused task is skipped for the pass, not consumed: it stays eligible and is retried once the overlapping claim releases.

### Liveness sweep

`sweep_expired_claims` (`_claims.py:1144`) batch-releases every lapsed claim — including all of one dead worker's claims — in a single pass, leaving still-valid claims untouched, and emits one `expired` event per reaped claim. SQLite compares ISO-timestamp strings lexically; Postgres compares `TIMESTAMPTZ` temporally.

**Partial: the sweep is implemented on all backends but has no production caller.** Reclaim today happens lazily, when another worker steals an individual lapsed lease via `acquire_claim`. Wiring a periodic sweep is left to a future spec.

### The orchestrator's own store

The orchestrator owns a store separate from core's: its own `task_claims` plus an `orchestrator_schema_version` sentinel, currently at `CURRENT_ORCH_SCHEMA_VERSION = 5` (`_claims.py:67`). A single SQLite file (or one Postgres database) can hold **both** stores; each layer touches only its own tables and never references the other's, and the two `*_schema_version` sentinels migrate independently. The orchestrate loop opens both on one file: the core lifecycle store and `SqliteClaimStore(db_path)` (`_orchestrate.py:797`).

Migrations are additive forward only — `CREATE TABLE IF NOT EXISTS` plus sentinel convergence, never drop-and-recreate. A store newer than the running code trips `OrchestratorSchemaError`. The orchestrator DDL lives inline in `_claims.py` (SQLite/in-memory) and `_claims_postgres.py` (Postgres) — there is no `.sql` mirror, and no `docs/` table reference yet documents the orchestrator table set ([persistence-tables.md](persistence-tables.md) covers only the core store).

| Version | Adds |
| --- | --- |
| v1 | `task_claims` + `orchestrator_schema_version` |
| v2 | `work_items`, `work_item_dependencies`, `source_syncs` |
| v3 | `task_claims.conflict_keys_json` |
| v4 | `orchestrator_events` |
| v5 (current) | `graph_snapshots`, `graph_snapshot_items` |

## The orchestrator ledgers

Two append-only observability surfaces live in the orchestrator store on all three backends.

### orchestrator_events

A durable, append-only ledger recording every **committed** claim-lease transition as its own immutable row, so an operator can reconstruct a task's full holder timeline from the store alone. Five event types (`_claims.py:73`):

| Type | Meaning |
| --- | --- |
| `acquired` | Fresh claim or same-worker re-acquire |
| `stolen` | Reclaim over a **different** worker's lapsed lease (deliberately distinct from `acquired`) |
| `renewed` | Lease extended |
| `released` | Claim explicitly dropped |
| `expired` | Claim reaped by the liveness sweep |

Each event row is inserted **in the same transaction** as the `task_claims` change it describes (`_claims.py:898`): a committed transition always has its event, and a refused, rolled-back, or no-op operation writes none. There is no `lost` event — a rejected renew commits no state change. The ledger is append-only by design: `list_events()` and `list_task_events(task_id)` read it in insertion order; there is no update or delete method.

### graph_snapshots

An append-only record of point-in-time WorkGraph cross-sections: one `GraphSnapshotItem` row per work item carrying its full materialized state (provenance, `priority`, `required_capabilities`, `conflict_keys`, lifecycle `state`, whether it was `ready` this pass, `claim_holder`, `resolved_prerequisites`), plus a `GraphSnapshotRecord` header stamping the snapshot's `last_event_id` — the `orchestrator_events` high-water mark at write time. Snapshots are the materialized-state half of WorkGraph observability; the event ledger is the delta half. Replaying events past a snapshot's recorded cursor reconstructs graph state at any later point.

The loop records **exactly one snapshot per scheduling pass**, at the top of the pass after graph/states/readiness are known and before any dispatch (`record_graph_snapshot`, `_orchestrate.py:862`). It is a pure read-and-record side channel — it never changes which task is dispatched. The header and every item row commit in one transaction (no partial cross-section), and `last_event_id` is store-computed inside that transaction so it cannot drift from the ledger. Reads: `list_graph_snapshots()`, `list_graph_snapshot_items(snapshot_id)`, `latest_graph_snapshot()`.

**Caveat: snapshots are per-pass with no throttle or retention.** A single `orchestrate` call records a bounded count (roughly one per task driven plus a terminal pass), but a long-lived daemon calling `orchestrate` repeatedly accumulates snapshots unbounded. Bounding that is a future spec.

There is also a relational work-item catalog (`work_items` / `work_item_dependencies` / `source_syncs`, schema v2) populated by `sync_work_source`. **It is a library capability, not active loop behavior: `sync_work_source` has no caller inside `orchestrate`**, so the shipped worker does not auto-populate this catalog.

## status --rollup

`flywheel status --rollup` is the evidence-derived read projection — the north-star "read side" of [vision.md](vision.md). Every node's status is **computed** from lifecycle state plus grader receipts, never operator-set (`_rollup.py:154`). The honest distinction a plain status table cannot draw: a task `done` with passing graders is `verified`; a task `done` with **no** graders is `accepted` — the agent's own unverifiable claim.

| Status | Meaning |
| --- | --- |
| `verified` | Done, with passing grader receipts |
| `accepted` | Done, but no graders ran (unverifiable self-claim) |
| `in_progress` | Currently running |
| `blocked` | Blocked awaiting input |
| `failed` | Terminal failure |
| `blocked_by_prereq` | Not started, and a prerequisite is not yet done |
| `not_started` | Not started, prerequisites satisfied |

The rollup is phase-grouped (file-backed rows group under their phase directory; external items under `source_ref`). Verification evidence reads the latest receipt-bearing attempt of the most recent `done` run, so an earlier failed attempt's red graders are not counted against a now-passing task (`_rollup.py:99`). It reads through the store protocol, so it runs identically on SQLite and Postgres. `--rollup --json` emits the same projection as structured JSON.

## Distributed mode

`[execution] mode` defaults to `local`; setting it to `distributed` requires `[store] backend = "postgres"` or `load_policy` fails fast with a `PolicyError` (`_policy.py:531`). SQLite corrupts under multi-host contention, so distributed runs must use the Postgres backend. See [configuration.md](configuration.md) for the `[execution]` and `[store]` tables.

**Scheduling, claims, and leases are always on, regardless of `[execution] mode`.** Today `execution_mode` is a pure load-time assertion — it gates no runtime behavior. Per-task leases, priority/capability selection, the WorkGraph, and both ledgers run identically in `local` and `distributed`; `mode = "distributed"` only forces the Postgres backend at load time. Distributed coordination across hosts is what the Postgres claim store provides; the mode flag is the assertion that you are using it.

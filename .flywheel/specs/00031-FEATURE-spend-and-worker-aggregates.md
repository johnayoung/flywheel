# Feature: Spend aggregation and per-worker activity query surface

## Outcome
The store and claim surfaces answer two fleet questions from persisted data alone,
with no on-disk task files and no raw `_connection` SQL at the call site:

1. **Spend across tasks (and over a time bound):** a store read method returns the
   summed token and cost rollups across all runs, and across only the runs whose
   activity falls inside a caller-supplied `[start, end)` window.
2. **What each worker is doing:** the claim store enumerates the currently-held
   leases with their owning `worker_id`, and the live/status read path surfaces the
   already-persisted `lifecycles.worker_id` for each in-flight run.

Measurably different when done: holding only the database (SQLite or Postgres) and
the claim store, an operator can compute total agent spend, spend within a date
range, and the worker -> task assignment — none of which has a query path today.

## Background
Token/cost rollups already exist per-attempt (`attempts.input_tokens` ..
`attempts.total_cost_usd`, schema v12) and are summed per-run by `_history.py`'s
`_attempt_rollups`, but no protocol method aggregates them *across* tasks or *over a
time bound*: a company watching agent burn has no total-spend query (audit Q2).
`ClaimStore` exposes only `load_claim(task_id)` — you must already know the task id
— so "list the live claims" is impossible, and `lifecycles.worker_id` is persisted
(`store_sqlite.py:270`) and read into `Lifecycle` but never selected by
`collect_live_rows` (`_workflow.py:889`) nor carried on `LiveRunRow`, so the
worker -> task mapping is dark on every read surface (audit Q3).

Tacit value a literal agent would miss: the spend aggregate is the *load-bearing*
deliverable, not a convenience over the existing per-run sum. It must (a) live on
the store protocol so every backend implements it identically and external readers
get one contract, (b) be a true cross-run rollup (sum over many runs, not one), and
(c) honor a time bound by the same activity timestamp the relational rollups already
use (`attempts.last_activity_at`), so a window query is not silently a no-op that
returns the grand total. The cheapest fake — returning the all-time total regardless
of the window, or summing one run — passes a naive "is it a number" check while
delivering nothing.

## Scope
### In scope
- A store-protocol read method that returns the cross-run token totals
  (input/output/cache-creation/cache-read) and summed `total_cost_usd`, computed over
  every attempt of every run in the store.
- The same method accepting an optional half-open time window `[start, end)`: when
  supplied, only attempts whose activity timestamp falls inside the window contribute
  to the sums.
- Implementation of that method on all three shipped backends (in-memory, SQLite,
  Postgres) returning identical results for identical data.
- A `ClaimStore` method that enumerates the store's current claims, each carrying its
  `task_id`, `worker_id`, `claimed_at`, `lease_expires_at`, and `version`.
- `worker_id` carried on the live in-flight read path: the per-in-flight-run snapshot
  exposes the owning worker, sourced from the persisted `lifecycles.worker_id`, and it
  appears in the `status --json` machine-readable output for in-flight runs.

### Out of scope
- Cross-task *selection* / scheduling logic (which task runs next, DAG ordering) —
  stays above core, untouched here.
- Persisting the prerequisite DAG (audit Q4), telemetry query/filter (Q5), an external
  redacted read contract (Q6), `list_lifecycles` / filter-by-status (Q1) — separate specs.
- Postgres read-path enablement for the *verbs* (M1) — this spec only requires the new
  store method to be implemented on the Postgres backend, not that `flywheel` verbs
  accept `backend = postgres`.
- Any change to how `worker_id` is *written* (the worker daemon already persists it);
  this spec only reads and surfaces it.
- A new operator verb dedicated to spend. The spend aggregate is a store method; whether
  a `spend` CLI verb wraps it is a downstream (00032) concern.

### Must not regress
- Existing per-run rollups (`flywheel history` / run-detail token and cost totals)
  remain byte-for-byte unchanged for the same data.
- `collect_live_rows` continues to read from relational rows only (no telemetry-event
  scan) and continues to include `running`/`validating`/`awaiting_approval` runs sorted
  by `task_id` (spec 00011/00025 invariant).
- `flywheel_core.task` and `flywheel_core.lifecycle` stay pure (no IO/JSON/path imports);
  the new aggregate lives on the store protocol + concrete stores, never in those modules.
- The full existing suite (`uv run pytest`) stays green.

## Success Criteria
Each criterion is atomic, grades an observable end-state, and carries its grader type,
visibility, the exact check, and the gaming move it forecloses.

1. When the spend-aggregate method is called with no time bound on a store holding
   attempts across two or more distinct runs, the returned total equals the sum of every
   attempt's token and cost columns across all those runs. [command | held-out]
   verify: held-out pytest seeds N>=2 runs each with >=1 attempt carrying known,
   distinct token/cost values (chosen so no single run equals the grand total and the
   grand total is not a round number), calls the method, and asserts each returned field
   equals the independently-computed cross-run sum.
   defends against: summing a single run, returning the latest run's totals, or
   returning a hardcoded/placeholder number — the assertion compares against a computed
   multi-run sum the agent cannot guess from the method signature.

2. When the spend-aggregate method is called with a half-open window `[start, end)`, the
   returned totals include only attempts whose activity timestamp is within the window
   and exclude every attempt outside it. [command | held-out]
   verify: held-out pytest seeds attempts at distinct activity timestamps straddling a
   window boundary (one strictly before `start`, one inside, one at-or-after `end`),
   calls with that window, and asserts the result equals only the in-window attempt's
   values — and separately that a window containing no attempts returns all-zero totals
   (not the grand total).
   defends against: ignoring the window and returning the all-time total (the cheapest
   fake), or an off-by-one on the half-open boundary — the empty-window-returns-zero and
   straddling-boundary cases both fail a total-ignoring implementation.

3. When the spend-aggregate method is called on an empty store (no runs), it returns
   all-zero totals rather than raising or returning null. [command | visible]
   verify: pytest opens a freshly bootstrapped store, calls the method with and without a
   window, and asserts every returned field is zero (0 tokens, 0.0 cost).
   defends against: a method that only works once data exists, or that raises on the
   empty case and is "passed" by never exercising it.

4. The spend-aggregate method returns identical results for identical seeded data across
   the in-memory, SQLite, and Postgres backends. [command | held-out]
   verify: a parameterized contract test (the existing store-contract harness over
   in-memory + SQLite, plus the Postgres-backed fixture which skips only when Docker is
   unavailable) seeds the same fixture and asserts the same totals from every backend.
   defends against: implementing the method on only one backend (e.g. SQLite) while the
   protocol claims all backends satisfy it — the Postgres/in-memory arms fail if the
   method is missing or divergent.

5. When the claim-enumeration method is called on a claim store holding two or more live
   claims owned by distinct workers, it returns one entry per held claim, each carrying
   the correct owning `worker_id` and `task_id`. [command | held-out]
   verify: held-out pytest acquires claims for distinct `(task_id, worker_id)` pairs on a
   claim store, calls the enumeration method, and asserts the returned set of
   `(task_id, worker_id)` pairs equals the set acquired (order-independent).
   defends against: returning a single claim, returning task ids without their worker, or
   reconstructing the list from a known input — the assertion is set-equality against
   pairs the test acquired, not a count.

6. When the claim-enumeration method is called after a claim is released, the released
   claim is absent from the result. [command | visible]
   verify: pytest acquires two claims, releases one, calls the enumeration method, and
   asserts only the still-held claim's `task_id` is present.
   defends against: enumerating a stale snapshot or never removing released rows, which
   would report a freed worker as still busy.

7. While an in-flight run has a persisted `worker_id`, the live in-flight snapshot for
   that run exposes that exact `worker_id`. [command | held-out]
   verify: held-out pytest creates a lifecycle in a live status (`running`) with a known,
   non-empty `worker_id`, builds the live snapshot from the store, and asserts the
   snapshot entry for that run reports that exact `worker_id` string.
   defends against: leaving the field unread (still `None`/empty) while claiming the
   feature is done, or hardcoding a worker id — the assertion compares against the
   specific seeded id.

8. When `status --json` is run against a store with an in-flight run carrying a persisted
   `worker_id`, the JSON entry for that run includes that worker id under a stable key.
   [command | held-out]
   verify: held-out test seeds an in-flight run with a known `worker_id`, invokes the
   status command in `--json` mode, parses stdout as JSON, and asserts the entry for that
   run contains the seeded worker id; `python3 -m json.tool` confirms the output is valid
   JSON.
   defends against: surfacing the worker only in human-rendered text (ungrep-able) or
   emitting malformed JSON — the test asserts on parsed JSON structure, not a substring.

9. If an in-flight run has no persisted `worker_id` (null/empty column), then the live
   snapshot and `status --json` for that run shall report the worker as absent (null /
   omitted key) rather than a fabricated or empty-string-as-present value. [command | visible]
   verify: pytest seeds an in-flight run whose `worker_id` is unset, builds the snapshot
   and the `--json` output, and asserts the worker field is null or the key is omitted —
   never a non-null placeholder.
   defends against: defaulting a missing worker to a sentinel like `"unknown"` or `""`
   treated as a real worker, which would corrupt fleet attribution.

10. The existing verification suite still passes after the change. [command | held-out | (verification-surface)]
    verify: `uv run pytest` exits 0 with no test deleted or skipped relative to the
    pre-change baseline (CI runs the full suite; the diff adds tests and does not remove
    or `@skip` existing ones).
    defends against: making new criteria pass by weakening, deleting, or skipping existing
    store/claim/live tests — the suite is run whole, out-of-band from the implementing agent.

11. The new spend-aggregate and claim-enumeration behaviors are proven by contract-level
    tests the implementing agent did not author against its own known inputs, exercised
    across every backend the protocol claims to support. [command | held-out | (verification-surface)]
    verify: criteria #1, #2, #4, #5, #7, #8 are graded by held-out pytest fixtures whose
    expected values are computed from seeded data the agent does not see; the Postgres arm
    runs whenever Docker is available and the SQLite/in-memory arms always run.
    defends against: an agent writing a test that asserts exactly what its implementation
    returns (tautological green) — the authoritative seeds and expected sums are held out
    and computed independently.

Verification surface: the existing suite still passes (#10); no existing store, claim, or
live test is relaxed, removed, or skipped, and any such change would be a blocking defect
requiring a named equal-or-greater replacement; the new behaviors are proven by held-out
contract checks the implementing agent did not author against its own known inputs (#11).

## Decomposition Hint (for /fw-plan)
Splits along the dependency arrow (core -> orchestrator), one slice per layer so a slice
never inherits a red suite:

- **Layer core / store protocol + backends (spend aggregate):** satisfies #1, #2, #3, #4.
  Adds the cross-run aggregate read method to the `StoreProtocol` surface
  (`store_protocols.py`) and implements it on the in-memory, SQLite, and Postgres stores.
  Returns a typed result (the four token sums + summed cost). Pure-module invariant holds:
  this is a store concern, not `task`/`lifecycle`.
- **Layer orchestrator / claim enumeration:** satisfies #5, #6. Adds the enumeration
  method to the `ClaimStore` protocol (`_claims.py`) and both concrete claim stores
  (in-memory, SQLite). Reads existing `task_claims` columns — no schema change.
- **Layer orchestrator / live + status read path:** satisfies #7, #8, #9; depends on the
  worker_id already persisted (no prior layer needed). Carries `worker_id` onto the
  in-flight snapshot (`LiveRunRow` in `_workflow.py`) by selecting the existing
  `lifecycles.worker_id` in `collect_live_rows`, and emits it in `status --json`.
- **Cross-cutting verification-surface DoD:** #10, #11 apply to every slice.

Shared invariants multiple layers / specs assert against:
- **SI-9 — Schema migration:** this spec needs NO new column — the spend aggregate sums
  existing `attempts` token/cost columns (schema v12) and claim enumeration / worker_id
  surfacing read existing columns. RECONCILED with spec 00030: BOTH specs land ZERO schema
  changes and `CURRENT_SCHEMA_VERSION` stays 12; depended on by 00032 (whose Postgres parity
  reads assume the same v12 projection). If any future need forces a bump it is a single
  coordinated migration across `store_protocols` + `store_sqlite` + `store_postgres` +
  `persistence-schema.sql` + its Postgres mirror — neither 00030 nor 00031 authors one.
- **SI-4 — Spend aggregate method (canonical):**
  `summarize_spend(self, *, since: datetime | None = None, until: datetime | None = None) -> SpendSummary`
  on the store protocol, implemented identically on all three backends and (per 00032's port)
  on `PostgresStore`. `SpendSummary` is a frozen dataclass carrying the four token sums
  (`input_tokens`, `output_tokens`, `cache_creation_tokens`, `cache_read_tokens` — names
  mirroring the existing `attempts` columns) plus `total_cost_usd: float`. The window is
  half-open `[since, until)` measured against `attempts.last_activity_at`; every backend
  applies the identical predicate. Field names are fixed so external readers and downstream
  (00032) consume one vocabulary.
- **SI-10 — Claim enumeration (canonical):** `list_claims(self) -> list[TaskClaim]` on the
  orchestrator-layer `ClaimStore` protocol + both concrete claim stores, returning only
  currently-held claims (released/deleted rows absent), each carrying `task_id`, `worker_id`,
  `claimed_at`, `lease_expires_at`, `version`. Cross-task selection stays out of core.
- **SI-11 — worker_id absence semantics:** an unset `worker_id` is null/absent, never a
  sentinel string — both the live snapshot and `status --json` assert this (#9).

## Decisions Log (ADR-style: immutable, supersede — never edit)

### D-1: Spend aggregate is a store-protocol method, not a verb or a raw-SQL helper  (Status: Accepted)
- Context: Audit Q2 wants spend across tasks and over a time bound; the existing per-run
  sum (`_history.py:_attempt_rollups`) reaches into `store._connection` with `# noqa:
  SLF001`. The intended end-state ("answers fleet questions from data alone") requires a
  contract every backend and external reader shares.
- Decision: Add a single read method to the store protocol —
  `summarize_spend(self, *, since: datetime | None = None, until: datetime | None = None) -> SpendSummary`
  (canonical SI-4) — that returns the cross-run token/cost totals with an optional half-open
  `[since, until)` window; implement on all three backends. No new CLI verb in this spec (that
  is 00032's call).
- Rejected: (a) a CLI-only aggregate computed in orchestrator raw SQL — leaves the
  protocol/external-reader contract unfilled and re-buries the query under `_connection`;
  (b) extending `_attempt_rollups` to take a list of run ids — still single-run-shaped and
  orchestrator-private, not a store contract.
- Consequences: Every backend (incl. Postgres) must implement the method, widening the
  Postgres test surface; the in-memory store must compute the same sum without SQL.

### D-2: This spec introduces no schema migration  (Status: Accepted)
- Context: All inputs the new methods read already exist in schema v12 — the `attempts`
  token/cost columns, `lifecycles.worker_id`, and the `task_claims` columns. The brief
  flags a shared migration with 00030.
- Decision: Add no column and no `CURRENT_SCHEMA_VERSION` bump in this spec. If 00030
  requires a bump, the reconcile step coordinates a single migration; this spec declares
  it touches the same schema/store surface but does not author a migration.
- Rejected: pre-emptively bumping the schema "to be safe" — a no-op migration would force
  every existing database to be re-created (`StoreSchemaError`) for nothing.
- Consequences: No forward-migration code here; the time-window aggregate depends on
  `attempts.last_activity_at` being populated (it is null before the first completed
  iteration), so attempts with no rollup yet contribute zero/are excluded from a window —
  acceptable, since pre-rollup attempts have zero counters anyway.

### D-3: Claim enumeration returns only currently-held claims, in the orchestrator layer  (Status: Accepted)
- Context: Audit Q3 — `ClaimStore` has only `load_claim(task_id)`; the table holds one row
  per held claim and a row is deleted on release. Cross-task concepts live above core.
- Decision: Add the enumeration method `list_claims(self) -> list[TaskClaim]` (canonical
  SI-10) to the orchestrator's `ClaimStore` protocol and its concrete stores, returning the
  live `TaskClaim` rows (each with `worker_id`). Released claims (deleted rows) are absent (#6).
- Rejected: putting claim enumeration in flywheel-core — claims are an orchestration-layer
  concept (the core schema header says so explicitly); core must not learn about workers.
- Consequences: An expired-but-not-deleted lease still appears in the listing (it is a held
  row until stolen/released); the listing reports raw claim state, and staleness is the
  caller's interpretation (consistent with the existing `load_claim` contract).

### D-4: worker_id is surfaced as null when absent, never a sentinel  (Status: Accepted)
- Context: `lifecycles.worker_id` is nullable; `load_lifecycle` already coerces null to
  `""`. Fleet attribution is corrupted if an unattributed run is reported under a fake
  worker.
- Decision: The live snapshot and `status --json` report the worker as null / omitted key
  when the column is unset; a real worker id is reported verbatim.
- Rejected: defaulting to `"unknown"` or treating `""` as a present worker — both fabricate
  attribution and would pass a naive "field exists" check.
- Consequences: Consumers must handle a null worker explicitly (#9 pins this).

### D-5: Reconciliation (2026-06-17) — canonical signatures (SI-4 spend, SI-10 list_claims), no schema bump (SI-9), and the worker_id surfacing is owned here  (Status: Accepted)
- Context: 00031 shares `StoreProtocol` + `store_sqlite` + `store_postgres` +
  `persistence-schema.sql` with 00030, and is HARD-depended-on by 00032. Reconcile pinned the
  exact method shapes so all three specs agree and 00032 has fixed targets to implement on
  Postgres.
- Decision: (1) Spend aggregate is canonically `summarize_spend(*, since=None, until=None) -> SpendSummary`
  (SI-4), with the four token-sum fields named after the `attempts` columns plus
  `total_cost_usd`; half-open `[since, until)` against `attempts.last_activity_at`. (2) Claim
  enumeration is canonically `list_claims() -> list[TaskClaim]` on the orchestrator `ClaimStore`
  (SI-10). (3) NO schema change; `CURRENT_SCHEMA_VERSION` stays 12 (SI-9), matching 00030. (4)
  `worker_id` SURFACING on the live snapshot + `status --json` (criteria #7-#9) is owned here;
  00032 only PORTS the existing `collect_live_rows`/`_latest_lifecycle_row` reads to Postgres
  parity and inherits whatever worker_id field this spec added. If 00031 and 00032 co-phase,
  00032's live-rows parity test (00032 #2) asserts the worker_id field this spec introduces.
- Rejected: 00031 defining a `spend` CLI verb (out of scope; 00032's call); putting
  `list_claims` in core (claims are orchestration-layer — D-3); pre-emptive schema bump (D-2).
- Consequences: `/fw-plan` for 00032 treats SI-3/SI-4/SI-10 and the new `LiveRunRow.worker_id`
  field as fixed contracts to implement on Postgres; a change to any ripples to 00032's port.

## Open Questions (accepted gaps)
None. Every criterion lowers to a deterministic `command` grader (pytest assertions, JSON
parse, exit-code/whole-suite run). The Postgres arm of #4 degrades to a clean skip when
Docker is unavailable, matching the existing store-contract harness — it never silently
passes.

## Next Steps
Run `/fw-plan 00031-FEATURE-spend-and-worker-aggregates` to compile these criteria into
flywheel tasks and graders.

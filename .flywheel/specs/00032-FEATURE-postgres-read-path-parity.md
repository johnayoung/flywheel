# Feature: Postgres read-path parity (commands stop refusing the postgres backend)

## Outcome
A project configured with `[store] backend = "postgres"` runs the read-path verbs
(`status`, `history`, `live`/`show`, and the `orchestrate` control-store reads) against
a live Postgres database and gets correct answers, instead of the runtime refusal it gets
today. The orchestrator's cross-task read functions execute through the backend-agnostic
query surface (introduced in specs 00030 and 00031), not through SQLite-only private SQL
(`store._connection`), so the same read produces the same shape and values on Postgres as it
does on SQLite. The Postgres-only refusal in `open_sqlite_bound_store` is gone.

## Background
`init` prompts for, writes, and validates `[store] backend = "postgres"`, but every command
that builds a store routes through `open_sqlite_bound_store`, which constructs the store and
then raises `StoreConfigError` if it is a `PostgresStore`
(`_store_factory.py:142-168`). The refusal exists because the orchestrator's read functions
reach past the store protocol into raw SQLite SQL — `_latest_lifecycle_row`, `collect_live_rows`,
`_list_blocked_lifecycles` in `_workflow.py` (167, 888, 1303) and `_attempt_rollups`,
`_select_lifecycles`, `resolve_run_id`, `collect_run_detail` in `_history.py` (212, 272, 351,
378), all via `store._connection` and all typed `SqliteStore`. So a Postgres store would fail
later on a missing private attribute; the factory refuses it up front with an actionable message
instead. The multi-host adopter — the only reason to configure Postgres — gets a config that
fails at runtime.

The tacit trap a literal agent would fall into: deleting the `StoreConfigError` raise satisfies
"no refusal" while leaving the read functions calling `store._connection`, which Postgres does
not expose — turning a clean refusal into an opaque `AttributeError` further down. The real
end-state is not "the old error is gone"; it is "the read verbs return correct data against a
real Postgres store." This spec therefore grades read correctness against a live Postgres
database, not the absence of one specific exception. It HARD-depends on specs 00030 and 00031,
which supply the backend-agnostic query surface these reads must route through; this spec ports
the orchestrator read paths onto that surface and lifts the refusal.

## Scope
### In scope
- Lift the Postgres refusal: `open_sqlite_bound_store` (or its successor seam) accepts a
  Postgres-backed store and hands it to the read verbs instead of closing it and raising
  `StoreConfigError`.
- Port the orchestrator cross-task read functions (`status`/`history`/`live`/`show` and the
  `orchestrate` control-store reads, currently the `store._connection` sites in `_workflow.py`
  and `_history.py`) onto the backend-agnostic query surface from 00030/00031, so they execute
  on both SQLite and Postgres.
- Implement, on `PostgresStore`, the query-surface protocol methods that 00030/00031 define, so a
  Postgres store satisfies the same read contract SQLite does.
- Parity: a read that returns a value or set on SQLite returns the equivalent value or set on
  Postgres, for the same persisted state.

### Out of scope
- Adding new cross-task query capabilities (cost/spend aggregates Q2, per-worker activity Q3,
  persisted DAG Q4, telemetry filters Q5) — this spec ports the *existing* reads to parity; new
  queries are separate findings/specs.
- The shape and method names of the query-surface protocol itself — that contract is owned by
  00030/00031; this spec consumes it and implements it for Postgres.
- The `init`/config write path, DSN resolution, and the postgres fail-fast paths
  (no DSN, missing extra) — already shipped and unchanged.
- The write/append paths on either backend (lifecycle/attempt/event/grader writes) — already at
  parity; untouched here.
- Telemetry (JSONL sink) reads, redaction (Q6), and external-tool SQL contracts.

### Must not regress
- SQLite remains the default and every existing `status`/`history`/`live`/`show`/`orchestrate`
  read against a SQLite store returns exactly what it returns today.
- The postgres fail-fast behavior stays: a `postgres` backend with neither `FLYWHEEL_PG_DSN` nor
  `DATABASE_URL` set still exits non-zero with the factory's message naming both env vars; the
  missing-`postgres`-extra case still names the install command.
- `import flywheel_core` still works without the `postgres` extra installed; no module that
  `flywheel_core/__init__` imports gains a top-level `import psycopg`/`psycopg_pool`.
- The orchestrator/core hard line holds: cross-task selection logic stays in the orchestrator;
  only single-store query primitives (one connection, no scheduler) may live in core.
- The existing store contract suite still passes on `memory` and `sqlite`.

## Success Criteria
Each criterion is atomic, grades an observable end-state, and carries its grader type,
visibility, the exact check, and the gaming move it forecloses. `/fw-plan` lowers each one to a
command / transcript / rubric / manual grader.

1. While a `PostgresStore` holds terminal lifecycles (`DONE`/`FAILED`/`FAILED_VALIDATION`) with
   attempts and grader receipts, when the `history` listing read is computed against it, the read
   (a) enumerates every terminal run the store holds, and (b) returns the same `HistoryRow`/`HistoryRun`
   field values (run id, task id, status, attempt count, token total, cost total, turn total) that
   the identical persisted state returns from a `SqliteStore` — where the enumeration of terminal runs
   is served by the backend-agnostic SI-3 seam `list_lifecycles(statuses=TERMINAL_STATUSES)` (the
   `(DONE, FAILED, FAILED_VALIDATION)` set already in `_history.py`), not by a SQLite-only
   `store._connection` lifecycle SELECT. [command | held-out]
   verify: a held-out pytest parametrized over both backends that, through the **write protocol**
   (`save_task` then `append_domain_event`/`save_attempt`/`append_grader_result`), seeds two tasks
   with multi-attempt terminal lifecycles and grader results plus at least one non-terminal lifecycle,
   computes the history listing on each backend, and asserts (i) the set of returned run ids equals
   exactly the seeded terminal run ids on BOTH backends (the non-terminal run is absent from each), and
   (ii) the two results are field-for-field equal `HistoryRow`/`HistoryRun` rows. `uv run pytest -k
   "history and parity"` exits 0 on a host with the Postgres test container reachable; skips (not
   fails) when it is not. (The Postgres arm computing a non-empty, complete terminal set proves the
   enumeration crossed backends through `list_lifecycles`, since a left-behind `store._connection`
   SELECT raises `AttributeError` on Postgres rather than enumerating.)
   defends against: leaving the terminal-run enumeration as a SQLite-only `store._connection` SELECT
   while only deleting the `StoreConfigError` raise (it would `AttributeError` against Postgres, so the
   Postgres arm cannot produce the complete terminal set — failing (i)); a partial seam that lists from
   the protocol on SQLite but special-cases Postgres to an incomplete or empty enumeration (the
   exact-set assertion on BOTH arms catches a missing or extra run); hardcoding or returning empty
   results (the SQLite baseline is non-empty and computed from the same seed, so empty/constant output
   diverges); special-casing the test's known run id (the assertion compares two independently-computed
   reads, both from seeded-at-test-time data the agent cannot pre-bake).

2. While a `PostgresStore` holds an in-flight (`running`/`validating`/`awaiting_approval`)
   lifecycle, when the live-rows read is computed against it, it returns one row per in-flight run
   with the same per-run rollup fields (tokens, cost, turns, iterations, attempt/iteration
   breadcrumb) the identical persisted state returns from a `SqliteStore`. [command | held-out]
   verify: a held-out pytest parametrized over both backends seeds an in-flight lifecycle plus
   attempts through the write protocol, computes the live-rows read on each backend, and asserts
   equal rows. `uv run pytest -k "live_rows and parity"` exits 0 with the container reachable;
   skips when not.
   defends against: leaving the SQLite-only `store._connection` SELECT in place (raises on
   Postgres); returning rows for terminal runs too (the assertion pins exactly the in-flight set);
   omitting the rollup fields (field-for-field compare against the SQLite baseline catches a
   row that is present but empty).

3. While a `PostgresStore` holds lifecycles for a task across several runs, when the most-recent-
   lifecycle-for-task read and the run-id resolution read are computed against it, they return the
   same run id the identical persisted state returns from a `SqliteStore`. [command | held-out]
   verify: a held-out pytest parametrized over both backends seeds two runs of one task with
   distinct update times via the write protocol, then asserts the by-task-id latest-lifecycle read
   and `resolve_run_id` (task-id form) select the same run id on both backends.
   `uv run pytest -k "latest_lifecycle and parity"` exits 0 with the container reachable; skips when not.
   defends against: returning an arbitrary run instead of the most-recently-updated one (the
   seed gives the two runs a defined recency order the assertion pins); a no-op that returns the
   first row (ordering is asserted, not just non-None).

4. When the `postgres`-backed store is built for a read verb, the store-construction seam returns
   the `PostgresStore` to the caller rather than closing it and raising `StoreConfigError`.
   [command | held-out]
   verify: a held-out pytest with the Postgres test container reachable sets `FLYWHEEL_PG_DSN`, calls
   the verb store-construction entry point with a `postgres` policy, and asserts it returns a usable
   (un-closed) `PostgresStore` and raises no `StoreConfigError`. `uv run pytest -k "postgres and not refus"`
   exits 0 with the container reachable; skips when not.
   defends against: keeping the refusal but widening its message; returning a closed store (the
   assertion uses the returned store for a read, which fails if it was closed).

5. When `flywheel status` (or `fw status`) runs in a project whose `flywheel.toml` sets
   `[store] backend = "postgres"` with a reachable database, the process does not emit the
   "does not support the postgres store backend" refusal and exits 0. [command | held-out]
   verify: a held-out pytest with the Postgres test container reachable writes a `flywheel.toml`
   with `backend = "postgres"`, sets `FLYWHEEL_PG_DSN`, invokes the `status` verb end to end, and
   asserts exit code 0 and that stderr/stdout contains neither "does not support the postgres
   store backend" nor an unhandled `AttributeError`/`psycopg` traceback. `uv run pytest -k "status and postgres"`
   exits 0 with the container reachable; skips when not.
   defends against: swallowing the error and printing nothing useful (exit 0 plus absence of a
   traceback is asserted, so a crash that prints a stack still fails); replacing the refusal with a
   different hard error (the no-traceback assertion catches it).

6. If a `postgres` backend is configured with neither `FLYWHEEL_PG_DSN` nor `DATABASE_URL` set,
   then a read verb still exits non-zero with the factory message naming both environment
   variables. [command | held-out]
   verify: existing `uv run pytest packages/flywheel-orchestrator/tests/test_store_factory.py -k "without_env_vars"`
   continues to exit 0 (the fail-fast message and exit 2 are unchanged); does not require a database.
   defends against: collapsing the no-DSN path into the now-accepted-construction path, silently
   building nothing or falling back to SQLite (the test asserts the named-both-vars message and
   non-zero exit, which a silent fallback would not produce).

7. The orchestrator read functions that this feature ports remain typed and importable without the
   `postgres` extra installed, and `import flywheel_core` succeeds with the extra absent.
   [command | held-out]
   verify: `uv run python -c "import flywheel_core; import flywheel_orchestrator"` exits 0 in an
   environment where `psycopg`/`psycopg_pool` are not importable (the contract suite already proves
   the optional-extra boundary; this asserts no new top-level psycopg import was introduced on the
   read-path port). A grep-style held-out check confirms no module imported by
   `flywheel_core/__init__` gained a module-level `import psycopg` / `import psycopg_pool`.
   defends against: implementing the Postgres query methods by importing psycopg at module top of a
   core-init-reachable module (breaks the optional-extra invariant); the agent would otherwise reach
   for the simplest import placement.

### Verification-surface criteria (this feature changes the verification surface)
This feature deletes an existing assertion (`test_open_sqlite_bound_store_refuses_postgres` in
`packages/flywheel-orchestrator/tests/test_store_factory.py:228`, which asserts the refusal this spec
removes) and extends the shared store contract suite to a new backend behavior. The standing
Definition-of-Done below is inherited by every task touching that surface.

8. The full existing test suite still runs and passes after the change, on a host where the
   Postgres test container is reachable. [command | held-out]
   verify: `uv run pytest` exits 0 with no errors and no collection failures; Postgres-parametrized
   tests execute (not all skipped) because the container is reachable. (verification-surface)
   defends against: making new parity checks pass by weakening or deleting existing tests — a green
   run that silently dropped coverage; the full-suite gate catches removed/failing tests.

9. When the refusal assertion is removed, a replacement assertion of equal-or-greater strength
   covering the new accept-and-return behavior exists in the same test surface. [command | held-out]
   verify: `uv run pytest packages/flywheel-orchestrator/tests/test_store_factory.py` exits 0 and the
   file contains a test asserting `open_sqlite_bound_store` (or its successor) returns a usable
   `PostgresStore` for a `postgres` policy with the container reachable; a held-out check confirms
   `test_open_sqlite_bound_store_refuses_postgres` no longer asserts that a `StoreConfigError` is
   raised for a reachable Postgres store. (verification-surface)
   defends against: deleting the refusal test with no replacement (a removed assertion with no
   equal-or-greater replacement is a blocking defect); leaving a now-false test that asserts the old
   refusal still holds (the suite would fail, or the agent would have weakened it — both caught).

10. New Postgres read parity is proven by a check the implementing agent did not author against its
    own known inputs: the parity assertions compare the Postgres read against the SQLite read of the
    same write-protocol-seeded state, so neither side is a literal the agent can pre-compute.
    [command | held-out]
    verify: criteria 1-3 are authored as held-out, backend-parametrized contract tests whose
    expected values are computed at run time from a shared seed (not embedded constants); a held-out
    check confirms the parity tests contain no hardcoded run-id/token/cost expected literals on the
    Postgres arm. (verification-surface)
    defends against: writing a "parity" test that asserts the Postgres read equals a constant the
    agent baked in from a known seed (the cross-backend compare, with both sides computed, forecloses
    the baked literal).

Verification surface: changed. The existing suite (`uv run pytest`) still passes; the removed
refusal assertion (`test_open_sqlite_bound_store_refuses_postgres`) is named and replaced with an
equal-or-greater accept-and-return assertion (#9); new Postgres read behavior is proven by held-out,
backend-parametrized parity checks computed from a shared seed, not by literals the agent authored
(#1-3, #10).

## Decomposition Hint (for /fw-plan)
The architectural layers this splits along, so /fw-plan can size one task per slice and chain them
with prerequisites. State layers and the criteria each must satisfy; do not prescribe implementation.

- Layer core/store-postgres: implement, on `PostgresStore`, the backend-agnostic query-surface
  protocol methods defined by 00030/00031 (the read primitives the orchestrator routes through).
  Satisfies #7 (no top-level psycopg leak) and is the precondition for #1-3. Depends on 00030/00031.
- Layer orchestrator/read-path port: rewrite the `store._connection` read functions in
  `_history.py` and `_workflow.py` (`_attempt_rollups`, `_select_lifecycles`, `resolve_run_id`,
  `collect_run_detail`, `_latest_lifecycle_row`, `collect_live_rows`, `_list_blocked_lifecycles`)
  to call the protocol surface instead of raw SQLite SQL, and retype them off `SqliteStore`-only.
  Satisfies #1, #2, #3, #10; depends on the core/store-postgres layer.
- Layer orchestrator/factory seam: lift the Postgres refusal in `open_sqlite_bound_store` so a
  Postgres store is returned to read verbs; preserve the fail-fast (no-DSN, missing-extra) paths.
  Satisfies #4, #6, #9; depends on the read-path port (refusing only after the reads are portable).
- Layer product/end-to-end: prove the verb runs end to end against a Postgres-configured project.
  Satisfies #5, #8; depends on every prior layer.

Shared invariants multiple layers assert against (name them so dependent tasks update together):
- **SI-3 / SI-4 / SI-10 — the query-surface protocol method set and signatures (owned by
  00030/00031, consumed here):** this spec implements them VERBATIM on `PostgresStore` and routes
  the orchestrator reads through them. The canonical signatures are fixed by the reconcile:
  `list_lifecycles(self, *, statuses: Collection[Status] | None = None, task_id: str | None = None) -> list[Lifecycle]`
  (SI-3, 00030); `summarize_spend(self, *, since=None, until=None) -> SpendSummary` (SI-4, 00031);
  `list_claims(self) -> list[TaskClaim]` (SI-10, 00031). A change to any ripples to both this
  spec's PostgresStore implementation and the read-path port. This spec pins BEHAVIOR (parity),
  not new names.
- The read functions' return shapes (`HistoryRow`, `HistoryRun`, `LiveRunRow` — including the
  `worker_id` field 00031 adds — and the `_latest_lifecycle_row` tuple) must be field-for-field
  identical across backends — the parity tests assert this; changing a field changes every parity
  assertion.
- **SI-9 — `CURRENT_SCHEMA_VERSION` stays 12** on both the SQLite and Postgres schema mirrors:
  00030 and 00031 land ZERO schema changes, so the reads here assume the same v12 projection on
  both backends. This spec authors no migration either.

## Decisions Log (ADR-style: immutable, supersede — never edit)

### D-1: Grade read correctness against a live Postgres store, not absence of the old error  (Status: Accepted)
- Context: The cheapest fake for "stop refusing Postgres" is to delete the `StoreConfigError` raise
  while the read functions still call `store._connection`, which Postgres does not expose — turning
  the refusal into an `AttributeError`. A criterion that only checks "no `StoreConfigError`" passes
  while the verb crashes.
- Decision: The authoritative criteria (#1-5) assert that reads return correct data against a real
  Postgres-backed store, parametrized over both backends and seeded through the write protocol.
- Rejected: asserting only that the refusal message is gone (gameable by a different crash);
  asserting only "verb exits 0" without a no-traceback/data-correctness check (a swallowed error
  exits 0). Consequences: parity criteria need a reachable Postgres test container, so they skip
  (not fail) where Docker/testcontainers/extra are unavailable — the per-PR signal on a CI host
  with the container is what lands the change; local runs without Docker stay green but unproven.

### D-2: Parity is asserted as a cross-backend equality of two computed reads  (Status: Accepted)
- Context: A "parity" test that asserts the Postgres read equals a constant is gameable — the agent
  bakes the constant from a known seed. Per-room coverage requires the expected side be independently
  computed.
- Decision: Reuse the existing backend-parametrized contract-suite pattern
  (`test_store_contract.py:63`, already parametrized memory/sqlite/postgres with graceful skip); seed
  identical state through the write protocol on each backend and assert the read outputs are
  field-for-field equal, with no embedded expected literals on the Postgres arm (#10).
- Rejected: a Postgres-only test with hand-written expected values (bakeable); comparing against a
  golden file (still a literal the agent authors). Consequences: the parity tests are coupled to the
  SQLite read being correct (the baseline) — acceptable, because SQLite parity is the
  must-not-regress floor and is independently covered by the existing suite.

### D-3: Port reads onto the 00030/00031 query surface; keep cross-task logic in the orchestrator  (Status: Accepted)
- Context: The hard line forbids cross-task selection in core; but the per-store read primitives
  (one connection, no scheduler) are exactly what core's query surface should expose, and 00030/00031
  define that surface.
- Decision: This spec consumes the 00030/00031 query-surface protocol (does not define it),
  implements those methods on `PostgresStore`, and rewrites the orchestrator's `store._connection`
  reads to call the protocol. Cross-task grouping/selection stays in the orchestrator functions; only
  the single-store SELECT primitives move behind the protocol.
- Rejected: defining the query surface here (would collide with 00030/00031 — declared as a HARD
  dependency instead); pushing the orchestrator grouping logic into core (violates the hard line).
  Consequences: this spec cannot complete before 00030 and 00031 land — the reconciler must add
  prerequisite edges (00032 requires 00030 and 00031). If those specs name the protocol methods
  differently than assumed, the read-path port adapts to whatever they shipped; this spec pins the
  *behavior* (parity), not the method names.

### D-4: Preserve the fail-fast paths; only the reachable-Postgres refusal is lifted  (Status: Accepted)
- Context: `open_sqlite_bound_store` today does double duty — it surfaces the no-DSN and
  missing-extra fail-fast errors *and* refuses a successfully constructed Postgres store. Only the
  second behavior is wrong.
- Decision: Lift only the "constructed Postgres store -> refuse" branch (#4); the no-DSN and
  missing-extra `StoreConfigError` paths stay verbatim (#6), still exit non-zero, still name both
  env vars / the install command.
- Rejected: removing all Postgres special-casing from the seam (would drop the actionable fail-fast
  messages adopters rely on). Consequences: the seam's name (`open_sqlite_bound_store`) becomes a
  misnomer; renaming is a path-level concern left to /fw-plan and the implementing agent, not graded
  here.

### D-5: Reconciliation (2026-06-17) — HARD prerequisite edges recorded; canonical surfaces (SI-3/SI-4/SI-10/SI-9) consumed verbatim  (Status: Accepted)
- Context: D-3 declared this spec HARD-depends on 00030/00031 and asked the reconciler to add the
  prerequisite edges. The store surface names were unpinned across the three specs until now.
- Decision: the cross-spec DAG records **`00032 -> 00030` AND `00032 -> 00031`** (both HARD — this
  spec cannot start until both query-surface specs have landed). The surfaces this spec implements
  on `PostgresStore` and ports the orchestrator reads onto are the canonical SI-3 (`list_lifecycles`),
  SI-4 (`summarize_spend` + `SpendSummary`), and SI-10 (`list_claims`) signatures fixed by 00030/00031;
  schema is v12 with no migration (SI-9). This spec adds NO new query capability and NO new method
  name — it implements the fixed contract on the missing backend and lifts the
  `open_sqlite_bound_store` refusal. The `LiveRunRow.worker_id` field (00031) is inherited; 00032's
  live-rows parity test (#2) asserts it crosses to Postgres identically.
- Rejected: defining or renaming any query-surface method here (owned by 00030/00031 — D-3);
  treating the dependency as soft/optional (the read-path port literally calls methods 00030/00031
  introduce, so it cannot compile against an un-landed surface).
- Consequences: 00032 is the terminal node of the store-layer chain; if 00030/00031 ship the
  canonical signatures, 00032's port adapts to nothing new. The reconciler has recorded the two
  HARD edges in the cross-spec DAG.

### D-6: Sharpen (2026-06-17) — the history read enumerates terminal runs through the SI-3 seam, not SQLite-only `store._connection`  (Status: Accepted)
- Context: `/fw-verify` flagged c1 UNDER-SPECIFIED: c1 asserted `HistoryRow`/`HistoryRun` field
  parity but left the *enumeration* of terminal runs unpinned. `collect_history_rows` must list every
  terminal run from a store alone, yet today it does so via `_select_lifecycles`, a SQLite-only
  `store._connection` lifecycle SELECT (`_history.py:272`). With no backend-agnostic enumeration seam
  declared, a literal agent could satisfy field-for-field parity prose while leaving the SQLite-only
  enumeration in place — which `AttributeError`s on Postgres, so a correct cross-backend reference was
  not even writable. A parity assertion over an un-enumerable read is hollow.
- Decision: c1 now DECLARES the enumeration seam: the history read enumerates terminal runs through
  the canonical SI-3 surface `list_lifecycles(statuses=TERMINAL_STATUSES)` — reusing the existing
  `(DONE, FAILED, FAILED_VALIDATION)` set already named `TERMINAL_STATUSES` in `_history.py` — rather
  than any `store._connection` lifecycle SELECT. c1's verify adds a blind-authorable parity check that
  the set of returned run ids equals exactly the seeded terminal run ids on BOTH backends (a seeded
  non-terminal run is absent on each), making a correct cross-backend reference writable. This pins the
  enumeration behavior; it names no new method (SI-3 is owned by 00030 — D-3, D-5) and changes no other
  criterion. The status set this consumes is exactly the orchestrator's `TERMINAL_STATUSES` and the SI-3
  `statuses: Collection[Status]` filter that subsumes it (00030 D-1) — no new vocabulary.
- Rejected: leaving c1 as field-parity-only (the enumeration substrate stays unpinned, so the
  SQLite-only `store._connection` SELECT survives and the Postgres arm has no enumerable read to assert
  against); declaring a 00032-local enumeration method (would collide with SI-3, which 00030 owns —
  D-3/D-5); asserting only that the read returns the same fields without an exact-set check (a backend
  that silently drops or duplicates a terminal run would still pass a field-shape compare).
- Consequences: c1's port now explicitly requires `_select_lifecycles`'s SQLite-only lifecycle SELECT
  to be rewritten onto `list_lifecycles` — the same migration 00030 #7 already mandates for the
  orchestrator's lifecycle reads. This sharpen tightens the contract on the seam 00030/00031 supply; it
  adds no schema change (SI-9, `CURRENT_SCHEMA_VERSION` stays 12) and no new dependency edge. The HARD
  prerequisite `00032 -> 00030` (D-5) is unchanged and remains the source of the `list_lifecycles` seam
  this criterion now names.

## Open Questions (accepted gaps)
None. Every criterion lowers to a `command` grader. The parity and end-to-end criteria (#1-5,
#8-10) require a reachable Postgres test container to *execute*; where it is unreachable they skip
rather than fail (matching the existing contract suite's behavior), so the authoritative signal is
"green on a host with the container," which the CI/landing host provides.

## Next Steps
Run `/fw-plan 00032-FEATURE-postgres-read-path-parity` to compile these criteria into flywheel tasks
and graders. The reconciler must record that 00032 requires 00030 and 00031 (the backend-agnostic
query surface this read-path port routes through).

# Feature: Distributed store unification

## Outcome
With `[store] backend = "postgres"`, the entire data plane — run records, claims/leases, and every orchestrator ledger — lives in the shared postgres database, and two independent flywheel instances of the same repo provably coordinate through it: no double-claims, cross-instance-visible status/history, and no state silently landing in sqlite.

## Background
Today `backend = "postgres"` routes only control-plane reads; the run datapath and the claim store are sqlite-pinned, so a postgres deployment is split-brain (postgres empty, `status`/`history` blind) and multi-instance coordination is impossible. This was found empirically during the infrared adoption smoke (2026-07-07): a full run landed with postgres completely empty. A working `PostgresClaimStore` already exists but is constructed nowhere in production, and concurrent schema bootstrap deadlocks at 3-wide worker startup. The tacit bar: "postgres mode" means postgres is the *only* authority — a mode that mostly uses postgres but leaks any state to local sqlite is a rejected result even if individual reads look correct.

## Scope
### In scope
- Run datapath persistence (task versions, lifecycles, attempts, run events, grader results) honors the configured store backend.
- Claim/lease and all orchestrator ledgers (claims, work items and dependencies, source syncs, orchestrator events, stop events, graph snapshots) honor the configured store backend.
- All operator read surfaces (`status`, `history`, TUI, autopilot backoff reads) read the same backend the writers use.
- Concurrent schema bootstrap from multiple instances is safe.
- Loud, immediate failure when postgres is configured but unusable.
### Out of scope
- Cross-repo shared work pools or repo-identity columns. Multi-repo isolation is one postgres schema per repo via the existing `[store] schema` knob.
- Migration of existing sqlite history into postgres. Flips start fresh; old history stays readable under `backend = "sqlite"`.
- Retry/backoff availability machinery for postgres outages.
- Changes to claim/lease semantics, scheduling, or the DAG.
### Must not regress
- `backend = "sqlite"` (and unset, which defaults to sqlite) behaves exactly as today; the existing suite passes unmodified.
- The `execution.mode = "distributed"` policy guard still rejects non-postgres backends.
- Core purity: `flywheel_core.task` and `flywheel_core.lifecycle` stay free of I/O imports (purity tests unchanged).
- The agent SDK remains an optional extra; no new hard dependency edges.

## Success Criteria
Each criterion is atomic, grades an observable end-state, and carries its grader
type, visibility, the exact check, and the gaming move it forecloses.

1. When a task runs to completion under `backend = "postgres"`, the complete run record — task version, lifecycle, every attempt, every grader result, and the run's events — shall be present in the configured postgres database. [command | held-out]
   verify: end-to-end run against the postgres testcontainer, then SQL asserts >=1 row per table (`task_versions`, `lifecycles`, `attempts`, `grader_results`, `events`) keyed to that run's task id; pytest exit 0 with the selected tests reported as passed, 0 skipped.
   defends against: persisting only a terminal summary row; environment sabotage that makes the postgres-marked tests skip and count as green.

2. When a task runs under `backend = "postgres"`, no run or orchestrator state shall be written to any sqlite store. [command | held-out]
   verify: hash the repo-derived sqlite db path (or assert absence) before and after the criterion-1 run; byte-identical or still absent.
   defends against: dual-writing to sqlite and postgres — the split-brain survives while every postgres-side check passes.

3. When two flywheel instances share one postgres database and concurrently contend for the same set of ready tasks, each task shall be claimed by exactly one instance and each instance shall obtain at least one claim. [command | held-out]
   verify: concurrency test against the postgres testcontainer with two genuinely concurrent claimants racing over a shared task set; assert per-task claim count == 1 for all tasks and each claimant's claim count >= 1.
   defends against: a "coordination" test that serializes the instances so contention never occurs, or where one idle instance vacuously never conflicts.

4. When at least 8 store instances (core and claim stores) bootstrap concurrently against a fresh postgres schema, all opens shall succeed and the database's server-side deadlock counter shall not increment. [command | held-out]
   verify: record `SELECT deadlocks FROM pg_stat_database WHERE datname = current_database()`, run >=8 concurrent first-opens of both store kinds, assert zero raised exceptions and deadlock-counter delta == 0.
   defends against: catch-and-retry masking — the server counter increments even when the client swallows `DeadlockDetected` and retries.

5. If `backend = "postgres"` is configured and no DSN resolves or the server is unreachable at startup, then the invoked process shall terminate with a non-zero exit and an error naming the store misconfiguration, without creating or modifying any sqlite store. [command | visible]
   verify: invoke a worker/run entry point with `backend = "postgres"` and an unresolvable DSN; assert exit != 0, stderr matches the backend/DSN error, and the repo sqlite db path was not created or modified.
   defends against: warn-and-continue silent fallback to sqlite — the exact failure mode this spec exists to kill.

6. When `flywheel status` or `flywheel history` runs in a working copy whose local sqlite is empty while `backend = "postgres"` points at a database populated by a different instance, the output shall report that other instance's runs and its orchestrator state (claims/stop events included). [command | held-out]
   verify: populate postgres via one instance (criterion-1 run plus a recorded stop event), then execute `status`/`history` from a separate temp working copy with the same store config and an empty local sqlite; assert the output contains the originating run/task identifiers and the stop-event-derived state.
   defends against: fixing only the run-read path while claims/stop-events still read local sqlite — the current partial shape, which passes any single-instance status check.

7. While `[store]` is unset or `backend = "sqlite"`, all persistence and operator reads shall behave exactly as before this change. [command | visible]
   verify: `scripts/check.sh` green on the full existing suite, plus a policy assertion that an absent `[store]` section resolves to the sqlite backend.
   defends against: making postgres the implicit default, or shipping a postgres path whose green tests mask a broken sqlite default.

Verification surface: CHANGED — this feature touches the persistence layer that records grader results and lifecycle state, and its checks join the suite. Definition-of-Done inherited by every task in this phase:
- The existing suite still runs and passes; no existing assertion is deleted, weakened, or skipped without being named and replaced by an equal-or-stronger check (a removal with no named replacement is a blocking defect). [command | held-out] (verification-surface)
- Criteria 1-4 and 6 are proven by held-out checks authored out-of-band (the /fw-verify oracle), not by tests the implementing agent wrote against its own outputs; postgres-marked oracle tests must be reported as executed (0 skipped) to count as passing. [command | held-out] (verification-surface)

## Decomposition Hint (for /fw-plan)
- Layer core store seam: the single-task run path accepts an injected store while the sqlite default stays byte-compatible — satisfies #1, #2 (write side), #7.
- Layer orchestrator store routing: claim-store construction routes on the policy backend at every production site (run loop, status reads, autopilot backoff), same for the datapath store handed to runs — satisfies #2 (orchestrator side), #3, #6; depends on the core seam.
- Layer bootstrap safety: both postgres stores serialize schema bootstrap — satisfies #4; independent of the other layers.
- Layer failure semantics: startup preflight for a configured-but-unusable postgres — satisfies #5; depends on store routing.
Shared invariants multiple layers assert against: the policy fields `store_backend`/`store_schema` as the single routing input; one factory as the only production construction point for stores; DSN resolution order (`FLYWHEEL_PG_DSN` then `DATABASE_URL`); the postgres testcontainer fixtures in the root conftest as the shared test substrate.

## Decisions Log

### D-1: Distribution boundary is one repo's fleet  (Status: Accepted)
- Context: the operator's goal is tasks flowing from many sources with any deployed instance picking them up; the boundary for this phase had to be pinned. | Decision: this phase makes one repo's fleet coordinate over one shared postgres; cross-repo isolation is one postgres schema per repo via the existing `[store] schema` knob.
- Rejected: multi-repo shared schema with repo-identity columns (touches every table key and query; destabilizes the shipped sqlite path); data-plane-only without a coordination proof (defers exactly the criterion that tests the vision). | Consequences: a future cross-repo work pool needs its own spec; schema-per-repo means no cross-repo status rollup yet. Recommended defaults accepted 2026-07-08 (operator AFK at interview).

### D-2: Postgres mode fails loud; sqlite fallback is forbidden  (Status: Accepted)
- Context: the infrared smoke showed silent sqlite writes under a postgres config — split-brain with blind status. | Decision: configured-but-unusable postgres is a hard startup error; mid-run failures surface through existing infra-failure handling; no code path may write run/orchestrator state to sqlite while postgres is configured.
- Rejected: fallback-with-warning (recreates the bug); retry-until-reachable (availability machinery layerable later without changing this contract). | Consequences: a postgres outage stops the fleet rather than degrading it; operators needing offline work flip to `backend = "sqlite"` explicitly.

### D-3: No sqlite-to-postgres migration  (Status: Accepted)
- Context: infrared has staged history in sqlite; the flip needed a data story. | Decision: postgres starts fresh; old history stays readable under `backend = "sqlite"`.
- Rejected: one-shot migration verb (a whole tested surface — idempotency, partial-failure — for historical telemetry); dual-read (permanent read-path complexity for a one-time event). | Consequences: cross-backend history is discontinuous at the flip; authoritative state (work items, claims) rebuilds from work sources.

## Open Questions
None — every criterion lowers to a command grader.

## Next Steps
Run `/fw-plan 00075-FEATURE-distributed-store-unification` to compile these criteria into flywheel tasks and graders.

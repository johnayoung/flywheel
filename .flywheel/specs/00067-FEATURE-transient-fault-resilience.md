# Feature: Transient-fault resilience

## Outcome
A transient fault encountered while driving a task — an API 429/overload response, a SQLite `database is locked` past `busy_timeout`, a dropped Postgres connection, or a Postgres pool-acquisition timeout — is absorbed by a bounded backoff-and-retry that does NOT consume the task's normal validation retry budget, so two consecutive API or store hiccups no longer fail a task or burn the autopilot cycle breaker. A genuinely permanent fault — a store/orchestrator schema-version mismatch — surfaces exactly once as a clearly-labelled permanent stop, distinct from the transient class, instead of N identical breaker strikes that kill the daemon confusingly.

## Background
Today the loop conflates recoverable infrastructure faults with logic failures. A 429 is recorded as `api_error_status` (`invoker.py:248`) but never acted on, so the iteration produces a `MissingEnvelope` and the harness spends the `AGENT_ERROR`/`max_retries` budget (default 1) on it, immediately and with no backoff; rate-limit events are telemetry only (`harness.py:3292`). Store errors are equally unguarded: SQLite past `busy_timeout=5000` (`store_sqlite.py:175`) and a severed Postgres connection both raise straight through, and on autopilot they ride the worker's flat 5-strike / 10s breaker (`worker.py:2117`) with no inner backoff. The Postgres pool is built with only min/max size (`store_postgres.py:201`), so exhaustion raises an uncaught `PoolTimeout`. Conversely a schema mismatch (`StoreSchemaError`/`OrchestratorSchemaError`) is deterministic and permanent, yet because the store is reopened every cycle (`_orchestrate.py:816`) it raises identically on cycles 1..5 and the daemon dies after burning the whole breaker on a fault that backoff can never fix.

The tacit requirement the interview surfaced: the value is not "retry more" — it is a *typed* split. Transient faults must be retried with a bounded budget that is accounted for SEPARATELY from validation retries (so an infra hiccup can never be mistaken for "the agent failed to satisfy the graders"), and permanent faults must short-circuit the breaker so they are reported once, plainly, rather than amortized into generic cycle noise. The classification itself must be observable so a grader — and an operator — can tell which class a given failure was assigned, because the cheapest fake here is a blanket retry that "passes" by silently retrying schema mismatches too.

Note: optimistic-concurrency version conflicts are already loop-retried by the harness; this feature must NOT add a competing inner retry around them (that would double-count the retry budget). The double-claim correctness hole is already closed by the post-claim terminal-state recheck (`_orchestrate.py:1101-1126`); the remaining defect is only that its regression test contends under load.

## Scope
### In scope
- A transient-fault classifier that maps a fault to exactly one of two classes — TRANSIENT (API 429/overload/rate-limit, SQLite locked-past-timeout, dropped Postgres connection, Postgres pool-acquisition timeout) or PERMANENT (store/orchestrator schema-version mismatch) — and exposes that classification observably.
- Bounded backoff-and-retry for the TRANSIENT class at the point each fault is raised, with a retry budget tracked separately from the harness validation `max_retries` budget.
- A backoff that grows between attempts and is bounded by a cap, so a returned-constant or unbounded-sleep implementation is detectable.
- A bounded Postgres pool-acquisition wait that turns exhaustion into a classified-TRANSIENT, retryable failure rather than an uncaught raise.
- A single, distinctly-labelled permanent-stop for a PERMANENT fault that short-circuits the worker cycle breaker instead of accumulating identical strikes.
- Deterministic exactly-once assertion in the two-worker double-claim test under contention, with the existing assertion unweakened.

### Out of scope
- Per-task wall-clock / turn deadlines and their default-on enablement (that is the prior Deadlines phase; this phase consumes it but does not build it).
- The escalate-once-then-human-queue handoff and the respawn-under-budget supervisor (later phases).
- Any new retry around optimistic-concurrency version conflicts (already loop-retried; explicitly forbidden here).
- Changing the agent's validation/`max_retries` semantics for genuine grader failures.

### Must not regress
- A genuine agent/grader failure (a real `MissingEnvelope` from a non-API-error iteration, a failing command grader) still consumes the validation retry budget and reaches `AGENT_ERROR`/`FAILED_VALIDATION` exactly as before — transient handling must not swallow real failures.
- `flywheel_core.task` and `flywheel_core.lifecycle` stay pure (no `json`/`pathlib`/`io`/`open`).
- The agent SDK stays an optional extra: `import flywheel_core` works without `claude-agent-sdk`; rate-limit/api-error handling touches the SDK only through the existing `_sdk` boundary.
- The full gate (`scripts/check.sh`: ruff -> pyright -> pytest) stays green.

## Success Criteria
Each criterion is atomic, grades an observable end-state, and carries its grader
type, visibility, the exact check, and the gaming move it forecloses.

1. When an iteration's API result reports a 429 / overload / rate-limit status, the task driver shall retry the iteration after a backoff WITHOUT decrementing the validation `max_retries` budget, such that a task whose first two iterations return a rate-limit status and whose third returns a valid passing envelope reaches DONE. [command | held-out]
   verify: `uv run pytest packages/flywheel-core/tests/test_harness.py -k rate_limit_retry` — a test injecting an invoke sequence [rate-limit, rate-limit, valid-pass] with `max_retries=0` asserts terminal status DONE (proving the rate-limit attempts did not draw on the validation budget). The held-out assertion is keyed on the SEPARATE transient-attempt count, not on wall-time.
   defends against: satisfying "it retries" by simply raising the default `max_retries` so a 429 is absorbed by the validation budget — which would still fail on the second hiccup and would mis-account an infra fault as an agent failure.

2. When the same iteration returns a rate-limit status more times than the configured transient-retry budget allows, the task driver shall stop retrying that iteration and surface a TRANSIENT-classified failure rather than looping unbounded. [command | held-out]
   verify: `uv run pytest packages/flywheel-core/tests/test_harness.py -k rate_limit_exhausted` — an invoke sequence of N+1 rate-limit results (N = budget) terminates in bounded time with a failure whose recorded classification is TRANSIENT and whose attempt count equals N+1, not an infinite loop.
   defends against: an unbounded `while rate_limited: retry` that never terminates, or a budget read from a value the agent can set to infinity.

3. When the transient driver backs off between two consecutive retries of the same fault, the second wait shall be no shorter than the first and every wait shall be no greater than the configured cap. [command | held-out]
   verify: `uv run pytest packages/flywheel-core/tests/test_invoker.py -k backoff_bounded` (or the module that owns the backoff helper) — a test captures the sequence of sleep durations via an injected sleep/clock and asserts monotonic-non-decreasing AND each <= cap. A returned constant delay fails the monotonic-growth half; an unbounded delay fails the cap half.
   defends against: a constant `sleep(0)` (passes a naive "it retried" check but provides no backoff) or an unbounded exponential with no cap (DoS / hung daemon).

4. When a store operation raises a SQLite `database is locked` error past the busy-timeout, the store shall retry the operation with bounded backoff and succeed once the lock clears, returning the operation's normal result without surfacing the error to the caller. [command | held-out]
   verify: `uv run pytest packages/flywheel-core/tests/test_store_sqlite.py -k locked_retry` — a test holds a write lock on the same DB file for a span shorter than the total transient budget, issues a competing store call, and asserts the call returns its expected value (no `OperationalError` propagated). The held-out check asserts the OPERATION'S result, not that a retry function was called.
   defends against: catching the lock and returning a fabricated/empty success value instead of actually completing the operation once the lock clears; or widening `busy_timeout` only (which does not cover dropped connections and is not retry-with-backoff).

5. When a transient store error exceeds the store-retry budget, the store shall raise a failure that the transient classifier labels TRANSIENT (not PERMANENT and not a generic uncaught error). [command | held-out]
   verify: `uv run pytest packages/flywheel-core/tests/test_store_sqlite.py -k locked_classified` — a lock held longer than the total budget causes the call to raise, and the raised/recorded fault classifies as TRANSIENT. A held-out assertion on the classification enum value.
   defends against: blanket-classifying every store error as TRANSIENT (which would also mislabel a schema mismatch and retry it forever) — this criterion pins the locked case to TRANSIENT while criterion 7 pins schema to PERMANENT, and criterion 9 grades them together so the two cannot be collapsed.

6. When Postgres pool acquisition cannot obtain a connection within the configured bound, the store shall surface a TRANSIENT-classified, retryable failure within that bounded wait rather than blocking unboundedly or raising an unclassified `PoolTimeout`. [command | held-out]
   verify: `uv run pytest packages/flywheel-core/tests/test_store_postgres.py -k pool_acquire_bounded` — against the Postgres test container, a pool sized to exhaustion with all connections held has a competing acquire return/raise a TRANSIENT-classified failure within the configured timeout (asserted with a wall-time upper bound generous to CI). Skips cleanly when no Postgres container is available, consistent with the existing `test_store_postgres.py` gating.
   defends against: raising the bound so high the test "passes" by never timing out (assert the failure DOES occur within a finite bound), or leaving the raise unclassified so the breaker still treats it as fatal.

7. When the store/orchestrator schema version does not match the expected version, the driver shall classify the fault PERMANENT and produce exactly one distinctly-labelled permanent-stop, not one strike per cycle. [command | held-out]
   verify: `uv run pytest packages/flywheel-orchestrator/tests/test_orchestrator.py -k schema_mismatch_permanent_stop` — a store whose `schema_version` row is wrong causes the worker loop to stop after a SINGLE cycle with a permanent-stop signal/return distinct from the transient-cycle-failure path, and the recorded classification is PERMANENT. Assert the cycle/strike count is 1, not `MAX_CONSECUTIVE_CYCLE_FAILURES`.
   defends against: counting a schema mismatch as a transient and retrying it (it can never succeed), or letting it ride all 5 breaker strikes — both burn the daemon; the count-is-1 assertion forecloses both.

8. If a transient fault is retried, then the worker's consecutive-cycle-failure breaker shall NOT be incremented for that fault when the retry ultimately succeeds. [command | visible]
   verify: `uv run pytest packages/flywheel-worktree/tests/test_worker.py -k transient_not_breaker` — a cycle that hits a transient store fault which then clears completes with the worker's `consecutive_failures` counter at 0, proving the transient retry absorbed it below the breaker rather than spending a strike.
   defends against: "handling" the fault by catching it at the breaker level (which still counts a strike and still backs off the whole cycle) instead of retrying at the fault site beneath the breaker.

9. While both a TRANSIENT and a PERMANENT fault are reachable through the same driver, the classifier shall assign the schema-mismatch case PERMANENT and the rate-limit / locked / dropped-connection / pool-timeout cases TRANSIENT in one composed exercise. [command | held-out] (composition holdout)
   verify: `uv run pytest packages/flywheel-core/tests/test_invoker.py -k transient_classification_matrix` (the module owning the classifier) — one parametrized test feeds every in-scope fault to the classifier and asserts the full TRANSIENT/PERMANENT mapping in a single run, so a per-case handler that passes its own test but mis-buckets another case fails here. Adds no requirement beyond criteria 1-7; it only composes their classifications.
   defends against: per-fault handlers that each satisfy their own narrow test while the union mapping is inconsistent (e.g. schema accidentally bucketed TRANSIENT, or a dropped connection bucketed PERMANENT).

10. While two workers contend for the same four ready tasks under an overlapping-claim delay, each task shall be run exactly once and the test shall pass deterministically across repeated runs without weakening the exactly-once assertion. [command | visible]
    verify: `uv run pytest packages/flywheel-orchestrator/tests/test_orchestrator.py -k two_workers_run_each_task_exactly_once` passes, and the assertions `ran_a.isdisjoint(ran_b)`, `ran_a | ran_b == set(task_ids)`, and `len(all_runs) == len(task_ids)` remain present and unchanged. A deterministic-interleave variant (modelled on `test_fresh_selection_rechecks_terminal_state_under_claim`) removes the load-dependent flake.
    defends against: deflaking by relaxing the assertion (e.g. allowing a task to appear in both sets, or asserting `>=` instead of `==`) — the criterion requires the exactly-once assertion to stay byte-for-byte intact while the determinism comes from controlling the interleave, not from loosening the check.

Verification surface: changed. This feature adds tests and a transient-retry/classification path that several existing suites exercise; the standing Definition-of-Done below is inherited by every task that touches it.

- (verification-surface) The existing verification suite (`scripts/check.sh`: ruff -> pyright -> pytest across all packages) still runs and still passes after each task; no existing assertion is deleted or weakened to make new code pass. [command | held-out]
  verify: `scripts/check.sh` exits 0.
  defends against: making a task green by relaxing or removing a pre-existing test rather than satisfying the new behavior.
- (verification-surface) The two-worker exactly-once assertions and the purity/optional-SDK invariant tests retain equal-or-greater strength; any check relaxed is named with a stronger replacement. [command | held-out]
  verify: `uv run pytest packages/flywheel-core/tests/test_task_module_purity.py packages/flywheel-core/tests/test_lifecycle_module_purity.py packages/flywheel-orchestrator/tests/test_orchestrator.py` exits 0 with the named assertions intact.
  defends against: weakening an invariant test (purity, SDK-optional, exactly-once) to fit the transient path in.

## Decomposition Hint (for /fw-plan)
The architectural layers this splits along, so /fw-plan can size one task per slice
and chain them with prerequisites. The shared invariant is the fault-classification
enum (TRANSIENT vs PERMANENT) plus the bounded-backoff helper; every retry site asserts
against it, so the classifier+backoff task is the prerequisite root, and the two-worker
deflake is independent.

- Layer classifier + backoff helper (the shared invariant): satisfies #3, #9. The TRANSIENT/PERMANENT enum and the bounded, capped backoff are defined and unit-graded here. Root prerequisite for the API and store retry sites.
- Layer API / iteration retry: satisfies #1, #2; depends on classifier+backoff. Consumes the rate-limit/api-error signal already collected in the invoker and retries the iteration on a separate budget.
- Layer SQLite store retry + classification: satisfies #4, #5; depends on classifier+backoff.
- Layer Postgres pool bound + classification: satisfies #6; depends on classifier+backoff.
- Layer schema-mismatch permanent-stop: satisfies #7; depends on classifier (PERMANENT label).
- Layer worker breaker integration: satisfies #8; depends on the store-retry and schema-stop layers (the breaker must see transient retries succeed beneath it and a permanent stop short-circuit it).
- Layer two-worker deflake (independent): satisfies #10; no dependency on the classifier work.

Shared invariants multiple layers assert against: the TRANSIENT/PERMANENT classification (an enum/typed value) and the bounded-backoff helper signature. Tasks that introduce new retry sites must update against the SAME classifier in the same change; #9 is the composition holdout that grades the union mapping so no site forks its own buckets.

## Decisions Log (ADR-style: immutable, supersede — never edit)

### D-1: Transient retries use a budget separate from validation `max_retries`  (Status: Accepted)
- Context: A 429 currently becomes a `MissingEnvelope` and spends the agent's validation retry budget, conflating an infra hiccup with an agent failure and failing a task on two API hiccups. | Decision: TRANSIENT faults retry on their own bounded budget at the fault site; the validation `max_retries` budget is untouched by transient handling (per phase decision D-C / respawn-under-budget framing: transient recovery is below the budget that governs genuine failures).
- Rejected: bumping the default `max_retries` (still fails on the second hiccup and mis-accounts the fault as an agent failure). | Consequences: two retry budgets exist and must be reported distinctly; a test must prove the validation budget is NOT drawn down by transient retries (criterion #1 with `max_retries=0`).

### D-2: Schema mismatch is PERMANENT and short-circuits the cycle breaker once  (Status: Accepted)
- Context: The store is reopened every cycle, so a deterministic schema mismatch raises identically on cycles 1..5 and the daemon dies after burning the whole breaker. | Decision: classify schema mismatch PERMANENT and produce a single distinctly-labelled permanent-stop that short-circuits the breaker (phase decision D-A: escalate-once-then-human-queue — a permanent fault is escalated once, not retried).
- Rejected: leaving it to ride the 5-strike breaker (confusing, slow death); retrying it (can never succeed). | Consequences: the worker loop gains a permanent-stop exit path distinct from the transient-cycle-failure path; criterion #7 asserts the strike count is exactly 1.

### D-3: Do NOT add an inner retry around optimistic-concurrency version conflicts  (Status: Accepted)
- Context: Version conflicts on the `version` column are already loop-retried by the harness. | Decision: the transient classifier explicitly does NOT treat a version conflict as a TRANSIENT fault to retry, to avoid double-counting the retry budget. | Rejected: classifying version conflict TRANSIENT (would compete with the existing loop retry and inflate attempts). | Consequences: version conflicts are out of the classifier's TRANSIENT set; only the four named infra faults are TRANSIENT.

### D-4: Backoff is bounded (monotonic-non-decreasing, capped)  (Status: Accepted)
- Context: An unbounded exponential hangs the daemon; a constant delay provides no real backoff. | Decision: the backoff helper grows between attempts and is capped, gradeable via injected sleep/clock. | Rejected: fixed sleep (no backoff); uncapped exponential (DoS). | Consequences: criterion #3 grades both halves; the helper must accept an injectable clock for deterministic testing.

## Open Questions (accepted gaps)
None. Every criterion lowers to a `command` grader against a real pytest target plus `scripts/check.sh`.

## Next Steps
Run `/fw-plan 00067-FEATURE-transient-fault-resilience` to compile these criteria into flywheel tasks and graders.

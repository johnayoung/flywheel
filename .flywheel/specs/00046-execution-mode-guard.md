# Feature: Execution-mode guard

## Outcome
A `flywheel.toml` carries an explicit execution mode (`[execution] mode = "local" | "distributed"`, default `local`), and `load_policy` rejects exactly one configuration cell — `distributed` paired with a non-Postgres store — by raising a keyed `PolicyError` at config-load time. Every pre-existing config (which has no `[execution]` table) keeps loading unchanged, and `mode` changes no runtime scheduling, claim, or lease behavior.

## Background
WorkGraph's distributed operation requires a store every worker can reach concurrently; SQLite (a single local file) silently corrupts or serializes under multi-host contention. Today there is no execution-mode concept at all, and the multi-worker scheduler plus the claim/lease store are already always-on regardless of any mode — so an operator who *intends* distributed operation has no way to declare that intent and no guard that catches the one fatal misconfiguration (distributed intent on a local-only store) before any work starts. This spec makes the intent explicit and turns the single invalid pairing into a fail-fast at load time. The tacit point a literal agent would miss: `mode` is *only* a validation assertion here — it must not be wired into scheduling, claims, or leases, because those are already on; special-casing one matrix cell or making `mode` toggle behavior would both satisfy a narrow reading while breaking the contract.

## Scope
### In scope
- A new top-level `[execution]` table with key `mode`, values `"local"` (default) and `"distributed"`, parsed and carried on `WorkPolicy`.
- A load-time cross-section guard: `mode = "distributed"` with `store.backend != "postgres"` raises a keyed `PolicyError`.
- Strict validation of the `mode` value (a present-but-unrecognized `mode` fails fast with a keyed message); forward-compat for unknown *keys* under `[execution]` (ignored), matching the repo's `_optional_*` convention.

### Out of scope
- Any change to scheduling, claim acquisition, lease renewal, or multi-worker coordination — those are already always-on and `mode` must not gate them.
- Validating Postgres reachability, DSN presence, pooler mode, or privileges (that is `_store_factory` / init territory).
- Introducing a third mode, or any `[execution]` key other than `mode`.

### Must not regress
- Every existing `flywheel.toml` and every existing test must still load and pass unchanged, because the default `mode` is `local` and `local + sqlite` is valid.
- `local + postgres` and `distributed + postgres` remain valid (no new constraint on those cells).
- The `[store]` validation, its keyed `PolicyError` messages, and the `store.backend` default (`sqlite`) are untouched.

## Success Criteria
Each criterion is atomic, grades an observable end-state, and carries its grader type, visibility, the exact check, and the gaming move it forecloses.

1. When a `flywheel.toml` omits the `[execution]` table, `load_policy` succeeds and the resolved policy reports execution mode `local`. [command | held-out]
   verify: `load_policy` on a `[source]`-only TOML returns a policy whose execution-mode attribute equals `"local"`; asserted in the held-out oracle `test_execution_mode.py`.
   defends against: defaulting to `distributed` (which would break every existing config), or adding a mode field that is never populated from the absent-table path.

2. When a `flywheel.toml` sets `[execution] mode = "local"` and `[store] backend = "sqlite"`, `load_policy` succeeds. [command | visible]
   verify: `load_policy` returns a policy with mode `local`; asserted in `test_policy.py`. (Valid matrix cell: local+sqlite.)
   defends against: an over-broad guard that rejects any sqlite store, or that fires on any `[execution]` table presence.

3. When a `flywheel.toml` sets `[execution] mode = "local"` and `[store] backend = "postgres"`, `load_policy` succeeds. [command | visible]
   verify: `load_policy` returns a policy with mode `local` and store backend `postgres`; asserted in `test_policy.py`. (Valid matrix cell: local+postgres.)
   defends against: a guard keyed on store backend alone (rejecting/forcing postgres regardless of mode).

4. When a `flywheel.toml` sets `[execution] mode = "distributed"` and `[store] backend = "postgres"`, `load_policy` succeeds. [command | held-out]
   verify: `load_policy` returns a policy with mode `distributed` and store backend `postgres`; asserted in the held-out oracle `test_execution_mode.py`. (Valid matrix cell: distributed+postgres — the one cell that proves the guard is conditional, not a blanket distributed ban.)
   defends against: implementing the guard as "reject all distributed" (a blanket ban passes criterion 5 but fails here), proving the guard is narrowed to exactly the invalid pairing.

5. If a `flywheel.toml` sets `[execution] mode = "distributed"` while `[store] backend = "sqlite"` (or any non-postgres backend), then `load_policy` raises `PolicyError` keyed by the policy file path and naming both `execution.mode`/`distributed` and the store backend. [command | held-out]
   verify: `pytest.raises(PolicyError, match=...)` against a message containing the policy file path, `distributed`, and `store`/`backend`; asserted in the held-out oracle `test_execution_mode.py`. (Invalid matrix cell: distributed+sqlite — the single rejected cell.)
   defends against: swallowing the misconfiguration and falling back to sqlite/local silently, or raising a bare `ValueError`/unkeyed message the CLI cannot attribute to a file; a message that names only one side could be satisfied without actually checking the pair.

6. If a `flywheel.toml` sets `[execution] mode` to a value that is neither `"local"` nor `"distributed"`, then `load_policy` raises `PolicyError` keyed by the policy file path and naming `execution.mode` and the offending value. [command | held-out]
   verify: `pytest.raises(PolicyError, match="execution.mode")` against a TOML with `mode = "cluster"`; asserted in the held-out oracle `test_execution_mode.py`.
   defends against: treating an unrecognized mode as `local` (silently degrading a typo into "no guard"), mirroring the `store.backend`/`source.kind` strict-validate pattern.

7. When a `flywheel.toml` carries an unknown key under `[execution]` (e.g. `[execution] workers = 4`) alongside a valid `mode`, `load_policy` ignores the unknown key and succeeds. [command | visible]
   verify: `load_policy` on `[execution] mode = "local"` plus an extra unrecognized key returns a policy with mode `local` and raises nothing; asserted in `test_policy.py`.
   defends against: a strict-reject-all-keys reading that would break forward-compat, diverging from the repo's `_optional_*` "unknown keys ignored" convention.

8. The full orchestrator test suite still passes after this change. [command | held-out] (verification-surface)
   verify: `uv run pytest packages/flywheel-orchestrator/tests/` exits 0, including the pre-existing `test_policy.py`, `test_store_factory.py`, and `test_store_registry.py` whose store-backend assertions must be unchanged.
   defends against: making the guard pass by weakening or deleting an existing store/policy assertion, or by changing the `store.backend` default; new behavior cannot be bought by lowering the existing bar.

Verification surface: this feature ADDS validation; it does not change the existing tests, grading commands, or fixtures. Criterion 8 is the inherited Definition-of-Done — the existing orchestrator suite must still run and still pass unchanged; no existing assertion may be relaxed or removed, and the new behavior is proven by a held-out oracle (`test_execution_mode.py`) the implementing agent does not author its passing into.

## Decomposition Hint (for /fw-plan)
This is a single cohesive vertical slice in one module — the config surface, the matrix guard, and their tests all live in `_policy.py` and its test suite. It is not worth splitting; the schema field has no observable behavior without the guard, and the guard has nothing to assert without the field.
- Layer config + guard: satisfies #1–#7, all in `load_policy` / a new `_optional_*`-style helper plus the cross-section check that mirrors the existing `sandbox.container` image guard.
- Layer must-not-regress: satisfies #8.
Shared invariant: the `mode` value carried on `WorkPolicy` and the cross-section pairing rule (`distributed` requires `store.backend == "postgres"`). Both halves assert against it, so they ship as one task — no inter-task edge.

## Decisions Log (ADR-style: immutable, supersede — never edit)

### D-1: Reuse `PolicyError`, keyed by the policy file path  (Status: Accepted)
- Context: The guard fires during `load_policy`, where every other validation failure raises `PolicyError(f"{path}: ...")`. A `StoreConfigError` exists but is a `_store_factory` construction-time error, fired when *building* a store, not when *parsing* config. | Decision: Raise `PolicyError` from `load_policy`, message prefixed with the policy file path, mirroring the `sandbox.container` image guard at `_policy.py:971-975` ("`{policy_file}: ... is required when ...`").
- Rejected: `StoreConfigError` — wrong layer (store build, not config parse) and would fire only when a store is later constructed, too late for a fail-fast intent. A new exception type — gratuitous; the CLI already attributes `PolicyError` to a file. | Consequences: The guard is a config-parse concern; the message must name both the mode and the store so an operator sees why the pair is illegal.

### D-2: Strict-validate the `mode` value; ignore unknown `[execution]` keys  (Status: Accepted)
- Context: The repo splits validation two ways — recognized values are strictly checked (an unknown `store.backend`/`source.kind` fails fast), while unknown *keys* under a table are forward-compat-ignored (the `_optional_*` pattern). | Decision: A present-but-unrecognized `mode` raises a keyed `PolicyError` naming `execution.mode` and the value; unknown keys under `[execution]` are ignored.
- Rejected: Silently coercing an unknown mode to `local` — degrades a typo into "no guard," the exact failure the strict-validate convention exists to prevent. Strictly rejecting unknown keys — breaks forward-compat and diverges from every other `[table]` in this file. | Consequences: A misspelled mode is caught immediately; a future `[execution]` key (e.g. `workers`) can be added by a later spec without breaking old binaries.

### D-3: Guard fires at `load_policy` time, not store-build time  (Status: Accepted)
- Context: The pairing could be checked either when parsing config or when constructing the store. | Decision: Check at `load_policy` time, immediately after both `mode` and `store.backend` are resolved, so an invalid config is rejected before any work, store, claim, or lease is touched.
- Rejected: Deferring to `build_store` — a distributed+sqlite config would parse "successfully" and only fail later when a store is built, defeating fail-fast and possibly after side effects. | Consequences: `load_policy` is the single fail-fast point; both `mode` and `store.backend` are already resolved there, so no new plumbing is needed.

### D-4: `mode` is a pure validation assertion — it toggles no runtime behavior  (Status: Accepted)
- Context: Multi-worker scheduling and the claim/lease store are already always-on regardless of any mode. There is a strong temptation to make `distributed` "turn on" coordination. | Decision: In this spec `mode`'s only enforced effect is the load-time guard. It must not gate scheduling, claims, leases, or any runtime path. `local + postgres` stays fully valid (postgres works in local mode).
- Rejected: Wiring `mode` into the scheduler/claims — out of scope, would change always-on behavior, and is not gradeable here without inventing requirements the architecture decision forbids. | Consequences: The default `local` is a no-op for behavior, so every existing config keeps its exact runtime; the guard is the entire observable surface.

## Open Questions (accepted gaps)
None. Every criterion lowers to a `command` grader run out-of-band by the orchestrator suite; no `rubric`/`manual`/`transcript` criterion remains.

## Next Steps
Run `/fw-plan 00046-execution-mode-guard` to compile these criteria into flywheel tasks and graders.

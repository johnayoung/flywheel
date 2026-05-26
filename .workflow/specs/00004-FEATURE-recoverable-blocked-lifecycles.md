# Feature: Structurally Recoverable Blocked Lifecycles

## Summary

Make `intent=blocked` envelopes carry machine-readable `requires` metadata so the harness can re-evaluate them later and transition `INTERRUPTED -> READY` automatically once prerequisites are satisfied. Replaces today's free-text `harness.blocked` event + hand-promoted recovery (three lifecycles stranded 17h in the 02-harness-resilience audit) with a typed predicate list, a persisted snapshot on the lifecycle row, a public recheck primitive, and a `flywheel recheck-blocked` CLI subcommand.

## Background

Today the harness records `intent=blocked` as `harness.blocked` with `payload={"reason": <free text>}`, finalizes the attempt as `CANCELLED`, and parks the lifecycle in `INTERRUPTED`. Recovery requires a human bulk-UPDATE on `lifecycles.status` — the audit `.workflow/audits/02-harness-resilience.md` (Cross-task patterns) shows three lifecycles all promoted at the same second on 2026-05-26T13:02:40Z after sitting stranded for 17 hours.

The `INTERRUPTED -> READY` lifecycle edge already exists (`src/flywheel/lifecycle.py:47`) and `run_task` entry-time normalization already drains `INTERRUPTED -> READY` (`src/flywheel/harness.py:466`). What is missing is the structured signal of *what would unblock the lifecycle* and the evaluator that decides whether to apply that transition.

Workspace isolation (commit `8270300`) eliminated the cross-task contamination class of blocks, but envelope-blocked still happens for: missing approval, ambiguous spec, environmental prereq, schema/test drift, etc. This feature is pure loop scope per `docs/strategy.md` — orchestration ("when to call recheck") remains the consumer's responsibility.

## Scope

### In Scope

- Extend the `<!-- LOOP_STATUS -->` envelope schema in `src/flywheel/envelope.py` and `docs/loop.md` so `intent=blocked` carries a structured `requires` list.
- Three predicate types in v1: `command_grader`, `file_exists`, `env_var_set`.
- Mandatory `requires` field when `intent=blocked` (envelopes without it are protocol failures).
- Parse-time validation of `requires` against the task definition (unknown grader names, unknown predicate types, and missing required fields are protocol failures, classified `AGENT_ERROR` -> `FAILED_VALIDATION`).
- Persistence of the structured `requires` payload on a new nullable column on the `lifecycles` row (mirror in SQLite + Postgres schemas), cleared when the lifecycle transitions back to `READY`.
- Public recheck primitive `flywheel.harness.recheck_blocked_lifecycle(store, run_id)` that re-evaluates the persisted requires against the worker CWD and applies the `INTERRUPTED -> READY` transition when all predicates are satisfied.
- `flywheel recheck-blocked` CLI subcommand that drives the primitive across all blocked lifecycles by default, with `--run-id <id>` to target one and `--dry-run` to report without transitioning.
- New harness events: `harness.recheck_attempted` (every evaluation, payload includes per-predicate satisfied/detail) and `harness.unblocked` (only on the actual transition).
- Extend `flywheel status` so `interrupted` rows render their `requires` summary (and the full payload in JSON mode).

### Out of Scope

- Deciding *when* to call recheck (consumer concern — e.g. `task-worker.sh` polling on idle).
- Operator dashboard UI.
- Cross-task "await sibling commit" semantics (depends on the strategy layer, which lives above the loop per `docs/strategy.md`).
- `manual_ack` / operator-acknowledgement predicate (deferred; if a lifecycle's `requires` can never be satisfied, the operator continues to mutate the row directly).
- Auto-recheck inside `run_task` entry-time normalization (recheck is **explicit-only**; the existing `INTERRUPTED -> READY` drain stays as-is for SIGINT-paused resumes).
- Recreating or re-entering the original attempt's sandbox at recheck time (the caller is expected to be in the correct CWD).
- `grader_results` rows for recheck evaluations (recheck is a control-plane operation; results live in event payloads only).

## Requirements

### Functional Requirements

1. **FR-1 — Envelope schema extension.** The `ValidEnvelope` dataclass in `src/flywheel/envelope.py` carries a `requires: tuple[BlockedRequirement, ...]` field that is populated only when `intent == Intent.BLOCKED`. The parser rejects an `intent=blocked` envelope that omits `requires`, has a non-list `requires`, or has any entry with an unknown `type` or missing per-type required fields.
   - **Acceptance:** unit tests in `tests/test_envelope.py` cover: (a) well-formed blocked envelope with each predicate type parses to a `ValidEnvelope`; (b) missing `requires` returns `MalformedEnvelope`; (c) `requires` not a list returns `MalformedEnvelope`; (d) unknown `type` value returns `MalformedEnvelope`; (e) each predicate type's required fields are validated; (f) non-blocked intents continue to parse with `requires=()` for backward compat.

2. **FR-2 — Predicate types ship in v1.** Three types are recognized:
   - `{"type": "command_grader", "name": "<grader-name>"}` — references a command grader already declared on the task; the grader spec is resolved from `task.graders` at recheck time. The agent may not supply an inline command.
   - `{"type": "file_exists", "path": "<relative-or-absolute>", "present": true | false}` — path predicate; `present` defaults to `true` when omitted. Path is resolved against the recheck caller's CWD.
   - `{"type": "env_var_set", "name": "<ENV_VAR>"}` — true when `os.environ` contains the named variable and its value is non-empty.
   - **Acceptance:** `tests/test_envelope.py` parses each shape successfully. `tests/test_harness_recheck.py` (new file) exercises each predicate against a controlled CWD/env.

3. **FR-3 — Task-aware envelope validation.** The envelope parser cannot validate `command_grader` predicates in isolation (it has no `Task`). The harness validates `requires` against the task immediately after `parse_envelope` returns a `ValidEnvelope` with `intent=blocked`. A predicate referencing a command-grader name absent from `task.graders` (or a name present but of the wrong grader type) is reclassified as a protocol failure: `harness.protocol_failure` event, attempt finalized as `AGENT_ERROR`, lifecycle transitions to `FAILED_VALIDATION`.
   - **Rationale:** keeps `src/flywheel/envelope.py` pure (no `Task` dependency); centralizes task-aware validation in the existing harness protocol-failure path.
   - **Acceptance:** `tests/test_harness.py` covers each invalid case and verifies the `FAILED_VALIDATION` transition, the `harness.protocol_failure` event payload, and that no `harness.blocked` event is emitted.

4. **FR-4 — Persistence on the lifecycle row.** A new nullable `blocked_requires_json TEXT` column is added to the `lifecycles` table in `src/flywheel/_schema/persistence-schema.sql` and the Postgres mirror. The column is populated when the harness transitions to `INTERRUPTED` via the blocked path (in addition to the existing `harness.blocked` event), and is cleared (set to `NULL`) on every transition back to `READY` (whether by recheck, by retry from `FAILED_VALIDATION`/`INTERNAL_ERROR`, or by run_task entry-time normalization).
   - **Acceptance:** `tests/test_store_sqlite.py` and `tests/test_store_postgres.py` (mirror) round-trip the column; `tests/test_harness.py` confirms it is populated on block and cleared on every `-> READY` transition.

5. **FR-5 — Public recheck primitive.** `flywheel.harness.recheck_blocked_lifecycle(store: HarnessStore, run_id: str, task: Task, *, now=None) -> RecheckOutcome` is exported. It:
   - Loads the lifecycle. If status is not `INTERRUPTED` or `blocked_requires_json` is NULL, returns `RecheckOutcome(applied=False, reason="not_blocked")` without emitting events.
   - Parses the persisted `requires`, evaluates each predicate against the worker CWD (and the supplied `task` for `command_grader` resolution).
   - Emits a `harness.recheck_attempted` event with `{per_predicate: [{type, identifier, satisfied, detail}], all_satisfied: bool}`.
   - If all predicates satisfied: transitions `INTERRUPTED -> READY` (using `_transition` so optimistic concurrency holds), clears `blocked_requires_json`, emits `harness.unblocked` with `{from_status: "interrupted", to_status: "ready"}`, returns `RecheckOutcome(applied=True, ...)`.
   - If any predicate unsatisfied: no transition, returns `RecheckOutcome(applied=False, ...)`.
   - **Acceptance:** `tests/test_harness_recheck.py` covers: (a) all-satisfied path transitions and clears the column; (b) partial-satisfied path emits `recheck_attempted` only, no transition; (c) calling on a non-INTERRUPTED lifecycle is a silent no-op; (d) calling on an INTERRUPTED lifecycle without `blocked_requires_json` is a silent no-op (preserves the SIGINT-pause case); (e) `command_grader` predicate re-runs the named grader and reports its exit code in `detail`.

6. **FR-6 — `flywheel recheck-blocked` CLI subcommand.** A new subparser in `src/flywheel/workflow.py`:
   - Default: scans for `status = 'interrupted' AND blocked_requires_json IS NOT NULL`, calls `recheck_blocked_lifecycle` for each, prints one line per lifecycle of the form `<run_id>: <unblocked|still blocked: <unmet predicate summary>>`.
   - `--run-id <id>` targets one lifecycle (other blocked lifecycles untouched).
   - `--dry-run` evaluates predicates and reports satisfied/unsatisfied without transitioning (does **not** emit `harness.unblocked`; still emits `harness.recheck_attempted` so the dry-run is auditable).
   - Standard `--tasks-dir` and `--db` flags following existing subcommand conventions.
   - Returns exit code 0 when all targeted lifecycles either unblocked or are still blocked on real-world predicates; non-zero only on store/IO errors.
   - **Acceptance:** `tests/test_workflow_cli.py` exercises default scan, `--run-id`, `--dry-run`, and the empty-store case.

7. **FR-7 — Operator surface via `flywheel status`.** The existing `flywheel status` subcommand renders a `blocked_on:` summary line per `interrupted` row when `blocked_requires_json` is non-null, listing the predicate types and primary identifiers (e.g. `blocked_on: command_grader=full-suite, file_exists=.workflow/lkg/.venv`). JSON mode includes the parsed list under a new `blocked_requires` key.
   - **Acceptance:** `tests/test_workflow_cli.py` covers both text and JSON outputs for an interrupted lifecycle with and without `blocked_requires_json`.

### Non-Functional Requirements

- **Purity invariants preserved.** `flywheel.task` and `flywheel.lifecycle` remain pure (no `json`/`pathlib`/`io`). `flywheel.envelope` stays pure — the structural shape of `requires` is validated in the parser; task-aware validation happens in the harness (per FR-3). The new `BlockedRequirement` dataclass family lives in `flywheel.envelope` (pure data only).
- **Performance.** `recheck_blocked_lifecycle` is O(predicates); a `file_exists` / `env_var_set` predicate is O(1). A `command_grader` predicate inherits the named grader's own runtime (could be tens of seconds for a full pytest suite); the CLI's batch loop is intentionally sequential — concurrency is the consumer's responsibility.
- **Concurrency.** The transition `INTERRUPTED -> READY` uses the existing optimistic-concurrency path via `lifecycle.version`; two concurrent recheck callers racing on the same lifecycle resolve via the same `StoreConcurrencyError` semantics the harness already uses (one wins, the other gets a stale-version error).
- **Schema migration.** SQLite supports `ALTER TABLE ADD COLUMN`; the Postgres mirror does too. Persistence-schema.sql gains the column with `DEFAULT NULL`. No migration script is required because new columns are NULL on existing rows and the harness treats NULL `blocked_requires_json` as "not recheckable" (this is the back-compat path for any lifecycle blocked before the migration landed — operators promote those by hand, same as today).
- **Security.** `command_grader` predicates are restricted to graders already declared on the task — the agent cannot inject arbitrary shell via `requires`. `file_exists` paths are checked via `os.path.exists`; no symlink unwrapping or content read. `env_var_set` reads `os.environ.get(name)`; the predicate does not echo the value back in any event payload.

## Behavior Specification

### Happy Path

1. The agent emits an iteration whose envelope reads:
   ```html
   <!-- LOOP_STATUS -->
   {
     "intent": "blocked",
     "reason": "sibling task left full-suite red",
     "requires": [
       {"type": "command_grader", "name": "full-suite"},
       {"type": "file_exists", "path": ".workflow/lkg/.venv"}
     ]
   }
   <!-- /LOOP_STATUS -->
   ```
2. `parse_envelope` returns `ValidEnvelope(intent=BLOCKED, reason=..., requires=(CommandGraderRequirement(name="full-suite"), FileExistsRequirement(path=".workflow/lkg/.venv", present=True)))`.
3. The harness validates the `command_grader` name resolves in `task.graders`. It then:
   - finalizes the attempt as `CANCELLED` (existing path),
   - persists the structured requires onto `lifecycles.blocked_requires_json`,
   - emits `harness.blocked` with the structured payload (existing event, new shape),
   - transitions `RUNNING -> INTERRUPTED`.
4. Later, a consumer (e.g. `task-worker.sh` polling on idle) runs `uv run flywheel recheck-blocked --db .workflow/flywheel.sqlite`. The CLI:
   - loads each blocked lifecycle,
   - re-runs `full-suite` (exit 0) and checks `.workflow/lkg/.venv` (exists),
   - emits `harness.recheck_attempted` and `harness.unblocked`,
   - clears `blocked_requires_json`,
   - transitions `INTERRUPTED -> READY`.
5. The next worker picks the lifecycle up from `READY` and resumes work via `run_task`.

### Error Handling

| Error Condition | Expected Behavior |
| --------------- | ----------------- |
| `intent=blocked` envelope missing `requires` | Parser returns `MalformedEnvelope`. Harness emits `harness.protocol_failure`, finalizes attempt as `AGENT_ERROR`, transitions `VALIDATING -> FAILED_VALIDATION`. |
| `requires` entry has unknown `type` | `MalformedEnvelope` at parse time; same downstream as above. |
| `command_grader` predicate names a grader not in `task.graders` (or names a non-command grader) | Parser returns `ValidEnvelope`; harness rejects it post-parse via the same protocol-failure path (FR-3). |
| `file_exists` predicate missing `path` field, or `path` is not a string | `MalformedEnvelope` at parse time. |
| `env_var_set` predicate missing `name` field, or `name` is not a string | `MalformedEnvelope` at parse time. |
| Recheck called on a lifecycle not in `INTERRUPTED` | Silent no-op; returns `RecheckOutcome(applied=False, reason="not_blocked")`. No events emitted. |
| Recheck called on an `INTERRUPTED` lifecycle with `blocked_requires_json IS NULL` (SIGINT-pause case) | Silent no-op (same as above). Preserves the existing SIGINT-pause recovery path which does **not** populate the column. |
| `command_grader` predicate's named grader is no longer present on the (mutated) task at recheck time | Treat as unsatisfied; `harness.recheck_attempted` records `satisfied: false, detail: "grader not found"`. No transition. Operator must mutate the row manually or update the task. |
| Recheck command grader subprocess fails to start (e.g. binary missing) | Treat as unsatisfied; `detail` includes the OS error message. No transition. |
| Concurrent recheck callers racing on the same lifecycle | Optimistic concurrency: one transition wins, the loser surfaces a stale-version error to its caller. |

### Edge Cases

| Case | Expected Behavior |
| ---- | ----------------- |
| Empty `requires: []` on `intent=blocked` | `MalformedEnvelope` — requires at least one predicate. (Rationale: an empty list would unblock immediately on first recheck, which is meaningless.) |
| Duplicate predicate entries (e.g. same file_exists path twice) | Accepted; each is evaluated independently. No de-dup. |
| `file_exists` with `present: false` and the path exists | Predicate unsatisfied. |
| `env_var_set` where the variable is set to the empty string | Predicate unsatisfied (matches "non-empty" rule in FR-2). |
| `command_grader` predicate references the same grader name as one of the task's validation graders | Allowed; recheck re-runs the grader exactly as validation would. |
| Lifecycle is `INTERRUPTED` from the SIGINT path (no `blocked_requires_json`) and the operator runs `flywheel recheck-blocked` | Lifecycle is skipped by the default scan (filter `WHERE blocked_requires_json IS NOT NULL`). Operator continues to use existing run_task entry-time normalization to resume it. |
| `--dry-run` recheck where all predicates satisfy | `harness.recheck_attempted` is emitted; `harness.unblocked` is **not** emitted; no transition; `blocked_requires_json` is not cleared. CLI prints `<run_id>: would unblock`. |
| Worker CWD at recheck time differs from the worktree the original attempt ran in | Predicates evaluate against the current CWD. The spec assigns this responsibility to the caller; the harness does not re-enter or validate the sandbox. |
| `flywheel status` JSON output for a lifecycle never blocked | `blocked_requires` key is omitted (or `null`), matching existing nullable-field rendering. |

## Technical Context

### Affected Modules

- `src/flywheel/envelope.py` — extend `ValidEnvelope` with `requires` tuple; add `BlockedRequirement` tagged-union dataclasses (`CommandGraderRequirement`, `FileExistsRequirement`, `EnvVarSetRequirement`); validate `requires` shape in `parse_envelope`. Must stay pure.
- `src/flywheel/harness.py` — in the `Intent.BLOCKED` branch of `_run_attempt`: validate `requires` against `task.graders`, populate `lifecycle.blocked_requires_json` before transitioning to `INTERRUPTED`. Add new `recheck_blocked_lifecycle` public function and a `RecheckOutcome` return dataclass. Clear `blocked_requires_json` on the existing `-> READY` transitions (centralize in `_transition` or in `recheck_blocked_lifecycle` and the retry/normalization paths — TBD during implementation).
- `src/flywheel/lifecycle.py` — add `blocked_requires_json: str | None = None` field on `Lifecycle`. Must stay pure (no JSON parsing in this module; the harness handles serialization/deserialization).
- `src/flywheel/_schema/persistence-schema.sql` — add `blocked_requires_json TEXT` column to `lifecycles`. Mirror in `store_postgres.py`'s schema.
- `src/flywheel/store_sqlite.py` and `src/flywheel/store_postgres.py` — extend INSERT/UPDATE/SELECT to round-trip the new column.
- `src/flywheel/workflow.py` — add `recheck-blocked` subcommand; extend `_cmd_status` and `build_status_rows` to surface `blocked_requires`. Possibly extend `TaskStatusSnapshot` to carry the requires.
- `docs/loop.md` — update the iteration envelope section to document `requires` as part of the `blocked` intent contract; document the recheck primitive and CLI subcommand in the harness behavior section.
- `docs/task-lifecycle.md` — note that `INTERRUPTED -> READY` can also be driven by `recheck_blocked_lifecycle` when the attached `requires` are satisfied.

### Integration Points

- **Existing `INTERRUPTED -> READY` edge** (`src/flywheel/lifecycle.py:47`): unchanged. Recheck just becomes a third caller alongside the SIGINT resume path and the manual operator path.
- **Existing `_transition` helper**: unchanged externally, but the call sites that drive `-> READY` must also `lifecycle.blocked_requires_json = None`. Easiest is to centralize the clear inside `_transition` keyed on `target == Status.READY`.
- **Existing command grader infrastructure** (`src/flywheel/grader_command.py`): reused for `command_grader` predicate evaluation. The recheck path calls into the same runner but does **not** persist a `grader_results` row (the recheck event payload carries the outcome instead).
- **Existing optimistic concurrency**: `recheck_blocked_lifecycle` participates by reading `lifecycle.version`, applying the transition, and letting the store reject stale writes.

### Relevant Existing Code

- `src/flywheel/harness.py:665-690` — the current `Intent.BLOCKED` branch; this is where the envelope's structured `requires` gets persisted and where the protocol-failure validation for task-aware predicates is inserted.
- `src/flywheel/harness.py:462-466` — `run_task` entry-time normalization that drains `INTERRUPTED -> READY` unconditionally. **Important:** this must continue to work for the SIGINT-pause case (where `blocked_requires_json` is NULL); the spec explicitly preserves it.
- `src/flywheel/lifecycle.py:47` — the `INTERRUPTED -> READY` transition definition.
- `src/flywheel/envelope.py:32-37` — `ValidEnvelope` definition that gains the `requires` field.
- `src/flywheel/workflow.py:543-560` — `_cmd_status` and the JSON output path that gains the `blocked_requires` surface.
- `.workflow/audits/02-harness-resilience.md` — the audit that motivated this work, especially the `drop-default-transcript-grader` finding and Cross-task patterns section.

## Decisions Log

| Decision | Choice | Rationale |
| -------- | ------ | --------- |
| v1 predicate set | `command_grader`, `file_exists`, `env_var_set` | Covers the realistic block classes the audit surfaced (schema/test drift, missing artifact, missing credentials). Manual operator ack deferred — operators continue to mutate rows directly for unsatisfiable blocks. |
| Envelope shape | List of tagged predicates, AND semantics | Conjunctive list covers >95% of real cases; no OR semantics needed in v1. Tagged unions keep the parser deterministic. |
| Required-ness | Mandatory from v1 | An `intent=blocked` envelope without `requires` is a protocol failure. Guarantees every blocked lifecycle is recheckable; agents whose prompts predate this feature will fail loud rather than silently re-enter the stranded-lifecycle pattern. |
| Eval context | Worker CWD at recheck time | Simple, no sandbox bookkeeping. Caller (out-of-scope orchestrator) is responsible for being in the right directory before invoking recheck. |
| Recheck trigger | Explicit only | Pure loop primitive. `run_task` entry-time normalization continues to drain `INTERRUPTED -> READY` unconditionally for SIGINT-paused lifecycles; envelope-blocked lifecycles wait for explicit recheck calls. Keeps "when to recheck" firmly in consumer hands. |
| Persistence | New `blocked_requires_json` column on `lifecycles` | O(1) lookup, cleared on `-> READY` transition like `error` is, matches the existing live-row pattern. Event payload alone would force O(events) scans for live state. |
| Status surface | Extend `flywheel status` | One mental model, one subcommand. JSON mode carries the full payload for tooling. No separate `flywheel blocked` subcommand in v1. |
| CLI default scope | All blocked lifecycles, recheck + transition | Matches the spec's "(d) drives (c) across all blocked lifecycles". `--run-id` for single targeting, `--dry-run` for report-only. |
| Recheck events | `harness.recheck_attempted` + `harness.unblocked` | Two events distinguish "I tried but the world hasn't changed" from "I actually transitioned." Full audit trail for forensic work. |
| Grader naming for `command_grader` predicate | Only graders already on the task | Tight, audit-able. Inline command specs would reintroduce the free-form problem this feature is fixing and add a shell-injection surface. |
| Bad `requires` handling | Parse-time `MalformedEnvelope` -> `AGENT_ERROR` -> `FAILED_VALIDATION` | Consistent with the existing "envelope is untrusted protocol input" contract; the agent loses the attempt rather than silently parking on unsatisfiable requires. Task-aware validation (unknown grader name) happens immediately post-parse in the harness, classified the same way. |
| Recheck audit row | No `grader_results` row; record in `harness.recheck_attempted` payload only | Recheck is a control-plane operation, not a validation attempt. Keeps `grader_results`' "one row per validation grader execution" invariant intact. |
| Path / env-var semantics | `file_exists.present` defaults to `true`; `env_var_set` requires non-empty value | Deterministic defaults; documented in FR-2. |
| Empty `requires` list | `MalformedEnvelope` | Empty list would unblock on first recheck; meaningless and almost certainly an agent bug. |

## Open Questions

None remaining for v1. Items deferred to follow-ups:

- A `manual_ack` predicate type (and corresponding `flywheel ack-block <run_id>` CLI verb) once we observe how often operators want a typed acknowledgement instead of a direct row mutation.
- Lifting recheck into `run_task` entry-time normalization once the consumer cadence is well-understood (today's "explicit only" puts the cadence question on the consumer where the spec wants it).
- Cross-task `await sibling lifecycle reached DONE` predicate — depends on the strategy layer per `docs/strategy.md`.

## Next Steps

Run `/task 00004-FEATURE-recoverable-blocked-lifecycles` to generate implementation tasks from this spec.

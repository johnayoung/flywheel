# Feature: In-Loop Verification Gate

## Summary

A phase that adds a new loop path (lifecycle state/transition, live-store schema column, grader type, store-contract/resolver entry, or control-command verb) cannot be declared done until that path has been exercised end-to-end against the real `orchestrate` loop and a real migrated store. A diff-derived marker auto-flags loop-path phases, a mechanical gate in `archive_completed_phases` refuses to archive a flagged phase without a DONE in-loop-verification task (or a recorded, auditable opt-out), and `/audit-phase` re-checks every opt-out against the diff.

## Background

Flywheel graders run inside the agent's worktree against an ephemeral SQLite store bootstrapped from the current schema file — never against the real `orchestrate` loop or the long-lived `.workflow/flywheel.sqlite`. A feature can therefore pass every grader without its loop path ever running once.

This has detonated twice:

- **Phase 17** (`.workflow/audits/17-manual-grader-approval-gate.md`) shipped `AWAITING_APPROVAL`, the resolver, and `approve`/`reject` verbs. The store has zero `awaiting_approval` lifecycles and zero `manual` grader receipts — the loop never entered the path it built. Verification came entirely from in-process pytest graders (`17:73-77`).
- **Phase 08** (`.workflow/audits/08-recoverable-blocked-lifecycles.md`) added `blocked_requires_json` to the schema file; graders passed via grep-on-file + dataclass-field checks. The long-lived store was never migrated, and the next task crashed in `store.load_lifecycle` — a 76-run `OperationalError` crash storm with an 8-minute silent telemetry gap (`08:51-53`, `08:80`, `08:92`).

Both share one root cause: the verification surface and the deliverable are the same surface (ephemeral-store pytest + file grep). "Graders pass" did not mean "the shipped path was ever run." This is proposal **P1** of `.workflow/proposals/17-manual-grader-approval-gate.md`.

## Scope

### In Scope

- A **diff-derived loop-path marker**: at phase-archive evaluation, the phase's cumulative merged diff vs base is inspected for a fixed set of decidable symbol-level signals; any hit marks the phase as loop-path-bearing.
- A **mechanical archive gate**: `archive_completed_phases` refuses to move a loop-path-marked phase to `archive/` unless it contains a DONE in-loop-verification task OR a recorded, attributable opt-out artifact.
- An **in-loop-verification task convention**: `/task` (and `/define`) emit such a task for loop-path features; the task drives a fixture through the real loop and asserts the new path both produces telemetry and survives a real prior-version migration.
- An **opt-out artifact**: a persisted, attributable file recording who/which-phase/why "no new loop path," downgrading a false-positive marker.
- An **audit backstop**: `/audit-phase` (`audit-phase.md`) re-derives the marker from the diff and flags (a) loop-path diffs archived with no verify task and no opt-out, and (b) opt-outs whose diff in fact added a watched symbol.

### Out of Scope

- Replacing or weakening any existing grader type (`command`/`rubric`/`manual`/`transcript`).
- Running a real (paid, nondeterministic) agent as part of the gate.
- Driving the loop against the literal production `.workflow/flywheel.sqlite` (pollution/corruption risk).
- Changing `orchestrate`'s single-stream contract or any `flywheel.task`/`flywheel.lifecycle` purity invariant.
- Auto-generating the verify task's fixture/assertions content (authoring discipline remains human; only the task slot is auto-required).
- Retroactively gating already-archived phases.

## Requirements

### Functional Requirements

1. **FR-1 — Diff-derived loop-path marker.** At phase-archive evaluation, compute the phase's cumulative diff vs base and mark the phase loop-path if the diff contains any watched signal (see Behavior Specification → Trigger Set). The marker is an intentional over-approximation: file/symbol presence cannot prove a new path exists.
   - Acceptance: a synthetic phase diff that adds a `Status` enum member is marked; a phase diff that only renames a local variable in `lifecycle.py` is not.

2. **FR-2 — Mechanical archive gate.** `archive_completed_phases` must not move a loop-path-marked phase to `archive/` unless the phase contains an in-loop-verification task whose lifecycle is DONE, or a recorded opt-out artifact. A blocked phase stays `active`, and the refusal reason is logged.
   - Acceptance: a marked phase with all feature tasks DONE but no verify task and no opt-out is NOT archived; adding a DONE verify task (or the opt-out) lets the next archive sweep move it.

3. **FR-3 — In-loop-verification task drives both loop-produced ends.** The verify task drives a fixture through the real `orchestrate`/harness using flywheel's existing injectable invoker seam fed a deterministic scripted envelope. It must exercise both halves of the new path, not just entry. For the manual-gate exemplar: scripted attempt → real harness parks the lifecycle at `AWAITING_APPROVAL` → a real `approve` control-command row is enqueued → the real orchestrator reactive sweep claims and applies it → assert the lifecycle reaches `done` with a persisted manual receipt.
   - Acceptance: the verify task fails if either the park step or the sweep-apply step is stubbed/skipped; it passes only when both real ends run.

4. **FR-4 — In-loop-verification exercises the real migrate-from-prior-version path.** For schema-touching features the verify task must seed a `v(N-1)` store fixture, run the real forward migration in `store_sqlite.py` to `vN`, then assert the new column/table exists AND the new path round-trips through the real `SqliteStore` read/write code path (not a raw `PRAGMA` alone). A fresh store bootstrapped from the current schema file does NOT satisfy this — it reproduces phase 08's exact blind spot.
   - Acceptance: the verify task fails when pointed at a current-schema-bootstrapped store and passes only against a `v(N-1)`-seeded store migrated forward; the assertion goes through `SqliteStore` (e.g. `load_lifecycle`), the call that crash-stormed in phase 08.

5. **FR-5 — Auditable opt-out.** A false-positive marker is downgraded only by a persisted, attributable artifact recording who, which phase, and why "no new loop path." A silent flag is not acceptable.
   - Acceptance: the gate honors the opt-out and lets the phase archive; the artifact is committed/persisted and is machine-readable enough for the audit to locate and re-check.

6. **FR-6 — Audit re-check of marker and opt-outs.** `/audit-phase` re-derives the marker from the phase diff and emits a finding when (a) a loop-path diff was archived with neither a verify task nor an opt-out, or (b) an opt-out claims "no new path" but the diff added a watched symbol (new `Status`/`Outcome`/transition, new `ADD COLUMN`/table, new `Grader` union variant, new `store_protocols` Protocol method + dispatch registration, or new `CONTROL_COMMAND_*` constant).
   - Acceptance: a deliberately mis-tagged opt-out over a diff that adds a `Status` member produces an audit finding; a correct opt-out over a pure refactor does not.

7. **FR-7 — Command-emission convention.** `/task` and `/define` produce an in-loop-verification task for features whose spec/diff fall in the trigger set, so authors do not have to remember to write it.
   - Acceptance: running `/task` on this spec (or a loop-path spec) yields a verify task slot wired to FR-3/FR-4.

### Non-Functional Requirements

- **Performance**: the gate and verify task must be deterministic and cheap enough to run on every archive sweep — no real-agent invocation, no network, repeatable. The verify task is a `command` grader driving `orchestrate` with a scripted invoker.
- **Security**: the verify task must never read, write, or migrate the production `.workflow/flywheel.sqlite`; it operates only on fixture/temp stores.
- **Determinism**: re-running the verify task yields the same result; the fixture invoker emits canned envelopes (no agent text under test).

## Behavior Specification

### Trigger Set (watched signals, ranked by trust)

| # | Signal | Decidable test | Trust |
| - | ------ | -------------- | ----- |
| 1 | New `Status`/`Outcome` enum member or transition-rule entry | added enum member / new `_VALID_EDGES`-style entry in `lifecycle.py` | rock-solid |
| 2 | New `ADD COLUMN` / table in `_schema/*.sql` the live store binds to | new column/table DDL (lifecycles, grader_results, events, attempts, control_commands) | rock-solid (the phase-08 signal; ephemeral tests always have the column, live store is the only place it shows) |
| 3 | New `Grader` union variant | new variant in `task.py`'s `Grader` union or new `grader_*.py` module the harness dispatches | clean |
| 5 | New control-command verb | new `CONTROL_COMMAND_*` constant in `invoker_client.py` | clean |
| 4 | New store-contract / resolver entry | new method on a `store_protocols.py` Protocol AND a new entry point registered into the reactive sweep / transition dispatch; "some new resolver function in harness.py/orchestrator.py" is the explicit file-touch fallback only | over-approximation — leans hardest on opt-out + audit re-check; keep it the tightest |

Signal 4 must be scoped to its decidable core. If it fires on "any new function in `harness.py`/`orchestrator.py`," it trips nearly every phase, opt-outs become routine, and the gate erodes back toward a manual tag.

### Happy Path

1. A phase adds a new loop path (e.g. a new `Status` member + migration + control verb).
2. `/task` emits the feature tasks plus an in-loop-verification task.
3. The verify task drives a fixture through real `orchestrate` (scripted invoker): both loop-produced ends run (FR-3), and the prior-version migration path is exercised (FR-4).
4. All feature tasks and the verify task reach DONE.
5. `archive_completed_phases` computes the diff, sees a watched signal, finds the DONE verify task, and archives the phase.
6. `/audit-phase` later re-derives the marker, finds the verify task present and the path has live telemetry — no finding.

### Error Handling

| Error Condition | Expected Behavior |
| --------------- | ----------------- |
| Loop-path diff, all feature tasks DONE, no verify task, no opt-out | Gate refuses to archive; phase stays `active`; refusal reason logged |
| Verify task present but its lifecycle is not DONE | Gate refuses to archive (same as missing) |
| Verify task asserts against a current-schema-bootstrapped store | Verify task fails by FR-4 (must use `v(N-1)`-seeded + real migration) |
| Verify task stubs either park or sweep-apply | Verify task fails by FR-3 (both real ends required) |
| Opt-out present but diff added a watched symbol | Phase archives (gate honors opt-out), but `/audit-phase` emits a finding (FR-6b) |
| Verify task attempts to touch `.workflow/flywheel.sqlite` | Disallowed; verify task must use fixture/temp stores only |

### Edge Cases

| Case | Expected Behavior |
| ---- | ----------------- |
| Pure refactor of a watched file (docstring/rename, no new symbol) | Symbol-level test does not fire (signals 1-3,5); for signal-4 file-touch fallback, downgrade via recorded opt-out |
| Diff adds a column but reverts it within the same phase | Marker is over-approximate; if no net new symbol, opt-out path applies; audit re-check sees no net symbol and does not flag |
| Multiple new loop paths in one phase | One in-loop-verification task may cover several, provided it exercises each path's both-ends + migration assertions; audit checks telemetry for each |
| Phase touches a watched file via dependency bump / generated code | Treated as candidate; downgrade via opt-out with recorded reason; audit re-checks for net new symbol |
| Re-bootstrap of an already-`vN` store in the verify fixture | Migration is a guarded no-op (version-pinned `UPDATE`); fixture must start at `v(N-1)` to exercise the real forward step |

## Technical Context

### Affected Apps

- `flywheel` (library): the archive gate lives in the consumer-facing workflow layer (`src/flywheel/workflow.py`, `archive_completed_phases`); diff-derivation is a new pure-ish helper (file/JSON I/O belongs in the workflow/loaders layer, not `flywheel.task`/`flywheel.lifecycle`).
- `.workflow/worker.py`: invokes `archive_completed_phases` in its poll loop — the gate must hold there.
- `/task`, `/define`, `/audit-phase` commands (`.claude/commands/`): emit the verify task and re-check the marker.

### Integration Points

- **`archive_completed_phases`** (`src/flywheel/workflow.py:1114`) — the operational "phase declared done" moment; the gate attaches here and needs access to the phase's cumulative diff vs base.
- **`orchestrate` / harness** (`src/flywheel/orchestrator.py`, `src/flywheel/harness.py`) — the verify task drives the real control flow; the injectable invoker seam feeds the deterministic envelope (the same seam the existing harness test suite uses).
- **`SqliteStore` forward migration** (`src/flywheel/store_sqlite.py`, version-pinned `3→4→5` migrations, `CURRENT_SCHEMA_VERSION`) — FR-4 seeds `v(N-1)` and exercises this exact path.
- **Control-command dispatch** (`src/flywheel/invoker_client.py` `CONTROL_COMMAND_*`, `control_commands` table) — FR-3's `approve` enqueue + reactive-sweep apply.

### Relevant Existing Code

- `src/flywheel/workflow.py:1114` (`archive_completed_phases`) — gate attach point.
- `src/flywheel/store_sqlite.py:197-256` — real forward-migration ladder; the FR-4 reference path (`load_lifecycle` is the phase-08 crash site).
- `src/flywheel/invoker_client.py:63-83` — control-command verb constants + applied/failed events.
- `src/flywheel/store_protocols.py:285+` — Protocol surface; signal-4's decidable core (new Protocol method).
- `tests/test_worker.py::test_run_once_merges_completed_task` — the model for driving a full real-loop cycle with a fake invoke against a real store.
- `.claude/commands/audit-phase.md`, `.claude/commands/task.md` — backstop and emission integration.

## Decisions Log

| Decision | Choice | Rationale |
| -------- | ------ | --------- |
| Verification mechanism | Dogfood task with real-loop command graders | One mechanism covers both demonstrated gaps (08 store-binding, 17 state-production); reuses existing command-grader machinery rather than a recursive "loop grader" |
| Enforcement | Both: commands emit the verify task AND a mechanical archive gate blocks without it AND audit backstops | Convention + post-hoc audit alone guarantees nothing (it ran after "done" in phase 17); only a mechanical gate at the archive moment blocks before done; the audit is the coverage check on the gate's trigger, not the guarantee |
| Marker assignment | Diff-derived, not a hand-applied tag | A manual tag relocates the human weak-link from "write the verify task" to "remember to tag"; diff-derivation makes the gate self-triggering |
| False-positive handling | Auto-derive (default-deny) + recorded, attributable, audited opt-out | Pure no-opt-out is too blunt (a docstring fix in a watched file forces a full verify task → authors dodge clean edits → gate dies); opt-out keeps the mechanical guarantee while bounding friction; the opt-out is a falsifiable claim, not a silent escape |
| Trigger granularity | Symbol-level where cheap (signals 1,2,3,5), file-touch fallback for signal 4 | Symbol-level kills the refactor false positive at the source so opt-outs stay rare; signal 4 scoped to new Protocol method + dispatch registration to avoid tripping every phase |
| Trigger set | 1 Status/Outcome/transition, 2 ADD COLUMN/table, 3 Grader variant, 4 store-contract/resolver entry (tightest), 5 control-command verb | Drawn from both audits + phase-17's approve/reject (a new verb none of 1-4 catch) |
| Fidelity | Real `orchestrate` + deterministic scripted invoker + real SqliteStore | Both at-risk things (loop produces the path; store is migrated) live in the harness/orchestrator/store layer, not the agent's tokens; a real agent adds zero coverage while adding nondeterminism, cost, and rate-limit flakiness; a command gate must be deterministic and repeatable |
| Sub-gap (a) coverage | Drive both loop-produced ends (harness park AND reactive sweep apply) | Phase 17 had both halves unit-only; one end alone re-creates the gap |
| Sub-gap (b) coverage | Seed `v(N-1)`, run real migration to `vN`, assert via `SqliteStore` | A fresh current-schema store already has the column — phase 08's exact trap; "uses SqliteStore" is insufficient, "exercises real migrate-from-previous-version" is the requirement |
| Production store | Never driven/migrated by the verify task | A repeatedly-run command grader would write fixture rows/migrations into the store the audits read; gap (b) needs the real open→migrate code path, not the literal production file |
| Scripted invoker vs no-mocks rule | Permitted; state explicitly in spec | Not a forbidden mock of logic-under-test — it is flywheel's existing injectable invoker seam fed a deterministic envelope (how the whole harness test suite runs); everything under test (gate entry, sweep, migration, receipt) is real and unmocked, only the agent's text (not what's being verified) is scripted |

## Open Questions

- **Diff-vs-base availability in `archive_completed_phases`.** The gate needs the phase's cumulative diff at archive-eval time. Confirm during `/task` whether the base ref / merged diff is reachable from the workflow layer (the worker FF-merges `flywheel/<phase>/<task>` branches), or whether the marker must be computed earlier and persisted.
- **Opt-out artifact location/format.** Exact path and shape (e.g. `active/<phase>/loop-path-exempt.md` with structured front-matter) so both the gate and `/audit-phase` can locate and machine-check it — to be settled in `/task`.

## Next Steps

Run `/task 00017-FEATURE-in-loop-verification-gate` to generate implementation tasks from this spec.

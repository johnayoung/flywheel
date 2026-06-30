# Feature: Stop Observability (nothing stops invisibly)

## Outcome
Every way a unit of work stops, parks, strands, or dead-ends emits a durable, queryable record at the moment it stops, so that after any such stop an events query (`list_domain_events(run_id)` for a finalized run, or the per-task orchestrator-events ledger for a pre-run stop) returns a record naming the reason, and `fw status` enumerates the stranded/stopped unit instead of leaving it indistinguishable from "no work" or "repo is green".

## Background
Today flywheel has two durable event ledgers — the core per-run domain-event stream (where `LandingParked` lives, surfaced by `fw status` as `stranded:`) and the per-task orchestrator_events ledger (claim transitions). But most stop paths reach neither: only three of ~seven landing-park paths emit `LandingParked` (held-out-gate FAIL, protected-paths refusal, PR push-failure, and the generic `submit()` swallow record nothing), the held-out gate verdict lives on an in-process `RunRecord` that is never persisted, and four pre-run dead-ends (dangling prerequisite, un-authorable autopilot cycle, container prepare-preflight skip, source-listing truncation / zero-grader drop) emit only a log line or a discarded in-memory value. The tacit requirement an operator holds and a literal agent misses: a stop that is logged is NOT observable — the authoritative state-vs-telemetry split (docs/data-taxonomy.md) makes logs telemetry (lossy, ~30-day) and requires that anything an operator must enumerate be queryable from durable state. This phase is the prerequisite for auto-re-driving (P5): you cannot re-drive what you cannot enumerate. It changes only WHETHER a stop is visible, never WHEN work stops.

## Scope
### In scope
- Every landing-park / submit-refusal / submit-swallow path emits a durable `LandingParked` (or equivalent same-ledger) record carrying its reason, on the same per-run domain-event surface the three existing park sites use.
- A held-out-gate-blocked landing persists a durable record of the FAIL verdict (today it is `RunRecord`-only, in-process).
- Each pre-run dead-end that has no run_id (dangling prerequisite, un-authorable / no-op autopilot cycle, recurring container prepare-preflight skip, source-listing truncation, zero-grader item drop) emits a durable record on a per-task queryable ledger.
- `fw status` enumerates the stopped/stranded units that the new records make queryable.

### Out of scope
- Changing WHEN or WHETHER work stops — no new gate, no new refusal, no retry/respawn/re-drive logic (re-driving is P5). This phase only makes existing stops visible.
- Draining or acting on the visible queue (P5 consumes it).
- Inventing a parallel telemetry channel, dashboard, metrics backend, or new CLI verb beyond what `fw status` already renders.
- Postgres-only or distributed-coordination behavior beyond mirroring the existing store contract.

### Must not regress
- The existing three park paths (uncommitted-work, divergent-base, standing-verify) still emit `LandingParked` and still surface under `fw status` as `stranded:` exactly as today.
- No stop path changes the run's terminal lifecycle status: a parked DONE run stays `Status.DONE`; an emitted record is an audit-witness whose fold is the identity (it advances `version` only).
- The orchestrator_events ledger stays append-only (no update/delete; N committed stops produce N records in insertion order), matching the claim-transition contract.
- The worker still never raises out of `submit()` into the orchestrate loop; recording a stop must not introduce an escape path.

## Success Criteria
Each criterion is atomic, grades an observable end-state, and carries its grader type, visibility, the exact check, and the gaming move it forecloses. `/fw-plan` lowers each one to a command / transcript / rubric / manual grader.

1. When a DONE run's landing is suppressed because the held-out gate returned FAIL, the per-run domain-event stream for that run contains a park record whose reason identifies the held-out gate. [command | held-out]
   verify: a test drives a run to DONE under a held-out source that FAILs, then asserts `store.list_domain_events(run_id)` contains a `LandingParked` whose `park_kind` marks the held-out-gate cause and whose `detail` is non-empty; assert no merge/PR landed.
   defends against: emitting only the existing stream log line (telemetry, not queryable) and counting that as "recorded"; or attaching the record to the wrong run.

2. When a submit refuses to land because the branch touches a protected path, the per-run domain-event stream for that run contains a park record naming the protected-path refusal as the reason. [command | held-out]
   verify: a test drives a DONE run whose branch touches a configured protected path through both the merge and the PR strategy, then asserts `list_domain_events(run_id)` contains a `LandingParked` whose `park_kind` marks the protected-path cause and whose `detail` names the offending path(s).
   defends against: recording the merge path but not the PR path (narrow coverage), or recording a generic park with no reason so the operator cannot tell it from an unrelated strand.

3. When a submit cannot land because the push to the remote was rejected or the gh invocation failed, the per-run domain-event stream for that run contains a park record naming the push/remote failure as the reason. [command | held-out]
   verify: a test drives a DONE PR-strategy run whose push is forced to fail, then asserts `list_domain_events(run_id)` contains a `LandingParked` whose `park_kind` marks the push-failed cause and whose `detail` carries the failure text.
   defends against: swallowing the git/gh stderr into a log line only; printing a success string while leaving the branch un-pushed.

4. If submit raises an unexpected exception that the worker swallows to protect the orchestrate loop, then the per-run domain-event stream for that run contains a park record marking the swallowed-error strand. [command | held-out]
   verify: a test forces `_submit` to raise, then asserts (a) `submit()` does not propagate the exception and (b) `list_domain_events(run_id)` contains a `LandingParked` whose `park_kind` marks the swallowed-error cause and whose `detail` carries the exception text.
   defends against: keeping the bare `except` that logs-and-returns so the strand stays invisible; or letting the new recording call re-introduce an exception escape into orchestrate.

5. When the work graph for a scheduling pass contains a task whose declared prerequisite names no existing task, the per-task orchestrator-events ledger for that referencing task contains a durable record naming the missing prerequisite. [command | held-out]
   verify: a test runs one orchestrate pass over items where task A declares a prerequisite on a non-existent id, then asserts the per-task events query for A returns a record marking a dangling-prerequisite stop whose payload names the missing id; assert A stays out of the ready set (unchanged behavior).
   defends against: continuing to discard the already-computed `GraphValidationIssue` in memory; recording it only as a transient log so a later audit sees nothing.

6. When an autopilot cycle completes without emitting any grader-bearing task (no actionable findings, or nothing authorable), the orchestrator-events ledger contains a durable record marking that no-op cycle and its reason. [command | held-out]
   verify: a test runs an autopilot pass forced to emit zero tasks, then asserts a per-cycle/per-source events query returns a no-op-cycle record whose payload carries the pass's reason and the target/observed queue depth; assert the pass still emits zero tasks (unchanged behavior).
   defends against: leaving the outcome only in the returned `AutopilotPassResult.reason` (in-process, lost after the call) so a burned cycle looks identical to "never ran".

7. When a task is skipped for a scheduling cycle because its sandbox prepare/preflight failed, the per-task orchestrator-events ledger for that task contains a durable record marking the prepare-skip and its cause. [command | held-out]
   verify: a test drives one orchestrate cycle where sandbox resolution raises for a picked task, then asserts the per-task events query for that task returns a prepare-skip record whose payload carries the failure text; assert the loop still releases the claim and keeps draining (unchanged behavior).
   defends against: emitting only the stream log line; recording success/landing for a task that never actually prepared.

8. When a work source truncates its listing at its page/item limit, or drops an item for carrying zero graders, the orchestrator-events ledger contains a durable record marking the truncation/drop and the source it came from. [command | held-out]
   verify: a test lists work from a source whose backing returns more than its page limit (and, separately, an item with zero graders), driven through the production source-construction path, then asserts a per-source events query returns a truncation record and a zero-grader-drop record each naming the source; assert the returned work sequence itself is unchanged (the records are a side channel).
   defends against: leaving `log=None` wired so the signals go nowhere and a 201-item board is indistinguishable from a green repo; faking the record from the test's own injected log rather than the production wiring.

9. When any of the newly-recorded stops above leaves a unit stopped or stranded, `fw status` lists that unit with its stop reason. [command | visible]
   verify: a test invokes the `status` command rendering (text and `--json`) over a store holding each new record kind and asserts the stopped/stranded unit appears with its reason — extending the existing `stranded:` surface rather than a separate view.
   defends against: persisting the record but never surfacing it, so it is queryable only by someone who already knows to look; rendering a count without the per-unit reason.

10. The shared stop-record shape is asserted by every producer-and-consumer together, not each in isolation. [command | held-out, verification-surface]
    verify: a composition holdout — one command grader that runs the full orchestrator + worktree + core suites as one (`scripts/check.sh`) so the per-run `LandingParked` `park_kind` vocabulary, the orchestrator-events stop-kind taxonomy, and the `fw status` reader are exercised against one another; a producer that emits a record shape the `fw status` reader or the taxonomy does not recognize fails here.
    defends against: a per-site record shape that passes its own narrow test but is unreadable by `fw status` or diverges from the shared taxonomy (the dominant cheat for a shared-invariant change).

Verification surface: the existing suite still passes; this feature adds new test files and new event/record kinds but relaxes or removes no existing check. Any check a task relaxes must be named with an equal-or-greater replacement (a removed assertion with none is a blocking defect). New stop-recording behavior is proven by a held-out check the implementing agent does not author against its own known inputs — each criterion's authoritative grade asserts a stored record read back from the store after the real stop path runs, out of the agent's reach.

## Decomposition Hint (for /fw-plan)
This splits along the two existing durable ledgers, because a stop either has a finalized run (per-run domain-event stream) or it does not (per-task orchestrator-events ledger). Size one task per ledger-and-call-site cluster; chain the `fw status` reader and the composition holdout after the producers.

- Layer **per-run park records** (core domain-event stream, `append_domain_event` / `LandingParked`): satisfies #1, #2, #3, #4. All four sites already have a `store` and a `run_id` in scope (the run finalized DONE); the shared invariant is the `LandingParked.park_kind` vocabulary — add the new kinds (held-out-gate, protected-paths, push-failed, submit-error) as one named set so every site emits the same shape and the `fw status` reader recognizes all of them. The worker sites reuse the existing `_record_landing_park` helper; the held-out-gate site (in the orchestrate loop) emits the same event onto the same per-run stream. This layer also satisfies finding #2 (held-out gate verdict persisted), because the park record IS the persisted verdict.
- Layer **per-task stop records** (orchestrator-events ledger, `list_task_events`): satisfies #5, #6, #7, #8. These stops have no run_id, so they cannot use the per-run stream; they belong on the per-task orchestrator-events append-only ledger that already records claim transitions. The shared invariant is a new orchestrator stop-event taxonomy (dangling-prerequisite, no-op-cycle, prepare-skip, source-truncation, zero-grader-drop) — define it once as a closed set the producers and the reader agree on. Append-only and same-transaction-where-a-state-change exists, mirroring the existing five-member claim-event taxonomy.
- Layer **status surface**: satisfies #9; depends on both record layers. Extends the existing `fw status` `stranded:` rendering to enumerate the new per-run park kinds and the new per-task stop kinds with their reasons.
- Layer **composition holdout**: satisfies #10; depends on every layer above. One seam grader running the combined suites so a producer whose record shape the reader or taxonomy does not recognize fails.

Shared invariants multiple layers assert against — name them so dependent tasks update together and no slice inherits a red suite:
- The `LandingParked.park_kind` string vocabulary (existing: `uncommitted-work`, `divergent-base`, `standing-verify`; adds: held-out-gate, protected-paths, push-failed, submit-error). Every per-run producer and the `fw status` reader share it.
- A new orchestrator stop-event kind taxonomy (closed set: dangling-prerequisite, no-op-cycle, prepare-skip, source-truncation, zero-grader-drop) on the orchestrator-events ledger; every per-task producer and the `fw status` reader share it.

## Decisions Log (ADR-style: immutable, supersede — never edit)

### D-1: Two ledgers, keyed by whether the stop has a finalized run  (Status: Accepted)
- Context: A stop either happens after a run finalized DONE (landing/submit, has a run_id) or before/without a run (graph validation, autopilot cycle, sandbox prepare, source listing — no run_id). Domain events require a run_id by construction (`_DomainEventBase.run_id`).
- Decision: Per-run stops record a `LandingParked` on the core per-run domain-event stream (reusing the existing surface `fw status` already reads); per-run-less stops record a new stop-event kind on the per-task orchestrator-events append-only ledger (reusing the existing claim-transition ledger surface).
- Rejected: One new universal "stops" table — rejected because it duplicates two existing append-only ledgers and forces `fw status` to read a third surface; a synthetic placeholder lifecycle/run_id for pre-run stops — rejected because it pollutes the lifecycle state machine and the run catalog with non-runs.
- Consequences: Two record shapes, not one; the `fw status` reader must union both. Accepted because each reuses a ledger that already exists with the right durability and access pattern (docs/data-taxonomy.md Ledger/Event class).

### D-2: Records are audit-witnesses; they never change WHEN work stops  (Status: Accepted)
- Context: The phase north star is visibility, not behavior. P5 owns re-driving.
- Decision: Every new record is emitted at the existing stop site with the existing control flow unchanged — same refusal, same skip, same suppressed submit, same returned reason. The record's fold is the identity (per-run: advances `version` only, run stays DONE; per-task: append-only ledger row).
- Rejected: Coupling the record to a new retry or a status transition — rejected as out of phase scope and a behavior change.
- Consequences: A grader that asserts the unchanged control-flow outcome (ready-set membership, claim release, returned task count, unchanged work sequence) sits alongside the record assertion, so a task cannot "fix" visibility by altering behavior.

### D-3: Append-only, same-transaction where a committed state change exists  (Status: Accepted)
- Context: The orchestrator-events ledger already guarantees one row per committed claim transition, written in the same transaction, append-only.
- Decision: Pre-run stop records follow the same contract: append-only (no update/delete), insertion-order, and written in the same store transaction as any state change they accompany (e.g. the prepare-skip's claim release). A stop with no accompanying state change is still appended exactly once per occurrence.
- Rejected: Deduping/coalescing recurring stops (e.g. a prepare-skip that recurs every cycle) into one row — rejected because the recurrence IS the signal P4 must expose; collapsing it hides a wedged task.
- Consequences: A recurring stop produces one record per cycle; the operator sees frequency, not just existence.

## Open Questions (accepted gaps)
None. Every criterion lowers to a `command` grader reading a stored record back from the store after the real stop path runs.

## Next Steps
Run `/fw-plan 00068-FEATURE-stop-observability` to compile these criteria into flywheel tasks and graders.

# fw-verify audit — 00069 Work re-driver (Phase 5)

Spec: `.flywheel/specs/00069-FEATURE-work-redriver.md` (14 criteria; #1-#13 held-out, #14 visible).
Staged tasks: `.flywheel/tasks/staged/05-work-redriver/` (8 tasks; STAGED, registrations sit until promotion).
Held-out root: `.flywheel/verification/held-out` (`[held_out] root`, git-ignored). Scratch: `.flywheel/verification/00069-work-redriver/` (git-ignored).

This phase's danger is silent, high-stakes policy failure: a re-driver that auto-clears a human gate, escalates unboundedly, walks silently to terminal FAILED, or spins forever. fw-verify blind-proves a discriminating oracle EXISTS for each of those policy-correctness behaviors. Authored before execute — the redriver implementation does not exist yet, so each oracle asserts a PROPERTY/MODEL RELATION over the DECLARED contract (a per-cycle/per-exhaustion decision function), never a read-off of code. The orchestrator (this agent) self-constructed the fence pack and authored directly: sound here because there is no implementation to leak from (pre-execute).

## Routing

| Crit | Route | Reason |
|---|---|---|
| #1 in-loop sweep finalizes a stranded lifecycle without a re-pick | SKIP | Store/claim STATE check (claim row gone, lifecycle finalized, task in ready set). fw-plan grades it with `test_redriver_lease_sweep.py` + the claims regression suite; deterministic, agent cannot game. |
| #2 a live (non-lapsed) claim is byte-identical before/after a sweep | SKIP | STATE invariant (row+version+status unchanged). Graded out-of-band by the claims suite; the double-claim hazard is a structural diff, not a behavior oracle. |
| #3 parked landing re-attempts up to bound; a cleared cause lands | **AUTHOR (boundedness)** | The bound + land-on-clear is the discriminating behavior; folded into the boundedness model (clearing-cause control arm). |
| #4 never-clearing landing -> exactly `bound` attempts then queue, no bound+1 | **AUTHOR (boundedness)** | Central anti-goal instance: exact-bound + route-then-stop discriminates unbounded / off-by-one / drop. |
| #5 first exhaustion escalates exactly once, not terminal FAILED | **AUTHOR (escalate-once)** | The headline D-A policy; a wrong impl that walks to FAILED or escalates forever is silent + high-stakes. |
| #6 second exhaustion -> queue `retries-exhausted-after-escalation`, never bare FAILED | **AUTHOR (escalate-once)** | Same oracle; the post-escalation routing + no-silent-terminal property. |
| #7 once-dangling task becomes eligible when prereq appears | SKIP | Graph/eligibility STATE check (B selected once A satisfiable). Graded by `test_redriver_prereq.py` + the graph suite; structural, not a hidden-oracle behavior. |
| #8 prereq missing past bound -> queue naming the prereq id, never dispatched | MANUAL | Behavior, but the boundedness shape is already proven generically (#4/#13); the prereq-id-naming + never-dispatch facet has no contract pin beyond "a reason naming the missing prerequisite id" that the generic boundedness oracle does not already cover. Routed to the existing manual/command gate rather than a near-duplicate brittle oracle. |
| #9 no-progress unit backs off + queues past bound, not re-attempted next cycle | SKIP->covered | The bounded-attempt + route-then-stop property is the boundedness oracle's shape; the "no-progress" trigger is a STATE definition fw-plan grades in `test_redriver_no_progress.py`. The dangerous part (unbounded spin) is killed by the boundedness oracle. |
| #10 AWAITING_APPROVAL never auto-cleared, surfaced `awaiting-approval` | **AUTHOR (human-gate-routing)** | THE most dangerous failure in the phase (D-E). A wrong impl that auto-approves/-rejects/-redrives is silent and defeats human authority. |
| #11 abort / budget-ceiling routed naming cause, never re-dispatched | **AUTHOR (abort-budget-routing)** | Same human-owns-gates invariant for the budget/abort ceiling; re-dispatch defeats the ceiling. |
| #12 single queue surface lists all routed kinds, machine-readable reason, no new silo | SKIP | "No new silo" + "one read returns all" is a STORE/STATE + schema assertion graded by `test_redriver_queue.py` and `test_redriver_aggregate.py` (no-new-table = structural diff). The machine-readable-reason facet is asserted inside every AUTHOR oracle above. |
| #13 every auto-retry/escalate/land path is bounded | **AUTHOR (boundedness)** | The central anti-goal; the boundedness model proves exact-bound-then-queue with no path able to loop unbounded. |
| #14 re-driver never forges a claim/status; routes through sanctioned APIs | SKIP | `visible` AST/grep + core-purity tests (`test_task_module_purity`, `test_lifecycle_module_purity`). Deterministic structural check fw-plan already wires; not oracle-able as a behavior. |

Authored **4** held-out oracles (orchestrator-direct, fence-pack from the declared contract only; no implementation existed to contaminate). All live git-ignored under `.flywheel/verification/00069-work-redriver/`.

## Oracle 1 — human-gate-routing (#10; #11 facet)

- Path: `.flywheel/verification/00069-work-redriver/human-gate-routing/oracle.py` (`verify(step)` factory) + `references/` + `mutants_check.py`.
- Form: property/model. `step(unit, cycle) -> {status, queue_entries}` folded over 5 cycles. P1: an awaiting-approval unit's status is ALWAYS exactly `awaiting_approval` (never moved to done/failed/ready/...). P2: a queue entry with `reason="awaiting-approval"` exists.
- Discrimination — 3/3 killed; correct passed (both flake runs):
  - `wrong_auto_approve` (status -> done after a cycle): KILLED on cycle 1 (the catastrophic auto-bypass).
  - `wrong_auto_redrive` (status -> ready): KILLED on cycle 0 (transient-failure bypass).
  - `wrong_never_queue` (status held but never surfaced): KILLED on P2 (silent intended stop).
- Flake screen: run-twice identical (PASS correct, 3/3 killed). Executable standalone (exit 0).

## Oracle 2 — escalate-once (#5, #6)

- Path: `.flywheel/verification/00069-work-redriver/escalate-once/oracle.py` (`verify(on_exhaustion)` factory).
- Form: property/model. `on_exhaustion(state) -> {action, status, queue_entries}` driven over exhaustions to a hard ceiling. P1: exactly one `escalate` action total. P2: ends in a queue entry, never a bare terminal `failed`. P3: reason `retries-exhausted-after-escalation`. P4: first exhaustion is not terminal `failed`.
- Discrimination — 3/3 killed; correct passed (both flake runs):
  - `wrong_straight_to_failed` (terminal FAILED on first exhaustion): KILLED on P1 (0 escalations = silent dead-end).
  - `wrong_escalate_forever` (escalate every exhaustion): KILLED on the unbounded-loop ceiling (hit ceiling 10 without queueing).
  - `wrong_silent_terminal_after_escalation` (escalate once then bare FAILED): KILLED on P2 (no queue entry after escalation).
- Flake screen: run-twice identical. Executable standalone (exit 0).

## Oracle 3 — boundedness (#13; #4 instance; #3 control)

- Path: `.flywheel/verification/00069-work-redriver/boundedness/oracle.py` (`verify(drive)` factory).
- Form: property/model. `drive(bound, cause_clears)` yields `attempt`/`queued`/`landed` events, pumped to a hard ceiling. Never-clearing arm: P1 exactly `bound` attempts; P2 exactly one `queued` with a machine-readable reason; P3 no attempt after the queue routing. Clearing arm: lands within bound, not force-queued.
- Discrimination — 3/3 killed; correct passed (both flake runs):
  - `wrong_unbounded` (loops forever): KILLED on the driver ceiling (22 attempts, never terminated).
  - `wrong_off_by_one` (bound+1 attempts): KILLED on P1 (4 attempts vs bound 3).
  - `wrong_drop_no_queue` (stop at bound, never queue): KILLED on P2 (0 queue routings = invisible strand).
- Flake screen: run-twice identical. Executable standalone (exit 0).

## Oracle 4 — abort-budget-routing (#11)

- Path: `.flywheel/verification/00069-work-redriver/abort-budget-routing/oracle.py` (`verify(step)` factory).
- Form: property/model. `step(unit, cycle) -> {dispatched, queue_entries}` over both `abort` and `budget-ceiling` stops, 4 cycles each. P1: `dispatched` is never True. P2: a queue entry whose reason names the cause (`abort` / `budget`).
- Discrimination — 3/3 killed; correct passed (both flake runs):
  - `wrong_redispatch` (re-drives after a backoff cycle): KILLED on P1 cycle 1 (ceiling defeat).
  - `wrong_unnamed_reason` (generic "stopped"): KILLED on P2 (reason names neither cause).
  - `wrong_drop` (never queues): KILLED on P2 (silent intended stop).
- Flake screen: run-twice identical. Executable standalone (exit 0).

Aggregate discrimination: **12/12 plausible-wrong references killed, 4/4 correct references passed, stable across two runs.**

## Registrations (sanctioned out-of-worktree channel)

Written to the git-ignored `[held_out] root` keyed by owning task id; absolute oracle paths; round-trip through `FilesystemHeldOutGraderSource.graders_for`:

- `.flywheel/verification/held-out/redriver-human-gate-routing.json` — 2 graders (human-gate-routing + abort-budget-routing oracles; #10 + #11).
- `.flywheel/verification/held-out/redriver-retry-escalation.json` — escalate-once oracle (#5/#6).
- `.flywheel/verification/held-out/redriver-discipline-and-aggregate.json` — boundedness oracle (#13/#4).

## non_goals fence (operator applies to each owning task)

Add to `redriver-human-gate-routing`, `redriver-retry-escalation`, `redriver-discipline-and-aggregate` (and any task whose criterion an oracle covers):

    non_goals: "Do not read or write under .flywheel/verification/"

## Honest limits (state plainly; do not oversell)

- **This stage proves blind that a discriminating oracle EXISTS and records the proof. It does NOT, by itself, grade the agent's real run.** Each oracle is a `verify(decision_fn)` model relation; its discrimination is proven against synthesized references. The execute-time gate on the agent's real work stays the task's OWN command graders + tests (the durable, CI-run guard fw-plan wired).
- **The registrations document the sanctioned out-of-worktree channel but are NOT yet a live behavioral gate.** These oracles assert against an injected model seam (the per-cycle/per-exhaustion decision function), which the unbuilt implementation does not yet expose by a pinned import path. As a bare command (`cwd = committed tree`) each oracle self-drives its bundled correct reference (exit 0, scratch-only ref absent from the worktree) — it proves the oracle is executable + discriminating, not that it grades the agent's redriver. Activating these as a true held-out gate requires the implementing tasks to expose the modelled decision seam (the redriver's per-cycle routing decision as an importable, model-conformant function) so the registered command can drive the REAL decision through `verify(...)`. That seam pinning is the integration step; until then the registration is the recorded channel, the proof is the durable artifact, and the live gate is the task's own suite.
- These are MODEL oracles over the declared policy contract, not over a concrete API the spec pins (the spec deliberately leaves the exact reason/event vocabulary to plan-time D-D reconciliation). They discriminate the POLICY (human-gate-never-cleared, escalate-once, exact-bound-then-queue, ceiling-never-re-dispatched); they do not pin the field names of the eventual queue schema.
- #8 routed to MANUAL (the prereq-id-naming + never-dispatch facet has no contract pin beyond the generic boundedness already proven); #1/#2/#7/#9/#12/#14 SKIP to deterministic command/state graders fw-plan already wired. No criterion ships a green-by-default holdout; no wrong reference was weakened to force a kill.

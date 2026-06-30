# Feature: Work re-driver (turn STRAND/DEAD-END into inputs, not endpoints)

## Outcome
Every stalled, stranded, exhausted, or dead-ended unit of work is automatically
re-driven or escalated **within a bounded budget**, and anything that cannot be
auto-recovered — including the intentional human gates — lands in a single
**visible human-review queue** instead of terminating silently. Concretely, in
the steady-state orchestrate loop:

- a worker killed mid-task has its lapsed lease actively swept and its stranded
  lifecycle finalized, so the task becomes eligible again *without* waiting for
  some other worker to happen to re-select that exact task;
- a stranded landing (standing-verify fail / divergent-base / gate / protected /
  push) is automatically re-attempted a bounded number of times, then queued;
- a task that exhausts its retry budget auto-escalates **once** (stronger model
  or re-decompose) and, if it still fails, appears in the queue with
  `reason=retries-exhausted-after-escalation` — never a silent terminal FAILED;
- a task whose prerequisite never materializes is re-driven when the prereq
  appears, and queued once a bound is crossed if it stays missing;
- a no-progress spin (never-passing phase verify, un-authorable autopilot repo)
  backs off after a bounded number of fruitless cycles and surfaces in the queue
  rather than burning agent cost forever;
- an intentional human stop (AWAITING_APPROVAL, intent=abort, budget ceiling) is
  **routed** into the same queue with its reason and is **never** auto-cleared.

The visible human-review queue is one durable, queryable surface built by
extending the existing orchestrator/domain event ledger (`LandingParked` and the
orchestrator `expired`/`released` events are already on it), not a new silo. The
net effect closes the north star for this phase: **autopilot never stops** — no
unit of work can reach a state from which nothing ever happens again.

## Background
Six classes of stopped work currently have no auto-recovery; each is a
silent endpoint today:

1. **Lazy-only lease reclaim, no sweep caller.** `sweep_expired_claims`
   (`_claims.py:1144`) is never called in production. Reclaim is opportunistic:
   a stranded `running` lifecycle is finalized only at orchestrate *entry*
   (`_recover_claimable_stranded`, `_orchestrate.py:460-502,825`), or
   incidentally when some worker re-selects that exact task. A worker that dies
   mid-task strands its lifecycle until that coincidence occurs.
2. **Stranded landings never auto-retry.** Terminal-DONE-but-unlanded runs
   (standing-verify fail `worker.py:609/659`, divergent-base `:635/685`, plus
   the P4-surfaced gate/protected/push parks) just sit; operator-only today.
3. **Retry exhaustion is a silent terminal dead-end.** `harness.py:1786-1810`
   walks an exhausted task straight to terminal `FAILED`;
   `lifecycle.consecutive_failed_runs()` (`lifecycle.py:257`) is computed but
   never consumed; no escalation is built on it.
4. **A dangling prerequisite is a permanent dead-end.** `_work_graph.py:155-163`
   records a `GraphValidationIssue` that orchestrate discards; the referencing
   task is never selected, with no give-up path.
5. **No-progress spins never give up to visible.** A never-passing `phase_verify`
   keeps a fully-DONE phase active forever (`worker.py:993`); an un-authorable
   autopilot repo re-runs full discovery every cycle (`_autopilot.py`).
6. **Intentional human gates can be bypassed if mishandled.** AWAITING_APPROVAL
   (`harness.py:3521-3542`), intent=abort -> FAILED (`harness.py:2229-2249`), and
   the budget ceiling -> FAILED (`harness.py:3806-3851`) are durable, *intended*
   stops. They must appear in the queue with their reason and must never be
   auto-cleared by the re-driver.

The tacit, load-bearing requirement an optimizing agent would otherwise miss:
the re-driver's job is **not** to make everything green. It is to guarantee
*forward motion or visibility* under a bound. The two failure modes that beat a
naive implementation are (a) the re-driver itself spinning forever (an unbounded
auto-retry is just a louder silent strand), and (b) the re-driver auto-clearing
an intentional human stop (an approval gate "resolved" by the machine defeats the
human's authority). Both are anti-goals, graded directly.

This phase **consumes** Phase 4's durable stop records (it re-drives what P4 made
visible) and runs the re-driven work over the Phase 1-2 containment/deadline
safety. Those upstream surfaces are assumed prerequisites (see Decisions D-D),
not built here.

## Scope
### In scope
- An **active expired-lease sweep** wired into the steady-state orchestrate loop
  (not only at entry): lapsed claims are released and their stranded lifecycles
  finalized so the task returns to an eligible state, on a bounded periodic
  cadence like the existing source reconciler.
- A **bounded stranded-landing re-driver**: a run parked unlanded
  (`LandingParked`) is automatically re-driven (re-claim -> re-run ->
  re-rebase + re-run command/standing graders + re-attempt land) up to a bound,
  then routed to the queue.
- A **bounded retry-exhaustion escalation** (Decision D-A): on exhaustion the
  task auto-escalates **once** (a stronger-model / re-decompose attempt), and on
  a second exhaustion is routed to the queue with a recorded
  `reason=retries-exhausted-after-escalation` — never a silent terminal FAILED.
- A **bounded dangling-prerequisite re-driver**: while the missing prerequisite
  is absent the task stays ineligible (today's behavior); when it later appears
  the task is re-driven; if it stays missing past a bound the referencing task is
  routed to the queue (consuming P4's dangling-prereq record).
- A **bounded no-progress back-off**: after a bounded number of cycles that make
  no progress on a unit (phase verify never passes; autopilot repo never
  authors), the condition backs off and is routed to the queue instead of
  re-running forever.
- **Human-gate routing**: AWAITING_APPROVAL, intent=abort, and budget-ceiling
  stops are surfaced into the same queue with their reason; the re-driver never
  transitions them out of their stop state.
- A **single visible human-review queue**: one durable, queryable surface
  (extending the existing event ledger / status read) listing every unit that
  could not be auto-recovered, each carrying a machine-readable `reason`.

### Out of scope
- Changing core's lifecycle transition rules or making `flywheel_core.task` /
  `flywheel_core.lifecycle` impure. The re-driver requests re-eligibility through
  sanctioned orchestrator state; the harness still owns every transition.
- Inventing a new persistence silo for the queue. It extends the existing
  orchestrator-events / domain-event ledger and the existing status read.
- Deciding *what* a stronger model or re-decomposition is, beyond selecting it —
  the escalation reuses existing model/decompose configuration; this phase does
  not design a new escalation policy engine.
- Auto-resolving any intentional human gate (approve/reject an approval, clear an
  abort, raise a budget). Those remain human-only.
- Speculative/batch landing or any change to the merge-flock, rebase, or
  protected-paths mechanics (owned by 00061/00064).

### Must not regress
- **Core purity and the git-free line.** `flywheel_core.task`/`lifecycle` stay
  pure; commit/base knowledge stays in the strategy; the generic orchestrator
  stays git-agnostic.
- **Harness owns transitions; agent claims untrusted.** The re-driver never
  forges an agent claim or a status; it re-drives by re-claiming/finalizing
  through sanctioned store APIs, exactly as `_recover_claimable_stranded` does.
- **Exactly-once execution, serialized landing, orphan-free shutdown** (00060).
- **Existing entry-time recovery** (`_recover_claimable_stranded` at orchestrate
  entry) keeps working; the in-loop sweep is additive, not a replacement.
- **A still-live claim is never swept**; a task a peer is actively running is
  never reclaimed.

## Success Criteria
Each criterion is atomic, grades an observable end-state, and carries its grader
type, visibility, the exact check, and the gaming move it forecloses. `/fw-plan`
lowers each one to a command / transcript / rubric / manual grader.

1. While the orchestrate loop is running, when a claim's lease lapses with its
   lifecycle stuck in a running/validating state, the sweep finalizes that
   lifecycle and the task returns to an eligible (re-selectable) state without
   any worker re-selecting that exact task first. [command | held-out]
   verify: drive a loop with an injected clock; create a lapsed claim plus a
   stranded `running` lifecycle for a task no other ready task references; run
   enough sweep cycles; assert the claim row is gone, the lifecycle is finalized,
   and the task appears in the ready/eligible set — with no intervening
   re-selection of that task id.
   defends against: implementing reclaim as same-task re-selection again (the
   lazy path), so a task no one re-selects strands forever; assert eligibility
   arises from the sweep, not from a re-pick.

2. When a still-live (non-lapsed) claim exists during a sweep cycle, the sweep
   leaves it and its lifecycle untouched. [command | held-out]
   verify: a claim with a future `lease_expires_at` and an in-flight lifecycle;
   run sweep cycles; assert the claim row, its version, and the lifecycle status
   are byte-identical before and after.
   defends against: a sweep that reaps by wall-clock alone and steals a task a
   live peer is actively running (the double-claim hazard).

3. When a run is parked unlanded (a `LandingParked` stop record exists), the
   re-driver automatically re-attempts landing up to a fixed bound, and a parked
   run whose underlying cause has cleared lands on a subsequent attempt.
   [command | held-out]
   verify: park a run via the strategy's landing-park path with a cause that
   clears (e.g. base no longer diverges); run re-drive cycles; assert the run
   re-attempts land and the change lands, and that the number of land attempts is
   <= the configured bound.
   defends against: an unbounded land-retry loop, and a re-driver that re-lands
   *without* re-running the rebase/standing graders against the exact base.

4. If a stranded landing's cause does NOT clear within the bound, then the run is
   routed to the human-review queue with a machine-readable reason naming the
   park cause, and no further automatic land attempts occur. [command | held-out]
   verify: park a run with a cause that never clears; run more than `bound`
   re-drive cycles; assert exactly `bound` land attempts were made, the run is
   present in the queue surface with a `reason` carrying its `park_kind`, and no
   `bound+1` attempt is made.
   defends against: a re-driver that keeps re-attempting a permanently-stranded
   landing forever (a louder silent strand), or that drops it without surfacing.

5. When a task exhausts its retry budget for the first time, the re-driver
   auto-escalates exactly once (a stronger-model / re-decompose attempt is
   dispatched) rather than terminating it. [command | held-out]
   verify: drive a task whose scripted agent fails every normal attempt; assert
   that after the first budget exhaustion an escalated attempt is dispatched
   (observable via the recorded escalation marker / distinct attempt), and that
   the task is NOT in terminal FAILED at that point.
   defends against: walking straight to terminal FAILED (today's silent
   dead-end), or escalating unboundedly (re-escalating on every exhaustion).

6. If an escalated task exhausts its budget a second time, then it is routed to
   the human-review queue with `reason=retries-exhausted-after-escalation` and is
   not silently recorded as a bare terminal FAILED. [command | held-out]
   verify: continue criterion #5's task so the escalated attempt also fails to
   exhaustion; assert it appears in the queue surface with
   `reason=retries-exhausted-after-escalation` and that exactly one escalation
   occurred (not a second escalation loop).
   defends against: the escalation itself becoming an unbounded retry loop, or
   the exhausted task vanishing into a terminal state no queue reads.

7. When a task referencing a missing prerequisite has that prerequisite later
   appear in the work source, the previously-ineligible task becomes eligible and
   is driven. [command | held-out]
   verify: a graph with task B requiring absent task A; run cycles (B stays
   ineligible); add A to the source; run cycles; assert B becomes eligible and is
   selected once A is satisfiable — the dangling issue no longer blocks it.
   defends against: a task with a once-dangling edge staying permanently
   ineligible even after the prerequisite materializes (the permanent dead-end).

8. If a prerequisite remains missing past a fixed bound (cycles or a window),
   then the referencing task is routed to the human-review queue with a reason
   naming the missing prerequisite id. [command | held-out]
   verify: task B requires absent A; run more than `bound` cycles without A
   appearing; assert B is in the queue surface with a `reason` naming the missing
   prerequisite id, and that B was never dispatched (no execution against a
   missing prereq).
   defends against: silently discarding the dangling-prereq issue forever (today)
   or, conversely, dispatching B against an unsatisfied prerequisite.

9. After a fixed bound of consecutive cycles that make no progress on a unit
   (e.g. a phase whose verify never passes, or an autopilot repo that never
   authors a task), the re-driver backs that unit off and routes it to the
   human-review queue with a no-progress reason, instead of re-running it the
   next cycle. [command | held-out]
   verify: simulate a unit that yields no progress every cycle; run more than
   `bound` cycles; assert the unit is present in the queue surface with a
   no-progress reason and that it is NOT re-attempted on the cycle after the bound
   is crossed (a backed-off marker / absence from the active set).
   defends against: a fully-DONE-but-never-verifying phase or an un-authorable
   repo burning agent cost forever with no give-up-to-visible.

10. While a lifecycle is in AWAITING_APPROVAL, the re-driver leaves its status
    unchanged and surfaces it in the human-review queue with
    `reason=awaiting-approval`. [command | held-out]
    verify: a lifecycle parked AWAITING_APPROVAL with a pending manual gate; run
    re-drive/sweep cycles; assert the status is still AWAITING_APPROVAL after the
    cycles (no auto-approve, no auto-reject) and the unit is in the queue with
    `reason=awaiting-approval`.
    defends against: the re-driver "resolving" an approval gate by transitioning
    it onward — auto-bypassing the human's authority (the most dangerous failure
    in this phase).

11. When a run ended via intent=abort or a budget-ceiling breach, the re-driver
    routes it to the human-review queue with a reason naming the stop cause and
    does not auto-retry or auto-clear it. [command | held-out]
    verify: produce one abort-terminated run and one budget-ceiling-terminated
    run; run re-drive cycles; assert each appears in the queue with a reason
    naming its cause (abort / budget-ceiling) and that neither is re-dispatched.
    defends against: the re-driver treating an intentional human/budget stop as a
    transient failure and re-driving it (defeating the intended ceiling).

12. The human-review queue is a single queryable surface: every unit routed to it
    by criteria 4, 6, 8, 9, 10, 11 is listable from one read, each entry carrying
    its task/run identity and a machine-readable `reason`. [command | held-out]
    verify: drive a scenario producing one unit of each routed kind; assert a
    single queue read returns all of them, each with a distinct machine-readable
    `reason`, and that the surface is the existing event/status ledger (no new
    silo table introduced).
    defends against: scattering escalations across N ad-hoc surfaces so no single
    operator view shows "everything that needs a human," or inventing a parallel
    silo that drifts from the canonical state.

13. Every automatic re-drive, escalation, and land-retry path is bounded: no
    unit is re-attempted more than its configured bound before it is queued.
    [command | held-out]
    verify: an aggregate test over the re-drive paths (#3, #5, #9 and the land
    loop) asserting that for a never-clearing cause the count of automatic
    attempts equals the configured bound and a queue entry is produced — proving
    no path can loop unbounded.
    defends against: the re-driver itself becoming the new infinite spin — the
    central anti-goal of this phase.

14. The re-driver never transitions a lifecycle by forging an agent claim or
    status; re-eligibility is requested only through sanctioned store/claim APIs.
    [command | visible]
    verify: the core purity tests
    (`test_task_module_purity`, `test_lifecycle_module_purity`) stay green, and a
    grep/AST check confirms the re-driver paths route through the existing
    claim/finalize/store APIs (as `_recover_claimable_stranded` does) rather than
    writing a status or synthesizing an agent envelope.
    defends against: the re-driver short-circuiting the harness's ownership of
    transitions — re-eligibility by forging state instead of requesting it.

Verification surface: unchanged. This feature adds an in-loop sweep, a re-driver,
and a queue read; it does not modify the tests, grading commands, CI config,
fixtures, or any machinery that decides whether OTHER changes are correct. The
existing orchestrator/worktree/core suites are the regression oracle for criteria
2 and 14; no existing check is relaxed, removed, or skipped. The held-out proofs
(#1-#13) assert observable lifecycle/claim/queue state the implementing agent
cannot pre-compute from known inputs.

## Decomposition Hint (for /fw-plan)
Split along these layers; chain with prerequisites so no slice inherits a red
suite. The **queue surface is the shared invariant** every routing layer asserts
against — define it first and have every later layer write to it.

- Layer **queue-surface** (the shared invariant): define the single durable,
  queryable human-review-queue read by extending the existing orchestrator-event
  / domain-event ledger (`LandingParked`, `expired`/`released`, and a routed
  `escalation`/`queued` event carrying a machine-readable `reason`) plus the
  existing status read. No new silo. Provides the surface criteria 4, 6, 8, 9,
  10, 11, 12 all write to and 12 reads. Foundational — every routing layer
  depends on it.
- Layer **lease-sweep**: wire `sweep_expired_claims` + stranded-lifecycle
  finalize into the steady-state orchestrate loop on a bounded cadence (mirrors
  `_source_reconcile_loop`), preserving the live-claim safety of
  `_recover_claimable_stranded`. Satisfies #1, #2. Depends on queue-surface only
  for the "still-stranded after bound" routing if any; otherwise independent.
- Layer **landing-redriver**: consume `LandingParked` records; re-claim ->
  re-drive -> re-rebase + re-run graders + re-attempt land, bounded; route to
  the queue on bound. Satisfies #3, #4 (and contributes to #13). Depends on
  queue-surface; depends on lease-sweep (a stranded landing must be reclaimable).
- Layer **retry-escalation**: consume `consecutive_failed_runs` / exhaustion at
  the orchestrator seam; escalate once (stronger model / re-decompose), queue on
  second exhaustion. Satisfies #5, #6 (and #13). Depends on queue-surface.
- Layer **prereq-redriver**: consume the dangling-prereq issue
  (`_work_graph.py:155-163`); re-drive when the prereq appears, queue past a
  bound. Satisfies #7, #8 (and #13). Depends on queue-surface.
- Layer **no-progress-backoff**: count consecutive no-progress cycles per unit;
  back off and queue past a bound. Satisfies #9 (and #13). Depends on
  queue-surface.
- Layer **human-gate-routing**: surface AWAITING_APPROVAL / intent=abort /
  budget-ceiling stops into the queue with their reason WITHOUT transitioning
  them. Satisfies #10, #11. Depends on queue-surface.
- Layer **transition-discipline guard** (cross-cutting): the purity + sanctioned
  -API check. Satisfies #14. Depends on whichever routing layers exist (folds
  naturally into each, but stated once so the invariant is graded as a whole).

Shared invariants multiple layers assert against: (a) the queue surface and its
machine-readable `reason` vocabulary (every routing layer writes the same shape);
(b) the bound semantics — every auto-retry/escalate/land path reuses a single
bounded-attempt notion so #13 can hold across all of them; (c) the
harness-owns-transitions rule and the git-free line (#14). Name these so
dependent tasks update together and no slice ships a queue entry the others
cannot read.

## Decisions Log (ADR-style: immutable, supersede — never edit)

### D-A: On retry exhaustion, auto-escalate ONCE then route to a visible queue — never silent terminal FAILED  (Status: Accepted)
- Context: today an exhausted task walks straight to terminal FAILED
  (`harness.py:1786-1810`) and `consecutive_failed_runs` is computed but unused;
  the north star is "autopilot never stops," so a silent terminal endpoint is
  the exact failure mode being removed. | Decision: on first exhaustion, escalate
  exactly once (a stronger model or a re-decomposition attempt); on a second
  exhaustion, route the task to the visible human-review queue with
  `reason=retries-exhausted-after-escalation`.
- Rejected: leaving terminal FAILED as-is (the silent dead-end); escalating on
  every exhaustion (an unbounded loop — the re-driver becomes the new infinite
  spin). | Consequences: a pathological task consumes its normal budget plus one
  escalation, then becomes visible — bounded and loud, matching 00060/00061
  posture.

### D-B: Timeouts / deadlines default-on  (Status: Accepted)
- Context: re-driven work must run under a deadline so a re-attempt cannot itself
  hang forever; Phase 2 ships the deadline mechanism. | Decision: the bounded
  re-drive paths run their attempts under Phase 2's default-on deadlines; this
  phase does not introduce a separate timeout knob.
- Rejected: opt-in deadlines (a hung re-attempt is indistinguishable from a
  stalled one — re-introduces the silent-spin failure). | Consequences: every
  re-driven attempt inherits a deadline; this phase assumes that mechanism from
  Phase 2 (see D-D).

### D-C: Respawn / re-drive happens under the budget  (Status: Accepted)
- Context: a re-driver that re-spawns work outside the cost ceiling can outspend
  the very budget gate it is supposed to respect. | Decision: every automatic
  re-drive/escalation/land-retry runs under the existing per-run budget ceilings
  (specs 00039/00042); a re-attempt that breaches a ceiling is itself a routed
  stop (criterion #11), not a free retry.
- Rejected: re-driving outside the budget (defeats the ceiling and contradicts
  human-owns-gates). | Consequences: the re-driver cannot create unbounded spend;
  budget breaches during re-drive surface in the queue like any other.

### D-D: Phases are sequential; this phase CONSUMES Phase 4's stop records and runs over Phase 1-2 safety  (Status: Accepted)
- Context: P4 makes durable stop records visible; P5 re-drives them. The re-driven
  work must run safely (P1 containment, P2 deadlines). The reliability program is
  P1 Containment -> P2 Deadlines -> P3 Transient resilience -> P4 Observability ->
  P5 Work re-driver -> P6 Supervision -> P7 Resource hardening. | Decision: this
  spec treats P4's stop-record surface and P1-2 safety as **assumed
  prerequisites** declared at plan time, not re-implemented here; the queue
  EXTENDS P4's ledger rather than minting a parallel one.
- Rejected: re-deriving stop records inside this phase (duplicates P4 and risks a
  second drifting surface); building the queue as a new silo (contradicts the
  "single visible surface" outcome). | Consequences: tasks here carry cross-phase
  prerequisite assumptions; if P4/P1-2 land differently the queue/event field
  names inherited here are the integration seam to reconcile.

### D-E: Human gates are ROUTED, never bypassed  (Status: Accepted)
- Context: AWAITING_APPROVAL, intent=abort, and budget ceilings are durable
  *intended* stops; auto-clearing any of them defeats human authority. | Decision:
  the re-driver surfaces each into the queue with its reason and never transitions
  it out of its stop state; only a human resolves them.
- Rejected: treating an abort or a pending approval as a transient failure to
  re-drive (the catastrophic bypass). | Consequences: these units sit visibly in
  the queue until a human acts; the re-driver provably never moves them
  (criteria #10, #11 grade the status is unchanged).

## Open Questions (accepted gaps)
- None blocking. Every criterion lowers to a `command` grader against an injected
  clock and the existing store/event/status reads. The cross-phase field names
  inherited from P4 (the exact `reason`/event vocabulary of the stop records) are
  an integration seam, not an un-gradeable criterion: this spec grades that a
  machine-readable `reason` exists and is queryable from one surface, leaving the
  exact P4 string to plan-time reconciliation (D-D).

## Next Steps
Run `/fw-plan 00069-FEATURE-work-redriver` to compile these criteria into
flywheel tasks and graders.

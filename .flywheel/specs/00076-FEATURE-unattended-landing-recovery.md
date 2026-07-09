# Feature: Unattended landing recovery

## Outcome
A DONE run whose branch cannot fast-forward or cleanly rebase onto the phase base — including approval-parked runs and merge-bearing/divergent-base branches — still lands unattended through the full out-of-band verification bar, or parks loudly with its branch preserved. Automation never deletes verified work.

## Background
The 2026-07-08 overnight run stranded two merge-bearing branches: the landing re-driver's only recovery rung is a rebase that aborts on conflict, and approval-parked runs never received an inline landing at all, so the operator hand-landed via a detached worktree, a merge, the full grader/verify ladder, and a compare-and-swap ref update (proposal P4, `.flywheel/proposals/overnight-2026-07-08.md`). The tacit bar surfaced in the interview: recovery must mechanize exactly that manual recipe — land through the same proofs, and when it cannot, preserve the work and say so. The repo invariant binds every rung: nothing lands that was not verified against the exact base it lands on.

## Scope
### In scope
- The merge-strategy landing path: the worktree submitter's parked-branch recovery and the landing re-driver.
- Post-approval landing for runs parked awaiting approval.
- A merge-fallback recovery rung and a bounded agentic conflict-resolution rung above it.
- Park surfacing and ledger records for every recovery outcome.
### Out of scope
- The PR submit strategy (GitHub owns landing there) and the container backend's own mechanics (it wraps the submit strategy and inherits this behavior).
- Consumption of approve/reject commands under the postgres store (phase `12-postgres-control-plane` owns that; this spec begins where a consumed approval leaves off).
- Changes to the held-out gate's internals (specs 00050/00051/00057).
### Must not regress
- The existing clean-FF and clean-rebase landing rungs, including post-rebase command-grader re-runs.
- `[submit] verify` (spec 00064) running on both land paths.
- Bounded re-drive semantics (spec 00069): witnesses, bounds, exactly-once human-review queueing.
- Retry-reuse of parked worktrees for non-DONE runs, including its discard-on-failed-rebase behavior.

## Success Criteria
Each criterion is atomic, grades an observable end-state, and carries its grader
type, visibility, the exact check, and the gaming move it forecloses.

1. When a DONE run's branch fails to rebase onto the current base, the landing machinery shall land it via a merge-based rung whose result leaves the branch's commits ancestors of the updated base ref. [command | held-out]
   verify: in a harness repo, a DONE run with a merge-bearing branch and an advanced base (rebase fails); after the re-driver pass, `git merge-base --is-ancestor <branch-head> <base>` exits 0 and the base ref advanced.
   defends against: marking the run landed in the ledger while the commits never become reachable from the base.

2. If a recovery landing candidate (merge-fallback or agent-resolved) fails any of its task command graders, `[submit] verify`, or a declared held-out gate against the candidate tree, then the landing machinery shall leave the base ref unmoved. [command | held-out]
   verify: same harness with a grader that fails on the merged tree; assert the base ref is byte-identical before and after the pass and no landing event is recorded.
   defends against: landing unverified content by skipping the re-verify rungs on the new path.

3. If any recovery step fails for a DONE run, then the landing machinery shall preserve the branch ref and its parked worktree and surface the park with a queryable park kind on the run's ledger and in `flywheel status`. [command | held-out]
   verify: induce failure at each rung (merge conflict with agent rung disabled/exhausted, failed re-verify); assert the branch ref still resolves, the worktree directory exists, and the status surface lists the run with its park kind.
   defends against: satisfying "no stranded branches" by discarding them — the current discard-on-failed-rebase generalized to DONE work.

4. When the merge-fallback rung itself conflicts, the landing machinery shall run a bounded agent session whose resolved tree, if produced within bounds, enters the same rungs as criteria 1-2 before landing. [command | held-out]
   verify: harness with a genuinely conflicting branch; assert the outcome is either a landed base with the ancestor relation and re-verify receipts, or the preserved-and-surfaced park of criterion 3; assert the session's recorded turn/wall usage respects its configured bound.
   defends against: unbounded landing-time agent loops; agent-resolved content landing without the out-of-band bar.

5. When a pending approval is consumed for a run parked awaiting approval, the landing machinery shall land that run's branch through the standard landing ladder without further operator action. [command | held-out]
   verify: harness where a run parks AWAITING_APPROVAL, an approve is enqueued and consumed; assert the base ref advances to include the branch (ancestor relation) with grader/verify receipts, with no operator-issued git commands.
   defends against: DONE-without-landing terminal states that depend on session recycling or manual landing.

6. The landing machinery shall record every recovery landing on the run's ledger naming the rung that landed it (rebase, merge-fallback, or agent-resolved). [command | visible]
   verify: after the criterion-1 and criterion-4 harness runs, a ledger/decision-record query returns the landing rung for each run.
   defends against: recovery paths that land work with no auditable trace distinguishing them from clean lands.

7. If recovery for the same run keeps failing, then the re-driver shall stop attempting it at its existing bound, leaving the criterion-3 park in place. [command | visible]
   verify: harness with a permanently failing re-verify; assert attempt count stops at the bound and the park record persists.
   defends against: retry storms burning agent cost and wall-clock on the landing path.

8. (verification-surface) When this feature's changes land, the repo's full gate shall pass with every existing landing-path test intact or replaced by a named equal-or-stronger check. [command | held-out]
   verify: `scripts/check.sh` exits 0; the diff removes or weakens no existing assertion covering the FF/rebase rungs, `[submit] verify`, or re-driver bounds without a named replacement.
   defends against: passing the new rungs by deleting the tests that constrain the old ones.

Verification surface: changed — this feature adds rungs to the machinery that decides what lands. The existing suite must still pass; any relaxed, removed, or skipped check must be named with an equal-or-greater replacement (a removed assertion with none is a blocking defect); new rung behavior is proven by out-of-band checks the implementing agent did not author against its own known inputs (criteria 1-5 held-out).

## Decomposition Hint (for /fw-plan)
- Merge-fallback rung in the parked-branch recovery/re-driver seam: satisfies #1, #2, #3, #6, #7.
- Approval-parked landing wiring (orchestrator-to-submitter seam): satisfies #5; independent of the merge rung except for shared receipts, so it can run in parallel.
- Agentic resolution rung: satisfies #4; depends on the merge-fallback rung (it escalates from that rung's conflict outcome).
Shared invariants: the park-kind vocabulary (new kinds must be queryable wherever park kinds surface today) and the landing-rung field on the run's ledger record — dependents of either update together.

## Decisions Log

### D-1: Recovery ladder is merge-fallback first, then a bounded agentic rung  (Status: Accepted)
- Context: rebase-only recovery strands merge-bearing/divergent-base branches; operator chose the escalation shape. | Decision: FF -> rebase -> merge-fallback -> bounded agent resolution -> park.
- Rejected: prepare-and-escalate only (landing stays attended, the overnight failure recurs as wait-for-human); agentic-only (skips the cheap deterministic win and maximizes the new trust surface). | Consequences: agent cost enters the landing path, strictly bounded and only after deterministic rungs fail; two new rungs to test and audit.

### D-2: Recovery rungs re-verify at rebase parity  (Status: Accepted)
- Context: the merged/resolved tree differs from what any grader graded. | Decision: task command graders + `[submit] verify` + the declared held-out gate re-run against the candidate tree; rubric judges do not re-run at landing.
- Rejected: full ladder incl. rubric (puts the flakiest, priciest signal on the landing path); command-graders-only (regresses the spec-00064 standing gate on this path). | Consequences: a recovery land is exactly as trusted as a rebase land; genuinely subjective regressions introduced by a merge remain uncaught at landing, as they are today on the rebase rung.

### D-3: Automation never deletes a DONE branch  (Status: Accepted)
- Context: the current recovery discards a parked worktree on rebase failure; for verified work that is unrecoverable data loss. | Decision: every recovery failure for a DONE run preserves branch and worktree; discard remains acceptable on the non-DONE retry-reuse path.
- Rejected: current behavior everywhere (the overnight hand-recovery would have been impossible); never discard anything (clutters retention for worktrees whose task simply re-runs). | Consequences: failed recoveries accumulate parked worktrees until retention or an operator acts; retention windows must tolerate that.

### D-4: The agentic rung is bounded by configured turn/wall limits with at most one resolution attempt per re-driver pass  (Status: Accepted)
- Context: unbounded agent sessions on the landing path are a cost and reliability hazard; operator delegated the bound shape. | Decision: the session carries explicit turn/wall bounds (default values chosen at plan time alongside the phase-12/14 config work); exhaustion parks per criterion 3 and counts toward the criterion-7 bound.
- Rejected: unbounded sessions (retry-storm risk observed elsewhere in the overnight run); zero attempts (collapses back to prepare-and-escalate). | Consequences: some resolvable conflicts park anyway when bounds are tight; the bound values are config, not spec.

## Open Questions
None.

## Next Steps
Run `/fw-plan 00076-FEATURE-unattended-landing-recovery` to compile these criteria into flywheel tasks and graders.

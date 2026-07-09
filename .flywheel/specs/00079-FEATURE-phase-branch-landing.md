# Feature: Phase-branch landing with a phase PR at archive

## Outcome
With `[submit] strategy = "phase"`, tasks land continuously onto a per-phase
integration branch (`flywheel/phase/<NN-name>`) with per-task verification
unchanged (FF + rebase-reverify + standing verify + protected paths + held-out
gate); when a phase completes its exit gates, the worker pushes the phase branch
and opens exactly one PR onto the true base with every task's grader receipts and
held-out verdict aggregated in the body; the phase archives only after that PR
merges. Verification granularity stays per-task; comprehension granularity becomes
per-phase, and one `git revert -m 1 <merge>` reverts a whole phase.

## Background
An unattended run lands dozens of FF commits onto one base: verified per-task but
with no human-comprehensible review unit — branches deleted, CI post-hoc, no
aggregate checkpoint between "task landed" and "operator wakes up". The phase
(which already has a spec, an exit verify gate, and an archive event) is the
natural review unit, and it currently leaves no git-side artifact. Today the
landing base is resolved once per worker process (`[submit] base` is static) and
the `phase_base` attribute is a misnomer; phase identity is already derivable at
submit time from the task file. Block-on-merge extends the existing trust ladder
upward: nothing lands that was not verified against the exact base it lands on,
and now nothing archives that was not comprehended by a human. Review debt is
deliberately visible as unarchived phases — that backpressure is the feature.

## Scope
### In scope
- A third registered submit strategy (`phase`) selected via `[submit] strategy`,
  landing each task onto an integration branch derived from the task's phase and
  created from the current true base on first use.
- Task worktrees for a phase branching off that phase's integration branch.
- Archive predicate extension: all tasks landed on the phase branch (spec 00077
  semantics, retargeted) AND the phase branch tip is an ancestor of the true base
  (the PR merged).
- Phase PR open/refresh at phase completion with aggregated per-task receipts and
  held-out verdicts; the phase-exit `[phase] verify` gate evaluated against the
  phase-branch tree.
- Cross-phase prerequisite hold: no task builds on work that is not reachable
  from its own landing base.
- Predicate parity across every archival caller (worker daemon, `flywheel
  archive` CLI, TUI).
### Out of scope
- PR stacking (branching phase N+1 off phase N's unmerged branch and retargeting
  after merge) — explicitly deferred; v1 phases are independent.
- Any automated phase-branch freshness/rebase daemon — keeping an open phase PR
  mergeable against an advancing base is the merge queue's job (team-mode spec
  00081 is the designed pair).
- Merging the phase PR from inside the loop — a human (or the merge queue) merges;
  the worker only opens and refreshes.
- The autopilot intake queue's landing posture (`[autopilot] landing`) and its
  perpetual `autopilot` phase directory — never phase-PR'd in v1.
- Changes to the `merge` and `pr` strategies.
### Must not regress
- `merge` and `pr` strategy behavior byte-identical; absent `[submit] strategy`
  still means `merge`.
- Held-out gate timing (pre-submit, strategy-agnostic) — a FAIL verdict still
  suppresses submit before any strategy runs, so no ungated diff can reach a
  phase branch or its PR.
- Spec 00077 landed-not-merely-DONE archival semantics, strand surfaces, and
  resolution attribution.
- The loop-path archive gate and `.loop-base` materialization.
- Protected paths refuse landing on every strategy.

## Success Criteria
Each criterion is atomic, grades an observable end-state, and carries its grader
type, visibility, the exact check, and the gaming move it forecloses.

1. When a DONE task lands under the phase strategy, the landing shall advance
   only the task's phase integration branch (created from the then-current true
   base on the phase's first landing) and the true base shall be unchanged.
   [command | held-out]
   verify: harness lands two tasks from one phase; the phase branch contains both
   changes in landing order, and the true base SHA equals its pre-run value.
   defends against: a rename-only "phase strategy" that still lands on the base.

2. When the phase branch advanced under a finished task, the task branch shall be
   rebased once, its command graders re-run, and the standing `[submit] verify`
   command (when set) run against the exact tree about to land, on both the
   clean-FF and rebase paths; a failure shall park with the existing park kind
   vocabulary and leave the phase branch unchanged. [command | held-out]
   verify: harness with (a) a base-advanced landing whose re-verify passes and
   standing verify fails — parks `standing-verify`, phase branch unchanged; (b) a
   protected-path diff — parks `protected-paths`.
   defends against: the phase path becoming a fast lane that skips the verify
   ladder the merge path enforces.

3. When every task in a phase is DONE and landed on the phase branch and the
   loop-path gate and `[phase] verify` — evaluated against the phase-branch tree,
   not the operator's checkout — pass, the worker shall push the phase branch and
   ensure exactly one open PR onto the true base whose body contains, for every
   task in the phase, that task's grader receipts and its held-out gate verdict.
   [command | held-out]
   verify: harness with a stubbed gh runner and two tasks (one with a held-out
   PASS, one NO_GATE): PR create is invoked once with head=phase branch,
   base=true base; the body contains one section per task id with its receipt
   rows and held-out outcome; a second sweep refreshes (edit) rather than
   duplicating; a failing `[phase] verify` against the phase-branch tree opens no
   PR.
   defends against: a decorative PR body (hardcoded or empty receipts); per-task
   PRs relabeled as the phase unit; running the phase gate against a checkout
   that does not contain the phase's work.

4. While the phase branch tip is not an ancestor of the true base (the phase PR
   is unmerged), the archive sweep shall leave the phase in `active/` with the
   open PR surfaced as the blocking reason, and shall not advance the true base
   itself. [command | held-out]
   verify: harness sweeps repeatedly with the PR open: the phase directory stays
   under `active/`, the status/sweep surface names the PR as the block, and the
   true base SHA is unchanged (the worker performed no local merge).
   defends against: the daemon "helpfully" merging its own PR to unblock itself,
   and against silent non-archival with no visible reason.

5. When the phase branch tip becomes an ancestor of the true base, the next
   archive sweep shall archive the phase with existing `.loop-base`
   materialization intact. [command | held-out]
   verify: harness merges the PR with a merge commit; the next sweep moves the
   phase to `archive/` containing a `.loop-base` file, and `git revert -m 1` of
   that merge commit applies cleanly.
   defends against: an archive predicate that trusts a GitHub state string
   instead of git ancestry.

6. If the phase PR reports merged on the remote but the phase branch tip is not
   an ancestor of the true base (a squash or rebase merge), then the sweep shall
   leave the phase active and surface the merge-method mismatch as a distinct
   reason. [command | visible]
   verify: harness simulates a squash-merge (content applied, ancestry broken);
   sweeps leave the phase active and the surfaced reason names the mismatch, not
   a generic block.
   defends against: treating remote PR state as truth, and against the silent
   forever-block a squash-merge would otherwise cause.

7. While a task's prerequisite is DONE but the prerequisite's landed work is not
   reachable from the base the dependent task would branch from, the dependent
   task shall not be claimed, and the status surface shall name the blocking
   phase. [command | held-out]
   verify: two-phase harness — phase A's PR open, phase B holds a task whose
   prerequisite landed on A's branch: the task stays unclaimed with the hold
   reason naming phase A; after A's PR merges, the task becomes claimable in the
   next pass.
   defends against: silently building on a stale base (the deferred-stacking
   rider), and against implementing the hold as invisible starvation.

8. When the strategy is phase, archival shall honor the same predicate (all tasks
   landed AND phase PR merged) regardless of which surface invokes it — worker
   daemon, `flywheel archive` CLI, or the console. [command | held-out]
   verify: with the PR open in the harness, invoking the archive CLI directly
   leaves the phase active with the same reason the daemon reports.
   defends against: the existing CLI/TUI gate-skip (they call the sweep without
   gates today) becoming a one-command bypass of block-on-merge.

9. The `merge` and `pr` strategies shall behave byte-identically to today, and an
   absent or `merge`/`pr` `[submit] strategy` shall involve no phase-branch
   machinery. [command | held-out]
   verify: the existing submit/pr strategy suites pass unchanged; a harness run
   under `strategy = "merge"` creates no `flywheel/phase/*` integration branch.
   defends against: satisfying the new criteria by rewriting the shared landing
   path in a way that changes the shipped strategies.

10. (verification-surface) When this feature lands, the repo's full gate shall
    pass with every existing landing, archival, strand, and gate test intact or
    replaced by a named equal-or-stronger check. [command | held-out]
    verify: `scripts/check.sh` exits 0; the diff removes or weakens no existing
    assertion covering submit paths, the archive sweep, 00077 strand semantics,
    or the held-out gate without a named replacement.
    defends against: passing the phase-strategy criteria by deleting the tests
    that pin the strategies it must not regress.

Verification surface: changed — landing and archival are the loop's own
bookkeeping machinery. The existing suite must still pass; any relaxed, removed,
or skipped check must be named with an equal-or-greater replacement (a removed
assertion with none is a blocking defect); new behavior is proven by out-of-band
checks the implementing agent did not author against its own known inputs
(criteria 1-5, 7-9 held-out).

## Decomposition Hint (for /fw-plan)
- Strategy layer (per-request landing-base derivation, phase-branch creation,
  worktree branching off the phase branch, verify-ladder parity): satisfies #1,
  #2, #9.
- Phase-completion layer (phase gate against the phase-branch tree, push,
  PR open/refresh with aggregated receipts + held-out verdicts): satisfies #3;
  depends on the strategy layer.
- Archive-predicate layer (ancestry-based merged check, blocking reasons,
  mismatch surface, caller parity): satisfies #4, #5, #6, #8; depends on the
  phase-completion layer for the PR to exist.
- Scheduling layer (cross-phase prerequisite reachability hold + status surface):
  satisfies #7; independent of the archive-predicate layer.
Shared invariants: the phase-branch naming scheme and the "landed base for a
task" derivation (which branch a task verifies against and lands on) — the
strategy, the archive predicate, and the scheduler hold all consume it; the
phase-PR body's per-task section shape is consumed by the completion layer and
any status rendering. Dependent slices update together.

## Decisions Log

### D-1: A new registered strategy, not a mode on merge  (Status: Accepted)
- Context: backward compatibility requires merge and pr byte-unchanged; the
  `SUBMIT_STRATEGIES` registry is the existing dispatch seam. | Decision: ship
  `phase` as a third registry entry selected by `[submit] strategy = "phase"`,
  reusing the merge submitter's verify ladder with a per-request landing base.
- Rejected: a boolean/mode flag on the merge strategy (forks every branch of its
  decision tree and risks regressing the default path); a separate daemon that
  re-lands from the base (re-introduces unverified motion between two bases).
- Consequences: consumers select it like any strategy; the registry stays the
  single dispatch point for spec 00080's tier routing.

### D-2: Archival blocks on PR merge  (Status: Accepted — operator decision)
- Context: the review unit is only real if something enforces it; archive-on-
  PR-opened makes review advisory and lets unmerged PRs pile up drifting from
  base. | Decision: the phase archives only when its branch tip is an ancestor of
  the true base. Review debt is visible backpressure (unarchived phases in
  status), not hidden in a GitHub tab.
- Rejected: archive-on-PR-opened with github_review feeding outcomes back
  (recreates the original nightmare with nicer packaging); configurable semantics
  (doubles the spec/test surface before either has been operated).
- Consequences: unattended runs accumulate open phase PRs awaiting morning
  review; other phases keep landing on their own branches meanwhile.

### D-3: Merged-ness is git ancestry, not remote PR state  (Status: Accepted)
- Context: GitHub "merged" can mean merge-commit, squash, or rebase; only a
  merge commit preserves ancestry and gives `git revert -m 1`. | Decision: the
  archive predicate is "phase tip is an ancestor of the true base"; a
  merged-but-not-ancestor PR (squash/rebase merge) blocks with a distinct,
  visible mismatch reason. The repo's merge method for phase PRs must be
  merge-commit; the team-mode runbook (00081) owns documenting the setting.
- Rejected: trusting `gh pr view` state (a remote claim, and squash-merges would
  archive a phase whose branch history was discarded); patch-id equivalence
  scanning (quadratic, and still loses the revertible merge commit).
- Consequences: a squash-merged phase needs operator resolution; ancestry is
  checkable offline in the harness.

### D-4: Phases are independent in v1 — hold, don't stack  (Status: Accepted — operator decision)
- Context: a task in phase B whose prerequisite landed in unmerged phase A would
  otherwise branch from a base missing that work. | Decision: prerequisite
  satisfaction requires the prerequisite's work to be reachable from the
  dependent task's landing base; otherwise the task holds with a visible reason
  naming the blocking phase.
- Rejected: full PR stacking (branch B off A's branch, retarget on merge) —
  powerful but heavy machinery, explicitly deferred; silently branching from the
  stale base (builds unverified assumptions into the work).
- Consequences: cross-phase-dependent work serializes on human review of the
  earlier phase; the hold surface makes that queue visible.

### D-5: No phase-branch freshness daemon  (Status: Accepted — operator decision)
- Context: while a phase PR sits open, other phase PRs merge and the base
  advances; per-task rebase-reverify exists but nothing owns phase-PR-level
  re-verification. | Decision: v1 ships no bespoke rebase/refresh machinery;
  conflicts surface at merge time, and keeping open PRs mergeable against an
  advancing base is exactly what GitHub's merge queue does — this spec and the
  team-mode spec (00081) are a designed pair.
- Rejected: a phase-PR rebase daemon (bespoke machinery 00081 would obsolete).
- Consequences: an open phase PR can go stale; the operator (or the merge queue,
  once adopted) resolves it at merge time.

### D-6: The phase-exit gate runs against the phase-branch tree  (Status: Accepted)
- Context: today `[phase] verify` runs with the operator's checkout as cwd;
  under phase-branch landing that checkout does not contain the phase's work, so
  the existing invocation would gate the wrong tree. | Decision: the gate
  evaluates the phase-branch tree (a dedicated checkout/worktree of the phase
  branch), and the PR opens only on a green gate.
- Rejected: keeping cwd=repo-root (gates a tree without the work — always green,
  never meaningful); gating only in CI after PR-open (moves the phase gate
  post-hoc, the exact failure mode this program removes).
- Consequences: phase completion costs one whole-repo gate run against a phase
  checkout; standing verify already keeps per-landing greenness (operator chose
  per-landing verify in the interview).

### D-7: The autopilot queue is out of scope  (Status: Accepted)
- Context: the `autopilot` phase directory never completes (tasks are added
  continuously), so PR-at-archive can never fire for it. | Decision: autopilot's
  landing posture is unchanged in v1; risk-tiered routing (spec 00080) is the
  intended mechanism for autopilot work later.
- Rejected: time-boxed rollup PRs for autopilot batches (a second, different
  review unit inside the same feature). | Consequences: autopilot work keeps
  landing per its configured `landing` strategy.

## Open Questions (accepted gaps)
None.

## Next Steps
Run `/fw-plan 00079-FEATURE-phase-branch-landing` to compile these criteria into
flywheel tasks and graders.

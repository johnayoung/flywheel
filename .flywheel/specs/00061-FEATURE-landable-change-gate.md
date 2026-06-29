# Feature: Landable-change gate

## Outcome
A task cannot reach a *landed success* on an empty or uncommitted change. When an
agent finishes a run (graders green, `intent=verify`) but its sandbox has no
committed, non-empty change against the branch's landing base, the run is treated
as a **non-success** and re-driven by the existing retry machinery — instead of
being recorded DONE and then silently parked at submit time. The check is supplied
by the landing strategy (which alone knows about commits and bases), so
flywheel-core and the orchestrator stay git-free, and task classes with no notion
of a diff (research, config, non-code) are unaffected. Autopilot-authored tasks
carry the same commit-before-done discipline that `/fw-plan` already bakes into
hand-authored tasks. The net effect: the silent strand that left three green
infrared-rust tasks unlanded on 2026-06-29 cannot recur — a task either lands its
change or surfaces as failed, never DONE-but-empty.

## Background
The loop owns a single task's lifecycle; landing lives one layer up in a
`SubmitStrategy` (see [strategy.md](../../docs/strategy.md)). Today the two halves
disagree about what "done" means:

- **Core decides DONE on graders alone.** `done` is reached when all declared
  graders pass and the agent emits `intent=verify` (`harness.py:3495,3526`).
  Command graders run with `cwd` set to the live sandbox working tree
  (`grader_command.py:143`), so they pass against **uncommitted** edits. Core is
  pure/git-free by invariant (`flywheel_core.task`/`lifecycle` purity tests), so it
  *cannot* and *should not* inspect commits.
- **The strategy is the only layer that knows about commits**, and it inspects
  them too late. `GitWorktreeSubmitter._submit` runs `git status --porcelain` and
  `git rev-list --count base..branch` (`worker.py:518-545`) — but only inside
  `submit`, which the orchestrator calls *after* the lifecycle is already written
  DONE, and which **must not raise** (it records its own outcome). On a dirty tree
  it logs "DONE with uncommitted changes … parking worktree" and records a
  `LandingParked` event; on zero commits it cleans up. Either way the lifecycle row
  stays DONE and the work never lands, with no feedback loop to re-drive it.

The autopilot widens the gap. Its authoring prompt (`_autopilot.py:1109-1160`)
emits tasks with `context: {}` and never sets the commit-before-done constraint
that the `fw-plan` skill bakes into hand-authored tasks
(`fw-plan.md:110,183`: "Commit the change with a clear message before reporting
done"). So autopilot agents are the most likely to finish green against a dirty
tree.

This is not a hole in flywheel's *mechanical* integration governance, which is
already strong and which spec 00060 (worker-concurrency-pool) reuses: landings are
serialized through the repo merge-flock (`worker.py:179,517`); a finished branch
whose base advanced is rebased and has its command graders re-run against the exact
base it lands on before the FF (`worker.py:564-607`, `_reverify` at `:643`);
protected paths are gated; conflicts park for forensics. The gap is narrower: there
is **no point at which an empty/uncommitted "done" is converted from a silent park
into a re-drivable non-success.** 00060 makes this urgent — at concurrency N, every
cycle is N chances to silently strand. Concurrency is opt-in (default 1), so this is
not a release blocker, but closing it is the prerequisite to raising N in
production.

The orchestrator already owns the right insertion point. The execute-time held-out
gate runs at `status is Status.DONE`, after `run_task` and before `submit`
(`_orchestrate.py:1245,1391`), and turns a failing oracle into a non-landing
outcome without core knowing git exists. The landable-change gate is a sibling at
the same seam, with the git-aware predicate supplied by the strategy rather than
hard-coded.

## Scope
### In scope
- A **landability predicate supplied by the landing strategy**: given the task,
  the run's terminal status, and the sandbox, the strategy reports whether the run
  produced a committed, non-empty, landable change against the branch's base. The
  worktree submitter implements it by reusing its existing `git status --porcelain`
  + `git rev-list --count base..branch` checks (`worker.py:518,534`). Strategies
  with no diff notion (research/config) supply a predicate that always reports
  "landable" (a no-op), so non-code task classes are untouched.
- An **execute-time landable-change gate** in the orchestrator at the existing
  post-run / pre-submit `Status.DONE` site (`_orchestrate.py:1245`). When the
  predicate reports a non-landable change, the gate prevents the run from being
  treated as a landed success and routes it to a **retryable non-terminal
  outcome**, so the existing retry / lease / work-source machinery re-drives the
  task against the same base. When the predicate reports landable (or is the no-op),
  behavior is byte-for-byte unchanged: the held-out gate (if any) and then `submit`
  run exactly as today.
- **Bounded retry**: a task that never commits is re-driven under the existing
  max-retries budget and ultimately surfaces as **failed** (not DONE), with a
  recorded reason naming the non-landable change. No infinite loop.
- **Autopilot constraint parity**: the autopilot authoring prompt sets a
  `context.constraints` entry equivalent to `fw-plan.md:183`, so emitted task files
  carry the commit-before-done discipline.

### Out of scope
- **Cross-task / standing-invariant coherence oracle** (Gap 3): a repo-level
  held-out gate not keyed per-task-id that checks the integrated base after each
  land, plus autopilot populating `conflict_keys` from `creates_files`/grader
  targets so overlapping tasks serialize at claim time. Framed in "Future
  directions"; a separate spec.
- Any change to 00060's shipped pool/landing code, the merge-flock, the
  rebase/re-verify path, or the protected-paths gate.
- Changing core's definition of `done` or adding any git/commit awareness to
  `flywheel_core` or to the generic orchestrator scheduling — the git knowledge
  stays inside the strategy.
- Detecting *semantic* emptiness (a commit that compiles but does nothing). The
  gate's notion of "landable" is structural: a non-empty committed diff against the
  base.

### Must not regress
- **Core purity and the git-free line.** `flywheel_core.task`/`lifecycle` stay
  pure; the orchestrator stays git-agnostic; all commit/base knowledge lives in the
  strategy (mirrors how the held-out gate is fed `committed_tree` without doing git
  itself).
- **Legitimate no-diff tasks.** A task whose graders are inspection-only and which
  correctly produces no change (e.g. "confirm invariant X holds") must still
  complete — the gate must distinguish "nothing to do" from "work done but not
  committed". This is keyed on task/strategy class, not on diff emptiness alone.
- **Single-worker and non-code task classes**: unchanged when no git strategy is
  active (the no-op predicate path).
- **00060 invariants**: exactly-once execution, serialized landing, submit-time
  re-verification, orphan-free shutdown.

## Success Criteria
Each criterion is atomic, grades an observable end-state, and carries its grader
type, visibility, the exact check, and the gaming move it forecloses. `/fw-plan`
lowers each one to a command / transcript / rubric / manual grader.

1. When an agent reaches `intent=verify` with all graders green but an
   **uncommitted** sandbox tree under a git landing strategy, the run is not
   recorded as a landed success; the task is re-driven, and a subsequent attempt
   that commits the change lands it. [command | held-out]
   verify: drive a task whose scripted agent edits files and passes graders without
   committing on attempt 1, then commits on attempt 2; assert attempt 1 does not end
   the task DONE-and-landed, the task is re-driven, and after attempt 2 the base
   contains the committed change.
   defends against: a gate that fires only inside `submit` (after DONE is already
   written), leaving the lifecycle DONE while the work is parked.

2. When a git landing strategy run reaches verify with **zero commits beyond the
   base** (empty diff), the run is not recorded as a landed success. [command | held-out]
   verify: scripted agent that touches nothing (or only the working tree, then
   reverts) yet emits `intent=verify`; assert the task does not reach a landed-DONE
   state and the base is unchanged.
   defends against: an empty branch being treated as a successful no-op land
   (the work silently vanishing).

3. A task that legitimately produces **no change** under a strategy with no diff
   notion (research/config/non-git, the no-op predicate) completes normally without
   any spurious retry. [command | held-out]
   verify: a non-code task with inspection-only graders and a no-op landability
   predicate reaches DONE on the first attempt with no extra attempts recorded.
   defends against: the gate over-firing on legitimate no-diff work, turning every
   inspection task into a retry loop (the dangerous false-positive).

4. A task whose agent **never** commits is re-driven under the existing max-retries
   budget and ultimately surfaces as **failed** (not DONE), with a recorded reason
   referencing the non-landable change. [command | visible]
   verify: scripted agent that passes graders but never commits on every attempt;
   assert attempts == max-retries, terminal status is FAILED, and a recorded
   reason/event names the non-landable change; no infinite loop.
   defends against: an unbounded retry loop, or the failure being swallowed back
   into a silent DONE.

5. When the landability predicate reports a **landable** change (committed,
   non-empty), the post-run path is byte-for-byte unchanged: the held-out gate (if
   configured) and then `submit` run exactly as today and the change lands. [command | visible]
   verify: a normal task that commits a change; assert it lands via the unchanged
   submit path, the existing held-out-gate ordering is preserved, and the worktree
   suite for the merge strategy passes unchanged.
   defends against: the new gate altering or reordering the existing
   held-out-gate / submit flow for the common (committed) case.

6. flywheel-core and the orchestrator gain **no git/commit awareness**: the
   commit/base inspection lives entirely in the strategy. [command | visible]
   verify: the core purity tests (`test_task_module_purity`,
   `test_lifecycle_module_purity`) stay green; a check (grep/AST) confirms the
   orchestrator gate calls a strategy-provided predicate rather than invoking git
   itself.
   defends against: the gate leaking `git`/`subprocess`/commit logic into core or
   the generic orchestrator, eroding the workspace's one-way dependency line.

7. An **autopilot-emitted task file** carries a commit-before-done entry in
   `context.constraints`. [command | visible]
   verify: run the authoring path (or assert on the authoring prompt + a
   round-tripped emitted task file under `active/autopilot/`) and assert
   `context.constraints` contains a commit-before-done instruction equivalent to
   `fw-plan.md:183`.
   defends against: autopilot continuing to emit `context: {}` tasks with no
   commit discipline — the original trigger.

Verification surface: this feature adds a gate and an authoring-prompt constraint;
it does not modify the tests, grading commands, CI config, or any machinery that
decides whether a change is *correct*. The existing worktree/orchestrator suites are
the regression oracle for criteria 5 and 6; no existing check is relaxed, removed,
or skipped. The held-out proofs (#1, #2, #3) assert observable lifecycle/base state
the implementing agent cannot pre-compute from known inputs.

## Decomposition Hint (for /fw-plan)
Split along these layers; chain with prerequisites so no slice inherits a red suite.
- Layer predicate-seam: add the strategy-supplied landability predicate to the
  `SubmitStrategy` seam (optional method; default/no-op reports "landable"), and
  implement it in `GitWorktreeSubmitter`/`GitPullRequestSubmitter` by lifting the
  existing porcelain + commit-count checks (`worker.py:518,534`) into a pure
  inspection that does not mutate or park. Contributes the predicate every later
  layer reads. Satisfies the seam half of #5, #6.
- Layer orchestrator-gate: invoke the predicate at the `Status.DONE` post-run site
  (`_orchestrate.py:1245`), before the held-out gate / submit; on a non-landable
  result route to a retryable non-terminal outcome reusing the existing retry/lease
  path, and on the no-op/landable result preserve today's flow exactly. Satisfies
  #1, #2, #3, #4, #5. Depends on predicate-seam.
- Layer autopilot-constraint: add the commit-before-done constraint to the
  authoring prompt template (`_autopilot.py:1145`) and confirm it round-trips into
  emitted task files. Satisfies #7. Independent of the other two layers.
Shared invariants multiple layers assert against: the git-free line (the predicate
is the *only* new place git is touched), and the retry budget (the gate reuses
existing max-retries, never invents an unbounded loop).

## Decisions Log (ADR-style: immutable, supersede — never edit)

### D-1: The landability check is a strategy-supplied predicate, run at the orchestrator DONE site  (Status: Accepted)
- Context: only the landing strategy knows about commits and bases; core is
  git-free by invariant, and the generic orchestrator must stay git-agnostic. The
  orchestrator already runs the held-out gate at `Status.DONE` before `submit`
  (`_orchestrate.py:1245`) as the model for "a git-aware decision fed in from
  above." | Decision: add an optional landability predicate to the `SubmitStrategy`
  seam; the orchestrator invokes it at that same DONE site and acts on a plain
  boolean/verdict.
- Rejected: baking `git rev-list`/`status` into the orchestrator (violates the
  one-way dependency line and the git-free orchestrator); having `submit` re-open
  the lifecycle (submit is terminal-status-driven and must not raise — it cannot
  cleanly transition DONE back to a retryable state). | Consequences: one new
  optional seam method; non-git strategies supply a no-op and are unaffected.

### D-2: A non-landable "done" is a retryable non-success, not a hard failure (bounded)  (Status: Accepted)
- Context: when graders are green but nothing committed, the agent did the work and
  forgot to commit; re-driving with the commit-before-done constraint in-prompt
  usually lands it on the next attempt. A stranded park is the failure mode we are
  removing. | Decision: route a non-landable DONE to a retryable non-terminal
  outcome that reuses the existing max-retries / lease / work-source re-drive path;
  on budget exhaustion it surfaces as FAILED with a recorded reason.
- Rejected: fail-fast on first non-landable verify (throws away recoverable work
  the agent actually did); silent park as today (the bug). | Consequences: a
  pathological never-commits task consumes its retry budget before failing —
  bounded and loud, matching the 00060 crash-retry posture.

### D-3: "Landable" is structural (a non-empty committed diff vs base), keyed on strategy class  (Status: Accepted)
- Context: the gate must not punish legitimate no-diff tasks (inspection-only
  graders) and must not pretend to judge semantic emptiness. | Decision: a change
  is landable iff the worktree is clean (no uncommitted modifications) and there is
  >=1 commit with a non-empty diff against the base; strategies with no diff notion
  return "landable" unconditionally (the no-op). The distinction "nothing to do" vs
  "work not committed" is made by *which strategy* is active, not by diff emptiness
  alone.
- Rejected: treating every empty diff as a failure regardless of task class (breaks
  inspection tasks); semantic-emptiness detection (out of scope, unbounded).
  | Consequences: a code task that should have changed something but committed an
  empty diff is caught as zero-commits/uncommitted; a task that legitimately
  changes nothing must run under a no-diff strategy class to opt out.

### D-4: Autopilot constraint parity is a complement, not the enforcement  (Status: Accepted)
- Context: constraints are advisory prompt text in core (`prompt.py:180`), never
  graded — they raise the odds an agent commits but cannot guarantee it. | Decision:
  add the commit-before-done constraint to the autopilot authoring prompt for parity
  with `fw-plan`, but rely on the D-1 gate for actual enforcement.
- Rejected: treating the constraint as sufficient (it is unenforced text — the
  original trigger proves agents ignore implicit discipline). | Consequences: two
  layers of defense — likelier-correct authoring plus a hard gate.

## Open Questions (accepted gaps)
- The exact retry-routing mechanism (which non-terminal status/outcome the
  orchestrator uses to re-drive a non-landable DONE without minting a double-run)
  must be lowered against the post-4dc477b claim/lease semantics during `/fw-plan`;
  criteria #1 and #4 are the held-out/visible proofs that pin it. Not a design gap —
  a lowering detail.

## Future directions (not this spec)
- **Cross-task coherence oracle (Gap 3).** A standing-invariant, repo-level
  held-out gate (not keyed per-task-id) that re-checks the integrated base after
  each land, catching textually-clean-but-semantically-incoherent siblings that each
  pass their own graders. Connects to the per-task-id held-out-gate limitation noted
  in external-adoption findings.
- **Authoring-time conflict governance.** Autopilot populating `conflict_keys`
  (`_sources.py:91`) from `creates_files`/grader targets so overlapping tasks
  serialize at claim time instead of one parking on a `divergent-base` rebase
  conflict; and/or sequencing conflicting findings as prerequisites rather than
  fanning them out concurrently.

## Next Steps
Run `/fw-plan 00061-FEATURE-landable-change-gate` to compile these criteria into
flywheel tasks and graders.

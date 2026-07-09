# Feature: Commit provenance trailers

## Outcome
Every commit the loop lands (merge strategy) or publishes (pr strategy) carries
harness-derived git trailers `Flywheel-Task: <task-id>`, `Flywheel-Run: <run-id>`,
and `Flywheel-Phase: <phase>`. `git log --format='%(trailers:key=Flywheel-Task,valueonly)'`
over a landed range yields the task id for every commit, and `fw show <commit-sha>`
renders the run that produced it. Provenance is stamped mechanically by the worker
at land time — never dependent on agent compliance.

## Background
An unattended multi-hour session lands dozens of commits under the single fixed
identity `Flywheel Worker <worker@flywheel.invalid>` with no machine-readable link
from a commit back to its task, run, receipts, or transcript; post-hoc attribution
is manual archaeology. The agent is untrusted (docs/vision.md, claim-based
signaling): a prompt instruction to add trailers is a claim, not a mechanism, so
the authoritative stamping must happen out-of-band in the worker's landing path.
The worker already rewrites task-branch commits at submit time (rebase-then-
reverify), so a message-only rewrite at the same point is an established operation
class. D-1 of the landing design ("the worker authors no commit on the operator's
branch") is preserved: stamping rewrites task-branch commits before the FF/push,
never commits on the base itself.

## Scope
### In scope
- Trailer stamping on the merge-strategy landing path and the pr-strategy push path.
- Harness values are authoritative: agent-authored `Flywheel-*` trailers are replaced.
- Commit-hash lookup on the run-detail CLI surface (`fw show <sha>`).
### Out of scope
- Commit signing (team-mode spec 00081 owns the design; the stamping point built
  here is its intended future signing point).
- Trailers on parked, never-landed worktrees.
- Container-backend changes (stamping is host-side and backend-agnostic).
- Store schema changes: the trailer is the durable artifact; no new columns.
### Must not regress
- Rebase-reverify, standing-verify, and protected-paths semantics and their park kinds.
- Zero-commit cleanup and non-DONE teardown paths.
- Held-out gate timing (pre-submit) and verdict validity — trees are unchanged by stamping.
- D-1: the worker still authors no commit on the base branch.

## Success Criteria
Each criterion is atomic, grades an observable end-state, and carries its grader
type, visibility, the exact check, and the gaming move it forecloses.

1. When a DONE task lands via the merge strategy, every commit the landing
   introduces into the base (`git log <base-before>..<base-after>`) shall carry
   `Flywheel-Task`, `Flywheel-Run`, and `Flywheel-Phase` trailers whose values are
   the landing task's id, run id, and phase directory name. [command | held-out]
   verify: harness repo whose scripted agent commits with NO trailers; after land,
   `git log --format='%(trailers:key=Flywheel-Task,valueonly)' <before>..<after>`
   prints the task id exactly once per commit (no empty lines); same for the Run
   and Phase keys.
   defends against: satisfying the outcome by prompt instruction — the harness
   agent deliberately omits trailers, so only mechanical stamping can pass.

2. When a DONE task is submitted via the pr strategy, every commit on the pushed
   branch beyond the PR base shall carry the same three trailers before the PR is
   opened or refreshed. [command | held-out]
   verify: harness with a stubbed gh runner; after submit, the trailer log over
   `<pr_base>..<branch>` on the pushed ref shows all three keys on every commit,
   and the stub records that push preceded PR create/edit.
   defends against: implementing stamping only on the merge path (the narrow reading).

3. If an agent-authored commit carries a `Flywheel-Task`, `Flywheel-Run`, or
   `Flywheel-Phase` trailer whose value differs from the harness-known values,
   then the landed commit shall carry the harness values and the agent's values
   shall not appear. [command | held-out]
   verify: harness commit forged with `Flywheel-Run: forged-123`; after land the
   trailer equals the real run id and `git log --grep='forged-123'` over the
   landed range matches nothing.
   defends against: an agent laundering provenance by mis-attributing its work to
   another task or run.

4. Trailer stamping shall be message-only: for every landed commit, the tree
   object shall be byte-identical to the corresponding pre-stamp commit's tree,
   in order. [command | held-out]
   verify: harness records `git rev-parse <c>^{tree}` for each of `base..branch`
   before landing; the landed range's ordered tree list is identical.
   defends against: an implementation that "stamps" by committing new tree state
   (re-committing the worktree), silently changing what verification approved.

5. When `fw show` is invoked with a landed commit's SHA, it shall render the same
   run detail as `fw show` with that run's id. [command | visible]
   verify: land a task in the harness; `fw show <sha>` exits 0 and its output
   contains the run id and the landing decision line the run-detail view renders.
   defends against: resolving only run/task ids and declaring the round-trip done
   in docs.

6. If `fw show` is given a commit SHA carrying no `Flywheel-Run` trailer (a human
   commit), then it shall exit non-zero naming the commit as un-attributed and
   render no run detail. [command | visible]
   verify: `fw show <operator-commit-sha>` in the harness exits non-zero with an
   un-attributed message; no run sections are printed.
   defends against: a fuzzy fallback (nearest run in time) fabricating provenance.

7. (verification-surface) When this feature lands, the repo's full gate shall pass
   with every existing landing-path test intact or replaced by a named
   equal-or-stronger check. [command | held-out]
   verify: `scripts/check.sh` exits 0; the diff removes or weakens no existing
   submit/landing/park assertion without a named replacement.
   defends against: making stamping "work" by deleting the submit-path tests that
   constrain it.

Verification surface: changed — the landing path is part of the machinery that
decides what lands. The existing suite must still pass; any relaxed, removed, or
skipped check must be named with an equal-or-greater replacement (a removed
assertion with none is a blocking defect); new behavior is proven by out-of-band
checks the implementing agent did not author against its own known inputs
(criteria 1-4 held-out).

## Decomposition Hint (for /fw-plan)
- Stamping engine (message-only rewrite of the task branch's `base..branch` range
  with authoritative trailer values): satisfies #1, #3, #4.
- Strategy wiring (merge landing path and pr push path invoke the engine at the
  right point in the land sequence): satisfies #1, #2; depends on the engine.
- Commit lookup on the run-detail surface: satisfies #5, #6; independent of the
  strategy wiring, shares the trailer-key vocabulary.
Shared invariants: the three trailer key names and their value derivation
(task id, run id, phase directory name) are consumed by both the engine and the
lookup — dependent slices update together.

## Decisions Log

### D-1: Land-time mechanical stamping, not hooks and not prompt constraints  (Status: Accepted)
- Context: trailers must be present on 100% of landed commits regardless of agent
  behavior; the agent is untrusted. | Decision: the worker stamps the task
  branch's commits at submit time (under the merge lock, before FF on the merge
  path; before push on the pr path), rewriting messages only.
- Rejected: prompt-constraint-only (violates the untrusted-agent invariant — a
  claim, not a mechanism); commit-msg hook installed at worktree provisioning
  (agent-bypassable via `--no-verify`, and per-worktree hook plumbing is fragile
  across the container backend's bind-mounted worktrees — could still be added
  later as a cosmetic nicety, but never as the mechanism); amend on the base
  after FF (mutates the base after verification and breaks D-1 of the landing
  design). | Consequences: landed SHAs differ from the SHAs the agent last saw
  (already true on the rebase path); parked worktrees carry no trailers.

### D-2: Harness values are authoritative; agent trailers are stripped  (Status: Accepted)
- Context: an agent that can pre-write the trailer controls the provenance
  record. | Decision: stamping removes any agent-authored `Flywheel-*` trailer
  and writes the harness-derived values.
- Rejected: keep-if-present (trusts the claim); append-both (two conflicting
  values make the record ambiguous). | Consequences: even a cooperative agent's
  correct trailers are rewritten; the record's authority is uniform.

### D-3: Commit-to-run lookup reads the trailer, not a new store column  (Status: Accepted)
- Context: the store records `landed_ref` only as a domain-event payload; there
  is no commit-to-run index. | Decision: `fw show <sha>` resolves the run id from
  the commit's trailer via git, then renders the existing run detail.
- Rejected: new store table/column mapping commit SHAs to runs (a second source
  of truth that can drift from git; schema churn for a read path git already
  serves). | Consequences: lookup requires running inside the repo; un-attributed
  commits are a hard error, never a guess.

## Open Questions (accepted gaps)
None.

## Next Steps
Run `/fw-plan 00078-FEATURE-commit-provenance-trailers` to compile these criteria
into flywheel tasks and graders.

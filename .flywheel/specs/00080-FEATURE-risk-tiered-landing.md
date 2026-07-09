# Feature: Risk-tiered landing

## Outcome
The landing strategy applied to each DONE task is a mechanical function of its
diff, not a repo-global constant: with `[[submit.tiers]]` configured, the changed
files are classified at submit time against operator-declared path rules — tier 0
lands direct (`merge`), tier 1 lands on the phase integration branch (`phase`,
spec 00079), tier 2 goes to a PR requiring human approval (`pr`) and never lands
locally. The highest tier of any touched file wins, unmatched files default to
tier 1, and classification reads only the worker-side policy — the agent is
structurally unable to reclassify its own diff. `protected_paths` remains a
distinct, absolute refuse-list that outranks every tier.

## Background
A docs typo and a change to CI config carry different risk, but today one
`[submit] strategy` governs both — so operators either over-review trivia or
under-review the verification surface. Classification must be external to the
agent (path rules over the actual changed files, evaluated by the worker),
because an optimizing agent asked to self-classify will discover that "docs-only"
is the cheapest label. Tiers and `protected_paths` are deliberately different
concepts: a tier-2 path may legitimately land through a human-approved PR, while
a protected path never lands through the loop at all — it is the fail-closed
floor under the verification surface, including the tier rules themselves.

## Scope
### In scope
- A tier classifier evaluated at submit time over the task branch's changed
  files, configured as path-pattern rules in `flywheel.toml`, with
  highest-tier-wins and a default tier for unmatched files.
- Tier-to-strategy routing through the existing `SUBMIT_STRATEGIES` registry
  (0 -> merge, 1 -> phase, 2 -> pr).
- Classification reads the policy the worker process loaded, never any copy in
  the task branch.
- Precedence: `protected_paths` evaluated independently and outranking tiers.
### Out of scope
- Enforcing that a human (not the agent's identity) approves tier-2 PRs — that
  is branch-protection/ruleset territory owned by the team-mode spec (00081);
  this spec guarantees only that tier-2 work never lands locally.
- Content-based classification (diff size, language heuristics, LLM judgment) —
  rules are path patterns only.
- Autopilot scoring/tier vocabulary (`docs/autopilot.md` tiers are unrelated;
  the config and docs must not conflate them).
- Changing the meaning of `[submit] strategy` for repos without `[[submit.tiers]]`.
### Must not regress
- Absent `[[submit.tiers]]`: behavior identical to the configured
  `[submit] strategy` today, byte-for-byte.
- `protected_paths` parks regardless of any tier assignment.
- The verify ladder of whichever strategy the tier routes to (each strategy's
  own gates run unchanged).

## Success Criteria
Each criterion is atomic, grades an observable end-state, and carries its grader
type, visibility, the exact check, and the gaming move it forecloses.

1. When a DONE task's changed files all classify tier 0, the work shall land via
   the merge strategy directly onto the configured landing base. [command | held-out]
   verify: harness with tier rules mapping `docs/**` to tier 0; a docs-only task
   lands on the base with no phase branch created and no PR opened.
   defends against: routing everything through the heaviest path and calling the
   feature shipped (tiering that never differentiates).

2. When any changed file classifies tier 2, the work shall be pushed and a PR
   opened, and neither the landing base nor any phase branch shall advance.
   [command | held-out]
   verify: harness task touching one tier-2 path among many tier-0 files (highest
   wins): the stub gh runner records a PR; base and phase branch SHAs unchanged.
   defends against: averaging or first-match classification that lets a tier-2
   file ride a mostly-tier-0 diff onto the base.

3. When a changed file matches no tier rule, it shall classify as the default
   tier, and the default shall be tier 1. [command | held-out]
   verify: harness task touching an unmatched path lands on the phase branch, not
   the base.
   defends against: unmatched-means-lowest, which makes every newly created path
   (the easiest thing for an agent to control) a direct-merge lane.

4. If the task's branch adds or edits any file matching the tier rules' own
   source (the policy file), then classification of that diff shall use only the
   rules the worker process loaded from the operator's checkout, and the
   policy-file path shall classify at the highest tier. [command | held-out]
   verify: harness task rewrites its worktree copy of `flywheel.toml` to declare
   everything tier 0; the landing still routes that diff per the operator-side
   rules (PR path), and no merge to base occurs.
   defends against: the agent reclassifying its own diff by editing the rules —
   the exact structural hole the milestone exists to close.

5. If a changed file matches `protected_paths`, then the work shall park with the
   existing protected-paths park kind regardless of its computed tier.
   [command | held-out]
   verify: harness with a path listed both tier 2 and protected: the task parks;
   no PR is opened and nothing lands.
   defends against: reading tier 2 as "a landing route for protected paths" —
   protection is refusal, not routing.

6. When `[[submit.tiers]]` is absent from the policy, landing behavior shall be
   identical to the configured `[submit] strategy` with no classifier in the
   path. [command | held-out]
   verify: existing submit-strategy suites pass unchanged; a harness run without
   tier config produces byte-identical landing behavior to today's merge default.
   defends against: making tiering implicitly mandatory and changing shipped
   behavior on upgrade.

7. When a tier decision routes a landing, the decision (per-file tier, winning
   tier, chosen strategy) shall be recorded on the run's ledger so it is
   diagnosable from the store alone. [command | visible]
   verify: harness lands one task per tier; the run's domain events (or landing
   record) carry the winning tier and strategy for each; a misclassification is
   reconstructable without re-running.
   defends against: an unauditable classifier whose decisions can only be
   inferred from which branch moved.

8. (verification-surface) When this feature lands, the repo's full gate shall
   pass with every existing submit-strategy and policy-parsing test intact or
   replaced by a named equal-or-stronger check. [command | held-out]
   verify: `scripts/check.sh` exits 0; the diff removes or weakens no existing
   strategy-selection or policy-validation assertion without a named replacement.
   defends against: passing the routing criteria by rewriting the strategy tests
   that pin today's dispatch.

Verification surface: changed — the classifier decides which gate a change faces,
which makes it part of the machinery that decides what lands. The existing suite
must still pass; any relaxed, removed, or skipped check must be named with an
equal-or-greater replacement (a removed assertion with none is a blocking
defect); new behavior is proven by out-of-band checks the implementing agent did
not author against its own known inputs (criteria 1-6 held-out).

## Decomposition Hint (for /fw-plan)
- Policy layer (tier-rule parsing/validation, default tier, precedence
  vocabulary): satisfies #3, #6 (parse half), #5 (precedence definition).
- Classifier layer (changed-files evaluation, highest-wins, worker-side-only
  rule source): satisfies #1-#4; depends on the policy layer.
- Dispatch layer (tier-to-strategy routing through the registry, decision
  recording): satisfies #1, #2, #7; depends on the classifier.
Shared invariants: the tier vocabulary (0/1/2 and the default), the changed-file
set derivation (the same merge-base-scoped diff `protected_paths` uses), and the
precedence rule (protected > tier) — policy, classifier, and dispatch all assert
against them; dependent slices update together.

## Decisions Log

### D-1: Tiers and protected_paths stay distinct concepts  (Status: Accepted)
- Context: both are path rules over the same diff, inviting consolidation.
- Decision: `protected_paths` remains an absolute refuse-to-land list evaluated
  independently and first; tiers only choose among legitimate landing routes.
- Rejected: modeling protection as "tier 3" (one config knob, but then loosening
  a tier boundary silently loosens the refusal floor, and a routing table
  becomes a security boundary). | Consequences: a path can appear in both; the
  park wins, and criterion 5 pins it.

### D-2: Rules are path patterns in `[[submit.tiers]]`, evaluated worker-side  (Status: Accepted)
- Context: classification must be mechanical and agent-external. | Decision:
  array-of-tables config (`tier`, `paths`) using the same glob semantics as
  `protected_paths` (`PurePath.full_match`, `**` crosses directories); the
  classifier consumes the `WorkPolicy` the worker loaded, never a file from the
  task branch or worktree.
- Rejected: agent-visible sidecar files or per-task tier hints (agent-writable);
  content heuristics (unauditable, gameable by formatting). | Consequences:
  changing tier rules requires landing a policy change, which itself classifies
  at the highest tier (criterion 4) and is protected in this repo.

### D-3: Unmatched files default to tier 1  (Status: Accepted)
- Context: the default decides what happens to every path nobody thought about,
  including paths the agent invents. | Decision: unmatched -> tier 1 (phase
  branch): reviewed at phase scope, never a silent direct merge.
- Rejected: default tier 0 (new files become an unreviewed lane); default tier 2
  (every ordinary code change demands a human PR, which operators would disable).
- Consequences: tier-0 fast lanes are always an explicit operator allowlist.

### D-4: Spec-only for now  (Status: Accepted — operator sequencing)
- Context: tier 1 routing depends on the phase strategy (00079) existing.
- Decision: this spec is authored now for review; `/fw-plan` compilation waits
  until 00079 is landed or in flight.
- Consequences: criteria referencing the phase strategy assume 00079's surface.

## Open Questions (accepted gaps)
None.

## Next Steps
After 00079 is in flight, run `/fw-plan 00080-FEATURE-risk-tiered-landing` to
compile these criteria into flywheel tasks and graders.

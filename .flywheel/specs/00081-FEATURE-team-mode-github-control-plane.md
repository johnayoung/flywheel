# Feature: Team mode — GitHub as the control plane

## Outcome
Multi-operator use has a documented, partially-shipped path: `flywheel init`
scaffolds hardened `[submit]` defaults into new consumer repos (defaults stop
being the bug); policy load refuses the single-machine merge landing under
distributed execution; and a committed runbook (`docs/team-mode.md`) specifies
the GitHub-side control plane — worker identity as a GitHub App, branch
protection/ruleset requirements, and the merge queue as the distributed merge
lock — with an explicit split between what is a flywheel feature and what is
documented operator setup.

## Background
The merge lock is `fcntl.flock` on `.flywheel/.merge.lock` — advisory, kernel-
local, single-machine by construction. Two workers on two machines sharing a
Postgres claim store would race the base branch today with no guard. For
multi-operator use, GitHub's merge queue and branch protection are the
distributed merge lock; flywheel should not rebuild them. Today the repo has
zero GitHub App/token/signing support (auth is entirely ambient `gh`), and
`flywheel init` renders `protected_paths` only as commented placeholder — every
new consumer repo starts with the open-by-default posture that produced the
overnight incident. The land-time stamping point built by spec 00078 is the
intended future host-side signing point (the key never enters the sandbox).

## Scope
### In scope
- `flywheel init` scaffolds an active hardened `[submit]` block (protected paths
  at minimum covering the CI directory and the policy file) into new repos.
- A load-time policy guard: distributed execution mode plus a local-flock landing
  strategy is a configuration error, not a silent race.
- The `docs/team-mode.md` runbook: GitHub App worker identity (auth for push/PR,
  signed commits at the land-time rewrite point), ruleset/branch-protection
  requirements (no direct pushes to the base, required checks, merge-commit
  method for phase PRs, the worker identity barred from approving or merging its
  own PRs), merge queue adoption, and the feature-vs-operator-setup split.
- A seam test keeping the runbook honest against the shipped surface.
### Out of scope
- Implementing GitHub App authentication, token minting, or commit signing in
  this spec's tasks — the runbook specifies the design; implementation is a
  follow-up spec once the runbook is operated once.
- Any change to claim/lease machinery (Postgres leases already span machines).
- Rebuilding merge-queue semantics inside flywheel.
### Must not regress
- `flywheel init` idempotence and its never-overwrite contract.
- Existing single-operator local behavior for policies that set neither
  distributed mode nor the new defaults.
- Ambient `gh` auth keeps working for single-operator use.

## Success Criteria
Each criterion is atomic, grades an observable end-state, and carries its grader
type, visibility, the exact check, and the gaming move it forecloses.

1. When `flywheel init` scaffolds a new repo, the rendered policy shall contain
   an active `[submit]` table whose `protected_paths` cover at least the repo's
   CI directory (`.github/**`) and the policy file itself (`flywheel.toml`).
   [command | held-out]
   verify: `flywheel init` in a temp repo; parsing the rendered `flywheel.toml`
   yields `protected_paths` containing both patterns as live config (not
   comments), and a re-run leaves the file byte-identical.
   defends against: satisfying "hardened defaults" with commented-out placeholder
   text, which is the current rendered form.

2. If the loaded policy sets distributed execution mode while the effective
   landing strategy serializes through the local merge lock (merge or phase),
   then policy load shall fail with an error naming both settings.
   [command | held-out]
   verify: a policy with `[execution] mode = "distributed"` and
   `[submit] strategy = "merge"` raises a policy error naming mode and strategy;
   the same policy with `strategy = "pr"` loads.
   defends against: two machines silently racing the base because the flock's
   single-machine scope is documented but unenforced.

3. The repo shall carry `docs/team-mode.md` specifying: (a) worker identity as a
   GitHub App with the push/PR auth flow and the land-time signing design,
   (b) the ruleset requirements — no direct pushes to the base, required status
   checks, merge-commit method for phase PRs, and the worker identity unable to
   approve or merge its own PRs, (c) merge-queue adoption as the distributed
   merge lock paired with spec 00079's block-on-merge, and (d) a table splitting
   flywheel features from operator setup. [manual, paired with #4 | visible]
   verify: operator review confirms each of (a)-(d) is specified concretely
   enough to execute against a real GitHub org — the runbook is the deliverable,
   not a stub.
   defends against: a heading-skeleton document that names the topics without
   specifying decisions.

4. Every flywheel CLI verb, config key, and doc topic `docs/team-mode.md` names
   shall exist on the shipped surface. [command | held-out]
   verify: a seam test (the `test_ops_skill_surface` pattern) extracts the verbs
   and config keys the runbook references and fails if any is absent from the
   shell or policy surface.
   defends against: a runbook that reads plausibly but instructs operators to
   run verbs or set keys that do not exist — the LLM-authored-docs failure mode.

5. (verification-surface) When this feature lands, the repo's full gate shall
   pass with every existing init and policy-validation test intact or replaced
   by a named equal-or-stronger check. [command | held-out]
   verify: `scripts/check.sh` exits 0; the diff removes or weakens no existing
   init-idempotence or policy-error assertion without a named replacement.
   defends against: making the new guard pass by loosening the policy-validation
   tests that pin today's errors.

Verification surface: changed — init scaffolding and policy validation shape
what every consumer repo's landing gate looks like. The existing suite must
still pass; any relaxed, removed, or skipped check must be named with an
equal-or-greater replacement (a removed assertion with none is a blocking
defect); new behavior is proven by out-of-band checks the implementing agent did
not author against its own known inputs (criteria 1, 2, 4 held-out).

## Decomposition Hint (for /fw-plan)
- Init layer (active hardened `[submit]` rendering, idempotence preserved):
  satisfies #1.
- Policy-guard layer (distributed-mode vs local-lock landing validation):
  satisfies #2; independent of the init layer.
- Runbook layer (`docs/team-mode.md` + the seam test): satisfies #3, #4;
  independent, but its content must reference the guard from #2 accurately.
Shared invariants: the set of "local-lock" strategies (merge, phase) — the
policy guard and the runbook's strategy table both assert it; a future strategy
addition must update both together.

## Decisions Log

### D-1: GitHub App identity over a machine-user PAT  (Status: Accepted)
- Context: multi-operator needs a worker identity that is auditable, revocable,
  org-ownable, and excludable from approval rules. | Decision: the runbook
  specifies a GitHub App (installation tokens for push/PR; commits attributed to
  the App's bot identity).
- Rejected: a machine-user PAT (a licensed seat, coarse revocation, counts as a
  "user" rulesets treat like a human); each operator's personal token (work
  attribution collapses onto whoever ran the worker — the exact provenance
  failure specs 00078+ exist to fix). | Consequences: token minting/refresh is
  real implementation work, deliberately deferred to a follow-up spec.

### D-2: Sign at the land-time rewrite point, never in the sandbox  (Status: Accepted)
- Context: signing agent commits requires a key wherever the commit is created;
  the sandbox is the untrusted party. | Decision: the runbook's signing design
  keys off spec 00078's land-time message rewrite — the worker re-signs the
  rewritten commits host-side; no key ever enters an agent worktree or container.
- Rejected: mounting a signing key into the sandbox (hands the untrusted party
  the identity credential). | Consequences: signatures attest "the loop landed
  this", not "the agent typed this" — which is the honest claim.

### D-3: The merge queue is the distributed merge lock  (Status: Accepted)
- Context: the flock cannot span machines; spec 00079 chose block-on-merge and
  deferred phase-PR freshness. | Decision: team mode routes all landing through
  PRs (pr or phase strategies) with GitHub's merge queue serializing merges and
  re-running required checks; flywheel ships no cross-machine lock.
- Rejected: a Postgres advisory-lock merge mutex (rebuilds queue semantics
  without CI integration, and still lets local FF bypass protections); NFS flock
  (unreliable by design). | Consequences: criterion 2's guard makes the
  merge/phase strategies explicitly single-machine; distributed operators must
  adopt the queue.

### D-4: Runbook first, implementation second  (Status: Accepted — operator sequencing)
- Context: App auth, signing, and queue adoption are cross-org operational
  changes that should be operated once by hand before being automated.
- Decision: this spec ships the enforced defaults (init, policy guard) plus the
  runbook; App-auth/signing implementation is a follow-up spec informed by
  operating the runbook. | Consequences: team mode is documented-manual before
  it is productized; the runbook's accuracy is mechanically pinned by #4.

## Open Questions (accepted gaps)
None.

## Next Steps
Run `/fw-plan 00081-FEATURE-team-mode-github-control-plane` to compile these
criteria into flywheel tasks and graders (after 00078/00079 sequencing per the
program plan).

# Team mode: GitHub as the control plane

Flywheel's default posture is one machine, one operator: landings serialize through a local flock (`.flywheel/.merge.lock`) that is advisory, kernel-local, and single-machine by construction. Team mode is multiple workers on multiple machines sharing a Postgres claim store — and GitHub owning everything the flock cannot: merge serialization, base-branch protection, and review.

Policy load enforces the boundary (spec 00081): `[execution] mode` `= "distributed"` combined with a landing that serializes through the local lock — `[submit] strategy` `merge` or `phase`, or any `[[submit.tiers]]` (its tiers 0/1 route through them) — is a `PolicyError` naming both settings. The distributed landing route is `pr`.

## Feature vs operator setup

What flywheel ships versus what you configure once per GitHub org/repo. Everything in the right column is executed on GitHub, not in `flywheel.toml`.

| Concern | Flywheel feature (shipped) | Operator setup (GitHub) |
|---|---|---|
| Claims and leases across machines | Postgres claim store (`[store] backend` `= "postgres"`; leases already span machines) | A Postgres instance reachable by every worker |
| Merge serialization | Local flock for `merge`/`phase`; load-time guard refusing them under distributed mode | Merge queue on the base branch |
| Base-branch protection | `[submit] protected_paths` (worker-side landing refusal) | Ruleset: no direct pushes to the base |
| Verification before merge | Task graders, `[submit] verify`, held-out gate — all pre-push | Required status checks, re-run by the queue |
| Review unit | `pr` strategy PR bodies carry grader receipts; phase PRs aggregate a whole phase (spec 00079) | Approval rules; worker identity barred from self-approval |
| Worker identity | Shells to ambient `gh` auth (works single-operator today) | GitHub App installation (below) |
| Commit signing | The land-time message-rewrite point exists (spec 00078 trailer stamping) | App-held key host-side; signing implementation is a follow-up spec |

## Worker identity: a GitHub App

Use an org-owned GitHub App, not a machine-user PAT (a licensed seat with coarse revocation that rulesets treat like a human) and not personal tokens (work attribution collapses onto whoever ran the worker — the provenance failure specs 00078+ exist to fix).

1. Create the App on the org: permissions **Contents: read and write** (push task/phase branches), **Pull requests: read and write** (open/refresh PRs), Metadata: read. No webhook, no user-facing UI.
2. Install the App on each consumer repo.
3. On each worker host, mint an installation token (App private key -> JWT -> installation token, 60-minute TTL) and expose it as `GH_TOKEN`, then `gh auth setup-git` so pushes and `gh pr` calls both use it. Token refresh is a host-side wrapper/cron concern; flywheel itself only shells to `gh`.
4. Commits and PRs then attribute to `<app-slug>[bot]` — auditable, revocable per installation, and excludable from approval rules.

**Signing design (deferred implementation).** Spec 00078's land-time stamping already rewrites commit messages host-side at the exact landing point; signing keys off that same rewrite: the worker re-signs the rewritten commits with the App-held key, host-side. The key never enters an agent worktree or container — the sandbox is the untrusted party. The signature attests "the loop landed this", not "the agent typed this". Implementation is a follow-up spec once this runbook has been operated once.

## Ruleset requirements on the base branch

Configure a ruleset (repo Settings -> Rules -> Rulesets) targeting the base branch:

1. **Restrict pushes** — no direct pushes for anyone, including the App. Every landing arrives as a PR; this is the remote enforcement of what the policy guard enforces locally.
2. **Required status checks** — wire the repo gate (the same command as `[submit] verify`, e.g. `scripts/check.sh`) as a required check. The merge queue re-runs required checks per queue entry, so nothing merges that was not verified against the exact tree it lands on — the same invariant `[submit] verify` enforces on the local paths.
3. **Merge method: merge commits** — phase PRs (spec 00079) MUST merge as merge commits. Phase archival is a git-ancestry predicate; a squash or rebase merge breaks ancestry and surfaces as a merge-method-mismatch refusal, and the phase never archives.
4. **Approvals** — require at least one review; dismiss stale approvals on new pushes. GitHub already refuses PR-author self-approval, so the App cannot approve its own PRs; do not weaken that by adding the App to any approval allowlist.
5. **No bypass actors** — the App must not be a ruleset bypass actor. Bypass would re-open the direct-push lane the ruleset exists to close.

## The merge queue is the distributed merge lock

Enable GitHub's merge queue on the base branch. Flywheel deliberately ships no cross-machine lock (a Postgres advisory-lock mutex would rebuild queue semantics without CI integration; NFS flock is unreliable by design): the queue serializes merges and re-runs required checks against the exact tree that lands. Pairing with spec 00079's block-on-merge: a completed phase stays active until its phase PR merges, and the queue supplies ordering and freshness for concurrent phase PRs — which is why 00079 shipped no freshness daemon.

Worker-side policy for team mode:

```toml
[store]
backend = "postgres"          # FLYWHEEL_PG_DSN in the environment, never here

[execution]
mode = "distributed"

[submit]
strategy = "pr"
remote = "origin"
pr_base = "main"
verify = "scripts/check.sh"
protected_paths = [".github/**", "flywheel.toml", "scripts/**"]
```

Run each worker with `fw worker`; `fw status` and `fw live` read the shared store from any machine. See `fw docs strategy` for the landing seam and `fw docs configuration` for every key above.

# 00041 — Sandbox retention policy (increment E of 00036)

Status: spec. Increment E of [00036](00036-sandbox-deploy-model.md): make the
worker's teardown behavior configurable via `[sandbox.retention]`. The last
self-contained increment before the container-backend arc (F -> G).

## Why

`GitWorktreeSubmitter` hardcodes teardown: `DONE` -> destroy the worktree+branch,
non-`DONE` -> park for forensics. `[sandbox.retention]` already parses
(`on_done`, `on_failure`, `sweep_days`) but is inert. E wires the two
submit-time knobs so an operator can pick **ephemeral** (destroy failed
worktrees immediately — no forensics left around) or **preserve** (keep a DONE
worktree for inspection).

## Scope (decided)

- **Submit-time retention only.** `on_done` (`destroy` | `preserve`) and
  `on_failure` (`park` | `destroy`), threaded
  `worker.main` -> `build_merge_submitter` -> `GitWorktreeSubmitter`. Defaults
  (`destroy`/`park`) equal today's behavior — non-breaking.
- **`sweep_days` stays CLI** (`--worktree-retention-days`); policy-threading it is
  a deferred minor.
- **Merge strategy only.** If `GitPullRequestSubmitter` carries separate park
  logic, apply the same config there; otherwise it inherits.
- **No general `teardown()` seam.** The 00036 design floated a `SubmitStrategy.
  teardown()` hook; that belongs to the container backend (G), which needs an
  explicit stop/rm. Worktree retention already lives in `_submit` — wire the
  config there, don't add the seam yet (same lesson as increment C).

## Success criteria (each lowers to a grader)

**SC-1 — `on_done` controls DONE teardown.** With `on_done = "preserve"`, a DONE
task's branch still FF-merges into the base but the worktree is **kept**; the
default `on_done = "destroy"` removes it (today). *Grader:*
`test_sandbox_retention.py`.

**SC-2 — `on_failure` controls failure teardown.** With `on_failure =
"destroy"`, a non-DONE task's worktree+branch are **removed**; the default
`on_failure = "park"` preserves them (today). *Grader:*
`test_sandbox_retention.py`.

**SC-3 — Threaded from policy; back-compat.** The config flows
`policy.sandbox.retention` -> `build_merge_submitter` ->
`GitWorktreeSubmitter`; the defaults reproduce today's destroy/park behavior and
the full suite stays green. *Grader:* full suite.

## Out of scope

`sweep_days` policy threading (stays CLI); the general `teardown()` seam
(container backend, G); zero-commit-DONE + preserve interaction (keep today's
cleanup for the empty-branch case).

## Task

- `sandbox-retention` (worktree) — SC-1/2/3. `GitWorktreeSubmitter` reads
  `on_done`/`on_failure`; `build_merge_submitter` + `worker.main` thread
  `policy.sandbox.retention`.

## Anchor files

- `packages/flywheel-worktree/src/flywheel_worktree/worker.py` —
  `GitWorktreeSubmitter.__init__` (add `on_done`/`on_failure`); `_submit`
  (~line 466: the non-DONE park branch and the DONE merge -> `_cleanup`);
  `build_merge_submitter` (~line 1201) and `main` (~line 1267, reads
  `policy.protected_paths`/`sandbox_setup`) thread `policy.sandbox.retention`.
- `packages/flywheel-worktree/src/flywheel_worktree/_submit_registry.py` — the
  builder signature the registry dispatches on.
- `packages/flywheel-orchestrator/src/flywheel_orchestrator/_policy.py` —
  `SandboxRetention` (`on_done`, `on_failure`, `sweep_days`; ~line 187), parsed.

# flywheel-worktree

The reference consumers built on [`flywheel-orchestrator`](../flywheel-orchestrator):
the git-worktree landing strategies plus a daemon poll loop. One worked example
of "here's what you can build on flywheel" — copy it, or swap the strategy for
your own.

Depends on `flywheel-orchestrator`. Library only; the daemon is launched
through the product shell as `flywheel worker` (which delegates in-process
to `flywheel_worktree.worker.main`).

## What it does

- **Landing strategies** — each task runs in its own worktree on branch
  `flywheel/<phase>/<task-id>`. `GitWorktreeSubmitter` (the default `merge`
  strategy) fast-forward-merges the branch into the base on `done` and removes
  the worktree; `GitPullRequestSubmitter` (the `pr` strategy, `flywheel_worktree.pr`)
  pushes the branch and opens a PR with the grader receipts in the body instead.
  Non-`done` work is parked for forensics. All git lives here, never in flywheel
  core (the orchestrator's `SubmitStrategy` / `prepare_sandbox` / `submit` seam).
- **Verified landing** — if the base advanced under a finished task, the branch
  is rebased and its command graders re-run against the rebased tree before the
  merge (nothing lands unverified against the exact base it lands on), and a
  `[submit] protected_paths` gate refuses to land work that rewrites the
  verification surface (grader config, CI).
- **Sandbox provisioning** — an optional `[sandbox] setup` command runs in each
  freshly created worktree before the agent enters (e.g. `uv sync`), so tasks
  never pay discovery cost for a bare checkout.
- **Daemon loop** — re-invokes `orchestrate` after recording each phase's base
  ref and archiving completed phases; per-run forensics logs; a merge flock so
  several workers can share one repo. The worker never creates commits on the
  operator's branch.

## Install / run

```bash
uv add flywheel
flywheel worker --once          # one drain cycle
flywheel worker                 # daemon loop
```

Run several against one store for parallelism — leases keep workers off the same
task, the flock serializes base merges.

## Build your own

`GitWorktreeSubmitter` and `GitPullRequestSubmitter` implement the
orchestrator's `SubmitStrategy` seam. For a different flow (in-place, remote
sandbox, a non-git VCS), implement `prepare_sandbox` / `submit` and pass your
object as `orchestrate(strategy=...)` — these two are the template.

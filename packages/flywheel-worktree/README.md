# flywheel-worktree

A reference consumer built on [`flywheel-orchestrator`](../flywheel-orchestrator):
the git-worktree submit strategy plus a daemon poll loop. One worked example of
"here's what you can build on flywheel" — copy it, or swap the strategy for
your own.

Depends on `flywheel-orchestrator`. Library only; the daemon is launched
through the product shell as `flywheel worker` (which delegates in-process
to :func:`flywheel_worktree.worker.main`).

## What it does

- **Git submit** — each task runs in its own worktree on branch
  `flywheel/<phase>/<task-id>`. On `done` the branch is fast-forward merged into
  the base and the worktree removed; otherwise it's parked for forensics. All
  git lives here, never in flywheel core (the orchestrator's `prepare_sandbox` /
  `submit` seam).
- **Daemon loop** — re-invokes `orchestrate` after committing newly-dropped task
  files and archiving completed phases; per-run forensics logs; a merge flock so
  several workers can share one repo.

## Install / run

```bash
uv add flywheel
flywheel worker --once          # one drain cycle
flywheel worker                 # daemon loop
```

Run several against one store for parallelism — leases keep workers off the same
task, the flock serializes base merges.

## Build your own

`GitWorktreeSubmitter` implements the orchestrator's submit seam. For a
different flow (in-place, PR-based, remote sandbox), write your own
`prepare_sandbox` / `submit` and call `flywheel_orchestrator.orchestrate`
directly — this package is the template.

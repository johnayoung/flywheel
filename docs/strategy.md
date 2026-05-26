# Strategy

Strategy is the work between "agent finished" and "result committed / merged / submitted" — branches, worktrees, commits, merges, submissions, review gates.

**Strategy lives in the consumer of the loop, not inside the loop.** `flywheel.harness` owns the lifecycle, envelopes, graders, attempts, and events. Strategy lives one layer up because it is task-class-specific (code tasks need commits; research / config / non-code tasks don't) and the loop is task-agnostic by design (see [vision.md](vision.md), "What it is not").

## The seam

flywheel exposes three contract points the consumer wraps around. No `Strategy` Protocol, no hooks the loop calls into, no default no-op to maintain.

| Direction      | Surface                          | What it carries                                                                                              |
| -------------- | -------------------------------- | ------------------------------------------------------------------------------------------------------------ |
| In             | `flywheel run --sandbox <dir>`   | The working environment for this attempt. The consumer provisions it (worktree, branch, snapshot, container, plain dir). |
| Out, streaming | `events` table (`harness.*`)     | Recorded in real time during the run; consumers can tail or query.                                           |
| Out, terminal  | `lifecycle.status`               | `done` / `failed` / `interrupted` — what to do next is the consumer's call.                                  |

## Reference implementation

`.workflow/task-worker.sh` is the current dogfooding strategy. It pins flywheel itself to `.workflow/lkg/` (see [lkg.md](lkg.md)) and hands the live repo root as the sandbox — there is no per-task isolation yet, so concurrent edits within a phase share one working tree. Adding per-task isolation is the next strategy-layer upgrade; it does not require any change to `src/flywheel/`.

## Future strategies

Per-task worktrees, branch-per-task with auto-merge on `done`, PR-creation on `done`, multi-worker scheduling, container-based isolation, non-git submission flows — all live above the loop. Each reads the events stream and the terminal `lifecycle.status` row and acts accordingly. flywheel does not need to know they exist.

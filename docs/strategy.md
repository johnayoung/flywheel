# Strategy

Strategy is the work between "agent finished" and "result committed / merged / submitted" — branches, worktrees, commits, merges, submissions, review gates.

**Strategy lives in the consumer of the loop, not inside the loop.** `flywheel_core.harness` owns the lifecycle, envelopes, graders, attempts, and events. Strategy lives one layer up because it is task-class-specific (code tasks need commits; research / config / non-code tasks don't) and the loop is task-agnostic by design (see [vision.md](vision.md), "What it is not").

## The seam

The named seam is `flywheel_orchestrator.SubmitStrategy` (`_strategy.py`): a structural protocol bundling the two hooks `orchestrate` calls around every run. No registration, no base class — any object with conforming methods works, passed as `orchestrate(strategy=...)` (or as the standalone `prepare_sandbox`/`submit` callables).

| Hook              | When                                | Contract                                                                                       |
| ----------------- | ----------------------------------- | ---------------------------------------------------------------------------------------------- |
| `prepare_sandbox` | before the run                      | Returns the directory the task runs in (worktree, container mount, plain dir). May raise: the task is skipped for the session, peers are never starved. |
| `submit`          | after the run, lease still held     | Acts on the terminal status (`SubmitRequest` carries the validated `Task`, run id, status, sandbox). MUST NOT raise — it records its own outcome.       |

`submit` running under the task's lease means two workers never land the same task concurrently. The core loop also exposes the `events` table (`harness.*`, streaming) and the terminal `lifecycle.status` row for strategies that observe rather than wrap.

## Reference implementation

`flywheel_worktree.worker.GitWorktreeSubmitter` is the reference `SubmitStrategy`. Each task runs in its own git worktree on branch `flywheel/<phase>/<task-id>`, branched off the worker's starting branch; on `done` the branch is fast-forward-merged back into that base and the worktree removed, while failed/interrupted worktrees are parked for forensics. If the base advanced under a finished task, the branch is rebased once and its command graders re-run against the rebased tree before the merge — nothing lands that was not verified against the exact base it lands on. The seam keeps all git in the consumer — flywheel core stays git-free.

## Future strategies

One `SubmitStrategy` per landing policy, forming a trust ladder consumers climb as graders earn trust: emit a patch artifact (touch no refs), push a branch and open a PR with grader receipts, auto-merge on green, direct FF-merge (the reference, full autonomy). Container-based isolation and non-git submission flows fit the same two hooks. flywheel does not need to know any of them exist.

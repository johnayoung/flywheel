# Strategy

Strategy is the work between "agent finished" and "result committed / merged / submitted" — branches, worktrees, commits, merges, submissions, review gates.

**Strategy lives in the consumer of the loop, not inside the loop.** `flywheel_core.harness` owns the lifecycle, envelopes, graders, attempts, and events. Strategy lives one layer up because it is task-class-specific (code tasks need commits; research / config / non-code tasks don't) and the loop is task-agnostic by design (see [vision.md](vision.md), "What it is not").

## The seam

The named seam is `flywheel_orchestrator.SubmitStrategy` (`_strategy.py`): a structural protocol bundling the two hooks `orchestrate` calls around every run. No base class — any object with conforming methods satisfies the protocol, passed as `orchestrate(strategy=...)` (or as the standalone `prepare_sandbox`/`submit` callables). Selecting a built-in strategy by name (the `flywheel.toml` `[submit] strategy` key) routes through the `SUBMIT_STRATEGIES` plugin registry (`flywheel_worktree._submit_registry`), which maps `merge`/`pr` to their submitters.

| Hook              | When                                | Contract                                                                                       |
| ----------------- | ----------------------------------- | ---------------------------------------------------------------------------------------------- |
| `prepare_sandbox` | before the run                      | Returns the directory the task runs in (worktree, container mount, plain dir). May raise: the task is skipped for the session, peers are never starved. |
| `submit`          | after the run, lease still held     | Acts on the terminal status (`SubmitRequest` carries the validated `Task`, run id, status, sandbox). MUST NOT raise — it records its own outcome.       |

`submit` running under the task's lease means two workers never land the same task concurrently. The core loop also exposes the `events` table (`harness.*`, streaming) and the terminal `lifecycle.status` row for strategies that observe rather than wrap.

## Shipped strategies

One `SubmitStrategy` per landing policy, forming a trust ladder consumers climb as graders earn trust. Selected per repo via `flywheel.toml` `[submit] strategy`.

- **`merge`** (default) — `flywheel_worktree.worker.GitWorktreeSubmitter`, full autonomy. Each task runs in its own git worktree on branch `flywheel/<phase>/<task-id>`, branched off the worker's starting branch; on `done` the branch is fast-forward-merged back into that base and the worktree removed, while failed/interrupted worktrees are parked for forensics. If the base advanced under a finished task, the branch is rebased once and its command graders re-run against the rebased tree before the merge — nothing lands that was not verified against the exact base it lands on.
- **`pr`** — `flywheel_worktree.pr.GitPullRequestSubmitter`, review-gated. Same provisioning; on `done` the branch is pushed to the remote and a PR opened (or refreshed) with the run's grader receipts rendered in the body, so reviewers see how "done" was decided. Nothing merges locally — review/CI own the merge. Park semantics are identical.

Both honor `[submit] protected_paths`: a finished branch touching the verification surface (grader configs, CI) never lands, regardless of strategy. The seam keeps all git in the consumer — flywheel core stays git-free.

## Future strategies

Emit a patch artifact (touch no refs), auto-merge on green, container-based isolation, non-git submission flows — all fit the same two hooks. flywheel does not need to know they exist.

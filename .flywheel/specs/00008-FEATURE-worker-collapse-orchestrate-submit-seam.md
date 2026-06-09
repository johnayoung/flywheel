# Feature: Collapse the task worker onto `orchestrate` + a git-submit seam

## Summary

Replace the 858-line `.workflow/task-worker.sh` with a Python consumer (`.workflow/worker.py`, ~790 lines incl. docstrings) that drives `flywheel.orchestrator.orchestrate` and injects git through a new optional seam on `orchestrate`: two callbacks, `prepare_sandbox(SandboxRequest) -> Path` and `submit(SubmitRequest) -> None`. The raw-LOC drop is modest because git-worktree mechanics are irreducible and must live in the consumer (the boundary); the real collapse is the ~346 lines of scheduling / lease / stranded-recovery / signal / subshell machinery the bash reimplemented, which is deleted and delegated to flywheel. What remains is purely git-submit + daemon — now typed and unit-tested. No git enters `src/flywheel/`.

## Background

`task-worker.sh` reimplemented in bash machinery flywheel already owns in Python: task selection (`select_next_task`), prerequisite promotion, reactive unblock/resume (`recheck_blocked_lifecycle`), per-task claim leases + heartbeat (`ClaimStore`), and stranded-lifecycle recovery (`recover_stranded_lifecycles`) — all of which live in `orchestrate`. It also carried obsolete signal machinery (`set -m`, escalating SIGTERM→SIGKILL on process groups, `reconcile_to_interrupted`): `workflow.run_task_object` now installs SIGTERM/SIGINT handlers that finalize the in-flight lifecycle to `interrupted` and re-raise `CancelledError`, so graceful shutdown no longer needs the worker to chase process groups.

What is genuinely the worker's job — and must not move into flywheel (`docs/strategy.md`, `docs/vision.md`) — is the git submit layer and the daemon poll loop. The only thing missing was a way to inject per-task git into `orchestrate`'s scheduling loop without duplicating that loop in the consumer.

## Scope

### In Scope
- **Seam on `orchestrate`** (`src/flywheel/orchestrator.py`): optional `prepare_sandbox: Callable[[SandboxRequest], Path]` and `submit: Callable[[SubmitRequest], None]`. New frozen dataclasses `SandboxRequest` (`task_id`, `task_file`, `run_id`, `mode`) and `SubmitRequest` (`task_id`, `task_file`, `run_id`, `status`, `sandbox`), plus type aliases `SandboxProvider` / `Submitter`. Exported from `flywheel`.
  - Defaults preserve today's behavior exactly: sandbox = `sandbox_root/<task-id>`, no submit. `flywheel orchestrate` CLI is unchanged (plain dirs).
  - The resolved sandbox is used both as the run's `--sandbox` and as the `cwd` for the blocked recheck, so predicates grade the prepared tree.
  - `submit` runs after the run finalizes but **before** the lease is released, so a consumer merge acts under the same exclusivity that kept peers off the task. `submit` must not raise.
  - `prepare_sandbox` may raise; `orchestrate` catches it, skips that task for the session (already in `attempted_fresh`/`attempted_resume`), and keeps draining peers — the starvation guarantee the old `SPAWN_FAILURES` breaker gave.
- **Consumer `.workflow/worker.py`** (replaces `task-worker.sh`):
  - `GitWorktreeSubmitter.prepare` — worktree create / reuse-on-retry + rebase-onto-base / recreate-on-branch-only / refuse-to-clobber, branch `flywheel/<phase>/<task-id>`, worktree `.workflow/worktrees/<task-id>`.
  - `GitWorktreeSubmitter.submit` — on `done`: uncommitted-park / zero-commit-cleanup / FF-merge (rebase-then-FF, else park); on `failed`/`interrupted`: park. All base-branch mutations under a repo-level merge flock.
  - Daemon loop: `commit_task_files` (pickup of newly-dropped JSON, flock'd) → `orchestrate(..., prepare_sandbox, submit)` → `archive_completed_phases` → interruptible sleep; `retention_sweep` at startup; `Heartbeat` thread over `collect_live_rows`; `--once` for a single cycle.
- `.gitignore`: add `.workflow/.merge.lock`; update the worktrees comment.
- Tests: `tests/test_orchestrator_submit_seam.py` (seam) and `tests/test_worker.py` (git submit logic + a `run_once` integration). Remove `tests/test_task_worker_circuit_breaker.py` (bash-specific).
- Doc touch-ups in `docs/strategy.md` and the `orchestrator.py` module docstring.

### Out of Scope
- Any change to `flywheel.task` / `flywheel.lifecycle` (purity invariants).
- Moving worktree/branch/merge logic into flywheel core.
- Remote push / PR creation / non-git submission flows.
- In-process N-slot parallelism: deliberately dropped in favor of N worker processes sharing one store via leases (worker default was already 1). The merge flock serializes their base-branch merges — finer-grained than the old single-instance `.worker.lock`.

## Guarantees preserved

Per-task isolation (`prepare`); worktree reuse + rebase-onto-base on retry (`prepare`); FF-merge on done, park on failure, zero-commit/uncommitted-DONE handling (`submit`); ≤1 base merge open at once (merge flock); pickup of newly-dropped task files (`commit_task_files`); phase archiving (`archive_completed_phases`); stranded recovery (`orchestrate` entry sweep); graceful stop → resumable `interrupted` (`run_task_object` signal path); no starvation on a broken task (`attempted_fresh` + daemon backoff).

## Verification

- `uv run pytest` green (seam + worker tests included); `flywheel orchestrate` defaults unchanged.
- pyright clean on `src/flywheel/orchestrator.py`, `src/flywheel/__init__.py`, `.workflow/worker.py`.
- `uv run python .workflow/worker.py --once` against a scratch repo with no tasks exits 0 after exercising phase-base detection, retention sweep, the real `orchestrate` call, and archive — no agent invoked.
- LOC: 858-line bash → ~790-line typed/tested Python consumer + ~155-line (155 insertions) flywheel seam. The headline is responsibility, not raw lines: ~346 lines of bash scheduling/lease/recovery/signal machinery deleted; the rest is irreducible git-submit + daemon.

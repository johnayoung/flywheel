# Feature: Per-Task Workspace Isolation

## Summary

Give each flywheel task its own git worktree branched off `main`, so a sibling task's in-flight or just-committed edits cannot poison the next task's grader runs. Auto-merge the task branch back to `main` (fast-forward only) on `lifecycle.status=done`. The new layer also subsumes the LKG snapshot's purpose — `.workflow/lkg/` and `promote-lkg.sh` are removed in this spec.

## Background

Phase `02-harness-resilience` shipped four DONE tasks but only one (`raise-sdk-cap`) finalized cleanly. The audit (`.workflow/audits/02-harness-resilience.md`) names two distinct contamination modes that hit `drop-default-transcript-grader` and `drop-implementation-notes`:

- `crash-retry-eligible` added `Status.INTERNAL_ERROR` without updating the enumeration test; every subsequent task in the phase whose graders include `full-suite` inherited a red suite.
- `drop-default-transcript-grader` blocked on the contaminated suite; `drop-implementation-notes` shipped `verify` against a known-red suite.

The audit's recommendation #5 names per-task worktrees as the fix. Per `docs/strategy.md`, this is strategy-layer work: flywheel already exposes `--sandbox <dir>` in, `harness.*` events out, and `lifecycle.status` out. Zero changes to `src/flywheel/` are in scope.

The LKG snapshot at `.workflow/lkg/` (see `docs/lkg.md`) pinned flywheel itself against mid-task edits. With per-task worktrees branched off a real commit on `main`, that pinning is redundant — the branch point *is* the pin.

## Scope

### In Scope
- `.workflow/task-worker.sh` upgraded to create a per-task git worktree before invoking flywheel and pass it as `--sandbox`.
- Worktree path: `.workflow/worktrees/<task-id>/`. Branch: `flywheel/<phase>/<task-id>`.
- Phase base is the branch the worker started on (`main`); each new worktree branches from its live tip at worktree-creation time.
- On `lifecycle.status=done`, wrapper auto-merges the task branch to phase base via fast-forward only; if FF fails, rebase once onto current base then retry FF; if rebase fails, park the worktree for operator handling.
- On `lifecycle.status` in `(failed, interrupted)`, wrapper leaves the worktree on disk for forensics. Worker-startup sweep removes worktrees and their branches whose mtime is older than `--worktree-retention-days` (default 7).
- DONE with uncommitted changes in the worktree is a fail-loud condition; wrapper refuses to merge and parks the worktree.
- DONE with zero commits on the task branch (branch tip == base) is a no-op merge: log it, remove the worktree, leave phase base where it is.
- Worktree-creation failure (disk full, branch collision, dirty index, etc.) exits the wrapper non-zero *before* a lifecycle is created. No flywheel state change. The worker's existing eligibility loop reselects the same task on the next iteration.
- Small bash helper(s) for worktree create / merge / sweep, called from `task-worker.sh`. No new Python module.
- `.workflow/worktrees/` added to `.gitignore`.
- Removal of `.workflow/lkg/` (directory), `.workflow/promote-lkg.sh`, the LKG bootstrap block in `task-worker.sh`, the `uv run --project .workflow/lkg` invocation pattern, and `docs/lkg.md`. References to LKG in `docs/strategy.md`, `CLAUDE.md`, etc. updated to point at the worktree layer instead.

### Out of Scope
- Any change to `src/flywheel/` (would violate the loop/strategy separation locked in by `docs/strategy.md`).
- Multi-worker concurrency. Design must not preclude two `task-worker.sh` processes running in parallel safely (branch names already scope by task id), but no locking work happens here.
- Remote push, PR creation, or any auto-publish of either `main` or task branches.
- The `harness.blocked` follow-through (machine-readable `requires` payload, auto-unblock loop). Tracked separately.
- Non-git submission flows (research-only, config-only, external systems).
- A second-opinion `verify`-precheck grader to catch agents claiming `verify` against a red suite. Out of scope here; see audit "Decisions deferred".

## Requirements

### Functional Requirements

1. **FR-1**: Each task runs in its own git worktree.
   - Acceptance: With two consecutive tasks A and B in the same phase, A's pre-merge edits to any tracked file are not visible inside B's `--sandbox` directory. Verifiable by inserting a sentinel file write in A's agent transcript and asserting absence in B's worktree.

2. **FR-2**: Worktrees live at `.workflow/worktrees/<task-id>/` and use branches named `flywheel/<phase>/<task-id>`.
   - Acceptance: After running a task `T1` in phase `03-foo`, `git worktree list` shows `.workflow/worktrees/T1` on branch `flywheel/03-foo/T1`. The phase segment is extracted from the task file's parent directory under `.workflow/tasks/active/`.

3. **FR-3**: On `lifecycle.status=done`, the wrapper fast-forward-merges the task branch into the phase base, then removes the worktree.
   - Acceptance: After a DONE lifecycle, `git log main` contains the task's commits and `git worktree list` no longer shows the task's worktree. The task branch is deleted.

4. **FR-4**: If fast-forward is not possible at merge time, the wrapper rebases the task branch onto current phase base once, then retries FF.
   - Acceptance: With a contrived scenario where phase base advanced between worktree creation and DONE such that FF would fail, the wrapper produces a successful merge after one rebase. Verifiable in an integration test.

5. **FR-5**: If the post-rebase FF still fails (rebase conflict), the wrapper aborts the rebase, leaves the worktree intact on its branch, logs the failure with the worktree path, and continues the worker loop.
   - Acceptance: Conflict scenario produces a worktree visible in `git worktree list`, a log line containing the path, and a worker that proceeds to select the next eligible task.

6. **FR-6**: On `lifecycle.status` in `(failed, interrupted)`, the worktree and branch are preserved.
   - Acceptance: After a lifecycle terminates non-DONE, the worktree is present on disk and the branch exists. No deletion happens at merge time.

7. **FR-7**: Worker startup sweeps `.workflow/worktrees/*` and removes worktrees whose mtime is older than `--worktree-retention-days` days (default 7), including the associated branches.
   - Acceptance: A worktree dated 8 days ago is gone after the next worker start; a worktree dated 6 days ago is preserved. Acceptance covers branch deletion too.

8. **FR-8**: DONE with uncommitted changes (staged or unstaged) in the worktree is treated as a failure of this step. The wrapper does not auto-commit, does not merge, leaves the worktree parked, and emits a clear log line.
   - Acceptance: Contrived test where the agent leaves an unstaged change at DONE produces no merge, a preserved worktree, and a worker-log line identifying the path.

9. **FR-9**: DONE with zero commits on the task branch is a no-op merge. Worktree is removed, branch deleted, phase base unchanged, log notes "no commits to merge".
   - Acceptance: A task whose agent reaches DONE without committing anything produces no change to `git log main`, removes the worktree, and logs the no-op.

10. **FR-10**: Worktree-creation failure exits the wrapper before any flywheel state is touched.
    - Acceptance: With an injected `git worktree add` failure, no lifecycle row is created in SQLite for that task on that iteration, no `harness.*` event is recorded, and the next worker iteration reselects the same task.

11. **FR-11**: LKG removal is complete and atomic with this feature.
    - Acceptance: After the change lands, `.workflow/lkg/` and `.workflow/promote-lkg.sh` do not exist; `docs/lkg.md` does not exist; `task-worker.sh` no longer references `LKG_DIR` or `uv run --project .workflow/lkg`; `docs/strategy.md`'s reference implementation paragraph names worktrees, not LKG; the `CLAUDE.md` "Authoritative specs in `docs/`" list no longer lists `lkg.md`.

12. **FR-12**: `.workflow/worktrees/` is gitignored.
    - Acceptance: `git status` after creating a worktree shows no untracked entries under `.workflow/worktrees/`.

### Non-Functional Requirements
- **Performance**: Worktree creation should add < 2s of latency per task start on a typical SSD-backed checkout. `git worktree add` is roughly equivalent to a small `cp` plus refs setup; this should hold.
- **Disk**: Worktree retention is bounded by the 7-day sweep. On a worker doing 10 tasks/day with a 100 MB repo, worst-case retained disk is ~7 GB; acceptable. Document the knob.
- **Security**: Standard practices. Worktrees inherit repo permissions. No new attack surface.
- **UX**: Operator can `cd .workflow/worktrees/<task-id>/` to inspect any parked worktree. The branch name `flywheel/<phase>/<task-id>` is greppable via `git branch --list 'flywheel/*'`.

## Behavior Specification

### Happy Path
1. Worker selects task `T1` from `.workflow/tasks/active/03-foo/T1.json`.
2. Wrapper runs the retention sweep (worktrees older than 7 days removed).
3. Wrapper creates worktree: `git worktree add .workflow/worktrees/T1 -b flywheel/03-foo/T1 main`.
4. Wrapper invokes flywheel: `uv run python -m flywheel.workflow run T1.json --sandbox .workflow/worktrees/T1 ...`.
5. Agent runs in the worktree, commits its work to `flywheel/03-foo/T1`.
6. Harness emits `harness.attempt_finalized` and lifecycle transitions to `done`.
7. Wrapper checks worktree for uncommitted changes — none.
8. Wrapper attempts `git merge --ff-only flywheel/03-foo/T1` from a `main` checkout. Succeeds.
9. Wrapper runs `git worktree remove .workflow/worktrees/T1` and `git branch -d flywheel/03-foo/T1`.
10. Worker loops to next task; new worktree branches from the now-advanced `main`.

### Error Handling
| Error Condition                                    | Expected Behavior                                                                                                                              |
| -------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------- |
| `git worktree add` fails (disk full, etc.)         | Wrapper exits non-zero, no lifecycle created. Worker eligibility loop reselects the same task on next iteration.                               |
| Branch name collision (`flywheel/<phase>/<task>` exists) | Treat as worktree-creation failure. Same fail-loud behavior. Operator clears the stale branch.                                          |
| Lifecycle ends `failed` (validation_failed)        | Worktree and branch preserved. No merge. Worker proceeds to next task. Eligible for retention sweep after 7 days.                              |
| Lifecycle ends `interrupted` (SIGINT, crash)       | Same as `failed`: preserve, no merge.                                                                                                          |
| DONE with uncommitted changes                      | Wrapper refuses to merge. Worktree preserved. Log line names the path. Operator decides.                                                       |
| DONE with zero commits (branch tip == base)        | No-op merge. Worktree removed, branch deleted, phase base unchanged. Logged as "no commits to merge".                                          |
| FF merge fails (phase base moved)                  | Wrapper rebases task branch onto current base once. On rebase success, retry FF. On rebase failure, abort rebase, park worktree, log path.     |
| Worker SIGINT mid-task                             | Existing flywheel handler finalizes the lifecycle as `interrupted` via the SIGINT path in `src/flywheel/workflow.py`. Worktree is preserved (see `failed/interrupted` row). Wrapper does not need new SIGINT handling beyond what's already there. |

### Edge Cases
| Case                                                              | Expected Behavior                                                                                                                                    |
| ----------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------- |
| Task file lives directly under `.workflow/tasks/active/` (no phase subdir) | Branch name falls back to `flywheel/_root/<task-id>`. Documented; not a normal path.                                                          |
| `.workflow/worktrees/` does not exist on first run                | Wrapper `mkdir -p` it.                                                                                                                               |
| Stale worktree from a prior worker death (no live lifecycle)      | Retention sweep covers it; until then, `git worktree add` for the *same task id* fails because the path exists. Wrapper logs and exits (FR-10 path). |
| Operator manually deleted the worktree directory but kept the branch | `git worktree prune` is called as part of the retention sweep to clean dangling worktree entries. Then `git worktree add` succeeds next time.       |
| Two tasks in different phases share the same task id              | Branch names differ (`flywheel/<phaseA>/<id>` vs `flywheel/<phaseB>/<id>`); worktree directory names collide. Treat as worktree-creation failure (the existing live worktree wins); operator resolves. |
| Worker restarts mid-task                                          | flywheel's existing recovery sweep transitions the stranded lifecycle to `interrupted`. The worktree stays. The branch stays. Retention sweep handles eventual cleanup. |
| Task A's merged commit breaks task B's tests                      | Not contamination — this is a real bug from A. B sees the failure; B's grader records it. Operator addresses by either fixing A's bug or expanding A's scope. This spec does not hide such failures. |
| Task does not commit but does write artifacts to the sandbox      | Same as "uncommitted changes" — fail-loud. Untracked files in the worktree at DONE count as uncommitted state for purposes of FR-8.                  |

## Technical Context

### Affected Apps
- `.workflow/task-worker.sh` — adds worktree creation, merge, sweep, and removes LKG bootstrap / `uv run --project .workflow/lkg/` invocation pattern.
- New bash helper(s) (e.g., `.workflow/worktree-create.sh`, `.workflow/worktree-merge.sh`, `.workflow/worktree-sweep.sh`) — exact shape is an implementation choice for `/task`.
- `.gitignore` — add `.workflow/worktrees/`.
- `docs/strategy.md` — update the "Reference implementation" paragraph: name worktrees, drop LKG reference.
- `CLAUDE.md` — drop `lkg.md` from the authoritative-specs list.
- `docs/lkg.md` — delete.
- `.workflow/lkg/` — delete (directory and contents).
- `.workflow/promote-lkg.sh` — delete.

### Integration Points
- **flywheel `--sandbox`** (in `src/flywheel/workflow.py`): the only flywheel-facing seam used. No change to its behavior.
- **flywheel `lifecycle.status`** (in SQLite): read after the run completes to decide merge-vs-park. Bash uses `sqlite3` CLI; query is one row per `run_id` (or by `task_id` ordered by `started_at DESC`).
- **Git**: `git worktree add/remove/list/prune`, `git branch -d`, `git merge --ff-only`, `git rebase`, `git status --porcelain` for the uncommitted-changes check.

### Relevant Existing Code
- `.workflow/task-worker.sh:99-106` — LKG bootstrap block to be removed.
- `.workflow/task-worker.sh:116-122` — `run_workflow` helper that routes via LKG; rewrite to use the live tree (per-task worktrees obviate the LKG indirection).
- `.workflow/task-worker.sh:184-194` — current `--sandbox $REPO_ROOT` invocation; change to per-task worktree path.
- `src/flywheel/workflow.py:529` — `--sandbox` argument plumbing; unchanged.
- `src/flywheel/workflow.py:433-444` — existing SIGINT/CancelledError handler that finalizes interrupted lifecycles; relied on, unchanged.
- `.workflow/promote-lkg.sh` — deleted.
- `docs/lkg.md` — deleted.
- `.workflow/audits/02-harness-resilience.md:111-122` — names the contamination chain this spec fixes.
- `.workflow/audits/02-harness-resilience.md:131-136` — recommendation #5 for the per-task isolation approach.

## Decisions Log

| Decision                              | Choice                                                                                          | Rationale                                                                                                                                                              |
| ------------------------------------- | ----------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Isolation mechanism                   | git worktree per task                                                                            | Native git, no full clone cost, branch-per-task model falls out for free. Matches audit recommendation #5.                                                              |
| Relationship to LKG                   | Replace LKG entirely with worktrees; remove `.workflow/lkg/`, `promote-lkg.sh`, `docs/lkg.md`    | A pinned phase-base commit on `main` subsumes LKG's purpose. One pinning mechanism, not two.                                                                            |
| Merge timing                          | On `lifecycle.status=done`, FF-only auto-merge                                                   | Keeps the autonomous-worker model. FF reflects the serial, single-worker reality.                                                                                       |
| Sibling base                          | Live tip of phase base                                                                          | After A merges, B branches from the advanced `main`. Isolation is *during* the run; A's merged-and-broken work is a real bug, not contamination.                        |
| Cleanup on `failed`/`interrupted`     | Keep for forensics, sweep after 7 days (configurable)                                            | Matches the "Monday inspection" cadence. Bounded disk usage. Operator can override.                                                                                     |
| Uncommitted changes at DONE           | Fail-loud, do not auto-commit, park worktree                                                     | The `/task` skill template already requires agents to commit. Auto-stashing risks landing junk (cache, IDE droppings).                                                  |
| Empty branch at DONE                  | No-op merge; remove worktree; advance phase base only if FF condition trivially holds            | Honest accounting; doc/analysis tasks that legitimately don't commit are common.                                                                                        |
| Worktree-creation failure              | Fail before lifecycle creation; worker reselects on next iteration                              | Environmental failures shouldn't pollute the lifecycle audit. The retry loop is the existing worker eligibility loop, not flywheel's `max_retries`.                     |
| Phase base                            | Current branch the worker started on (typically `main`)                                          | Matches today's mental model. No new configuration surface.                                                                                                             |
| Branch / path naming                  | `.workflow/worktrees/<task-id>/`, branch `flywheel/<phase>/<task-id>`                            | Greppable, mirrors `.workflow/tasks/active/<phase>/<task-id>.json`, sorts by phase.                                                                                     |
| Wrapper shape                         | Pure bash in `task-worker.sh` + small helper scripts                                             | Matches existing strategy-layer style (`promote-lkg.sh`, `task-worker.sh`). No second runtime.                                                                          |
| Multi-worker concurrency              | Out of scope; design must not preclude                                                          | Single-worker assumption holds today. Branch-per-task naming already permits future concurrency. No locking work in this spec.                                          |
| FF-fallback behavior                  | Rebase once; on success FF; on rebase failure, park worktree                                     | Covers the rare race; never silently produces a dirty merge commit; lets the operator handle real conflicts.                                                            |
| Remote push / PR creation             | Out of scope                                                                                    | Separate strategy concern per `docs/strategy.md` "Future strategies".                                                                                                   |
| Retention default                     | 7 days, configurable via `--worktree-retention-days`                                             | Inspection window vs. disk pressure trade-off. Operator can extend.                                                                                                     |
| Python wrapper                        | None                                                                                            | Bash + `sqlite3` CLI + `git` cover all needs at this scope. Avoid introducing a second runtime in the strategy layer.                                                   |

## Open Questions

None. All open questions from the input were resolved during discovery. Defer-list (already noted in the audit and explicitly out of scope here):

- `harness.blocked` machine-readable `requires` payload and auto-unblock loop.
- A `verify`-precheck grader / second-opinion model for agents claiming `verify` against a known-red suite.

## Next Steps

Run `/task 00003-FEATURE-per-task-workspace-isolation` to generate implementation tasks from this spec.

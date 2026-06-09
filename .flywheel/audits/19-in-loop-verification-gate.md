# Phase audit: 18-restore-worker-run-logs + 19-in-loop-verification-gate

**Source:** `.workflow/flywheel.sqlite`, `logs/worker/`, `.workflow/tasks/archive/18-restore-worker-run-logs`, `.workflow/tasks/archive/19-in-loop-verification-gate`
**Audited:** 2026-06-04
**Wall-clock window:** `2026-06-04T13:30:04Z` -> `2026-06-04T14:10:59Z` (~41 min, single `worker.py --once` cycle)

Both phases ran in one worker invocation and are diagnosed together because they share a single root cause. Phase 18 is one task; phase 19 is seven.

## Summary

| Metric                              | Value |
| ----------------------------------- | ----- |
| Tasks in phase(s)                   | 8     |
| Tasks reaching DONE                 | 8     |
| Total lifecycles                    | 8     |
| Tasks requiring >1 lifecycle        | 0     |
| In-lifecycle retries                | 0     |
| `harness.crash` events              | 0     |
| `harness.budget_exceeded` events    | 0     |
| Median attempt wall-clock (seconds) | 310   |

One-line health verdict: **The harness ran clean end-to-end, but neither phase's deliverable executed for its own phase — the long-lived worker process ran the code it imported at startup, not the code each task merged mid-cycle.**

## Per-task findings

### clean
- `restore-per-run-worker-logs` — 1 lifecycle, 1 attempt, 1 iteration, both graders pass. (`run-8fc5d7ae`, 367s, $2.70, 37 turns)
- `loop-path-diff-marker` — 1 lifecycle, 1 attempt, 1 iteration, both graders pass. (`run-93ced70f`, 430s, $1.92, 24 turns)
- `loop-path-optout-artifact` — 1 lifecycle, 1 attempt, both graders pass. (`run-3be0fabe`, 254s, $2.47, 23 turns)
- `phase-base-diff` — 1 lifecycle, 1 attempt, both graders pass. (`run-d97f57ab`, 454s, $3.61, 46 turns)
- `archive-gate-loop-path` — 1 lifecycle, 1 attempt, both graders pass. (`run-b145496f`, 502s, $2.87, 53 turns)
- `audit-phase-marker-recheck` — 1 lifecycle, 1 attempt, both graders pass. (`run-4106bcf8`, 242s, $1.03, 20 turns)
- `task-command-emit-verify` — 1 lifecycle, 1 attempt, both graders pass. (`run-2e194990`, 144s, $0.82, 17 turns)
- `docs-in-loop-verification-gate` — 1 lifecycle, 1 attempt, both graders pass. (`run-956ad247`, 57s, $0.37, 7 turns)

Every one of the 16 grader receipts has `passed=1` on attempt 1; no flaps, no agent-vs-grader disagreements, no protocol failures. There are no per-task loop failures to report. The findings below are phase-level.

## Phase-level finding 1 — Neither phase's deliverable ran for its own phase (stale worker process)

**What happened**

Both phases modify the worker/workflow layer (`​.workflow/worker.py`, `src/flywheel/workflow.py`). The worker is a single long-lived Python process (`uv run python .workflow/worker.py --once`) that imports its modules once at startup, then runs one cycle: `commit_task_files` -> `orchestrate` (drains all 8 tasks) -> `archive_phases`. Each task FF-merged its code into `main` as it finished, but the running process kept executing the modules it loaded before the cycle began. So every code change shipped this cycle took effect on disk and for the *next* worker invocation — never for the cycle that produced it.

**Evidence**

- Git timeline vs. lifecycle completion (commits in `-0400`, lifecycles in UTC; `13:36:11Z` = `09:36:11 EDT`):
  - `e542512 2026-06-04 09:35:54 fix: restore per-run worker log files` (phase 18) — lifecycle `run-8fc5d7ae` DONE `2026-06-04T13:36:11Z`.
  - `aad3c15 09:54:44 feat: record per-phase base sha ...` — lifecycle `run-d97f57ab` DONE `13:55:10Z`.
  - `154dcbe 10:03:05 feat: gate archive on in-loop-verification ...` — lifecycle `run-b145496f` DONE `14:03:33Z`.
  - The `[worker] started pid=3699318` banner appears once at the top of the run log; the process is never restarted before `[worker] Shutting down.` 41 minutes later.
- Phase 18 deliverable absent: `ls logs/worker/ | grep -E '20260604|20260603'` -> no files; `grep -rl '<run_id>' logs/worker/` -> `0` files for all 8 run_ids. The newest file in `logs/worker/` predates this run (`*_20260528T*`). The per-run log writer the task added (`e542512`) was not in the running process, so no log file was written for any of the 8 runs — including `restore-per-run-worker-logs` itself.
- Phase 19 deliverable absent: `.workflow/tasks/archive/19-in-loop-verification-gate/.loop-base` does not exist (`(pdir/".loop-base").exists()` -> `False`). The capture function shipped and is wired into the on-disk cycle (`​.workflow/worker.py:477` calls `write_phase_base_if_missing`, invoked before `orchestrate`), but the running process predated `aad3c15`, so it never ran for phases 18 or 19.

**Diagnosis**

The harness layer (invoke -> validate -> grade -> retry -> merge) worked correctly: all 8 tasks reached DONE with passing graders. The gap is at the worker-process boundary: a self-modifying worker that edits its own importable code cannot observe those edits within the same process lifetime. This is the same class of blind spot phase 19 was built to close (the spec's phase-08/17 precedents: "graders pass" did not mean "the shipped path ran") — here the per-run-log writer and the `.loop-base` capture both passed their ephemeral pytest graders while never executing against the live worker.

## Phase-level finding 2 — FR-6 audit re-check is non-evaluable for phase 19

**What happened**

This command's Step 5 (the FR-6 marker re-check phase 19 itself shipped) depends on the archived phase's `.loop-base` to reconstruct the diff. Because finding 1 left `.loop-base` uncaptured, the re-check reads an empty diff and reports "no signals" — not because phase 19 is provably a pure refactor, but because its base input is missing.

**Evidence**

- Running Step 5's own snippet against `archive/19-in-loop-verification-gate`:
  - `read_phase_base(pdir)` -> `None`
  - `phase_diff_vs_base(Path("."), pdir)` -> length `0` (degrades to empty by design when `.loop-base` is absent)
  - `detect_loop_path_signals("")` -> `[]`
  - `load_loop_path_optout(pdir)` -> absent; no `in-loop-verification`-tagged task in either phase.
- Independent cross-check (reconstructing the real diff range the capture would have recorded): `git diff e542512 c924747` (95,453 bytes) through the shipped marker -> `[]`; `git diff 6e5d6a0 e542512` (phase 18, 10,431 bytes) -> `[]`. So both phases are in fact genuinely non-loop-path-bearing (no `Status`/`Outcome`, no `ADD COLUMN`/table, no `Grader` variant, no control verb, no store `Protocol` method added), and the gate correctly would not have required a verify task for either.

**Diagnosis**

There is no real coverage gap this cycle — but that is luck, not verification. The re-check returned the right answer (`[]`) via the wrong path (empty-diff degradation, finding 1) rather than via an actual diff of a captured base. An empty `phase_diff_vs_base` is indistinguishable, at the audit surface, between "no base recorded" and "base recorded, pure refactor." For this phase both the gate (old code, ran without the marker) and the audit re-check (new code, ran against a missing base) were no-ops; the first loop-path-bearing phase that enters under a stale worker would hit the same two no-ops silently.

## Phase-level finding 3 — Live token/cost surface read zero for the entire running window

**What happened**

Each task ran to completion in a single agent iteration, so the only usage-bearing event (`harness.iteration_completed`) fired once, at the end. For the whole multi-minute running window the heartbeat showed `tokens=0 cost=-- turns=0`; the real totals only appeared at the `validating` transition.

**Evidence**

- `harness.iteration_completed` count per run = `1` for every run (`run-8fc5d7ae`, `run-d97f57ab`, `run-b145496f` all `iters=1`).
- Provided run log, `loop-path-diff-marker`: the heartbeat prints `tokens=0 cost=-- turns=0` from `age=0s` through `age=175s` (identical `USER tool_result(13276B)` line repeating at 10s intervals), then nothing until the run ends.
- Real usage from the single `iteration_completed` payload (the numbers the heartbeat showed as `0`): `run-d97f57ab` `total_tokens=6,417,765`, `$3.61`, 46 turns; `run-b145496f` `~5,236,590` tokens, `$2.87`, 53 turns. The longest blackout windows are `run-b145496f` (502s) and `run-d97f57ab` (454s) — ~7-8 min each with the live surface reading zero.

**Diagnosis**

The heartbeat sources token/cost from completed-iteration aggregates; a long *single* iteration produces no intermediate aggregate, so the live surface cannot distinguish "burning 6.4M tokens" from "idle" until the iteration closes. The repeated identical heartbeat line with a rising `age` is the stale-frame symptom of the same single-iteration shape.

## Loop-path marker re-check (FR-6)

- **Base SHA:** absent — `.loop-base` was never written for phase 19 (see finding 1).
- **Re-derived signals:** `[]` (vacuous: from an empty diff, not a verified pure-refactor; independent reconstruction `git diff e542512 c924747` also yields `[]`).
- **`in-loop-verification` task:** absent in both phases (neither phase is loop-path-bearing).
- **Opt-out:** absent.

No FR-6a or FR-6b finding: both phases' real diffs trip zero watched signals, so there is no coverage gap to flag. Recorded here only to note the re-check ran against a missing base and is therefore non-evaluable rather than affirmatively clean (finding 2).

## Cross-task patterns

- **Single shared root cause, two missing deliverables:** the stale-process boundary (finding 1) is the only cross-task pattern; it independently explains the absent `logs/worker/` files (phase 18) and the absent `.loop-base` (phase 19).
- **No infra noise:** zero `harness.crash`, zero `harness.budget_exceeded`, zero retries, zero protocol failures, no recurring `error` strings across all 8 runs (`error` is NULL on every lifecycle and attempt row).
- **No prerequisite cascade or idle gap:** lifecycles complete in dependency order with no large `updated_at` gaps; the seven phase-19 tasks ran back-to-back from `13:36Z` to `14:11Z`.
- **`worker_id` is NULL** on all 8 lifecycles — but it is NULL on all 33 lifecycles in the store, so this is standing worker behavior, not phase-specific friction; noted, not a finding.

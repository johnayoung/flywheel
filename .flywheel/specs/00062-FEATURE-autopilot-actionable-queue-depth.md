# Feature: Autopilot actionable queue depth

## Outcome
Autopilot's refill decision counts only *actionable* work — active tasks the
worker can still drive — instead of the raw count of task files under
`active/`. A task that has landed (`DONE`) or terminally failed (`FAILED`) no
longer counts toward queue depth, so the queue refills toward `target_depth` as
soon as work completes rather than waiting for an entire batch to finish, and a
single stuck/failed task can no longer pin depth at target and permanently
suppress intake.

## Background
Archival is all-or-nothing per phase: `archive_completed_phases`
(`_workflow.py`) moves an `active/<phase>/` dir to `archive/` only when **every**
task in it has a `DONE` lifecycle (`_has_done_lifecycle`). All autopilot tasks
share one phase dir (`active/autopilot/`), and nothing prunes individual task
files. Meanwhile autopilot measured depth as the raw file count —
`_directory_queue_depth = len(DirectoryWorkSource.list_work())` — with no
lifecycle-status filter.

Together these produce two defects, both observed live on 2026-06-29:
- **Sawtooth, not top-up.** A landed task's JSON lingers in `active/autopilot/`
  until the whole batch archives, so depth stays at target and autopilot refuses
  to refill — defeating the "keep the queue full" intent.
- **Permanent wedge.** `_has_done_lifecycle` matches `DONE` only, so a terminally
  `FAILED` task never lets the phase archive; its `DONE` siblings' files never
  leave either, depth stays pinned at target, and autopilot never emits again.
  Spec 00061 (landable-change gate) makes terminal `FAILED` reachable
  (never-commit → bounded retry → `FAILED`), so the fix opened a new way to trip
  this.

## Scope
### In scope
- `actionable_queue_depth(tasks_dir, store)` in `_autopilot.py`: counts active
  task files whose task has no `DONE`/`FAILED` lifecycle. Resilient — a per-task
  store read error counts the task as actionable rather than crashing the cycle.
- Wire it through `flywheel autopilot`: `_autopilot_run.py` opens the policy
  store once at startup and passes a store-bound depth closure into
  `run_single_pass` → the existing `run_refill_pass(queue_depth=...)` seam. Store
  build failure degrades to the raw count (today's behavior), never crashes.

### Out of scope
- Changing phase archival semantics (whole-phase, DONE-gated) — the lingering
  `DONE`/`FAILED` files are inert for selection; only the depth metric was wrong.
  Per-task archival or treating `FAILED` as archive-eligible is a separate change.
- Any change to scoring/selection, the worker, or 00060/00061 code.

### Must not regress
- The `--once` and daemon paths still run with no store available (store build
  failure → `queue_depth=None` → raw count, exactly as before).
- A legitimately full queue (target actionable tasks in flight) still reports at
  or above target and emits nothing.

## Success Criteria
1. `actionable_queue_depth` excludes `DONE` and `FAILED` tasks and counts
   fresh/running/interrupted ones. [command]
   verify: 5 active files (fresh/done/failed/running/interrupted) with seeded
   lifecycles → raw depth 5, actionable depth 3.
2. A fully-terminal-but-unarchived batch reports depth 0 (the 1b wedge). [command]
   verify: two files, one `DONE` one `FAILED`, no archival → actionable depth 0.
3. A store read error never crashes the count; the task is counted as
   actionable. [command]
   verify: a store whose `list_lifecycles` raises → depth equals the file count.
4. The daemon threads the store-bound counter and degrades to the raw count when
   the store can't be opened. [command | covered by the wiring + the existing
   autopilot loop/daemon suites staying green]

## Decisions Log
### D-1: Depth means actionable work, fixed in the metric (not archival)  (Status: Accepted)
- Context: the wedge is a disagreement between "queue full?" (file count) and
  "phase archived?" (all-DONE). | Decision: redefine depth as non-terminal
  active tasks, using the store the worker already owns; leave archival
  untouched. Rejected: per-task archival (touches the load-bearing phase model:
  loop-base, protected-paths, phase-exit gate); treating `FAILED` as
  archive-eligible (would silently move failures out of `active/`).
### D-2: Store backing is best-effort  (Status: Accepted)
- Context: the daemon must never crash on a transient SQLite lock or a missing
  db. | Decision: store build failure → raw count; per-task read failure →
  count as actionable. Loud-degraded, never fatal.

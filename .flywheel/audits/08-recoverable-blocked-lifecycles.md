# Phase audit: 08-recoverable-blocked-lifecycles

**Source:** `.workflow/flywheel.sqlite`, `logs/worker/`, `.workflow/tasks/archive/08-recoverable-blocked-lifecycles/`
**Audited:** 2026-05-28
**Wall-clock window:** 2026-05-28T16:19:57+00:00 -> 2026-05-28T17:03:40+00:00 (~44 min)

## Summary

| Metric                              | Value |
| ----------------------------------- | ----- |
| Tasks in phase                      | 4     |
| Tasks reaching DONE                 | 4     |
| Total lifecycles                    | 4     |
| Tasks requiring >1 lifecycle        | 0     |
| In-lifecycle retries                | 0     |
| `harness.crash` events              | 0     |
| `harness.budget_exceeded` events    | 0     |
| Median attempt wall-clock (seconds) | 452.65 (range 301.0 - 718.7) |
| Pre-lifecycle crash spawns (off-DB) | 76 (harness-recheck-primitive) |

One-line health verdict: _"Looks clean in the DB; the loop actually thrashed silently for ~4 minutes on one task because the failure mode escaped both the lifecycle-row schema and the spawn-failure circuit breaker."_

## Per-task findings

### clean
- `envelope-blocked-requires` -- 1 lifecycle, 1 attempt, 5 graders pass. (`run_id: run-4c5ad5c8b757475f9ce81fdd93e1bb02`, 301.0 s)
- `workflow-recheck-cli-and-status` -- 1 lifecycle, 1 attempt, 6 graders pass. (`run_id: run-abb2c9f8c2d24f659d54e4ee9772f91f`, 514.5 s)

### `lifecycle-blocked-requires-column` -- grader scope blind spot

**What happened**
- Lifecycle (status `done`, `run-b7d5d5ac69214724933994770a6fa301`) shipped a schema change to `src/flywheel/_schema/persistence-schema.sql` and a new `Lifecycle.blocked_requires_json` field. Graders passed end-to-end (7/7). The commit message records: _"Schema version is intentionally not bumped -- adding a nullable column without a default is the back-compat path accepted by the spec."_ (commit `cf45b58`).
- The long-lived store at `.workflow/flywheel.sqlite` was not migrated by anything in the lifecycle. Subsequent tasks ran against the un-migrated DB.

**Evidence**
- `grader_results(run_id=run-b7d5d5..., grader_name='sqlite-schema-column').payload_json.run`:
  `"grep -q 'blocked_requires_json' src/flywheel/_schema/persistence-schema.sql"` (grep against the schema text file, exit 0).
- `grader_results(... grader_name='postgres-schema-column')` -- same grep, against `persistence-schema-postgres.sql`.
- `grader_results(... grader_name='field-default').payload_json.run`:
  `"uv run python -c \"from flywheel.lifecycle import Lifecycle; lc = Lifecycle(task_id='t'); assert lc.blocked_requires_json is None\""` -- dataclass-only assertion, no DB I/O.
- `grader_results(... grader_name='store-and-lifecycle-tests')` -- `pytest tests/test_lifecycle.py tests/test_lifecycle_module_purity.py tests/test_store_sqlite.py tests/test_store_memory.py tests/test_store_contract.py` -- store tests instantiate ephemeral DBs from the schema file, so they always have the new column. None of them check `.workflow/flywheel.sqlite`.
- `sqlite3 .workflow/flywheel.sqlite "PRAGMA table_info(lifecycles);"` later shows column `12|blocked_requires_json|TEXT|0||0`, but its appearance is outside the lifecycle window (see next finding).

**Diagnosis**
- The task's verification surface (grep on a checked-in file + dataclass field check + ephemeral-store pytest) and the task's deliverable (a schema-file edit) are the same surface. No grader ever queries the live, long-lived `.workflow/flywheel.sqlite`. From the loop's perspective the task passed; from the next task's perspective the prerequisite was hollow.

### `harness-recheck-primitive` -- silent crash storm, then opaque recovery

**What happened**
- Lifecycle row in `lifecycles` shows a single clean run: status `done`, 1 attempt, 6/6 graders pass (`run_id: run-97f392c6921f4053a68b81185b2b94c0`, 718.7 s). The DB tells you nothing went wrong.
- The worker actually spawned this task 76 separate times between 2026-05-28T12:31:34 and 12:35:12 local. Every spawn assigned a fresh `run_id`, crashed in `store.load_lifecycle` before any DB row could be written, and exited.
- 76 distinct worker logs in `logs/worker/harness-recheck-primitive_cf45b5_20260528T12313*.log` through `..123512.log`, each with a unique `run_id` header line. Wall-clock cadence: ~3 s between spawns.
- 8-minute silent gap from 12:35:12 (last crash log) to 12:43:03 (first non-crash log, the successful run). No worker logs, no DB events, no `events` rows, no `attempts` rows for this window.

**Evidence**
- Verbatim crash, log `logs/worker/harness-recheck-primitive_cf45b5_20260528T123138.log:4-46`:
  ```
  [workflow] run_id  : run-131f699fd89f4439b39d4c1149f80547
  Traceback (most recent call last):
    ...
    File ".../src/flywheel/harness.py", line 674, in run_task
      stored = store.load_lifecycle(lifecycle.run_id)
    File ".../src/flywheel/store_sqlite.py", line 293, in load_lifecycle
      row = self._connection.execute(...).fetchone()
  sqlite3.OperationalError: no such column: blocked_requires_json
  ```
- Same trace verified at the last crash log `..123512.log` with `run_id: run-8e6f9e976ba742a5b313f41f08daccd7`.
- Counts: `ls logs/worker/harness-recheck-primitive_cf45b5_20260528T12313{4..9}*.log logs/worker/..T1232*.log logs/worker/..T1233*.log logs/worker/..T1234*.log logs/worker/..T12350*.log logs/worker/..T12351*.log | wc -l` -> 76 distinct files; each `grep "^\[workflow\] run_id"` resolves to a distinct `run-...` value (76 unique).
- DB confirms none of those 76 run_ids landed:
  `SELECT COUNT(*) FROM lifecycles WHERE task_id='harness-recheck-primitive'` -> `1` (only the successful run).
  `SELECT COUNT(*) FROM events WHERE ts BETWEEN '2026-05-28T16:31:00' AND '2026-05-28T16:43:00'` -> `1` (just the finalize for the prior task; nothing in the storm window).
- Successful log `logs/worker/harness-recheck-primitive_cf45b5_20260528T124303.log`:
  ```
  [workflow] run_id  : run-97f392c6921f4053a68b81185b2b94c0
  [workflow] status  : done
  ```

**Diagnosis** (three failures stacked)

1. **Crash happens before the lifecycle row exists.** `flywheel/harness.py:674` calls `store.load_lifecycle(lifecycle.run_id)` before `store.create_lifecycle`. When the read raises `OperationalError` (column missing), control never reaches the write. Result: no row, no event, no `attempt`, no `harness.crash` event -- the DB has zero trace of the failure mode, only the worker log does.

2. **Spawn-failure circuit breaker is reset by its own happy path.** `.workflow/task-worker.sh:759` sets `SPAWN_FAILURES[$task_id]=0` immediately after a successful `create_worktree`. The counter is only incremented later in `remove_finished` (line 693) when `read_lifecycle_status` returns empty -- i.e. exactly this failure mode. Per outer-loop iteration the sequence is `create_worktree (reset to 0)` -> `spawn` -> `harness crash` -> `remove_finished (increment to 1)`. The counter ping-pongs 0/1 and never reaches `SPAWN_FAILURE_THRESHOLD=3`. The threshold is structurally unreachable for any post-spawn crash that leaves no lifecycle row -- it only catches `create_worktree` failures.

3. **No grader for the actual prerequisite contract.** The prerequisite task's graders (see `lifecycle-blocked-requires-column` finding above) verify the schema text and the dataclass field; they don't verify the long-lived DB has the column the next task will query. The dependency-graph machinery enforced ordering correctly (harness-recheck-primitive did wait for lifecycle-blocked-requires-column to reach DONE), but "DONE" did not imply "the column the next task needs is in the live DB."

The 8-minute gap from 12:35:12 to 12:43:03 has no telemetry. The loop has no record of what happened. The successful run's worktree code is identical (commit `cf45b5` is in every log filename including the success), so the gap is consistent with an out-of-band `ALTER TABLE lifecycles ADD COLUMN blocked_requires_json TEXT` performed by the operator (followed by a worker restart or just allowing the next poll iteration to retry). The loop's history cannot tell us what the operator did; it can only tell us the failure stopped at 12:35:12 and the next attempt at 12:43:03 succeeded.

## Cross-task patterns

- **Schema drift between checked-in DDL and the live store is invisible to the loop.** `lifecycle-blocked-requires-column` shipped (graders pass), then `harness-recheck-primitive` couldn't load. Evidence chain: `grader_results.payload_json` for `sqlite-schema-column`/`postgres-schema-column`/`field-default` (all file/dataclass checks, no live-DB queries) + the `OperationalError: no such column` in 76 worker logs + the eventual silent fix appearing in `PRAGMA table_info(lifecycles)`. Flywheel currently has no concept of "this task migrates infrastructure that downstream tasks bind to."

- **Crashes before `create_lifecycle` are invisible to every loop subsystem except the worker log.** The 76 crashed `run_id`s appear in `logs/worker/harness-recheck-primitive_cf45b5_*` but produce zero rows in `lifecycles`, `attempts`, `events`, or `grader_results`. The loop's own observability (status, audit-stream, heartbeat) is built around lifecycle rows; this failure shape exists entirely off the DB.

- **The spawn-failure circuit breaker covers only one of the two pre-lifecycle failure shapes.** `create_worktree` failures accumulate (the success path of spawn doesn't run, so no reset). Post-spawn harness crashes that emit zero DB rows are reset on every outer-loop iteration. The two paths share `SPAWN_FAILURES[$task_id]` but only one resets it. Evidence: worker spawned 76 times against a threshold of 3 (`.workflow/task-worker.sh:58`).

- **Phase 08 is a re-do of phase 04.** Task IDs and contents are near-identical to `.workflow/tasks/archive/04-recoverable-blocked-lifecycles/`; tag diff is just `+"phase-08"`. `git log` shows commit `690839d "feat: Implement recoverable blocked lifecycles feature"` landed manually on 2026-05-28 12:18, ten minutes before phase 08 started running. The loop has no representation of "this phase was previously attempted, abandoned, manually shipped, and is now being re-run for verification" -- that context lives only in commit history and the duplicated phase directories.

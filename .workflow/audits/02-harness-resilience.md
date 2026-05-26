# Phase audit: 02-harness-resilience

**Source:** `.workflow/flywheel.sqlite`, `logs/worker/`, `.workflow/tasks/archive/02-harness-resilience/`
**Audited:** 2026-05-26
**Wall-clock window:** 2026-05-25T19:40:51Z -> 2026-05-26T15:02:03Z

## Summary

| Metric                              | Value      |
| ----------------------------------- | ---------- |
| Tasks in phase                      | 4          |
| Tasks reaching DONE                 | 4          |
| Total lifecycles                    | 4          |
| Tasks requiring >1 lifecycle        | 0          |
| In-lifecycle retries                | 1          |
| `harness.crash` events              | 0          |
| `harness.budget_exceeded` events    | 0          |
| `harness.blocked` events            | 1          |
| `harness.retry_scheduled` events    | 1          |
| Median attempt wall-clock (seconds) | 330.3      |

**Verdict:** The loop misbehaved. Only 1 of 4 lifecycles (`raise-sdk-cap`) finalized cleanly through the harness. Two lifecycles were left stranded in `running` by uncaught `KeyboardInterrupt` and required out-of-band reconciliation to reach `done`. A third had its grader subprocess SIGINT'd mid-run, which the harness recorded as a logical grader failure and used to schedule a retry it never got to finish. A cross-task contamination chain (one task's commit broke a sibling task's `full-suite` grader) blocked one agent and forced another to ship `verify` against a failing test.

## Per-task findings

### `crash-retry-eligible` — SDK crash (uncaught), no `harness.crash` emitted, lifecycle stranded

**What happened**
- Lifecycle `run-5bc35656fb934b71b74599f467377775` started 2026-05-25T19:40:51Z. Only `harness.attempt_started` was ever emitted (`events.id=18`). No `attempt_finalized`, no `harness.crash`. `attempts(run-5bc35656…, 1).ended_at` is `NULL`. `timestamps_json` jumps straight from `running: 2026-05-25T19:40:51Z` to `done: 2026-05-26T13:02:40Z` (~17 hours later, with no `validating` step).

**Evidence**
- `logs/worker/crash-retry-eligible_6d3ff5_20260525T154050.log:96-99`:
  ```
  File "/home/johnayoung/.local/share/uv/python/cpython-3.13.7-linux-x86_64-gnu/lib/python3.13/asyncio/runners.py", line 123, in run
      raise KeyboardInterrupt()
  KeyboardInterrupt
  [worker] Shutdown requested, waiting for current task...
  ```
- Stack at point of interrupt was inside `src/flywheel/invoker.py:181` (`async for msg in source`) — the SDK's `query.receive_messages` (anyio `WouldBlock` -> `asyncio.CancelledError` -> `KeyboardInterrupt`).
- `lifecycles.agent_output` for this run is empty; `lifecycles.session_id`, `artifacts_dir`, `worker_id` are all empty.
- `events` table has exactly one row (`id=18`) for this `run_id`.

**Hypothesis**
The harness only emits `harness.crash` when its own crash handler catches an exception; an external `SIGINT` to the worker propagates up through `asyncio.run` as `KeyboardInterrupt`, kills the process before `_run_attempt` can reach its finalizer, and leaves the lifecycle in `running` and the attempt without `ended_at`. The promotion to `done` at 2026-05-26T13:02:40Z came from outside the harness path (same exact timestamp as two other stranded runs — see Cross-task patterns).

**Suggested follow-up**
- `src/flywheel/workflow.py:455` and `src/flywheel/harness.py:_run_attempt`: install a top-level `SIGINT/KeyboardInterrupt` handler that finalizes the in-flight attempt as `Outcome.INTERNAL_ERROR` with `error="worker interrupted"`, emits `harness.crash`, and transitions the lifecycle to the new `INTERNAL_ERROR` status (the very feature this task was meant to enable — it shipped, but the worker entry-point never invokes it on `SIGINT`).
- Add a recovery sweep on worker startup that scans `lifecycles` for rows stuck in `running` / `validating` with no live worker and either re-enqueues them or marks them `INTERNAL_ERROR` (no more silent reconciliation by hand).

### `drop-default-transcript-grader` — Cross-task spec contamination, agent blocked, lifecycle promoted out-of-band

**What happened**
- Lifecycle `run-5d98e15840864ba2aa9cd4d0dada3873`. Attempt 1 ran 330.3s and finalized with `outcome=cancelled` after the agent emitted a `blocked` envelope.
- `harness.blocked` (`events.id=22`) fired; lifecycle then sat in `interrupted` until promoted to `done` at 2026-05-26T13:02:40Z — same instant as two other stranded runs.

**Evidence**
- `events.id=20` `harness.iteration_completed` envelope:
  ```json
  {"kind":"valid","intent":"blocked",
   "reason":"Full pytest suite fails on tests/test_lifecycle.py::test_status_enumerates_exactly_the_eight_spec_states due to pre-existing breakage from the sibling crash-retry-eligible work (commit 1e209a4 added a 9th Status value internal_error without updating the test). My task's non-goals forbid retroactively rewriting tasks in .workflow/tasks/active/, so I cannot land the test fix here. Graders 1, 2, and 4 pass; the /task skill template edit and docs: commit are in place on main."}
  ```
- `attempts(run-5d98e158…, 1).outcome = 'cancelled'`, same `error` text.
- `grader_results` has zero rows for this run — the agent stopped before validation could be attempted.
- `logs/worker/drop-default-transcript-grader_1e209a_20260525T154408.log` is 4 lines long, ending at `status: interrupted`.

**Hypothesis**
Two flywheel failures stacked:
1. **Cross-task workspace contamination.** Tasks in the same phase run serially in one git working tree. The sibling task `crash-retry-eligible` added `Status.INTERNAL_ERROR` but did not update `tests/test_lifecycle.py::test_status_enumerates_exactly_the_eight_spec_states`, leaving the suite red. Every later task in the phase whose graders include `full-suite` inherits that failure. There is no per-task isolation (the LKG snapshot from `a2acb60` was not yet in place — it only kicked in for `raise-sdk-cap`).
2. **The `blocked` outcome has no operator-facing recovery.** Once the agent emitted `blocked`, the lifecycle parked in `interrupted` and required a human to bulk-update it to `done` 17 hours later. The harness produced no follow-up envelope, no escalation event, no automatic unblock-once-prereqs-pass check.

**Suggested follow-up**
- `.claude/skills/task/SKILL.md` (the `/task` template): require that any task introducing a new enum value, schema column, or shared invariant *also* list every test/grader that enumerates the old set, so the agent updates them in the same commit. This is exactly the spec hole that produced the broken-test cascade.
- Workspace isolation: continue rolling out the LKG snapshot approach (`.workflow/lkg/`) to all tasks, not just dogfooded ones. Or, run each task on its own git branch off the phase base and only merge to main after grading.
- `src/flywheel/harness.py` (blocked path): emit a `harness.blocked` event with structured `requires` metadata (which prereq is missing) so an operator-facing dashboard can show "this lifecycle is blocked on X" instead of going dark.

### `drop-implementation-notes` — SIGINT-during-grader misclassified as validation failure, retry interrupted too

**What happened**
- Lifecycle `run-2e330b6ababf488ea3664dd8f9f3cf84`. Attempt 1: 382.3s, `outcome=validation_failed`, grader `full-suite-sqlite-memory` killed by SIGINT. Retry scheduled. Attempt 2 started but never finalized; the worker was again killed by `KeyboardInterrupt` and the attempt is stuck with `ended_at=NULL`. Lifecycle later promoted to `done` at 2026-05-26T13:02:40Z (same out-of-band batch as the other two).

**Evidence**
- `grader_results(run-2e330b6a…, attempt 1, ordinal 1).payload_json` — the "failed" grader:
  ```json
  {"run":"uv run pytest",
   "exit_code":-2,
   "stdout_tail":"... !!! KeyboardInterrupt !!!\n.../selectors.py:398: KeyboardInterrupt\n(to show a full traceback on KeyboardInterrupt use --full-trace)\n============================= 45 passed in 49.27s ==============================\n",
   "termination":"signal","signal":2,"duration_ms":49868}
  ```
  Pytest was 45 tests in when SIGINT (signal 2) hit it. No test actually *asserted false*; the subprocess was killed.
- `events.id=25` `harness.attempt_finalized`: `{"number":1,"outcome":"validation_failed","error":"command grader 'full-suite-sqlite-memory' failed"}` — the harness flattened "killed by SIGINT" into a generic "failed" and proceeded to retry.
- `events.id=26` `harness.retry_scheduled`: `{"retries_used":0,"max_retries":1}`.
- `events.id=27` attempt 2 `harness.attempt_started` — followed by silence.
- `logs/worker/drop-implementation-notes_574193_20260525T154940.log:87-89` — the retry's worker process died at `subprocess_cli.py:418 connect()` -> `anyio.to_thread.run_sync` -> `CancelledError` -> `KeyboardInterrupt`. The SDK had not even spawned the agent CLI yet.
- Attempt 1 iteration envelope (`events.id=24`) reveals the agent already knew the suite was red on a sibling-task reason: *"The lone failing pytest case (test_status_enumerates_exactly_the_eight_spec_states) is a pre-existing failure introduced by sibling task FR-3 and is out of scope per constraints."* — it submitted `verify` anyway. The grader never reached that test (only 45 tests in when it was killed).

**Hypothesis**
Three independent flywheel issues compound here:
1. **Grader subprocess SIGINT is not distinguished from grader logical failure.** `_run_command_grader` records `termination="signal", signal=2` in the payload but the harness treats any non-zero exit as `validation_failed`. The retry was spent against a non-grader event (operator intervention or worker shutdown).
2. **The retry then took the budget without producing usable signal**, because the worker died again before the SDK transport even connected — wasting the configured `max_retries=1` against an environmental failure.
3. Same **cross-task workspace contamination** as `drop-default-transcript-grader`: the agent saw the suite was red because of `crash-retry-eligible`'s missed test update and shipped `verify` hoping the harness would ignore it.

**Suggested follow-up**
- `src/flywheel/harness.py` (validation path): when a command grader payload has `termination == "signal"` (especially `signal in (2, 15)`), classify the attempt as `INTERNAL_ERROR` (operator interruption / environmental), not `validation_failed`. Do not consume a `max_retries` slot for it.
- Same SIGINT-recovery and workspace-isolation follow-ups as the previous two findings.

### `raise-sdk-cap` — clean

- 1 lifecycle (`run-760e9023f5114560abcb753852c846b0`), 1 attempt, 275.6s, 6/6 graders passed, finalized through the normal path. First task in the phase to run under the LKG snapshot (`logs/worker/raise-sdk-cap_5bf959_20260526T105726.log:1` — "VIRTUAL_ENV=.venv does not match the project environment path `.workflow/lkg/.venv`").

## Cross-task patterns

- **Identical out-of-band finalization timestamp.** Three of four lifecycles have `timestamps_json` `done = 2026-05-26T13:02:40Z` to the second:
  - `run-5bc35656…` (crash-retry-eligible)
  - `run-5d98e158…` (drop-default-transcript-grader)
  - `run-2e330b6a…` (drop-implementation-notes)
  No `harness.attempt_finalized` event corresponds to those transitions for the first two runs; the third has a finalized attempt 1 but never finalized attempt 2. This is a manual / scripted bulk update of `lifecycles.status` after the fact — a strong signal that the loop's "running" → "done" path was broken three times and was patched up by hand.

- **`KeyboardInterrupt` is the recurring failure mode, not an SDK fault.** Two worker logs end in identical-shaped `KeyboardInterrupt` tracebacks (`crash-retry-eligible_…log:71-97`, `drop-implementation-notes_…log:64-88`). The SDK trace varies (one inside `receive_messages`, one inside `transport.connect()`), but the parent cause in both is `asyncio.runners.run` raising `KeyboardInterrupt` and `[worker] Shutdown requested, waiting for current task...`. The harness never gets to emit a `harness.crash` event, never transitions to `INTERNAL_ERROR`. The very feature this phase shipped (crash retry via `INTERNAL_ERROR`) does not cover the case where the *worker itself* is interrupted.

- **One commit broke a sibling task's grader prereq.** `commit 1e209a4` added `Status.INTERNAL_ERROR` without updating `tests/test_lifecycle.py::test_status_enumerates_exactly_the_eight_spec_states`. That made the `full-suite` grader red for every subsequent task in the phase that ran in the same workspace. Cleaned up later by `commit 5bf9592 test: expand Status enumeration check for INTERNAL_ERROR` on 2026-05-26 — i.e., a human fix-up commit was needed to unblock the phase.

- **`harness.protocol_failure` was never observed.** The envelope channel itself worked. The friction was at the worker process boundary (SIGINT) and at the grader subprocess boundary (SIGINT vs failure), not in the envelope/lifecycle contract.

## Recommendations for flywheel

Ordered by leverage.

1. **`src/flywheel/workflow.py` (`_cmd_run` / `run_task_file`)**: wrap the top-level `asyncio.run` in a `try / except KeyboardInterrupt` that calls into the harness's finalize-as-INTERNAL_ERROR path. Today the worker dies silently and leaves the DB in `running`. — fixes the *SDK crash (uncaught)* pattern hit by `crash-retry-eligible` and the second attempt of `drop-implementation-notes`.

2. **`src/flywheel/harness.py` (validation path, around the `Outcome.VALIDATION_FAILED` assignment)**: inspect command-grader payloads and treat `termination == "signal"` (especially signal 2/15) as `Outcome.INTERNAL_ERROR`, not `VALIDATION_FAILED`. Do not decrement the retry budget for operator-induced subprocess kills. — fixes the *SIGINT-misclassified-as-grader-failure* on `drop-implementation-notes` attempt 1.

3. **Worker startup recovery sweep** (new helper in `src/flywheel/workflow.py` or a new `src/flywheel/recovery.py`): on boot, look for lifecycles with `status IN ('running','validating')` and no live worker and either (a) re-enqueue them or (b) finalize them as `INTERNAL_ERROR`. — removes the need for the 13:02:40Z hand-reconciliation that quietly papered over three stranded lifecycles this phase.

4. **`.claude/skills/task/SKILL.md`**: require any task that adds an enum value, a schema column, or a new field to enumerate every test/grader that asserts on the *old* shape and either update them in scope or declare an explicit prerequisite. — closes the spec hole that let `crash-retry-eligible` ship a half-update of `Status` and break two sibling tasks' `full-suite` graders.

5. **Workspace isolation per task** (likely `task-worker.sh` + `src/flywheel/workflow.py`): the LKG snapshot (`.workflow/lkg/`, introduced in `a2acb60`) eliminated the cross-task contamination class for `raise-sdk-cap` — extend the same isolation to non-dogfooded phases, or branch-per-task with a clean checkout. — prevents one task's intermediate state from poisoning the next task's graders.

6. **`harness.blocked` follow-through** (`src/flywheel/harness.py`): structure the `blocked` event payload with machine-readable `requires` / `unblocks_when` fields, and add an automatic re-check loop so a `blocked` lifecycle can return to `ready` once the prereq grader passes. — today `blocked` is a one-way trip to needing a human; that hand-promotion is what we saw at 13:02:40Z.

## Decisions deferred

- Whether `harness.protocol_failure` should fire for envelopes that claim `intent=verify` while the agent's own scratch log shows known test failures (as in `drop-implementation-notes` attempt 1). The agent did not lie about the envelope shape; it lied about the underlying state. Catching that requires either a second-opinion model or a verify-precheck grader — note for the next audit cycle.
- Whether the 17-hour gap between worker death and the 13:02:40Z bulk fix indicates we need an explicit operator dashboard. No data yet on how long stranded-lifecycle alerts should fire before paging.
- The phase shipped the right *fixes* (`Status.INTERNAL_ERROR`, raised SDK turn cap, dropped default transcript grader, dropped `implementation_notes` column) but the loop that shipped them did so by surviving its own bugs. Re-audit after the recommendations above land to confirm the next phase's lifecycles all finalize through the harness rather than via reconciliation.

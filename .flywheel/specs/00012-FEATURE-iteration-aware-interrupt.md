# Feature: Iteration-aware graceful interrupt

## Summary

Make an operator-initiated stop (SIGINT/SIGTERM) deterministically finalize the
in-flight lifecycle to `INTERRUPTED`, even when the signal arrives while the
agent is mid-stream inside the invoker's message loop. Today the worker's signal
handling is cycle-level, so a `KeyboardInterrupt` propagating from inside
`invoke_iteration`'s `async for msg in source` can strand a lifecycle in
`running` — documented in `.workflow/audits/02-harness-resilience.md`. This
feature closes that gap so a stopped run is always cleanly `INTERRUPTED` and
resumable, delivering the exogenous-pause semantics `docs/vision.md:167-171`
declares. It is the minimal control primitive and the prerequisite for the
bidirectional control channel (feature 00013).

## Background

`docs/vision.md:167-171`: "External systems can halt a running task. This moves
the task into `interrupted`... The loop records the interruption, preserves
execution history as configured, and allows the task to resume through `ready`
when released."

The current gaps:

- The invoker drains the SDK stream in `async for msg in source`
  (`src/flywheel/invoker.py:181`). A cancellation/`KeyboardInterrupt` there
  unwinds without the harness finalizing the lifecycle.
- The worker installs SIGTERM/SIGINT handlers but acts at the whole-cycle
  boundary (`.workflow/worker.py:668-754`); `asyncio.run` (inside `orchestrate`)
  takes the signals over during a run, so a mid-iteration interrupt is not
  cleanly routed to a lifecycle finalization.
- Recovery exists but is after-the-fact: `recover_stranded_lifecycles` (run at
  orchestrate startup) and `flywheel.workflow recover` finalize lifecycles stuck
  in `running`/`validating` on the *next* run — they do not make the interrupt
  itself clean.

The fix makes interruption a first-class, in-band transition rather than relying
on a later sweep.

## Scope

### In Scope

- Treat task cancellation (the asyncio cancellation raised when the operator
  stops the worker, including a `KeyboardInterrupt` surfacing inside the
  invoker's stream loop) as an explicit interrupt: the harness finalizes the
  in-flight attempt/lifecycle to `INTERRUPTED`, preserving the messages already
  persisted (live, post-00010), then re-raises so shutdown proceeds.
- Ensure the invoker propagates cancellation cleanly from the stream loop
  (`async for msg in source`) without swallowing it and without losing the
  partial `iteration_result`.
- Wire the worker's SIGINT/SIGTERM path so a single signal cancels the in-flight
  task, lets the harness finalize it to `INTERRUPTED`, and then stops the loop —
  no lifecycle left in `running`.
- An emitted observability event recording the interruption (telemetry kind,
  e.g. `harness.interrupted`, consistent with the existing `harness.*` event
  family) so the audit stream shows the exogenous stop.
- Tests: cancellation mid-stream finalizes `INTERRUPTED`; the lifecycle is
  resumable via `ready`; no stranded `running` after a simulated SIGINT during
  an iteration.

### Out of Scope

- **Mid-run steering / message injection / model change** — feature 00013. This
  is stop-only.
- **A store-backed control channel** — also 00013. Here the trigger is an OS
  signal to the worker, not a stored command.
- **Removing the startup recovery sweep.** `recover_stranded_lifecycles` stays
  as the backstop for SIGKILL/OOM/reboot (which no in-process handler can catch).
- **Per-iteration partial-grading.** An interrupted attempt does not run graders;
  it finalizes to `INTERRUPTED`, not `FAILED_VALIDATION`.
- **Changing resume mechanics.** Resume through `ready` already exists; this
  feature only guarantees the clean entry into `INTERRUPTED`.

## Requirements

### Functional Requirements

1. **FR-1: Mid-stream interrupt finalizes INTERRUPTED.** A cancellation while the
   invoker is draining messages results in the lifecycle reaching `INTERRUPTED`,
   not stranded `running`.
   - Acceptance: a test cancels a fake invoke partway through its message stream
     and asserts the lifecycle's terminal state is `INTERRUPTED`.

2. **FR-2: Partial history preserved.** Messages persisted before the interrupt
   (live via 00010) remain in the store; the interrupt does not roll them back.
   - Acceptance: a test asserts the pre-interrupt messages are present after
     finalization.

3. **FR-3: Resumable.** An `INTERRUPTED` lifecycle can return to `ready` and run
   again without losing prior execution history.
   - Acceptance: a test transitions the interrupted lifecycle to `ready` and
     asserts a subsequent attempt proceeds (per existing transition rules).

4. **FR-4: Interruption is audited.** An observability event records the
   exogenous stop in the audit stream.
   - Acceptance: a test asserts the interrupt event appears for the run.

5. **FR-5: Clean worker shutdown.** A single SIGINT/SIGTERM to the worker cancels
   the in-flight task, finalizes it `INTERRUPTED`, and stops the loop with no
   lifecycle left in `running`.
   - Acceptance: a worker-level test (or the existing harness-resilience
     harness) asserts no stranded `running` lifecycle after a simulated signal
     during an iteration.

### Non-Functional Requirements

- **Determinism**: finalization must not depend on a later recovery sweep for
  the graceful (signal) path; the sweep remains only for ungraceful kills.
- **Purity**: `INTERRUPTED` is an existing lifecycle status; no schema change, no
  domain-event-kind change. `flywheel.task` / `flywheel.lifecycle` purity is
  untouched.
- **Idempotence**: a second signal during shutdown must not corrupt the
  finalization or raise.

## Behavior Specification

### Happy Path

1. The agent is mid-iteration; the operator presses Ctrl-C on the worker.
2. The signal cancels the in-flight task; the cancellation surfaces through the
   invoker's stream loop without being swallowed.
3. The harness catches the cancellation at the attempt boundary, finalizes the
   attempt/lifecycle to `INTERRUPTED`, emits the interruption event, and
   re-raises so the worker stops cleanly.
4. The audit stream shows the persisted messages up to the stop, then the
   interruption event. The lifecycle is resumable via `ready`.

### Error Handling

| Error Condition | Expected Behavior |
| --------------- | ----------------- |
| Signal arrives between iterations (not mid-stream) | Same outcome: in-flight lifecycle finalized `INTERRUPTED`. |
| Finalization write fails during interrupt | Follow the existing `harness.audit_write_failed` semantics; do not leave the lifecycle silently `running`. |
| SIGKILL / OOM / reboot (uncatchable) | Not covered in-band; `recover_stranded_lifecycles` finalizes on the next orchestrate startup (unchanged backstop). |

### Edge Cases

| Case | Expected Behavior |
| ---- | ----------------- |
| Second signal during shutdown | Idempotent; finalization completes once, no crash. |
| Interrupt during `validating` (after invoke, before graders) | Finalize `INTERRUPTED`; graders do not run. |
| Concurrent workers, one interrupted | Only the interrupted worker's in-flight task is affected; leases free it for reclaim per existing rules. |

## Technical Context

### Affected Apps

- `flywheel` (root package): invoker cancellation propagation, harness attempt
  finalization, the interruption event; `.workflow/worker.py` signal routing;
  tests.

### Integration Points

- **Feature 00010** (prerequisite): live persistence is what makes the
  pre-interrupt history durable, so an interrupted run is meaningfully
  inspectable.
- **`recover_stranded_lifecycles` / `flywheel.workflow recover`**: remain the
  backstop for ungraceful termination; this feature reduces reliance on them for
  the graceful path.
- **Resume path** (`ready` transition, `recheck-blocked`): unchanged consumer of
  the `INTERRUPTED` state.

### Relevant Existing Code

- `src/flywheel/invoker.py:181` — `async for msg in source`, where cancellation
  must propagate cleanly with the partial result intact.
- `src/flywheel/harness.py` — `_run_attempt` / attempt finalization and the
  existing `_handle_audit_failure` pattern; where the cancellation is caught and
  routed to `INTERRUPTED`.
- `.workflow/worker.py:668-754` — `_arm_signals`, the shutdown flag handler, and
  the `KeyboardInterrupt`/`CancelledError` handling around `run_once`.
- `.workflow/audits/02-harness-resilience.md` — the documented stranded-`running`
  defect this feature closes.
- `docs/task-lifecycle.md` — `INTERRUPTED` status and its legal transitions.

## Decisions Log

| Decision | Choice | Rationale |
| -------- | ------ | --------- |
| Interrupt outcome | Finalize `INTERRUPTED` (not `FAILED_VALIDATION`) | Exogenous pause, not a verification failure; matches `vision.md:167-171`. |
| Where finalization happens | In-band at the harness attempt boundary | Deterministic; does not depend on a later sweep for the graceful path. |
| Recovery sweep | Keep as backstop for SIGKILL/OOM/reboot | No in-process handler can catch those; the sweep stays for ungraceful kills. |
| Interruption visibility | Emit a telemetry `harness.*` event | Consistent with the existing harness event family; no domain-event change. |
| Trigger | OS signal to the worker | A stored control-command trigger is feature 00013; this is stop-only. |

## Open Questions

None — design resolved during the observability/interactivity planning pass
(`~/.claude/plans/ok-it-worked-but-spicy-firefly.md`).

## Next Steps

Run `/task 00012-FEATURE-iteration-aware-interrupt`. This is the minimal control
primitive feature 00013 (bidirectional steering) builds on.

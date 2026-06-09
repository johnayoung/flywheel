# Feature: Message-granular audit-stream persistence

## Summary

Persist each SDK message to the store **as it arrives** instead of in a single
batch after the iteration ends, so the durable audit stream — the surface every
out-of-process observer reads (`flywheel.workflow live`, `flywheel.audit
--follow`, the worker `Heartbeat`) — becomes real-time at message granularity.
Today the in-process `on_message` stream is the only live path and it never
reaches the store until the iteration closes; for a one-shot task that is the
entire run, producing a multi-minute observability blackout where `live` shows
`iter=?` and the same `harness.attempt_started` line for the whole run. This
change makes every existing observer live with zero changes to the observers
themselves. MVP is persistence-cadence only: no new event kind, no observer
rewrite (that is feature 00011), no interactivity.

## Background

`docs/vision.md:42-46` declares the audit stream — "every harness event and
every SDK message... under a single per-run monotonic sequence... replay and
live tailing share one iterator" — the canonical "what did the agent actually
do?" surface. The infrastructure exists and is correct; it is starved of timely
data.

The durable path is written once per iteration:

- `_persist_sdk_messages` -> `store.save_sdk_messages` is called only *after*
  `invoke_iteration` returns (`src/flywheel/harness.py:411-435` and the
  iteration loop in `_drive_iterations`, ~`harness.py:1635`).
- The live `on_message` seam (`src/flywheel/invoker.py:194-199`) fires per SDK
  message in real time, but is wired only to a stdout printer in the production
  invoker (`src/flywheel/workflow.py:705-708, 837`); it is never persisted and
  the worker daemon does not surface that stdout.
- `collect_live_rows` (`src/flywheel/workflow.py:1311`) reads `iteration_number`
  from the newest `sdk_messages` row, so until the end-of-iteration batch write
  there is no row and `live` renders `iter=?`.

The store already supports the right shape: `events` and `sdk_messages` share
one per-run monotonic `sequence` allocated atomically by `_next_run_sequence`
(`src/flywheel/store_sqlite.py`, `INSERT ... ON CONFLICT ... RETURNING`), and
`flywheel.audit` pages over `sequence > cursor` (`read_audit_since`), so any row
that lands with a higher sequence surfaces live with no audit-side change. The
fix is purely the *cadence* of one write path.

## Scope

### In Scope

- Add `append_sdk_message(record) -> SdkMessageRecord` to the store protocol and
  every implementation (sqlite, in-memory, postgres) plus the
  `_EventStreamingStore` wrapper — the single-row mirror of `append_event`:
  assigns one `sequence` tick from the shared counter, inserts, calls
  `notifier.notify`.
- Thread an `on_message` persistence callback from the harness into the invoker
  via `InvocationRequest`, composed with (not replacing) the existing stdout
  renderer so both fire, each independently try/except-isolated.
- In `_drive_iterations`, persist each message via `append_sdk_message` as it
  arrives and **remove** the end-of-iteration `_persist_sdk_messages` batch call.
- Preserve the strict-audit-failure guarantee: a persistence failure must still
  finalize the attempt as `INTERNAL_ERROR` via the existing
  `harness.audit_write_failed` path, without aborting the agent run mid-stream.
- Tests covering: per-message persistence ordering, exactly-once (no batch
  double-write), the `iter=?` -> `iter=N` resolution, mid-run audit-stream
  visibility, and audit-failure finalization through the new write path.

### Out of Scope

- **Observer/CLI rewrite.** Rendering lifecycle position, running tokens/cost,
  and latest tool call in `live` / the heartbeat is feature 00011. This feature
  only makes the data land live; existing observers consume it unchanged.
- **A new event kind.** The per-message `sdk_messages` rows are themselves the
  progress signal observers already render; adding a parallel telemetry event
  would duplicate every message and double sequence consumption.
- **Any interactivity.** No interrupt, inject, steer, or persistent
  `ClaudeSDKClient` (later features).
- **Schema migration.** `sdk_messages` already carries `attempt_number`,
  `iteration_number`, and `sequence` per row; `CURRENT_SCHEMA_VERSION` is
  unchanged.
- **Removing `save_sdk_messages`.** It stays in the protocol and all
  implementations for backward compatibility (existing tests, the wrapper); the
  harness simply stops calling it.

## Requirements

### Functional Requirements

1. **FR-1: Per-message persistence.** Each SDK message observed during an
   iteration is written to `sdk_messages` as it arrives, via
   `append_sdk_message`, before the iteration completes.
   - Acceptance: a harness test with a fake invoke that calls
     `request.on_message` per pre-built message asserts the messages are
     queryable from the store *before* the `harness.iteration_completed` event,
     in sequence order.

2. **FR-2: Exactly-once.** Removing the batch write means each message is
   persisted once; no duplicates.
   - Acceptance: a test asserts `list_sdk_messages(run_id)` count equals the
     number of streamed messages (no doubling from a residual batch call).

3. **FR-3: Shared-sequence ordering.** `append_sdk_message` allocates from the
   same per-run monotonic counter as `append_event`; messages and events
   interleave in one strictly ascending sequence.
   - Acceptance: a store-contract test appending message, event, message asserts
     strictly increasing `sequence` across both record types.

4. **FR-4: `iter=N` resolves immediately.** Because `iteration_number` is set
   before the invoke, the first persisted message carries it.
   - Acceptance: a test asserts `collect_live_rows` returns `iteration=N` (not
     `None`) after a single message append while the lifecycle is `running`.

5. **FR-5: Mid-run audit visibility.** `flywheel.audit.stream(run_id,
   follow=True)` surfaces SDK messages while the lifecycle is still `running`,
   with no change to the audit module.
   - Acceptance: an audit test drives incremental appends during `running` and
     asserts the records stream live.

6. **FR-6: Strict-audit-failure preservation.** A failure in the per-message
   write finalizes the attempt as `INTERNAL_ERROR` via
   `harness.audit_write_failed`, and never aborts the agent run mid-stream.
   - Acceptance: a store stub that raises on `append_sdk_message` yields exactly
     one `harness.audit_write_failed` event and an `INTERNAL_ERROR`
     finalization.

### Non-Functional Requirements

- **Performance**: write frequency rises from per-iteration to per-message
  (more small INSERTs). With WAL + `synchronous=NORMAL` the per-message cost is
  negligible; message counts per run are modest. Confirm the store PRAGMA and
  spot-check a realistic run shows no material slowdown.
- **Compatibility**: `save_sdk_messages` remains in the protocol and all stores;
  `_EventStreamingStore` gains a pass-through `append_sdk_message`. The
  `harness.iteration_completed` payload and timing are unchanged.
- **Purity**: `flywheel.task` / `flywheel.lifecycle` / `events.apply` are
  untouched — SDK messages are not domain events; no `DomainEventKind` change.

## Behavior Specification

### Happy Path

1. The harness increments `iteration_number`, builds a `_persist_one(msg)`
   closure over `append_sdk_message`, and passes it to the invoker via
   `InvocationRequest.on_message`.
2. As the SDK streams messages, the production invoker fires the composed
   observer: stdout renderer + persistence closure, each isolated.
3. Each message lands in `sdk_messages` with the next shared `sequence` and
   triggers `notifier.notify`; `audit --follow` and `live` see it immediately.
4. The invoker returns; the harness builds the observation / usage breakdown
   from the in-memory `iteration_result.messages` (no store read) and emits
   `harness.iteration_completed` exactly as today.

### Error Handling

| Error Condition | Expected Behavior |
| --------------- | ----------------- |
| `append_sdk_message` raises mid-stream | The closure captures the first failure into a `nonlocal first_audit_error: _AuditWriteError \| None` and returns normally (the invoker's best-effort `on_message` contract is honored; the stream drains fully). After the invoker returns, `_drive_iterations` re-raises it, re-entering the existing `_AuditWriteError -> _handle_audit_failure -> harness.audit_write_failed -> INTERNAL_ERROR` path. |
| stdout renderer raises | Swallowed by the invoker as today; persistence closure still fires. |

### Edge Cases

| Case | Expected Behavior |
| ---- | ----------------- |
| Iteration produces zero messages | No appends; `harness.iteration_completed` still emitted; behavior identical to today. |
| Crash mid-iteration | The messages persisted up to the crash remain in the store — strictly better for forensics than today's all-or-nothing batch. The harness never relied on batch atomicity. |
| Concurrent workers writing to one store | Unchanged: `_next_run_sequence` is atomic and per-run; per-message appends serialize the same way batch writes do. |

## Technical Context

### Affected Apps

- `flywheel` (root package): store protocol + implementations, harness invoke
  seam, invoker request, and tests.

### Integration Points

- **Audit stream** (`flywheel.audit` / `python -m flywheel.audit`): surfaces the
  per-message rows live with no code change — it already pages on `sequence`.
- **`flywheel.workflow live` / worker `Heartbeat`**: both call
  `collect_live_rows`; they gain live data and `iter=N` for free (richer
  rendering is feature 00011).
- **Transcript grader / usage breakdown**: unchanged — they read
  `iteration_result.messages`, not the store.

### Relevant Existing Code

- `src/flywheel/harness.py:411-435` — `_persist_sdk_messages` (the batch call to
  remove) and `_serialize_sdk_message`.
- `src/flywheel/harness.py` `_drive_iterations` (~1635) — the emit site and where
  the per-message closure is wired through `InvocationRequest`.
- `src/flywheel/harness.py` `HarnessStore` protocol (~161) — add
  `append_sdk_message`.
- `src/flywheel/invoker.py:140, 194-199` — `invoke_iteration` and the
  best-effort `on_message` seam.
- `src/flywheel/store_protocols.py` (~393) — `SdkMessageStore` /
  `SdkMessageRecord`.
- `src/flywheel/store_sqlite.py` — `_next_run_sequence`, `save_sdk_messages`
  (factor the per-row body into `append_sdk_message`).
- `src/flywheel/store_memory.py`, `src/flywheel/store_postgres.py` — mirror.
- `src/flywheel/workflow.py:627` (`_EventStreamingStore`), `:705-708, 837`
  (production-invoker `on_message` composition), `:1311` (`collect_live_rows`,
  source of `iter=?`).
- `tests/test_harness.py` — `_AuditFailureStore` (~1786) and the
  save-sdk-messages-failure test (~1980) to update to the new write path.

## Decisions Log

| Decision | Choice | Rationale |
| -------- | ------ | --------- |
| Persistence cadence | Incremental per-message append; remove the batch | A hybrid reconcile would re-serialize the same messages under a second sequence with no dedup key — pure write-amplification and duplicate audit rows. |
| New store verb | `append_sdk_message`, single-row mirror of `append_event` | Keeps the contract clean; `save_sdk_messages` is batch-shaped. Retain it for compatibility. |
| Progress signal | Reuse `sdk_messages` rows; no new event kind | Observers already render messages; a parallel telemetry event duplicates every message and doubles sequence consumption. |
| Audit-failure semantics | Capture-then-re-raise after drain | Honors the invoker's best-effort `on_message` contract verbatim while preserving strict durability via the existing `_AuditWriteError` machinery — no new failure-handling code. |
| Write seam location | Compose persistence with stdout renderer in the production invoker | The harness supplies only the persistence observer; the invoker layer owns transport composition. |

## Open Questions

None — design resolved during the observability/interactivity planning pass
(`~/.claude/plans/ok-it-worked-but-spicy-firefly.md`).

## Next Steps

Run `/task 00010-FEATURE-message-granular-audit-stream` to generate
implementation tasks from this spec. Feature 00011 (operator-surface
enrichment) builds directly on this.

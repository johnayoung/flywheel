# Feature: Audit Stream

## Summary

A first-class, end-to-end audit stream for every run: every SDK message the
agent emits, every harness event, every grader receipt, captured into the
store as a single totally-ordered per-run sequence, and exposed through one
streaming API that powers both live tailing and post-hoc replay. Operators
read it through `python -m flywheel.audit`; programmatic consumers read it
through the same `flywheel.audit` library API.

## Background

Flywheel's vision (`docs/vision.md`) names observability a first-class
property and asserts the loop must be "the e2e auditable/traceable task
completer." The current implementation falls short:

- `flywheel.invoker` observes every `AssistantMessage`, `ToolUseBlock`,
  `ToolResultBlock`, `ResultMessage`, `RateLimitEvent`, and
  `HookEventMessage` from the `claude-agent-sdk` stream, then discards the
  raw messages after distilling them into `InvocationSignals`. Replay,
  audit, and "what did the agent actually do?" questions cannot be answered
  from the store.
- Structured `EventRecord`s land in the store, but there is no library or
  CLI to read them back. Inspecting a run today means writing SQL by hand.
- There is no live visibility while a run is in flight. The worker log in
  `logs/worker/*.log` shows the harness's stdout, not the agent's tool
  decisions or reasoning.

These gaps undermine the loop's claim to durable, legible execution
history. This feature closes them.

## Scope

### In Scope

- Persist every `Message` observed by `flywheel.invoker.invoke_iteration`
  (`AssistantMessage`, `UserMessage`, `ResultMessage`, `RateLimitEvent`,
  `HookEventMessage`) into the store, keyed by `(run_id, attempt_number,
  iteration_number)`, with no payload truncation.
- Extend the store protocol and all three backends
  (`InMemoryStore`, `SqliteStore`, `PostgresStore`) with the new write
  path and a unified read API.
- Introduce a per-run monotonic sequence number that orders `EventRecord`s
  and SDK message records into one totally-ordered stream.
- New `flywheel.audit` module exposing `audit.stream(run_id, follow=...)`
  that yields chronologically-merged audit records and supports live
  tailing while a run is in flight.
- New `python -m flywheel.audit` CLI with human-readable default output
  and `--json` newline-delimited JSON output.
- Optional `flywheel.audit.attach_logger(logger)` emitter that pipes every
  audit record through a caller-supplied `logging.Logger` for consumers
  who route Python `logging` to handlers (file, syslog, JSON sinks).
- Strict-audit failure policy: if persisting an SDK message or audit
  record fails, the attempt finalizes as `INTERNAL_ERROR`.
- Schema migration for SQLite and Postgres; in-memory store updated in
  place.

### Out of Scope

- Redaction of sensitive content. Captures are verbatim; the store is
  sensitive-by-default and documented as such. A redaction layer is a
  future feature on top of this one.
- Size caps and truncation. Stores hold payloads as-is for MVP.
- Backwards compatibility with stores created before this feature.
  Existing stores must be re-created; no migration shim for old runs.
- A streaming push API (LISTEN/NOTIFY, callbacks, asyncio queues). Live
  tail uses cursor-based polling against the store.
- Filtered CLI flags (`--kind`, `--attempt`) and a `--summary` mode. These
  are explicit non-goals for MVP; follow-up features can add them on top
  of the iterator API.
- Capturing `claude-agent-sdk` internal subprocess stderr / debug log
  lines beyond what the SDK already surfaces as `Message` instances.
- Deprecating or removing `Attempt.agent_output` / `Lifecycle.agent_output`.

## Requirements

### Functional Requirements

1. **FR-1: Capture every SDK message.**
   For each iteration, every `Message` instance yielded by the
   `claude-agent-sdk` stream (or the test message stream) is persisted to
   the store in the order observed, with its full payload, the originating
   `run_id`, `attempt_number`, `iteration_number`, capture timestamp, and a
   per-run monotonic sequence number.
   - Acceptance: a test that injects a known sequence of `AssistantMessage`
     / `UserMessage` / `ResultMessage` / `RateLimitEvent` /
     `HookEventMessage` and asserts the persisted records reproduce the
     exact sequence and payloads byte-for-byte.

2. **FR-2: Unified totally-ordered stream.**
   Each store maintains a per-run monotonic sequence counter shared by
   `EventRecord` writes and SDK message writes. The `audit.stream` API
   yields records in ascending sequence order regardless of which write
   path produced them.
   - Acceptance: a test interleaves event emission and SDK message
     persistence in a known order and asserts `audit.stream` returns them
     in that same order.

3. **FR-3: Streaming reader API.**
   `flywheel.audit.stream(run_id, *, follow: bool = False) -> Iterator[AuditRecord]`
   yields a unified `AuditRecord` union of `EventRecord` and
   `SdkMessageRecord`. With `follow=False`, the iterator drains everything
   currently persisted and stops. With `follow=True`, it polls the store
   on a configurable interval (default 250ms) and continues yielding new
   records until the lifecycle reaches a terminal status (`DONE`,
   `FAILED`).
   - Acceptance: a test that writes records concurrently while
     `audit.stream(..., follow=True)` consumes them, then transitions the
     lifecycle to `DONE` and asserts the iterator terminates.

4. **FR-4: CLI inspection.**
   `python -m flywheel.audit <run_id>` reads from the configured store and
   prints a human-readable chronological view. `--json` switches output to
   newline-delimited JSON. `--follow` enables live tailing.
   - Acceptance: end-to-end test invoking the module against a fixture
     store and asserting both output formats render every record.

5. **FR-5: Strict-audit persistence.**
   If the harness or invoker fails to persist an SDK message or an
   `EventRecord`, the attempt finalizes with `Outcome.INTERNAL_ERROR`. The
   lifecycle transitions to `INTERNAL_ERROR` and is subject to the normal
   retry policy.
   - Acceptance: a test that stubs the store to raise on `save_messages`
     mid-iteration and asserts the attempt outcome is `INTERNAL_ERROR`,
     no `DONE` transition occurred, and a `harness.audit_write_failed`
     event was emitted before the abort.

6. **FR-6: Optional `logging.Logger` emitter.**
   `flywheel.audit.attach_logger(logger, *, run_id: str | None = None)`
   returns a handle that subscribes to the audit stream and emits each
   record as a `logging.LogRecord` with structured fields. Detaching the
   handle stops emission. No global side effects; consumers must call this
   explicitly.
   - Acceptance: a test attaches a `logging.Handler` capturing records,
     runs a task, and asserts each persisted audit record produced exactly
     one `LogRecord` with the expected structured fields.

7. **FR-7: Store-protocol contract.**
   `flywheel.store_protocols` grows `save_sdk_messages`, `list_sdk_messages`,
   and `read_audit_since(run_id, cursor)` methods. All three backends
   implement them and pass a shared conformance test suite.
   - Acceptance: existing store conformance tests are extended; each
     backend passes the same suite.

### Non-Functional Requirements

- **Performance**: cursor-based polling at 250ms must not regress the
  harness's iteration throughput. SDK message persistence happens in the
  invoker / harness write path; large payloads (multi-MB tool outputs) are
  stored as-is.
- **Security**: store contents are sensitive-by-default. Document this in
  `docs/vision.md` and the store schema header. No redaction is performed.
- **UX**: CLI human-output is readable at terminal width without
  horizontal scrolling for typical messages; large tool inputs/outputs are
  rendered with elided previews and a hint to use `--json` for the full
  payload.

## Behavior Specification

### Happy Path

1. Caller starts a task via `flywheel.harness.run_task`.
2. For each iteration, `invoke_iteration` produces an `IterationResult`
   and the harness (or the invoker, depending on implementation) calls
   `store.save_sdk_messages(run_id, attempt_number, iteration_number,
   messages)`. Each message is assigned the next per-run sequence number.
3. Harness emits its usual `EventRecord`s; each one is also assigned the
   next per-run sequence number atomically with persistence.
4. An operator runs `python -m flywheel.audit <run_id> --follow` in
   another terminal. The CLI begins yielding records in real time as the
   run progresses.
5. On terminal status, the `--follow` iterator drains remaining records
   and exits.
6. Later replay: the same command without `--follow` prints the full
   chronological audit trail.

### Error Handling

| Error Condition                                         | Expected Behavior                                                                                                                                  |
| ------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------- |
| Store raises while persisting SDK messages              | Harness emits `harness.audit_write_failed` event, finalizes attempt as `INTERNAL_ERROR`, transitions lifecycle to `INTERNAL_ERROR`; retry policy applies. |
| Store raises while persisting an `EventRecord`          | Same as above. Audit-write failures are non-recoverable mid-iteration.                                                                             |
| `audit.stream(run_id)` called for unknown run_id        | Yields nothing and returns; no exception. CLI prints "no records for run_id <id>" and exits 0.                                                     |
| `audit.stream(run_id, follow=True)` for terminal run    | Drains existing records and exits immediately (no polling).                                                                                        |
| CLI invoked against a pre-feature store                 | Out of scope. Store-version check should refuse to open the legacy schema and surface a clear "store must be re-created" message.                  |
| `attach_logger` called twice with same logger + run_id  | Second call returns a distinct handle; both emitters receive records (caller is responsible for not double-handling).                              |

### Edge Cases

| Case                                                                              | Expected Behavior                                                                                                                       |
| --------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| Iteration produces zero messages (e.g. immediate failure before any SDK message)  | No SDK message rows written; harness still records `EventRecord`s for the iteration. Audit stream shows only the events for that gap.   |
| Iteration produces a multi-MB tool result                                         | Persisted in full. No truncation. Documented as a known cost trade-off.                                                                 |
| Two attempts on the same run_id                                                   | Sequence number continues monotonically across attempts; messages are partitioned by `attempt_number`/`iteration_number` for filtering. |
| Live follower polls during a write                                                | Cursor-based reads see only records with assigned sequence numbers; partial writes are not visible (transactional boundary).            |
| Harness crashes mid-iteration                                                     | `finalize_stranded_lifecycle` already exists; it must also assign sequence numbers to any `EventRecord`s it emits during finalization.  |
| Concurrent CLI followers                                                          | Each maintains its own cursor; both observe the same total ordering.                                                                    |
| Records appear with identical timestamps                                          | Sequence number breaks the tie; readers see deterministic ordering.                                                                     |

## Technical Context

### Affected Apps

- `flywheel` (single Python package): new module `flywheel.audit`; schema
  migrations; store protocol extension; harness/invoker writes.

### Integration Points

- `claude-agent-sdk`: source of the `Message` stream captured in
  `flywheel.invoker`. No changes to the SDK; we serialize its dataclasses
  to JSON-compatible payloads in the new persistence path.
- `flywheel.store_sqlite`: schema migration adds `sdk_messages` table and
  a `sequence` column on `events`; per-run sequence counter persisted in
  a `run_sequence` table or as a max() lookup with appropriate locking.
- `flywheel.store_postgres`: same migration applied to the Postgres mirror
  (`src/flywheel/_schema/persistence-schema.sql` updated; Postgres mirror
  alongside it).
- `flywheel.store_memory`: in-process equivalents, no migration needed.
- Python `logging`: opt-in emitter that converts `AuditRecord` to
  `LogRecord` for consumers wiring stdlib logging.

### Relevant Existing Code

- `src/flywheel/invoker.py`: where `Message` instances are collected today
  before being distilled into `InvocationSignals`. The capture write goes
  here or in a thin wrapper called by the harness.
- `src/flywheel/harness.py`: emits all current `EventRecord`s via the
  `_emit` helper; that helper is the natural place to integrate sequence
  assignment.
- `src/flywheel/store_protocols.py`: `EventRecord` and the read/write
  protocols live here; extended with `SdkMessageRecord` and new methods.
- `src/flywheel/store_sqlite.py`, `src/flywheel/store_postgres.py`,
  `src/flywheel/store_memory.py`: each backend gets the new tables/methods.
- `src/flywheel/_schema/persistence-schema.sql`: authoritative schema.
- `docs/vision.md`: observability section to be updated to reference the
  audit-stream contract.
- `docs/loop.md`: where envelope and harness behavior is canonized; will
  reference audit records as the canonical observability surface.

## Decisions Log

| Decision                                              | Choice                                                                            | Rationale                                                                                                                                                              |
| ----------------------------------------------------- | --------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Where SDK records live                                | Same store, new tables                                                            | Single source of truth, transactional consistency with `EventRecord`, queryable via existing store backends.                                                           |
| What gets captured                                    | Everything observed (lossless `Message` stream)                                   | Vision-level promise of full traceability; selective capture would force the operator to guess what mattered before they had the bug.                                  |
| Inspection UX                                         | Python API + `python -m flywheel.audit` CLI                                       | Same loader serves operators (CLI) and programmatic consumers (library). No new top-level script.                                                                      |
| Persistence-failure policy                            | Fail the attempt (`INTERNAL_ERROR`)                                               | Auditability is non-negotiable; a run with a missing audit trail is a broken run.                                                                                      |
| Redaction                                             | Out of scope                                                                      | Capture verbatim; store is sensitive-by-default. Redaction belongs in a future layer that operates on top of the audit stream.                                         |
| Size caps                                             | No truncation                                                                     | Auditability > storage cost for MVP. Modern SQLite/Postgres handle multi-MB blobs fine. Revisit only if real usage proves it's needed.                                 |
| Existing `agent_output` fields                        | Keep both; transcript derivable from message stream but still persisted           | Backward compatibility, read efficiency, no behavior change for current consumers.                                                                                     |
| Python `logging` integration                          | Optional `attach_logger`, store-only canonical                                    | Store is authoritative; `logging.Logger` is a convenience for ops setups that route stdlib logging to existing sinks. No global side effects unless opted in.          |
| Live vs replay priority                               | Live first; replay = bounded form of same iterator                                | One code path, two modes. Polling cursor handles both without bifurcating the API.                                                                                     |
| Streaming mechanism                                   | Polling with monotonic cursor                                                     | Uniform across all three backends without push/notify wiring. Latency floor is acceptable for the target inspection use cases.                                         |
| API surface                                           | Single `audit.stream(run_id, follow=...)` iterator                                | Replay is `follow=False`; live tail is `follow=True`. Avoids two APIs that share 90% of the implementation.                                                            |
| Stream ordering                                       | Single per-run monotonic sequence shared by events and SDK messages               | Unambiguous chronological ordering for human readers and programmatic consumers; eliminates ts-tiebreak ambiguity.                                                     |
| CLI output formats                                    | Human-readable default + `--json` NDJSON                                          | Two formats cover the operator-debug case and the pipeline-consumer case. `--kind` / `--attempt` / `--summary` are explicit follow-ups.                                |
| Migration for pre-feature stores                      | Require fresh store; no backfill                                                  | This codebase has no production stores yet. A clean break avoids carrying a backfill shim forever for a small short-term population.                                   |

## Open Questions

- Sequence-counter implementation detail: a dedicated `run_sequence` table
  vs `MAX(seq) + 1` per insert under a per-run advisory lock. Either works;
  decision deferred to the `/task` implementation pass.
- Whether `SdkMessageRecord.payload` should be a typed Python dict or an
  opaque JSON blob. Typed risks coupling to the SDK's evolving dataclasses;
  opaque is simpler. Lean toward opaque JSON blob plus a small set of
  promoted top-level columns (`message_type`, `stop_reason`, `is_error`)
  for indexable queries.

## Next Steps

Run `/task 00006-FEATURE-audit-stream` to generate implementation tasks
from this spec.

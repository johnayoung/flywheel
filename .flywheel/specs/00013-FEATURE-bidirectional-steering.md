# Feature: Bidirectional steering

## Summary

Let an operator interrupt, inject a message into, or change the model of a
*running* agent, driven entirely from the CLI through a store-backed control
channel. This adopts the SDK's bidirectional `ClaudeSDKClient` in place of the
one-shot `query()` the invoker uses today, and adds a `control_commands` table
plus thin `flywheel` subcommands that enqueue commands the in-process invoker
applies live. Every applied command is itself an audit event, so the stream
remains the record of truth — including human intervention. This is the
architectural payoff of the observability/interactivity program and depends on
the live stream (00010) and clean interruption (00012) landing first.

## Background

The invoker drives the agent with the SDK's one-shot `query()`
(`src/flywheel/invoker.py:188`), a drain-to-completion async generator with no
control channel. The SDK also ships a bidirectional client — `ClaudeSDKClient`
(in `claude_agent_sdk`) — exposing `interrupt()`, `set_model()`,
`set_permission_mode()`, and a streaming `query(AsyncIterable[dict])` over a
persistent connection. Flywheel does not use it.

`docs/vision.md:159-172` frames blocking and interruption as first-class
intervention points; the loop is "the controller for a single task's execution
lifecycle" (`vision.md:33-46`). Steering is the natural extension: an operator
or system adjusts a run in flight without restarting it. The cross-process
trigger must be the store — Flywheel is store-centric and poll-everything (the
worker, `live`, and `audit` already coordinate through it), and `flywheel steer`
must work across the worker-daemon boundary where process stdin is unavailable.

This feature is sequenced last: it changes the invoke seam, so it is only safe
once the stream is live (00010, to observe the effect) and interruption is clean
(00012, the stop primitive it generalizes).

## Scope

### In Scope

- Adopt `ClaudeSDKClient` in the invoker for the production path: open a
  persistent connection per iteration, submit the prompt, and consume the
  response stream while a concurrent watcher can act on it. Preserve the existing
  `on_message` observation/persistence seam (00010) and the cancellation
  semantics (00012).
- A `control_commands` table and store verbs: `enqueue_command(run_id, kind,
  payload)` and `claim_commands(run_id)` (claim-once so a command applies a
  single time), behind the store protocol with sqlite/in-memory/postgres
  implementations and a schema-version bump + migration.
- A watcher coroutine in the invoker that races the message stream, claims
  pending commands for the run, and applies them to the `ClaudeSDKClient`:
  - `interrupt` -> `client.interrupt()` (generalizes 00012's stop into an
    in-band, store-triggered interrupt)
  - `say` -> inject an operator message into the live session
  - `set_model` -> `client.set_model(...)`
  - (`set_permission_mode` optional, same mechanism)
- Each claimed-and-applied command emitted as a telemetry audit event so the
  stream records the intervention and its effect.
- CLI producers: `flywheel steer <run_id> --say "..."`, `flywheel interrupt
  <run_id>`, `flywheel set-model <run_id> <model>` — thin commands that enqueue
  and exit.
- Tests with a fake bidirectional client: enqueue interrupt mid-stream -> agent
  stops + audit event; enqueue `say` -> message surfaces in the next turn;
  enqueue `set_model` -> applied once.

### Out of Scope

- **Auto-steering / policy-driven intervention** (e.g. acting on context-pressure
  thresholds). This is operator-driven only; automated triggers are a separate
  feature built on the same channel.
- **Multi-operator conflict resolution** beyond claim-once semantics. Commands
  apply in enqueue order; no locking UI.
- **Editing the immutable task definition.** Steering affects the running session
  only; the original `Task` stays immutable (clarifications live in lifecycle
  records, per project invariants).
- **A web/socket transport.** The channel is the store; CLI is the only producer
  in this feature.
- **Changing graders or validation.** Steering is execution-time; verification is
  unchanged.

## Requirements

### Functional Requirements

1. **FR-1: Persistent bidirectional session.** The production invoker drives the
   agent via `ClaudeSDKClient` with a connection that stays open across the
   iteration's response stream, preserving live message persistence (00010).
   - Acceptance: an integration-style test with a fake client asserts the prompt
     is submitted and the response stream is consumed message-by-message through
     the existing `on_message` seam.

2. **FR-2: Store-backed command channel.** `enqueue_command` persists a command;
   `claim_commands` returns and marks pending commands for a run exactly once.
   - Acceptance: a store-contract test enqueues two commands, claims them once
     (both returned), claims again (none returned).

3. **FR-3: Interrupt applies live.** An enqueued `interrupt` stops the running
   agent via `client.interrupt()`.
   - Acceptance: a test enqueues `interrupt` mid-stream and asserts the fake
     client's `interrupt()` was called and the run halted.

4. **FR-4: Message injection.** An enqueued `say` injects the operator message
   into the live session so it influences the agent's next turn.
   - Acceptance: a test enqueues `say` and asserts the injected content reached
     the client before the next response.

5. **FR-5: Model change.** An enqueued `set_model` applies via
   `client.set_model(...)` once.
   - Acceptance: a test asserts the fake client's `set_model` was called with the
     requested model exactly once.

6. **FR-6: Interventions are audited.** Each applied command emits a telemetry
   event into the run's audit stream.
   - Acceptance: a test asserts an audit event per applied command, in sequence.

7. **FR-7: CLI producers.** `flywheel steer/interrupt/set-model` enqueue the
   corresponding command for a run and exit; they work without being attached to
   the worker process.
   - Acceptance: a CLI test invokes each subcommand and asserts the matching
     `control_commands` row is enqueued.

### Non-Functional Requirements

- **Cross-process**: producers and the consuming watcher communicate only through
  the store; no shared process state, so `flywheel steer` works against a
  detached worker daemon.
- **Best-effort isolation**: the watcher must not break the agent run — a failed
  command application is recorded (audit event) and does not abort the stream,
  mirroring the `on_message` best-effort contract.
- **Claim-once**: a command applies a single time even with watcher restarts or
  concurrent workers (claim marks state atomically).
- **Purity**: `flywheel.task` / `flywheel.lifecycle` untouched; control commands
  are execution-time telemetry, not domain events; the task definition stays
  immutable.
- **Migration**: the new table ships with a schema-version bump and a forward
  migration; existing stores upgrade cleanly.

## Behavior Specification

### Happy Path

1. A run is live; the operator sees it via `flywheel.workflow live` (00011).
2. The operator runs `flywheel steer <run_id> --say "focus on the failing
   grader first"`; the command is enqueued in `control_commands`.
3. The invoker's watcher claims the command on its next poll, applies it to the
   `ClaudeSDKClient` (injects the message), and emits an audit event.
4. The agent's next turn reflects the injected guidance; the audit stream shows
   the persisted messages, the intervention event, and the resulting turns in
   one sequence.

### Error Handling

| Error Condition | Expected Behavior |
| --------------- | ----------------- |
| Command targets a run that is not currently in-flight | The command stays pending or is recorded as not-applicable; it is never applied to a different run. No crash. |
| `client.interrupt()` / `set_model()` raises | Record a failed-application audit event; do not abort the stream (best-effort). |
| Watcher poll loses the store connection briefly | Retry on the next tick; commands are claim-once so no double-apply. |
| Command enqueued after the iteration ended | Not applied to a completed iteration; remains for the next eligible iteration or is recorded as stale, per claim semantics. |

### Edge Cases

| Case | Expected Behavior |
| ---- | ----------------- |
| Multiple commands enqueued rapidly | Applied in enqueue order; each emits its own audit event. |
| `interrupt` via this channel vs SIGINT (00012) | Both reach `INTERRUPTED` via the same finalization; the store-triggered path additionally records a control event. |
| Concurrent workers, command for run owned by worker A | Only the owning worker's watcher applies it (the run is in-flight under A's lease); claim-once prevents B from double-applying. |
| `set_model` to an invalid model | Application fails -> failed-application audit event; run continues on the prior model. |

## Technical Context

### Affected Apps

- `flywheel` (root package): invoker (adopt `ClaudeSDKClient`, add the watcher),
  store protocol + implementations (control-command verbs, migration),
  `workflow.py` (new subcommands), tests.

### Integration Points

- **Feature 00010** (prerequisite): live persistence lets the operator observe
  the effect of a command and keeps the watcher's intervention events in the same
  live sequence.
- **Feature 00012** (prerequisite): clean cancellation/`INTERRUPTED` is the stop
  primitive the store-triggered `interrupt` generalizes; both share one
  finalization path.
- **Feature 00011**: the live operator surface is how an operator finds the
  `run_id` and watches a steer take effect.
- **Audit stream** (`flywheel.audit`): surfaces the control events with no audit
  change (they are ordinary telemetry events on the shared sequence).

### Relevant Existing Code

- `src/flywheel/invoker.py:135-200` — `invoke_iteration`, the one-shot `query()`
  call (`:188`), and the `on_message` seam to preserve.
- `claude_agent_sdk` `ClaudeSDKClient` — `query()`, `interrupt()`, `set_model()`,
  `set_permission_mode()`, `receive_response()` (the bidirectional API to adopt).
- `src/flywheel/store_protocols.py` — where the control-command verbs and record
  type register, alongside `EventStore` / `SdkMessageStore`.
- `src/flywheel/store_sqlite.py` — `_next_run_sequence`, schema version /
  migrations; add `control_commands` and bump the version.
- `src/flywheel/store_memory.py`, `src/flywheel/store_postgres.py` — mirror the
  verbs and table.
- `src/flywheel/workflow.py` — subcommand registration (mirror existing
  `_cmd_*` patterns) for `steer` / `interrupt` / `set-model`.
- `src/flywheel/_schema/persistence-schema.sql` — the SQLite schema and its
  Postgres mirror; add the `control_commands` table.
- `docs/vision.md:159-172` — the intervention-points framing this realizes.

## Decisions Log

| Decision | Choice | Rationale |
| -------- | ------ | --------- |
| Transport | Store-backed `control_commands` table | Store-centric, poll-everything design; works across the worker-daemon boundary where stdin is unavailable. |
| SDK API | Adopt `ClaudeSDKClient` (persistent) | One-shot `query()` has no control channel; the bidirectional client is required for interrupt/inject/model-change. |
| Sequencing | Last, after 00010 + 00012 | Changes the invoke seam; only safe once the stream is observable and interruption is clean. |
| Intervention record | Telemetry audit event per applied command | Keeps the stream the source of truth, including human steering; no domain-event/purity impact. |
| Claim semantics | Claim-once, enqueue-order | Prevents double-apply across watcher restarts and concurrent workers. |
| Trigger scope | Operator-driven CLI only | Automated/policy steering (e.g. on context pressure) is a separate feature on the same channel. |

## Open Questions

None — design resolved during the observability/interactivity planning pass
(`~/.claude/plans/ok-it-worked-but-spicy-firefly.md`). The exact watcher polling
mechanism (store notifier push vs interval poll) is an implementation choice for
the agent, bounded by: claim-once, best-effort isolation, and no event-loop
blocking.

## Next Steps

Run `/task 00013-FEATURE-bidirectional-steering` after 00010 and 00012 land. This
completes the observability/interactivity program
(`~/.claude/plans/ok-it-worked-but-spicy-firefly.md`).

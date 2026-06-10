# Feature: TUI Operator Console

## Summary
A new `flywheel-tui` workspace package providing an interactive terminal console (contrabass-style): a realtime dashboard of all in-flight runs, with Enter-to-drill-in to a rendered live transcript of the selected agent session, and inline steering (send message, interrupt, approve/reject gates) via the existing control-command channel.

## Background
`flywheel-orchestrate live` renders a static text table of in-flight runs. The contrabass CLI demonstrates the desired experience: a realtime worker table plus the ability to open an agent's session and interact with it. All required infrastructure already exists — specs 00010 (message-granular audit stream), 00011 (live rows), 00012 (interrupt), and 00013 (bidirectional steering) shipped the data and control planes. This feature is purely a presentation layer over them; nothing changes in `flywheel.task` or `flywheel.lifecycle`.

## Scope

### In Scope
- New workspace package `packages/flywheel-tui` (depends on `flywheel-orchestrator`; nothing depends on it), CLI entry point `flywheel-tui`, built on Textual.
- Interactive-first command surface: bare invocation opens the TUI; `--json` or non-TTY stdout emits a machine-readable snapshot and exits (Claude Code print-mode pattern).
- Dashboard screen: one row per active run (RUNNING / VALIDATING / AWAITING_APPROVAL) with task id, status, attempt/iteration, age, tokens, cost, last action; summary header with active-worker count, queued/done/failed counts, total tokens/cost, runtime; keyboard navigation (up/down, Enter, quit, help).
- Session screen: rendered transcript of the selected run — agent text, tool calls (name + key args, collapsed results), operator injections, and lifecycle/gate events interleaved by sequence; tail-follows live, scrollback into history.
- Steering from the session screen: compose box (`say`), interrupt key binding, and approve/reject (with optional feedback) when the run is AWAITING_APPROVAL — all via `enqueue_command` with existing `CONTROL_COMMAND_*` verbs.
- Command feedback: enqueued commands render as pending until a `harness.control_command_applied` / `_failed` event appears in the run's stream.
- Store resolution identical to `flywheel-orchestrate` (policy-carried runtime paths under `.flywheel/`), works against both SQLite and Postgres via existing store protocols.

### Out of Scope
- `set_model` UI surface (channel supports it; no v1 binding).
- Push/notify freshness channel — polling only; no new store infrastructure.
- Changes to `flywheel-orchestrate live`/`status` — they remain the lightweight scriptable surfaces.
- A full all-tasks board (queued/blocked/done browsing); dashboard shows active work only.
- Historical-run browser (opening transcripts of runs no longer in or lingering past the active set).
- Implementing the redaction layer (spec 00014) — the TUI consumes whatever operator-facing read path 00014 establishes when it lands; raw reads until then.
- Any new `Status`/`Outcome`, schema, grader, store-protocol, or control-command additions.

## Requirements

### Functional Requirements
1. **FR-1**: `flywheel-tui` with a TTY opens a Textual app on the dashboard screen; `flywheel-tui --json` (or piped stdout) prints one JSON snapshot of the dashboard data and exits 0.
   - Acceptance: run both forms against a store with active runs; TUI renders rows, JSON mode emits parseable output with the same fields.
2. **FR-2**: Dashboard rows reflect store state with at most ~1s staleness, sourced by polling `collect_live_rows()` (or its extraction); summary header aggregates across rows plus task-state counts.
   - Acceptance: with a run mid-iteration, token/last-action columns visibly update within 2 polls; counts match `flywheel-orchestrate status`.
3. **FR-3**: Up/down moves row selection; Enter opens the session screen for the selected run; Escape/back returns to the dashboard; `q` quits; `?` toggles a key-binding help footer.
   - Acceptance: scripted Textual pilot test drives the navigation and asserts screen transitions.
4. **FR-4**: The session screen renders the run's merged stream (via `read_audit_since` cursor tailing): agent text blocks, tool calls with collapsed results, operator `say` injections, and lifecycle/gate events, ordered by sequence; new messages append while the view tail-follows; scrolling up pauses follow, end-key resumes it.
   - Acceptance: against a live run, injected and agent messages appear in order without restart; pilot test over a seeded store asserts rendering of each message class.
5. **FR-5**: Compose box enqueues `CONTROL_COMMAND_SAY` for the viewed run; a key binding enqueues `CONTROL_COMMAND_INTERRUPT`; when status is AWAITING_APPROVAL the gate instruction is shown with approve/reject (reject accepts optional feedback) using the existing verbs.
   - Acceptance: each action produces exactly one row in `control_commands` with the correct kind/payload; live-loop test sees the watcher claim and apply it.
6. **FR-6**: An enqueued command renders as pending in the session view until the corresponding `harness.control_command_applied` or `_failed` event arrives, then resolves (applied `say` messages appear in the transcript; failures surface the error). Steering verbs are disabled when the viewed run is not in an active status.
   - Acceptance: pilot test seeds the applied event after a delay and asserts the pending-to-applied transition; verbs unavailable on a DONE run.
7. **FR-7**: When a run reaches a terminal state: the open session view stays put, shows a terminal-status banner, keeps the transcript scrollable, and disables steering; on the dashboard the row lingers dimmed for ~30s before dropping.
   - Acceptance: drive a run to DONE while viewing; banner appears, compose disabled; dashboard shows dimmed row, gone after linger window.

### Non-Functional Requirements
- **Performance**: ~1s poll cadence; session tailing is cursor-incremental (`read_audit_since`), never re-reading full history per tick. Must stay responsive with 10+ concurrent runs.
- **Security**: transcript reads go through the same operator-facing read path the audit surface uses, inheriting spec 00014 redaction when it ships.
- **UX**: keyboard-only operation; bottom help bar listing bindings (contrabass-style); no emojis; degrades gracefully on terminal resize.

## Behavior Specification

### Happy Path
1. Operator runs `flywheel-tui` in a project with an active `orchestrate` loop.
2. Dashboard shows the summary header and one row per in-flight run, refreshing ~1s.
3. Operator arrows to a worker, presses Enter; the session screen opens, tail-following the rendered transcript.
4. Operator types a message in the compose box and submits; it shows as pending, the watcher claims it, the `applied` event arrives, and the injected turn renders in the transcript followed by the agent's response.
5. Escape returns to the dashboard; `q` exits cleanly.

### Error Handling
| Error Condition | Expected Behavior |
| --------------- | ----------------- |
| No store found at resolved path | Exit with a clear message naming the path and the `init` remedy; no TUI launch |
| Store has zero active runs | Dashboard renders header + empty-state line; keeps polling |
| Steering a run whose status left the active set between render and submit | Command not enqueued; inline notice that the run is no longer steerable |
| `harness.control_command_failed` arrives for a pending command | Pending marker becomes failure notice with the event's error detail |
| Store read raises (e.g. DB locked/unavailable) | Status-bar warning, last good frame stays on screen, polling retries; persistent failure does not crash the app |
| `--json` with no TTY-only flags | Snapshot printed, exit 0, no escape sequences |

### Edge Cases
| Case | Expected Behavior |
| ---- | ----------------- |
| Viewed run terminates mid-session | Terminal banner, transcript scrollable, steering disabled (FR-7) |
| Run finishes while on dashboard | Row dims, lingers ~30s, then drops |
| Command enqueued but worker dead (never claimed) | Marker stays pending indefinitely; no false "delivered" state |
| Operator scrolled up when new messages arrive | Follow paused; indicator shows new activity; End resumes follow |
| Two TUI instances against one store | Both are read-only pollers plus enqueue-only writers; no conflict (claim-once semantics live in the watcher) |
| Very large transcript on open | Load most recent window first, lazy-fetch older history on scrollback |
| AWAITING_APPROVAL run selected | Session screen surfaces the gate instruction and approve/reject affordances prominently |

## Technical Context

### Affected Apps
- `packages/flywheel-tui` (new): Textual app, screens, snapshot mode, CLI entry point, tests.
- `packages/flywheel-orchestrator`: at most a refactor to make `collect_live_rows()` (`_workflow.py:892`) importable as a public seam; no behavior change.
- `packages/flywheel` (core): no changes expected.

### Integration Points
- Read plane: `collect_live_rows()` for dashboard rows; `read_audit_since` / `list_sdk_messages` (`store_protocols.py`) for transcript tailing; `load_lifecycle` for status/gate detail.
- Write plane: `enqueue_command` with existing `CONTROL_COMMAND_SAY` / `_INTERRUPT` / `_APPROVE` / `_REJECT` (`invoker_client.py:64-68`); the in-process watcher (spec 00013) remains the sole claimant/applier.
- Feedback: `harness.control_command_applied` / `_failed` events in the run's event stream.
- Store resolution: same policy-carried runtime paths as `flywheel-orchestrate`.

### Relevant Existing Code
- `packages/flywheel-orchestrator/src/flywheel_orchestrator/_workflow.py:892-1008`: `LiveRunRow` / `collect_live_rows()` — dashboard data source.
- `packages/flywheel-orchestrator/src/flywheel_orchestrator/_workflow.py:1091-1220`: existing `live`/`status` renderers — the non-interactive surfaces that stay untouched.
- `packages/flywheel/src/flywheel/invoker_client.py`: control verbs and watcher semantics the TUI writes against.
- `packages/flywheel/src/flywheel/store_protocols.py:432`: `read_audit_since` cursor API for tailing.
- `.flywheel/specs/00010` – `00013`: the shipped substrate this feature presents.

## Decisions Log

| Decision | Choice | Rationale |
| -------- | ------ | --------- |
| MVP scope | Dashboard + drill-in + steering in one spec | Data and control planes already shipped; the drill-in + steer loop is the point of the feature |
| Placement | New package `flywheel-tui` depending on orchestrator | Keeps the Textual dependency out of orchestrator installs; respects the one-way dependency arrow |
| Command surface | `flywheel-tui` interactive-first; `--json`/non-TTY snapshot mode | Mirrors Claude Code's interactive-default + print-mode pattern; `live`/`status` remain scriptable surfaces |
| Steering verbs in v1 | say, interrupt, approve/reject; no set_model | Operator-confirmed; set_model lacks v1 demand |
| Session view | Rendered chat-style transcript | "Opening the agent's session" requires readable turns, not a log feed |
| Framework | Textual | Async-native, ships widgets/bindings/screens needed here; dependency isolated to the new package |
| Freshness | Poll store ~1s; cursor-incremental tailing | Matches existing cross-process patterns; zero harness changes; both backends supported |
| Dashboard rows | In-flight only + summary header | Contrabass model: "what is happening right now"; full board out of scope |
| Terminal-state UX | Banner + ~30s dashboard linger | Completions visible; reading a finished transcript is not interrupted |
| Steering feedback | Track enqueued-to-applied via events; verbs disabled on inactive runs | Honest about the async store channel; avoids silent loss on dead workers |
| Redaction | Consume the operator-facing read path 00014 defines; raw until it lands | Single enforcement point for secrets policy; not a blocker |
| Loop-path coverage | Not required | Read-only over existing tables plus existing `CONTROL_COMMAND_*` verbs; trips no spec-00017 Trigger Set signal (no new status, schema, grader, protocol def, or control constant) |

## Open Questions
None.

## Next Steps
Run `/task 00020-FEATURE-tui-operator-console` to generate implementation tasks from this spec.

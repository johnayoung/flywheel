# Feature: Data taxonomy alignment

## Summary

Realign flywheel's persistence with `docs/data-taxonomy.md`: the relational
store shrinks to its taxonomy-correct role — OLTP rows (lifecycles, task
catalog, claims) plus a true append-only ledger (domain events,
grader_results) — while telemetry (harness telemetry events and the verbatim
SDK message stream) moves out of the database into per-run JSONL files behind
a pluggable `TelemetrySink`. Load-bearing aggregates the dashboard reads are
rolled up into OLTP rows, operator steering becomes a ledger fact, and each
run records the world state it executed against.

## Background

The store currently mixes three taxonomy classes in one database:

- `sdk_messages` is telemetry stored at ledger grade: 100-10,000 rows/run at
  0.5-5 KB each, written per message, and never read by anything load-bearing
  — only the TUI transcript tailer and the forensic log renderer consume it.
- `events` interleaves sparse domain rows (a textbook ledger: folded into
  state, replayed on load) with high-volume `category='telemetry'` rows in
  the same table, sharing a per-run sequence counter (`run_sequence`) that
  churns once per agent message.
- `.flywheel/logs/worker/*.log` is then a *re-render* of that same DB data
  (`write_run_log` reads `list_events` and formats lines) — telemetry written
  into the system of record and copied out to files, the inverse of the
  taxonomy's flow.

Consequences: the database grows without bound, the dashboard derives
tokens/cost/idle by scanning telemetry events on the request path (an
analytical read served by the OLTP store), ephemeral coordination rows
(`control_commands`, `task_claims`) accumulate as if they were ledger, and
operator interventions are visible only while a queue row happens to survive.

This spec partially unwinds 00010 (message-granular audit-stream persistence)
by design: the per-message *cadence* and live observability it delivered are
preserved, but the durable destination changes from the store to a telemetry
file. The 00014 `Redactor` is wiring-agnostic over `AuditRecord` and carries
over unchanged: files are verbatim and sensitive-by-default like the store
was; redaction stays read-time.

## Scope

### In Scope

- A `TelemetrySink` protocol (pluggable destination for telemetry records)
  with one MVP implementation, `FileTelemetrySink`, writing one JSONL file
  per run.
- Routing SDK messages and telemetry-category harness events to the sink
  instead of the store; mirroring domain events into the sink so each run
  file is a self-contained, totally-ordered observability stream.
- Schema change: drop `sdk_messages` and `run_sequence`; `events` becomes
  domain-only. Bump `CURRENT_SCHEMA_VERSION`; stores refuse pre-feature
  databases with the existing "store must be re-created" error.
- Store-protocol cleanup: remove `append_sdk_message`, `save_sdk_messages`,
  `list_sdk_messages`, `read_audit_since`, and the events-side telemetry
  write path from the protocols and every implementation (sqlite, memory,
  postgres) and wrapper.
- OLTP roll-up: per-attempt token/iteration/activity aggregates updated at
  iteration boundaries so the dashboard snapshot is computed from relational
  rows alone, with no telemetry scan.
- Retargeting the observability readers — TUI `TranscriptTailer`,
  `flywheel.audit` stream/CLI — to tail the per-run JSONL file, preserving
  read-time redaction (00014).
- Deleting `write_run_log` (the forensic re-render) and the log-retention
  sweep; the run JSONL is the primary and only telemetry artifact.
- Recording operator steering in the ledger: a domain event is appended when
  a control command is applied; applied command rows and released/expired
  claim rows are deleted (queue hygiene, not retention).
- World-state reproducibility: each attempt records the resolved base commit
  SHA of its workspace and the effective agent model id.
- Documentation: update `docs/vision.md` (audit stream section),
  `docs/loop.md` if it references the merged sequence, the persistence-schema
  header comments, and the `.flywheel` init scaffold/`.gitignore`.

### Out of Scope

- **Retention, rotation, and archival of telemetry files.** Flywheel emits
  the files; their lifecycle (rotation, expiry, promotion to an analytics
  archive) is the operator's responsibility via external tooling. No sweep,
  no `[retention]` config, no automatic phase-archive promotion. (Decided
  explicitly — see Decisions Log.)
- Columnar/OLAP export of decision-analytics data (grader_results payloads,
  archived transcripts). Future work when audit tooling outgrows the
  relational store and grep.
- Write-time redaction. Files are verbatim; the 00014 read-time `Redactor`
  applies on read. The sink keeps the seam open for a later opt-in.
- A streaming/event-log hop for multi-host observability. `FileTelemetrySink`
  is worker-local; a TUI on a different host than the worker cannot tail
  transcripts. The `TelemetrySink` protocol is the extension point; building
  a networked sink is future work.
- Forward migration of existing databases. Old stores are refused, not
  migrated.
- Auto-generating the in-loop verification fixture. This spec trips the
  loop-path trigger set (see Decisions Log); only the slot is auto-required —
  the test body is authored by hand.

## Requirements

### Functional Requirements

1. **FR-1: TelemetrySink protocol.** A new protocol with a single append
   verb taking a telemetry record (kind, ts, attempt/iteration coordinates,
   payload mapping). Implementations own durability semantics. The protocol
   lives where the store protocols live; nothing in core imports a concrete
   sink.
   - Acceptance: harness tests run against a fake sink; the protocol has no
     file/JSON imports at its definition site beyond what `store_protocols`
     already allows.

2. **FR-2: FileTelemetrySink.** Writes one JSONL file per run at
   `<logs_root>/runs/<run_id>.jsonl` (default logs root `.flywheel/logs`),
   one record per line, append-only, flushed per line. File write order is
   the canonical observability ordering for the run.
   - Acceptance: a run produces a single file whose lines parse as JSON and
     appear in emission order; concurrent runs write disjoint files.

3. **FR-3: SDK messages route to the sink.** Each SDK message is appended to
   the sink as it arrives (per-message cadence preserved from 00010) and is
   no longer written to the store. `sdk_messages` store verbs are removed
   from the protocol and all implementations.
   - Acceptance: after a run, the store contains zero SDK-message rows and
     the run file contains one line per streamed message, in order; grep
     shows no remaining callers of the removed verbs.

4. **FR-4: Telemetry events route to the sink.** Harness telemetry
   (`iteration_completed`, `message_turn`, `grader_started/finished`,
   `occupied_total_tokens`, `rate_limit_event`, ...) is appended to the sink,
   not the `events` table. Domain events continue to be appended to the
   store exactly as today (ledger), and are additionally mirrored as lines
   into the sink so the run file renders the full timeline. The store row
   remains authoritative; the file line is disposable.
   - Acceptance: after a run, every `events` row has `category='domain'`
     semantics (see FR-5); the run file interleaves messages, telemetry, and
     domain mirrors in emission order.

5. **FR-5: Schema reduction.** New schema version drops `sdk_messages` and
   `run_sequence`. `events` holds only domain rows; the `category` column
   and its partial index are removed (or `events` is renamed to
   `domain_events` — implementer's choice, recorded in the schema header).
   Domain-event ordering stays strictly monotonic per run without the shared
   cross-table counter. Stores compare `schema_version` on open and refuse
   older databases with the existing re-create error.
   - Acceptance: bootstrap of a fresh DB matches the new schema; opening a
     prior-version DB raises `StoreSchemaError`; replay (`list_domain_events`
     fold) behaves identically on the new schema.

6. **FR-6: OLTP aggregates.** Attempt rows carry rolled-up counters —
   cumulative input/output/cache tokens, iterations completed, turns, and a
   last-activity timestamp — updated at iteration boundaries through the
   existing optimistic-concurrency write path. The dashboard snapshot
   (`build_snapshot`, `collect_live_rows`) computes tokens, cost, idle, age,
   and iteration position from relational rows only.
   - Acceptance: snapshot output for a driven run matches today's values
     (tokens, cost, iterations, idle/age within timer tolerance) with the
     telemetry tables absent; no snapshot code path reads the sink.

7. **FR-7: Telemetry loss is non-fatal; ledger loss stays fatal.** A sink
   append failure must not abort or finalize the attempt: the harness
   records the failure once (stderr + a marker line attempt in the sink when
   possible) and continues; subsequent failures are silent for that run. A
   domain-event or lifecycle write failure keeps today's strict behavior
   (`INTERNAL_ERROR` finalization).
   - Acceptance: a sink stub that always raises yields a completed run with
     correct lifecycle state; a store stub that raises on domain append
     yields `INTERNAL_ERROR` exactly as today.

8. **FR-8: Readers tail the file.** The TUI `TranscriptTailer` and
   `flywheel.audit` stream/follow/CLI read from the per-run JSONL (cursor =
   byte offset or line count), preserving: live follow during a running
   lifecycle, terminal-status exit, and read-time redaction via the 00014
   `Redactor` applied to each parsed record. A missing file reads as empty;
   a partial trailing line (crash mid-write) is held back until complete or
   EOF-final.
   - Acceptance: TUI session screen and `flywheel audit --follow` render a
     live run end-to-end from the file; `--redact` output contains no
     seeded secret.

9. **FR-9: Forensic re-render removed.** `write_run_log`, `write_run_logs`,
   the per-run `.log` format, and the log-retention sweep
   (`DEFAULT_RETENTION_DAYS` log half) are deleted. The worktree-hygiene
   half of the sweep (parked worktrees) is unchanged.
   - Acceptance: no `.log` files are produced by a run; worktree sweep tests
     still pass; grep shows no callers of the removed functions.

10. **FR-10: Steering is ledger.** When the in-run watcher claims and
    applies a control command, a domain event recording the command (kind,
    payload, operator-supplied fields, applied-at) is appended to the
    ledger. Applied command rows are deleted after the domain event commits;
    released or expired `task_claims` rows are deleted on release/steal.
    - Acceptance: after an interrupt/say/set_model is applied, the run's
      domain events include the steering record; `control_commands` contains
      no applied rows; completed tasks leave no claim rows.

11. **FR-11: World-state pinning.** Every attempt records (in
    `agent_context_json` or the `AttemptStarted` payload — one place,
    documented) the resolved base commit SHA its workspace was created from
    and the effective model id actually passed to the SDK (after
    `--model`/policy/default resolution).
    - Acceptance: a driven run's attempt record contains a 40-char SHA that
      matches the worktree base and the resolved model id; both survive
      replay.

### Non-Functional Requirements

- **Performance**: removes per-message DB writes and `run_sequence` churn
  from the hot path; sink appends are buffered-line writes. Dashboard
  snapshot becomes O(rows) over lifecycles/attempts instead of scanning
  telemetry. No regression in iteration wall time on a realistic run.
- **Security**: run JSONL files are sensitive-by-default (verbatim payloads),
  covered by `.flywheel/.gitignore`; the schema-header confidentiality note
  extends to the logs directory. Redaction remains read-time (00014).
- **Purity**: `flywheel_core.task` and `flywheel_core.lifecycle` untouched;
  purity tests unweakened. The `TelemetrySink` protocol definition carries
  no I/O imports; `FileTelemetrySink` lives outside the pure modules.

## Behavior Specification

### Happy Path

1. The worker claims a task, resolves the phase base SHA and effective model,
   and seeds the lifecycle; `AttemptStarted` carries the world-state pin.
2. The harness opens (or is handed) a `FileTelemetrySink` for the run; as the
   SDK streams, each message and each telemetry event is appended as one
   JSONL line; each domain event is appended to the store (ledger) and
   mirrored as a line.
3. At each iteration boundary the harness rolls token/iteration/activity
   counters into the attempt row via the existing versioned write.
4. The TUI tails `<logs_root>/runs/<run_id>.jsonl` for the transcript and
   reads the dashboard snapshot from relational rows only.
5. An operator issues `interrupt`; the watcher claims the command, applies
   it, appends the steering domain event, and deletes the queue row.
6. The run finishes; the store holds the lifecycle, attempts with aggregates,
   domain events, and grader_results; the run file holds the full
   observability stream. Flywheel does nothing further with the file.

### Error Handling

| Error Condition | Expected Behavior |
| --------------- | ----------------- |
| Sink append raises (disk full, unwritable dir) | Run continues; failure recorded once (stderr); lifecycle unaffected. |
| Domain-event / lifecycle write raises | Unchanged: `INTERNAL_ERROR` finalization via existing machinery. |
| Steering domain-event append fails after command claimed | Command application proceeds (the live SDK action already happened); queue row is retained (not deleted) so the failure is visible; error surfaced via stderr. |
| Pre-feature database opened | `StoreSchemaError` with the existing "store must be re-created" message. |
| Reader opens a run with no file yet | Treated as empty stream; follow mode waits for the file to appear. |
| Partial trailing JSONL line (crash mid-write) | Reader withholds the incomplete line; on next append or EOF-final it is completed or discarded. |

### Edge Cases

| Case | Expected Behavior |
| ---- | ----------------- |
| Zero-message run | Run file may contain only domain mirrors (or not exist if nothing was emitted); readers handle both. |
| Claim stolen mid-run, resumed on another worker | Each worker appends to its own local file for the same run_id; the ledger remains the single source of truth. Split files are a documented limit of the file sink. |
| Two runs of the same task | Distinct run_ids, distinct files; no collision. |
| TUI opens mid-run | Tail starts from file beginning; cursor advances normally. |
| Postgres backend | Identical behavior: ledger/OLTP in Postgres, telemetry in worker-local files via the same sink seam. |
| Operator deletes a run file while the TUI tails it | Reader surfaces end-of-stream/missing-file gracefully; no crash; ledger queries unaffected. |

## Technical Context

### Affected Apps

- `flywheel-core`: store protocols (remove sdk-message/audit-merge verbs, add
  `TelemetrySink` protocol + telemetry record type), schema + bump, sqlite/
  memory/postgres stores, harness write paths, audit module retarget,
  steering domain event kind, attempt aggregates.
- `flywheel-orchestrator`: `_workflow.py` (init scaffold, `DEFAULT_LOG_DIR`),
  claims deletion on release, policy (`[paths]` logs root if added).
- `flywheel-worktree`: `worker.py` — delete `write_run_log(s)` and the log
  sweep half; record base SHA at workspace prep.
- `flywheel`: TUI `TranscriptTailer`/session screen file tailing, dashboard
  snapshot from OLTP rows, supervisor log path unchanged.
- `docs/`: vision.md audit-stream section, loop.md if affected, taxonomy
  cross-reference.

### Integration Points

- **00010**: per-message cadence and live follow preserved; durable
  destination changes from store to sink. The shared `run_sequence` design is
  retired with it.
- **00014**: `Redactor` applies unchanged at the new read path (file tail ->
  parse -> redact -> render). Sink keeps the write-time seam open.
- **00017**: this spec trips the loop-path trigger set (signals 2 and 4);
  `/task` must emit an `in-loop-verification` slot. Because migration policy
  is refuse-not-migrate, the schema half of that fixture asserts the
  `StoreSchemaError` refusal on a v(N-1) store rather than a forward
  migration.

### Relevant Existing Code

- `packages/flywheel-core/src/flywheel_core/_schema/persistence-schema.sql`
  — tables to drop/reshape; header comments to rewrite.
- `packages/flywheel-core/src/flywheel_core/store_protocols.py` — verbs to
  remove; sink protocol home.
- `packages/flywheel-core/src/flywheel_core/store_sqlite.py` (and memory/
  postgres mirrors) — `append_sdk_message`, `_next_run_sequence`,
  `read_audit_since` removal; aggregate columns.
- `packages/flywheel-core/src/flywheel_core/harness.py` — per-message
  persistence closure (retarget to sink), telemetry `append_event` call
  sites, audit-failure semantics split (FR-7), iteration-boundary roll-up.
- `packages/flywheel-core/src/flywheel_core/audit/__init__.py` and
  `audit/_cli.py` — retarget from `read_audit_since` to file tailing.
- `packages/flywheel-core/src/flywheel_core/events.py` — new
  `DomainEventKind` for steering.
- `packages/flywheel-worktree/src/flywheel_worktree/worker.py:552-660` —
  `write_run_log(s)`, `retention_sweep`.
- `packages/flywheel/src/flywheel/_session.py`, `_snapshot.py`,
  `_dashboard.py` — tailer + snapshot retarget.

## Decisions Log

| Decision | Choice | Rationale |
| -------- | ------ | --------- |
| Spec scope | One comprehensive spec | Pre-MVP; the pieces share seams (harness write paths, store protocol, schema) — doing them together avoids re-touching the same seams across specs. |
| Migration | Refuse old DBs, require re-create | No production data exists; avoids a destructive forward migration; matches the schema header's stated policy. |
| Telemetry destination | Pluggable `TelemetrySink`; `FileTelemetrySink` is the only MVP implementation, used for both backends | Taxonomy-correct now, and multi-host/Postgres fleets get a networked sink later without re-opening the harness. |
| Retention/rotation | Not flywheel's responsibility | Operator-owned via external tooling (logrotate etc.). Flywheel emits files and never deletes them; the existing 7-day log sweep is removed with the re-render. |
| Archival/analytics promotion | Not flywheel's responsibility | No automatic phase-archive copy of transcripts; operators/audit tooling decide what to keep. The ledger (domain events, grader_results) remains the durable decision record. |
| File redaction posture | Verbatim files, read-time redaction | Consistent with 00014; files inherit the store's sensitive-by-default contract and `.gitignore` coverage. |
| Telemetry failure semantics | Non-fatal (taxonomy: loss acceptable); ledger failures stay fatal | Follows directly from the taxonomy's telemetry guarantees; ends the current behavior where a telemetry write failure kills the attempt. |
| Run-file ordering | File write order is canonical for observability; ledger ordering canonical for state | Dissolves the cross-table `run_sequence` machinery; domain events keep strict per-run ordering in the store. |
| Domain mirror lines | Domain events dual-written to the sink | Makes each run file a self-contained timeline for rendering; the file is disposable, the row is authoritative. |
| Loop-path coverage | Required — spec trips signals 2 (schema) and 4 (store protocol) | `/task` must emit an `in-loop-verification` slot; fixture asserts schema refusal (not migration) per the migration decision. Auto-generating the fixture body is out of scope. |

## Open Questions

None — all decisions resolved during definition (2026-06-11).

## Next Steps

Run `/task 00025-FEATURE-data-taxonomy-alignment` to generate implementation
tasks from this spec.

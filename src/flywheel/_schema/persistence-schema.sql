-- Flywheel loop persistence schema.
--
-- Tables mirror the conceptual model: tasks is the content-addressed
-- catalog of task definitions a run can reference, lifecycles is the row
-- that mutates (with a Version column for optimistic concurrency), attempts
-- is the per-execution history, events is the timeline of harness-emitted
-- events, grader_results is the per-grader receipt log produced during
-- validation, sdk_messages is the verbatim agent message stream captured per
-- iteration, and run_sequence is the per-run monotonic counter that orders
-- events and sdk_messages into a single audit stream.
--
-- grader_results is append-only by contract: rows are written once when a
-- grader finishes and must not be updated or deleted. Treat the table as an
-- audit log — corrections go in new rows or compensating events. sdk_messages
-- and events share a per-run monotonic sequence number assigned at insert
-- time via run_sequence; readers merge the two on that column for the
-- canonical audit ordering.
--
-- Store contents are sensitive-by-default; payloads are persisted verbatim
-- and unredacted. Operators must treat the database file as confidential.
--
-- schema_version pins the on-disk schema version against the constant
-- CURRENT_SCHEMA_VERSION declared in flywheel.store_protocols. Stores read
-- the row on open and refuse pre-feature databases with a clear
-- "store must be re-created" error; there is no in-place migration.
--
-- Pragmas: WAL gives readers concurrent visibility while a writer is active;
-- foreign_keys is off-by-default in SQLite and we want it on.

PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

-- Task identity, versions, and orchestration metadata are modeled as three
-- tiers so every task reference in the store is a checkable foreign key
-- rather than a free-floating string:
--
--   tasks               -- the logical task (stable id). The FK anchor.
--   task_versions       -- immutable, content-addressed definitions.
--   task_tags           -- mutable labels for grouping/filtering.
--   task_prerequisites  -- mutable DAG edges for dependency scheduling.
--
-- A user thinks in all three tiers: a task is a durable thing they author,
-- tag, depend on, and run repeatedly (tasks); each run pins the exact
-- definition it executed (task_versions); and harnesses layered on flywheel
-- read tags + prerequisites to build parallelizable task DAGs (task_tags,
-- task_prerequisites). Splitting them lets the definition stay immutable
-- while the orchestration metadata around it stays editable.

-- tasks: the logical task identity. One row per id, created the first time a
-- task is saved or referenced. Carries no definition — it exists so that
-- task_versions, task_tags, task_prerequisites, and lifecycles can all
-- foreign-key a real, catalogued identity. A row may exist with no
-- task_versions yet (a task declared as a prerequisite before it is defined).
CREATE TABLE IF NOT EXISTS tasks (
  id            TEXT PRIMARY KEY,
  first_seen_at DATETIME NOT NULL,
  updated_at    DATETIME NOT NULL
);

-- task_versions: immutable, content-addressed task definitions. Keyed by
-- (task_id, content_hash) where content_hash is flywheel.loaders.task_digest
-- over the *executed* definition — goal, graders, and context only. tags and
-- prerequisites are deliberately excluded from the hash: they are mutable
-- orchestration metadata (see task_tags / task_prerequisites), so retagging
-- or rewiring the DAG never forks the definition a run pinned. Storage is
-- immutable and deduped: re-saving an unchanged definition is a no-op,
-- editing goal/graders/context adds a new version row. A run pins the exact
-- version it ran via lifecycles.task_content_hash.
CREATE TABLE IF NOT EXISTS task_versions (
  task_id       TEXT NOT NULL,
  content_hash  TEXT NOT NULL,
  goal          TEXT NOT NULL,
  graders_json  TEXT NOT NULL,
  context_json  TEXT NOT NULL,
  created_at    DATETIME NOT NULL,
  PRIMARY KEY (task_id, content_hash),
  FOREIGN KEY (task_id) REFERENCES tasks(id)
);
CREATE INDEX IF NOT EXISTS idx_task_versions_id_created
  ON task_versions(task_id, created_at);

-- task_tags: mutable labels on a logical task. Not part of the content hash,
-- so editing tags does not fork a definition. last-write-wins on save_task.
-- position preserves authoring order on read; the idx over tag serves the
-- reverse lookup ("every task labelled X") that grouping/filtering harnesses
-- rely on.
CREATE TABLE IF NOT EXISTS task_tags (
  task_id   TEXT NOT NULL,
  tag       TEXT NOT NULL,
  position  INTEGER NOT NULL,
  PRIMARY KEY (task_id, tag),
  FOREIGN KEY (task_id) REFERENCES tasks(id)
);
CREATE INDEX IF NOT EXISTS idx_task_tags_tag ON task_tags(tag);

-- task_prerequisites: mutable DAG edges. Both endpoints foreign-key tasks(id)
-- so a task can never depend on an uncatalogued id; save_task auto-registers
-- a bare identity row for any prerequisite that has not been defined yet, so
-- a DAG can be authored in any order without violating the FK. The CHECK
-- mirrors the schema rule that a task cannot be its own prerequisite. The idx
-- over prereq_task_id serves the dependents lookup ("everything waiting on
-- X") that a parallel scheduler walks to release the ready frontier.
CREATE TABLE IF NOT EXISTS task_prerequisites (
  task_id         TEXT NOT NULL,
  prereq_task_id  TEXT NOT NULL,
  position        INTEGER NOT NULL,
  PRIMARY KEY (task_id, prereq_task_id),
  FOREIGN KEY (task_id) REFERENCES tasks(id),
  FOREIGN KEY (prereq_task_id) REFERENCES tasks(id),
  CHECK (task_id <> prereq_task_id)
);
CREATE INDEX IF NOT EXISTS idx_task_prerequisites_prereq
  ON task_prerequisites(prereq_task_id);

-- lifecycles: the single mutable row per run, keyed by run_id. version drives
-- optimistic concurrency — every update carries an expected version and a
-- stale writer is rejected with OptimisticConcurrencyError. status, retries,
-- error, and agent_output are the rolled-up current state; timestamps_json
-- and blocked_requires_json are JSON text like the rest of the schema.
-- task_id foreign-keys tasks(id): every run references a real, catalogued
-- task identity (the seed auto-registers it), so a run can never point at a
-- task the store has never heard of. task_content_hash pins the exact
-- task_versions row the run executed and is the recovery key
-- load_task_for_run resolves; the harness always saves the definition before
-- seeding, so the pinned version is present. session_id carries the agent SDK
-- session id used to resume the brain across iterations.
CREATE TABLE IF NOT EXISTS lifecycles (
  run_id                  TEXT PRIMARY KEY,
  task_id                 TEXT NOT NULL,
  status                  TEXT NOT NULL,
  version                 INTEGER NOT NULL,
  retries                 INTEGER NOT NULL,
  error                   TEXT,
  agent_output            TEXT,
  session_id              TEXT,
  artifacts_dir           TEXT,
  worker_id               TEXT,
  timestamps_json         TEXT NOT NULL,
  updated_at              DATETIME NOT NULL,
  blocked_requires_json   TEXT,
  task_content_hash       TEXT,
  -- ordinal (index in task.graders) of the manual gate the lifecycle is
  -- currently parked on. NULL in every state except AWAITING_APPROVAL; the
  -- column is cleared on every -> READY/-> DONE/-> FAILED_VALIDATION edge
  -- in Lifecycle.transition_to (the same back-compat path as
  -- blocked_requires_json). Added in schema_version 5 via the forward
  -- migration each concrete store applies on bootstrap.
  awaiting_manual_ordinal INTEGER,
  FOREIGN KEY (task_id) REFERENCES tasks(id)
);

-- agent_context_json captures the agent identity for this attempt so the run
-- is interpretable across model, SDK, and prompt changes. Suggested keys:
-- model_id, model_version, agent_sdk_version, prompt_template_hash.
-- Per-grader failures are not denormalized here — query grader_results with
-- (run_id, attempt_number) and passed = 0 instead.
CREATE TABLE IF NOT EXISTS attempts (
  run_id              TEXT NOT NULL,
  number              INTEGER NOT NULL,
  attempt_run_id      TEXT,
  started_at          DATETIME NOT NULL,
  ended_at            DATETIME,
  outcome             TEXT,
  agent_output        TEXT,
  error               TEXT,
  agent_context_json  TEXT,
  PRIMARY KEY (run_id, number),
  FOREIGN KEY (run_id) REFERENCES lifecycles(run_id)
);

-- category discriminates a state-bearing 'domain' event (an event-sourced
-- member of the lifecycle log, folded into state) from a pure-observability
-- 'telemetry' event. Both share this table and the per-run sequence ordering
-- so the audit stream is one totally-ordered log; only domain rows are folded.
-- kind holds the DomainEventKind value for domain rows and the harness.*
-- string for telemetry rows.
CREATE TABLE IF NOT EXISTS events (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  run_id          TEXT NOT NULL,
  attempt_number  INTEGER,
  ts              DATETIME NOT NULL,
  kind            TEXT NOT NULL,
  payload_json    TEXT NOT NULL,
  sequence        INTEGER NOT NULL,
  category        TEXT NOT NULL DEFAULT 'telemetry',
  FOREIGN KEY (run_id) REFERENCES lifecycles(run_id),
  UNIQUE (run_id, sequence)
);

-- One row per grader execution. Append-only — see header.
--
-- grader_type is the discriminator (`command`, `transcript`, `rubric`,
-- `manual`). grader_spec_json snapshots the exact grader object as it
-- appeared in the task at run time, so historical truth survives later
-- edits to the task definition. payload_json holds per-type execution
-- detail; the expected shape by grader_type is:
--   command:    {run, exit_code, stdout_tail, stderr_tail,
--                stdout_path?, stderr_path?}
--   transcript: {observed: {turns, total_tokens, wall_seconds},
--                breached: "max_turns" | "max_total_tokens" | "max_wall_seconds"}
--   rubric:     {judge_model, judge_model_version, prompt_path,
--                artifacts: [...], per_assertion: [{text, verdict, rationale}],
--                usage: {input_tokens, output_tokens}}
--   manual:     {operator_id, decision, notes, decided_at}
CREATE TABLE IF NOT EXISTS grader_results (
  id                INTEGER PRIMARY KEY AUTOINCREMENT,
  run_id            TEXT NOT NULL,
  attempt_number    INTEGER NOT NULL,
  ordinal           INTEGER NOT NULL,
  grader_type       TEXT NOT NULL,
  grader_name       TEXT,
  grader_spec_json  TEXT NOT NULL,
  passed            INTEGER NOT NULL,
  duration_ms       INTEGER NOT NULL,
  payload_json      TEXT NOT NULL,
  ts                DATETIME NOT NULL,
  FOREIGN KEY (run_id, attempt_number) REFERENCES attempts(run_id, number)
);

-- events read paths. idx_events_run serves telemetry reads ordered by
-- wall-clock; idx_events_run_sequence serves cursor-paginated audit-stream
-- reads over the shared per-run sequence. idx_events_domain is a partial
-- index over the domain minority so list_domain_events (the event-sourced
-- state fold run on every lifecycle load) scans only the rows it folds
-- instead of walking interleaved telemetry. The partial index is additive:
-- CREATE INDEX IF NOT EXISTS materializes it on reopen of an existing
-- database, so it needs no schema_version bump (same path as task_claims).
-- grader_results reads are always scoped to one attempt's graders in ordinal
-- order.
CREATE INDEX IF NOT EXISTS idx_events_run ON events(run_id, ts);
CREATE INDEX IF NOT EXISTS idx_events_run_sequence ON events(run_id, sequence);
CREATE INDEX IF NOT EXISTS idx_events_domain
  ON events(run_id, sequence) WHERE category = 'domain';
CREATE INDEX IF NOT EXISTS idx_grader_results_run_attempt ON grader_results(run_id, attempt_number, ordinal);

-- sdk_messages: verbatim agent SDK message stream per iteration. Keyed by
-- an autoincrement id; the per-run audit ordering is carried by the
-- (run_id, sequence) tuple that's also the table's uniqueness invariant.
-- payload_json is opaque JSON; the schema does not parse it. The
-- (run_id, attempt_number, iteration_number) index supports filtering an
-- iteration's worth of messages, and the (run_id, sequence) index covers
-- audit-stream reads keyed by the shared per-run cursor.
CREATE TABLE IF NOT EXISTS sdk_messages (
  id                INTEGER PRIMARY KEY AUTOINCREMENT,
  run_id            TEXT    NOT NULL,
  attempt_number    INTEGER NOT NULL,
  iteration_number  INTEGER NOT NULL,
  sequence          INTEGER NOT NULL,
  message_type      TEXT    NOT NULL,
  payload_json      TEXT    NOT NULL,
  ts                DATETIME NOT NULL,
  FOREIGN KEY (run_id) REFERENCES lifecycles(run_id),
  UNIQUE (run_id, sequence)
);
CREATE INDEX IF NOT EXISTS idx_sdk_messages_run_sequence
  ON sdk_messages(run_id, sequence);
CREATE INDEX IF NOT EXISTS idx_sdk_messages_run_attempt_iter
  ON sdk_messages(run_id, attempt_number, iteration_number);

-- run_sequence: per-run monotonic counter shared by events and sdk_messages.
-- Stores increment next_seq atomically via
-- `INSERT INTO run_sequence (run_id, next_seq) VALUES (?, 1)
--   ON CONFLICT(run_id) DO UPDATE SET next_seq = next_seq + 1 RETURNING next_seq`
-- so a single ordering exists across both write paths regardless of which
-- table the inserter targets.
CREATE TABLE IF NOT EXISTS run_sequence (
  run_id    TEXT PRIMARY KEY,
  next_seq  INTEGER NOT NULL
);

-- task_claims: multi-worker mutual exclusion. At most one row per task_id;
-- a worker acquires the row (when free, expired, or already its own) before
-- running the task and releases it on completion. lease_expires_at is the
-- liveness signal: a crashed worker's claim is reclaimable once its lease
-- lapses. version drives optimistic concurrency for renew/release so a
-- worker whose claim was stolen (lease expired, another worker took over)
-- learns it lost.
--
-- task_id here is intentionally NOT foreign-keyed to tasks(id) — the one
-- task reference in the schema that stays a bare string. A claim is taken
-- before the task definition is recorded (the orchestrator acquires the lease
-- during selection, then run_task saves the task), and it is deleted on
-- completion: this is transient coordination state, not part of the audit
-- record, so it has no catalog row to anchor to and nothing reads it as
-- history. That is the opposite of lifecycles.task_id, which is durable audit
-- state and therefore does foreign-key tasks(id).
CREATE TABLE IF NOT EXISTS task_claims (
  task_id          TEXT PRIMARY KEY,
  worker_id        TEXT NOT NULL,
  claimed_at       DATETIME NOT NULL,
  lease_expires_at DATETIME NOT NULL,
  version          INTEGER NOT NULL
);

-- control_commands: operator-issued steering commands routed through the
-- store. A producer (the CLI) enqueues a row; the in-process watcher in
-- the running worker periodically claims pending rows for its run and
-- applies them to the live SDK client (interrupt, inject a message,
-- change the model). Each row carries the run scope, the kind
-- discriminator, an opaque JSON payload, and the enqueue timestamp.
-- claimed_at flips from NULL to the claim moment in a single atomic
-- UPDATE so a command applies exactly once even across watcher restarts
-- or concurrent workers (claim-once semantics). Enqueue order is the
-- autoincrement id, which claim_commands uses as the canonical ordering
-- key. Added in schema_version 4 via the forward migration below.
CREATE TABLE IF NOT EXISTS control_commands (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  run_id        TEXT NOT NULL,
  kind          TEXT NOT NULL,
  payload_json  TEXT NOT NULL,
  enqueued_at   DATETIME NOT NULL,
  claimed_at    DATETIME,
  FOREIGN KEY (run_id) REFERENCES lifecycles(run_id)
);
CREATE INDEX IF NOT EXISTS idx_control_commands_pending
  ON control_commands(run_id, id) WHERE claimed_at IS NULL;

-- schema_version pins the on-disk schema. The CHECK clause forces a
-- single sentinel row at id = 1 so ``INSERT OR IGNORE`` is a true upsert
-- against re-bootstrap; stores compare ``version`` on open and refuse
-- mismatched databases with StoreSchemaError. Forward migrations from
-- earlier schema versions are applied by the concrete store's bootstrap
-- (e.g. v3 -> v4 bumps this row after CREATE TABLE IF NOT EXISTS above
-- materializes control_commands on an existing database; v4 -> v5 adds
-- the lifecycles.awaiting_manual_ordinal nullable column; v5 -> v6 drops
-- the unused claude_session_store table — see the store bootstrap).
CREATE TABLE IF NOT EXISTS schema_version (
  id      INTEGER PRIMARY KEY CHECK (id = 1),
  version INTEGER NOT NULL
);
INSERT OR IGNORE INTO schema_version (id, version) VALUES (1, 7);

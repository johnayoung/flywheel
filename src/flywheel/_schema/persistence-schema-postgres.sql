-- Flywheel loop persistence schema — Postgres dialect.
--
-- Mirror of docs/persistence-schema.sql translated to Postgres-native types:
-- JSONB for *_json columns, TIMESTAMPTZ for timestamp columns, BIGSERIAL for
-- autoincrement ids, BOOLEAN for grader_results.passed. Tables span the
-- conceptual model: tasks is the content-addressed catalog of task
-- definitions a run can reference, lifecycles is the row that mutates (with a
-- version column for optimistic concurrency), attempts is the per-execution
-- history, events is the timeline of harness-emitted events, grader_results
-- is the per-grader receipt log produced during validation, sdk_messages
-- is the verbatim agent message stream captured per iteration, and
-- run_sequence is the per-run monotonic counter that orders events and
-- sdk_messages into a single audit stream.
--
-- grader_results is append-only by contract: rows are written once when a
-- grader finishes and must not be updated or deleted. The append-only
-- invariant is enforced at the database layer via BEFORE UPDATE and BEFORE
-- DELETE triggers that raise a clear exception, defending even against
-- callers who bypass the store API. Treat the table as an audit log —
-- corrections go in new rows or compensating events. sdk_messages and events
-- share a per-run monotonic sequence assigned at insert time via
-- run_sequence; readers merge the two on that column for the canonical audit
-- ordering.
--
-- Store contents are sensitive-by-default; payloads are persisted verbatim
-- and unredacted. Operators must treat the database as confidential.
--
-- schema_version pins the on-disk schema version against the constant
-- CURRENT_SCHEMA_VERSION declared in flywheel.store_protocols. Stores read
-- the row on open and refuse pre-feature databases with a clear
-- "store must be re-created" error; there is no in-place migration.
--
-- Bootstrap is idempotent: every CREATE statement uses IF NOT EXISTS, and
-- the trigger creations are wrapped in DO $$ guards that no-op when the
-- triggers already exist. Foreign keys are enforced by Postgres by default
-- (no equivalent of SQLite's PRAGMA foreign_keys = ON is required).
--
-- Schema-name handling is the implementer's call at bootstrap time: this
-- file uses unqualified names and is intended to run under whatever
-- search_path the caller configures (for example by issuing
-- `SET search_path TO <schema>, public` before executing this script, or
-- by substituting a `${schema}.` qualifier prefix in front of every
-- object name at bootstrap).

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
  first_seen_at TIMESTAMPTZ NOT NULL,
  updated_at    TIMESTAMPTZ NOT NULL
);

-- task_versions: immutable, content-addressed task definitions. Keyed by
-- (task_id, content_hash) where content_hash is flywheel.loaders.task_digest
-- over the *executed* definition — goal, graders, and context only. tags and
-- prerequisites are deliberately excluded from the hash: they are mutable
-- orchestration metadata (see task_tags / task_prerequisites), so retagging
-- or rewiring the DAG never forks the definition a run pinned. Storage is
-- immutable and deduped; a run pins the exact version it ran via
-- lifecycles.task_content_hash.
CREATE TABLE IF NOT EXISTS task_versions (
  task_id       TEXT NOT NULL,
  content_hash  TEXT NOT NULL,
  goal          TEXT NOT NULL,
  graders_json  JSONB NOT NULL,
  context_json  JSONB NOT NULL,
  created_at    TIMESTAMPTZ NOT NULL,
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
-- and blocked_requires_json hold structured state like the rest of the
-- schema. task_id foreign-keys tasks(id): every run references a real,
-- catalogued task identity (the seed auto-registers it). task_content_hash
-- pins the exact task_versions row the run executed and is the recovery key
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
  timestamps_json         JSONB NOT NULL,
  updated_at              TIMESTAMPTZ NOT NULL,
  blocked_requires_json   TEXT,
  task_content_hash       TEXT,
  -- ordinal (index in task.graders) of the manual gate the lifecycle is
  -- currently parked on. NULL in every state except AWAITING_APPROVAL; the
  -- column is cleared on every -> READY/-> DONE/-> FAILED_VALIDATION edge
  -- in Lifecycle.transition_to (the same back-compat path as
  -- blocked_requires_json). Added in schema_version 5 via the forward
  -- migration the store applies on bootstrap.
  awaiting_manual_ordinal INTEGER,
  FOREIGN KEY (task_id) REFERENCES tasks(id)
);

-- agent_context_json captures the agent identity for this attempt so the run
-- is interpretable across model, SDK, and prompt changes. Suggested keys:
-- model_id, model_version, agent_sdk_version, prompt_template_hash.
-- Per-grader failures are not denormalized here — query grader_results with
-- (run_id, attempt_number) and passed = false instead.
CREATE TABLE IF NOT EXISTS attempts (
  run_id              TEXT NOT NULL,
  number              INTEGER NOT NULL,
  attempt_run_id      TEXT,
  started_at          TIMESTAMPTZ NOT NULL,
  ended_at            TIMESTAMPTZ,
  outcome             TEXT,
  agent_output        TEXT,
  error               TEXT,
  agent_context_json  JSONB,
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
  id              BIGSERIAL PRIMARY KEY,
  run_id          TEXT NOT NULL,
  attempt_number  INTEGER,
  ts              TIMESTAMPTZ NOT NULL,
  kind            TEXT NOT NULL,
  payload_json    JSONB NOT NULL,
  sequence        BIGINT NOT NULL,
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
  id                BIGSERIAL PRIMARY KEY,
  run_id            TEXT NOT NULL,
  attempt_number    INTEGER NOT NULL,
  ordinal           INTEGER NOT NULL,
  grader_type       TEXT NOT NULL,
  grader_name       TEXT,
  grader_spec_json  JSONB NOT NULL,
  passed            BOOLEAN NOT NULL,
  duration_ms       INTEGER NOT NULL,
  payload_json      JSONB NOT NULL,
  ts                TIMESTAMPTZ NOT NULL,
  FOREIGN KEY (run_id, attempt_number) REFERENCES attempts(run_id, number)
);

-- events read paths. idx_events_run serves telemetry reads ordered by
-- wall-clock; idx_events_run_sequence serves cursor-paginated audit-stream
-- reads over the shared per-run sequence. idx_events_domain is a partial
-- index over the domain minority so list_domain_events (the event-sourced
-- state fold run on every lifecycle load) scans only the rows it folds
-- instead of walking interleaved telemetry. The partial index is additive:
-- CREATE INDEX IF NOT EXISTS materializes it on reopen of an existing
-- schema, so it needs no schema_version bump (same path as task_claims).
-- grader_results reads are always scoped to one attempt's graders in ordinal
-- order.
CREATE INDEX IF NOT EXISTS idx_events_run ON events(run_id, ts);
CREATE INDEX IF NOT EXISTS idx_events_run_sequence
  ON events(run_id, sequence);
CREATE INDEX IF NOT EXISTS idx_events_domain
  ON events(run_id, sequence) WHERE category = 'domain';
CREATE INDEX IF NOT EXISTS idx_grader_results_run_attempt
  ON grader_results(run_id, attempt_number, ordinal);

-- sdk_messages: verbatim agent SDK message stream per iteration. Keyed by
-- an autoincrement id; the per-run audit ordering is carried by the
-- (run_id, sequence) tuple that's also the table's uniqueness invariant.
-- payload_json is opaque JSONB; the schema does not parse it. The
-- (run_id, attempt_number, iteration_number) index supports filtering an
-- iteration's worth of messages, and the (run_id, sequence) index covers
-- audit-stream reads keyed by the shared per-run cursor.
CREATE TABLE IF NOT EXISTS sdk_messages (
  id                BIGSERIAL PRIMARY KEY,
  run_id            TEXT        NOT NULL,
  attempt_number    INTEGER     NOT NULL,
  iteration_number  INTEGER     NOT NULL,
  sequence          BIGINT      NOT NULL,
  message_type      TEXT        NOT NULL,
  payload_json      JSONB       NOT NULL,
  ts                TIMESTAMPTZ NOT NULL,
  FOREIGN KEY (run_id) REFERENCES lifecycles(run_id),
  UNIQUE (run_id, sequence)
);
CREATE INDEX IF NOT EXISTS idx_sdk_messages_run_sequence
  ON sdk_messages(run_id, sequence);
CREATE INDEX IF NOT EXISTS idx_sdk_messages_run_attempt_iter
  ON sdk_messages(run_id, attempt_number, iteration_number);

-- run_sequence: per-run monotonic counter shared by events and sdk_messages.
-- Stores increment next_seq atomically via
-- `INSERT INTO run_sequence (run_id, next_seq) VALUES (%s, 1)
--   ON CONFLICT (run_id) DO UPDATE
--   SET next_seq = run_sequence.next_seq + 1 RETURNING next_seq`
-- so a single ordering exists across both write paths regardless of which
-- table the inserter targets.
CREATE TABLE IF NOT EXISTS run_sequence (
  run_id    TEXT   PRIMARY KEY,
  next_seq  BIGINT NOT NULL
);

-- task_claims: multi-worker mutual exclusion. At most one row per task_id;
-- a worker acquires the row (when free, expired, or already its own) before
-- running the task and releases it on completion. lease_expires_at is the
-- liveness signal: a crashed worker's claim is reclaimable once its lease
-- lapses. version drives optimistic concurrency for renew/release so a
-- worker whose claim was stolen learns it lost.
--
-- task_id here is intentionally NOT foreign-keyed to tasks(id) — the one
-- task reference in the schema that stays a bare string. A claim is taken
-- before the task definition is recorded (the orchestrator acquires the lease
-- during selection, then run_task saves the task) and deleted on completion:
-- transient coordination state, not audit history, so it has no catalog row
-- to anchor to. That is the opposite of lifecycles.task_id, which is durable
-- audit state and does foreign-key tasks(id).
CREATE TABLE IF NOT EXISTS task_claims (
  task_id          TEXT PRIMARY KEY,
  worker_id        TEXT NOT NULL,
  claimed_at       TIMESTAMPTZ NOT NULL,
  lease_expires_at TIMESTAMPTZ NOT NULL,
  version          INTEGER NOT NULL
);

-- control_commands: operator-issued steering commands routed through the
-- store. A producer (the CLI) enqueues a row; the in-process watcher in
-- the running worker periodically claims pending rows for its run and
-- applies them to the live SDK client. claimed_at flips from NULL to the
-- claim moment in a single atomic UPDATE (with FOR UPDATE SKIP LOCKED on
-- the inner SELECT) so a command applies exactly once across watcher
-- restarts and concurrent workers. Enqueue order is the autoincrement id;
-- claim_commands returns rows in ascending id order. Added in
-- schema_version 4 via the forward migration the store applies on
-- bootstrap.
CREATE TABLE IF NOT EXISTS control_commands (
  id            BIGSERIAL PRIMARY KEY,
  run_id        TEXT        NOT NULL,
  kind          TEXT        NOT NULL,
  payload_json  JSONB       NOT NULL,
  enqueued_at   TIMESTAMPTZ NOT NULL,
  claimed_at    TIMESTAMPTZ,
  FOREIGN KEY (run_id) REFERENCES lifecycles(run_id)
);
CREATE INDEX IF NOT EXISTS idx_control_commands_pending
  ON control_commands(run_id, id) WHERE claimed_at IS NULL;

-- schema_version pins the on-disk schema. The CHECK clause forces a
-- single sentinel row at id = 1 so ``ON CONFLICT DO NOTHING`` is a true
-- upsert against re-bootstrap; stores compare ``version`` on open and
-- refuse mismatched databases with StoreSchemaError. Forward migrations
-- from earlier schema versions are applied by the concrete store's
-- bootstrap (e.g. v3 -> v4 bumps this row after CREATE TABLE IF NOT
-- EXISTS above materializes control_commands on an existing database;
-- v4 -> v5 adds the lifecycles.awaiting_manual_ordinal nullable column;
-- v5 -> v6 drops the unused claude_session_store table — see bootstrap).
CREATE TABLE IF NOT EXISTS schema_version (
  id      INTEGER PRIMARY KEY CHECK (id = 1),
  version INTEGER NOT NULL
);
INSERT INTO schema_version (id, version) VALUES (1, 7)
  ON CONFLICT (id) DO NOTHING;

-- Append-only enforcement for grader_results. The trigger function raises a
-- clear exception with SQLSTATE check_violation that callers (and the store
-- layer) can map to a meaningful error. CREATE OR REPLACE keeps the function
-- definition idempotent; the trigger creations are wrapped in a DO $$ guard
-- that skips when the trigger already exists, so this script can be re-run
-- against an already-bootstrapped database without raising.
CREATE OR REPLACE FUNCTION grader_results_reject_mutation()
RETURNS TRIGGER AS $$
BEGIN
  RAISE EXCEPTION 'grader_results is append-only; % is not permitted', TG_OP
    USING ERRCODE = 'check_violation';
END;
$$ LANGUAGE plpgsql;

DO $$
BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM pg_trigger
    WHERE tgname = 'grader_results_no_update'
      AND tgrelid = 'grader_results'::regclass
  ) THEN
    CREATE TRIGGER grader_results_no_update
      BEFORE UPDATE ON grader_results
      FOR EACH ROW EXECUTE FUNCTION grader_results_reject_mutation();
  END IF;

  IF NOT EXISTS (
    SELECT 1 FROM pg_trigger
    WHERE tgname = 'grader_results_no_delete'
      AND tgrelid = 'grader_results'::regclass
  ) THEN
    CREATE TRIGGER grader_results_no_delete
      BEFORE DELETE ON grader_results
      FOR EACH ROW EXECUTE FUNCTION grader_results_reject_mutation();
  END IF;
END
$$;

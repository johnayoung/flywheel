-- Flywheel loop persistence schema — Postgres dialect.
--
-- Mirror of persistence-schema.sql translated to Postgres-native types:
-- JSONB for *_json columns, TIMESTAMPTZ for timestamp columns, BIGSERIAL for
-- autoincrement ids, BOOLEAN for grader_results.passed. Tables span the
-- conceptual model: tasks is the content-addressed catalog of task
-- definitions a run can reference, lifecycles is the row that mutates (with a
-- version column for optimistic concurrency), attempts is the per-execution
-- history, events is the domain-event ledger the lifecycle state is folded
-- from, and grader_results is the per-grader receipt log produced during
-- validation.
--
-- events holds domain rows only (since schema_version 11). Telemetry —
-- harness telemetry events and the verbatim SDK message stream — is not
-- stored here: it flows to a per-run JSONL file under .flywheel/logs via
-- the TelemetrySink seam. The former category column, the SDK-message
-- table, and the shared per-run counter table that interleaved telemetry
-- with domain rows into one merged audit stream were all removed; the
-- table name was kept rather than renamed to domain_events. sequence is
-- allocated per run as MAX(sequence) + 1 inside the append transaction,
-- so domain-event ordering stays strictly monotonic per run without a
-- cross-table counter.
--
-- grader_results is append-only by contract: rows are written once when a
-- grader finishes and must not be updated or deleted. The append-only
-- invariant is enforced at the database layer via BEFORE UPDATE and BEFORE
-- DELETE triggers that raise a clear exception, defending even against
-- callers who bypass the store API. Treat the table as an audit log —
-- corrections go in new rows or compensating events.
--
-- Store contents are sensitive-by-default; payloads are persisted verbatim
-- and unredacted. The same contract extends to the per-run telemetry files
-- under .flywheel/logs. Operators must treat the database and the logs
-- directory as confidential.
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

-- Task definitions are content-addressed across two tables so every task
-- reference in the store is a checkable foreign key rather than a free-
-- floating string:
--
--   tasks          -- the logical task (stable id). The FK anchor.
--   task_versions  -- immutable, content-addressed definitions.
--
-- Flywheel owns the lifecycle of a *single* task, so the catalog records only
-- what defines and verifies that one task: goal, graders, tags, context. The
-- dependency DAG between tasks (prerequisites) is not a flywheel-core concept
-- — it belongs to the orchestration layer built on top of flywheel, which
-- keeps its own scheduling state — so it is deliberately absent here.

-- tasks: the logical task identity. One row per id, created the first time a
-- task is saved or referenced by a run. Carries no definition — it exists so
-- that task_versions and lifecycles can foreign-key a real, catalogued
-- identity rather than a bare string.
CREATE TABLE IF NOT EXISTS tasks (
  id            TEXT PRIMARY KEY,
  first_seen_at TIMESTAMPTZ NOT NULL,
  updated_at    TIMESTAMPTZ NOT NULL
);

-- task_versions: immutable, content-addressed task definitions. Keyed by
-- (task_id, content_hash) where content_hash is flywheel.loaders.task_digest
-- over the definition — goal, graders, tags, context. Storage is immutable
-- and deduped: re-saving an unchanged definition is a no-op, editing any
-- hashed field adds a new version row. A run pins the exact version it ran via
-- lifecycles.task_content_hash, so historical truth survives later edits.
CREATE TABLE IF NOT EXISTS task_versions (
  task_id       TEXT NOT NULL,
  content_hash  TEXT NOT NULL,
  goal          TEXT NOT NULL,
  graders_json  JSONB NOT NULL,
  tags_json     JSONB NOT NULL,
  context_json  JSONB NOT NULL,
  created_at    TIMESTAMPTZ NOT NULL,
  PRIMARY KEY (task_id, content_hash),
  FOREIGN KEY (task_id) REFERENCES tasks(id)
);
CREATE INDEX IF NOT EXISTS idx_task_versions_id_created
  ON task_versions(task_id, created_at);

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

-- agent_context_json captures the agent identity and world state for this
-- attempt so the run is interpretable across model, SDK, and prompt
-- changes. Suggested keys: model_id, model_version, agent_sdk_version,
-- prompt_template_hash. This is also the canonical home of the world-state
-- pin (spec 00025 FR-11): model_id is the effective model resolved from
-- --model / policy / default before the SDK is invoked, and
-- base_commit_sha is the 40-char commit the workspace was created from
-- (omitted when the run has no git worktree). The mapping rides the
-- AttemptStarted payload, so both values survive domain-event replay.
-- Per-grader failures are not denormalized here — query grader_results with
-- (run_id, attempt_number) and passed = false instead.
--
-- The aggregate columns (input_tokens .. last_activity_at) are rolled-up
-- counters the harness writes at iteration boundaries through the
-- versioned save_attempt path (schema_version 10). Token columns mirror
-- the SDK usage breakdown keys and accumulate per-iteration deltas;
-- turns / total_cost_usd accumulate the SDK's session-cumulative
-- readings (summed at the boundary, same overcount policy as the
-- telemetry stream); iterations_completed counts completed iterations;
-- last_activity_at is the timestamp of the most recent rollup (NULL
-- before the first completed iteration). The dashboard snapshot reads
-- these relational columns instead of scanning telemetry events.
CREATE TABLE IF NOT EXISTS attempts (
  run_id                       TEXT NOT NULL,
  number                       INTEGER NOT NULL,
  attempt_run_id               TEXT,
  started_at                   TIMESTAMPTZ NOT NULL,
  ended_at                     TIMESTAMPTZ,
  outcome                      TEXT,
  agent_output                 TEXT,
  error                        TEXT,
  agent_context_json           JSONB,
  input_tokens                 BIGINT NOT NULL DEFAULT 0,
  output_tokens                BIGINT NOT NULL DEFAULT 0,
  cache_creation_input_tokens  BIGINT NOT NULL DEFAULT 0,
  cache_read_input_tokens      BIGINT NOT NULL DEFAULT 0,
  iterations_completed         INTEGER NOT NULL DEFAULT 0,
  turns                        INTEGER NOT NULL DEFAULT 0,
  total_cost_usd               DOUBLE PRECISION NOT NULL DEFAULT 0,
  last_activity_at             TIMESTAMPTZ,
  PRIMARY KEY (run_id, number),
  FOREIGN KEY (run_id) REFERENCES lifecycles(run_id)
);

-- events: the domain-event ledger. Every row is a state-bearing,
-- event-sourced member of the lifecycle log; kind holds the
-- DomainEventKind value. sequence is per-run and strictly monotonic
-- (MAX + 1 allocated inside the append transaction); list_domain_events
-- replays rows in ascending sequence order. The UNIQUE (run_id, sequence)
-- constraint both enforces the ordering invariant and provides the index
-- that read path uses.
CREATE TABLE IF NOT EXISTS events (
  id              BIGSERIAL PRIMARY KEY,
  run_id          TEXT NOT NULL,
  attempt_number  INTEGER,
  ts              TIMESTAMPTZ NOT NULL,
  kind            TEXT NOT NULL,
  payload_json    JSONB NOT NULL,
  sequence        BIGINT NOT NULL,
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

-- events reads are served by the index the UNIQUE (run_id, sequence)
-- constraint materializes; no separate events index is needed.
-- grader_results reads are always scoped to one attempt's graders in ordinal
-- order.
CREATE INDEX IF NOT EXISTS idx_grader_results_run_attempt
  ON grader_results(run_id, attempt_number, ordinal);

-- Multi-worker mutual exclusion (the task_claims lease) is NOT a flywheel-core
-- concern — a single task's lifecycle has no notion of competing workers. It
-- lives in the orchestration layer's own store (flywheel_orchestrator), which
-- can share this database but owns its own tables.

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
INSERT INTO schema_version (id, version) VALUES (1, 11)
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

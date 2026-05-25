-- Flywheel loop persistence schema — Postgres dialect.
--
-- Mirror of docs/persistence-schema.sql translated to Postgres-native types:
-- JSONB for *_json columns, TIMESTAMPTZ for timestamp columns, BIGSERIAL for
-- autoincrement ids, BOOLEAN for grader_results.passed. Five tables span the
-- conceptual model: lifecycles is the row that mutates (with a version column
-- for optimistic concurrency), attempts is the per-execution history, events
-- is the timeline of harness-emitted events, grader_results is the per-grader
-- receipt log produced during validation, and claude_session_store persists
-- Claude Code agent transcripts.
--
-- grader_results is append-only by contract: rows are written once when a
-- grader finishes and must not be updated or deleted. The append-only
-- invariant is enforced at the database layer via BEFORE UPDATE and BEFORE
-- DELETE triggers that raise a clear exception, defending even against
-- callers who bypass the store API. Treat the table as an audit log —
-- corrections go in new rows or compensating events.
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

CREATE TABLE IF NOT EXISTS lifecycles (
  run_id               TEXT PRIMARY KEY,
  task_id              TEXT NOT NULL,
  status               TEXT NOT NULL,
  version              INTEGER NOT NULL,
  retries              INTEGER NOT NULL,
  error                TEXT,
  agent_output         TEXT,
  implementation_notes TEXT,
  session_id           TEXT,
  artifacts_dir        TEXT,
  worker_id            TEXT,
  timestamps_json      JSONB NOT NULL,
  updated_at           TIMESTAMPTZ NOT NULL
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

CREATE TABLE IF NOT EXISTS events (
  id              BIGSERIAL PRIMARY KEY,
  run_id          TEXT NOT NULL,
  attempt_number  INTEGER,
  ts              TIMESTAMPTZ NOT NULL,
  kind            TEXT NOT NULL,
  payload_json    JSONB NOT NULL,
  FOREIGN KEY (run_id) REFERENCES lifecycles(run_id)
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

CREATE INDEX IF NOT EXISTS idx_events_run ON events(run_id, ts);
CREATE INDEX IF NOT EXISTS idx_grader_results_run_attempt
  ON grader_results(run_id, attempt_number, ordinal);

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

-- claude_session_store: Claude Code agent transcript persistence.
-- One row per transcript entry; seq orders entries within a
-- (project_key, session_id, subpath) tuple. The empty string is the subpath
-- sentinel for the main transcript (subagent rows use a non-empty subpath).
CREATE TABLE IF NOT EXISTS claude_session_store (
  seq         BIGSERIAL PRIMARY KEY,
  project_key TEXT   NOT NULL,
  session_id  TEXT   NOT NULL,
  subpath     TEXT   NOT NULL DEFAULT '',
  entry       TEXT   NOT NULL,
  mtime       BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS claude_session_store_lookup_idx
  ON claude_session_store (project_key, session_id, subpath, seq);
CREATE INDEX IF NOT EXISTS claude_session_store_list_idx
  ON claude_session_store (project_key, session_id) WHERE subpath = '';

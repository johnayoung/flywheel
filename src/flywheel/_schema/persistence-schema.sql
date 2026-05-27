-- Flywheel loop persistence schema.
--
-- Tables mirror the conceptual model: lifecycles is the row that mutates
-- (with a Version column for optimistic concurrency), attempts is the
-- per-execution history, events is the timeline of harness-emitted events,
-- grader_results is the per-grader receipt log produced during validation,
-- sdk_messages is the verbatim agent message stream captured per iteration,
-- and run_sequence is the per-run monotonic counter that orders events and
-- sdk_messages into a single audit stream.
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

CREATE TABLE IF NOT EXISTS lifecycles (
  run_id               TEXT PRIMARY KEY,
  task_id              TEXT NOT NULL,
  status               TEXT NOT NULL,
  version              INTEGER NOT NULL,
  retries              INTEGER NOT NULL,
  error                TEXT,
  agent_output         TEXT,
  session_id           TEXT,
  artifacts_dir        TEXT,
  worker_id            TEXT,
  timestamps_json      TEXT NOT NULL,
  updated_at           DATETIME NOT NULL
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

CREATE TABLE IF NOT EXISTS events (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  run_id          TEXT NOT NULL,
  attempt_number  INTEGER,
  ts              DATETIME NOT NULL,
  kind            TEXT NOT NULL,
  payload_json    TEXT NOT NULL,
  sequence        INTEGER NOT NULL,
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

CREATE INDEX IF NOT EXISTS idx_events_run ON events(run_id, ts);
CREATE INDEX IF NOT EXISTS idx_events_run_sequence ON events(run_id, sequence);
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

-- schema_version pins the on-disk schema. The CHECK clause forces a
-- single sentinel row at id = 1 so ``INSERT OR IGNORE`` is a true upsert
-- against re-bootstrap; stores compare ``version`` on open and refuse
-- mismatched databases with StoreSchemaError.
CREATE TABLE IF NOT EXISTS schema_version (
  id      INTEGER PRIMARY KEY CHECK (id = 1),
  version INTEGER NOT NULL
);
INSERT OR IGNORE INTO schema_version (id, version) VALUES (1, 2);

-- claude_session_store: Claude Code agent transcript persistence.
-- One row per transcript entry; seq orders entries within a
-- (project_key, session_id, subpath) tuple. The empty string is the subpath
-- sentinel for the main transcript (subagent rows use a non-empty subpath).
CREATE TABLE IF NOT EXISTS claude_session_store (
  seq         INTEGER PRIMARY KEY AUTOINCREMENT,
  project_key TEXT    NOT NULL,
  session_id  TEXT    NOT NULL,
  subpath     TEXT    NOT NULL DEFAULT '',
  entry       TEXT    NOT NULL,
  mtime       INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS claude_session_store_lookup_idx
  ON claude_session_store (project_key, session_id, subpath, seq);
CREATE INDEX IF NOT EXISTS claude_session_store_list_idx
  ON claude_session_store (project_key, session_id) WHERE subpath = '';

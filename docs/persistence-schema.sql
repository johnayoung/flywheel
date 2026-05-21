-- Flywheel loop persistence schema.
--
-- Four tables mirror the conceptual model: lifecycles is the row that mutates
-- (with a Version column for optimistic concurrency), attempts is the
-- per-execution history, events is the timeline of harness-emitted events,
-- and grader_results is the per-grader receipt log produced during validation.
--
-- grader_results is append-only by contract: rows are written once when a
-- grader finishes and must not be updated or deleted. Treat the table as an
-- audit log — corrections go in new rows or compensating events.
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
  implementation_notes TEXT,
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
CREATE INDEX IF NOT EXISTS idx_grader_results_run_attempt ON grader_results(run_id, attempt_number, ordinal);

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

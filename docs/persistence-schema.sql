-- Flywheel loop persistence schema.
--
-- Four tables mirror the conceptual model: lifecycles is the row that mutates
-- (with a Version column for optimistic concurrency), attempts is the
-- per-execution history, events is the timeline of harness-emitted events,
-- and verifications is the per-command receipt log produced by Tier-1.
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
  current_step         INTEGER NOT NULL,
  error                TEXT,
  agent_output         TEXT,
  implementation_notes TEXT,
  session_id           TEXT,
  artifacts_dir        TEXT,
  worker_id            TEXT,
  timestamps_json      TEXT NOT NULL,
  steps_completed_json TEXT,
  updated_at           DATETIME NOT NULL
);

CREATE TABLE IF NOT EXISTS attempts (
  run_id              TEXT NOT NULL,
  number              INTEGER NOT NULL,
  attempt_run_id      TEXT,
  started_at          DATETIME NOT NULL,
  ended_at            DATETIME,
  outcome             TEXT,
  agent_output        TEXT,
  error               TEXT,
  validation_failures TEXT,
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

CREATE TABLE IF NOT EXISTS verifications (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  run_id          TEXT NOT NULL,
  attempt_number  INTEGER NOT NULL,
  ordinal         INTEGER NOT NULL,
  command         TEXT NOT NULL,
  exit_code       INTEGER NOT NULL,
  duration_ms     INTEGER NOT NULL,
  stdout_tail     TEXT,
  stderr_tail     TEXT,
  ts              DATETIME NOT NULL,
  FOREIGN KEY (run_id, attempt_number) REFERENCES attempts(run_id, number)
);

CREATE INDEX IF NOT EXISTS idx_events_run ON events(run_id, ts);
CREATE INDEX IF NOT EXISTS idx_verifications_run_attempt ON verifications(run_id, attempt_number, ordinal);

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

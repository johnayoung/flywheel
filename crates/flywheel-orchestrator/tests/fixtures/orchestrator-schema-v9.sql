-- FROZEN FIXTURE: the orchestrator_schema_version 9 layout (before project_id
-- partitioning), kept verbatim so the in-place upgrade tests migrate a real
-- legacy store. Never edit; the live DDL is SCHEMA_SQL in
-- crates/flywheel-orchestrator/src/claims.rs (and claims_postgres.rs).

PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS task_claims (
  task_id            TEXT PRIMARY KEY,
  worker_id          TEXT NOT NULL,
  claimed_at         DATETIME NOT NULL,
  lease_expires_at   DATETIME NOT NULL,
  version            INTEGER NOT NULL,
  conflict_keys_json TEXT NOT NULL DEFAULT '[]'
);

CREATE TABLE IF NOT EXISTS orchestrator_schema_version (
  id      INTEGER PRIMARY KEY CHECK (id = 1),
  version INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS work_items (
  task_id                    TEXT PRIMARY KEY,
  source_kind                TEXT,
  source_ref                 TEXT,
  source_url                 TEXT,
  source_version             TEXT,
  task_content_hash          TEXT,
  priority                   INTEGER NOT NULL DEFAULT 0,
  required_capabilities_json TEXT NOT NULL DEFAULT '[]',
  conflict_keys_json         TEXT NOT NULL DEFAULT '[]',
  first_seen_at              TEXT NOT NULL,
  last_seen_at               TEXT NOT NULL,
  disappeared_at             TEXT,
  metadata_json              TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS work_item_dependencies (
  task_id              TEXT NOT NULL,
  prerequisite_task_id TEXT NOT NULL,
  created_at           TEXT NOT NULL,
  PRIMARY KEY (task_id, prerequisite_task_id)
);

CREATE INDEX IF NOT EXISTS idx_work_item_dependencies_prerequisite
  ON work_item_dependencies (prerequisite_task_id);

CREATE TABLE IF NOT EXISTS source_syncs (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  source_kind    TEXT NOT NULL,
  source_name    TEXT NOT NULL,
  started_at     TEXT NOT NULL,
  finished_at    TEXT,
  status         TEXT NOT NULL,
  observed_count INTEGER NOT NULL DEFAULT 0,
  error          TEXT,
  metadata_json  TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS orchestrator_events (
  id               INTEGER PRIMARY KEY AUTOINCREMENT,
  task_id          TEXT NOT NULL,
  worker_id        TEXT NOT NULL,
  event_type       TEXT NOT NULL,
  version          INTEGER NOT NULL,
  lease_expires_at TEXT NOT NULL,
  occurred_at      TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_orchestrator_events_task
  ON orchestrator_events (task_id, id);

CREATE TABLE IF NOT EXISTS orchestrator_stop_events (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  kind        TEXT NOT NULL,
  subject     TEXT NOT NULL,
  detail      TEXT NOT NULL,
  occurred_at TEXT NOT NULL,
  run_id      TEXT NOT NULL DEFAULT '',
  attribution TEXT NOT NULL DEFAULT ''
);

CREATE INDEX IF NOT EXISTS idx_orchestrator_stop_events_subject
  ON orchestrator_stop_events (subject, id);

CREATE TABLE IF NOT EXISTS graph_snapshots (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  captured_at   TEXT NOT NULL,
  item_count    INTEGER NOT NULL,
  last_event_id INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS graph_snapshot_items (
  snapshot_id                 INTEGER NOT NULL,
  task_id                     TEXT NOT NULL,
  source_kind                 TEXT,
  source_ref                  TEXT,
  source_url                  TEXT,
  source_version              TEXT,
  priority                    INTEGER NOT NULL DEFAULT 0,
  required_capabilities_json  TEXT NOT NULL DEFAULT '[]',
  conflict_keys_json          TEXT NOT NULL DEFAULT '[]',
  state                       TEXT NOT NULL,
  ready                       INTEGER NOT NULL,
  claim_holder                TEXT,
  resolved_prerequisites_json TEXT NOT NULL DEFAULT '[]',
  PRIMARY KEY (snapshot_id, task_id)
);

CREATE INDEX IF NOT EXISTS idx_graph_snapshot_items_snapshot
  ON graph_snapshot_items (snapshot_id);

CREATE TABLE IF NOT EXISTS orchestrator_session_pauses (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  run_id      TEXT NOT NULL DEFAULT '',
  reset_at    TEXT NOT NULL,
  pause_until TEXT NOT NULL,
  recorded_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_orchestrator_session_pauses_pause_until
  ON orchestrator_session_pauses (pause_until);

CREATE INDEX IF NOT EXISTS idx_orchestrator_stop_events_kind
  ON orchestrator_stop_events (kind, id);

INSERT OR IGNORE INTO orchestrator_schema_version (id, version) VALUES (1, 9);

-- FROZEN FIXTURE: the SQLite orchestrator_schema_version 10 layout (project_id
-- partitioned, before session-pause accounts and clear records), with seeded
-- rows in every orchestrator table including an active session pause (run
-- 'run-limit', pause_until 2026-05-28T12:00:30Z) and an expired one. Kept
-- verbatim so the in-place upgrade tests migrate a real v10 store. Never
-- edit; the live DDL is SCHEMA_SQL in crates/flywheel-orchestrator/src/claims.rs.
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS task_claims (
  project_id         TEXT NOT NULL,
  task_id            TEXT NOT NULL,
  worker_id          TEXT NOT NULL,
  claimed_at         DATETIME NOT NULL,
  lease_expires_at   DATETIME NOT NULL,
  version            INTEGER NOT NULL,
  conflict_keys_json TEXT NOT NULL DEFAULT '[]',
  PRIMARY KEY (project_id, task_id)
);

CREATE TABLE IF NOT EXISTS orchestrator_schema_version (
  version INTEGER NOT NULL,
  id      INTEGER PRIMARY KEY CHECK (id = 1)
);

CREATE TABLE IF NOT EXISTS work_items (
  project_id                 TEXT NOT NULL,
  task_id                    TEXT NOT NULL,
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
  metadata_json              TEXT NOT NULL DEFAULT '{}',
  PRIMARY KEY (project_id, task_id)
);

CREATE TABLE IF NOT EXISTS work_item_dependencies (
  project_id           TEXT NOT NULL,
  task_id              TEXT NOT NULL,
  prerequisite_task_id TEXT NOT NULL,
  created_at           TEXT NOT NULL,
  PRIMARY KEY (project_id, task_id, prerequisite_task_id)
);

CREATE INDEX IF NOT EXISTS idx_work_item_dependencies_prerequisite
  ON work_item_dependencies (project_id, prerequisite_task_id);

CREATE TABLE IF NOT EXISTS source_syncs (
  project_id     TEXT NOT NULL,
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

CREATE INDEX IF NOT EXISTS idx_source_syncs_project
  ON source_syncs (project_id, id);

CREATE TABLE IF NOT EXISTS orchestrator_events (
  project_id       TEXT NOT NULL,
  id               INTEGER PRIMARY KEY AUTOINCREMENT,
  task_id          TEXT NOT NULL,
  worker_id        TEXT NOT NULL,
  event_type       TEXT NOT NULL,
  version          INTEGER NOT NULL,
  lease_expires_at TEXT NOT NULL,
  occurred_at      TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_orchestrator_events_task
  ON orchestrator_events (project_id, task_id, id);

CREATE INDEX IF NOT EXISTS idx_orchestrator_events_project
  ON orchestrator_events (project_id, id);

CREATE TABLE IF NOT EXISTS orchestrator_stop_events (
  project_id  TEXT NOT NULL,
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  kind        TEXT NOT NULL,
  subject     TEXT NOT NULL,
  detail      TEXT NOT NULL,
  occurred_at TEXT NOT NULL,
  run_id      TEXT NOT NULL DEFAULT '',
  attribution TEXT NOT NULL DEFAULT ''
);

CREATE INDEX IF NOT EXISTS idx_orchestrator_stop_events_subject
  ON orchestrator_stop_events (project_id, subject, id);

CREATE INDEX IF NOT EXISTS idx_orchestrator_stop_events_kind
  ON orchestrator_stop_events (project_id, kind, id);

CREATE TABLE IF NOT EXISTS graph_snapshots (
  project_id    TEXT NOT NULL,
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  captured_at   TEXT NOT NULL,
  item_count    INTEGER NOT NULL,
  last_event_id INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_graph_snapshots_project
  ON graph_snapshots (project_id, id);

CREATE TABLE IF NOT EXISTS graph_snapshot_items (
  project_id                  TEXT NOT NULL,
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
  PRIMARY KEY (project_id, snapshot_id, task_id)
);

CREATE TABLE IF NOT EXISTS orchestrator_session_pauses (
  project_id  TEXT NOT NULL,
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  run_id      TEXT NOT NULL DEFAULT '',
  reset_at    TEXT NOT NULL,
  pause_until TEXT NOT NULL,
  recorded_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_orchestrator_session_pauses_pause_until
  ON orchestrator_session_pauses (project_id, pause_until);

INSERT INTO task_claims (project_id, task_id, worker_id, claimed_at, lease_expires_at, version,
  conflict_keys_json) VALUES ('proj', 't1', 'w1', '2026-05-28T12:00:00+00:00',
  '2026-05-28T12:00:30+00:00', 2, '["k"]');
INSERT INTO work_items (project_id, task_id, source_kind, source_ref, priority, first_seen_at,
  last_seen_at) VALUES ('proj', 't1', 'directory', 'tasks/t1.json', 3,
  '2026-05-28T12:00:00+00:00', '2026-05-28T12:00:00+00:00');
INSERT INTO work_item_dependencies (project_id, task_id, prerequisite_task_id, created_at)
  VALUES ('proj', 't1', 't0', '2026-05-28T12:00:00+00:00');
INSERT INTO source_syncs (project_id, source_kind, source_name, started_at, finished_at, status,
  observed_count) VALUES ('proj', 'directory', 'tasks', '2026-05-28T12:00:00+00:00',
  '2026-05-28T12:00:01+00:00', 'ok', 1);
INSERT INTO orchestrator_events (project_id, task_id, worker_id, event_type, version,
  lease_expires_at, occurred_at) VALUES ('proj', 't1', 'w1', 'acquired', 1,
  '2026-05-28T12:00:30+00:00', '2026-05-28T12:00:00+00:00');
INSERT INTO orchestrator_stop_events (project_id, kind, subject, detail, occurred_at, run_id,
  attribution) VALUES ('proj', 'merge-conflict', 't1', 'd', '2026-05-28T12:00:00+00:00',
  'run-1', '');
INSERT INTO graph_snapshots (project_id, captured_at, item_count, last_event_id)
  VALUES ('proj', '2026-05-28T12:00:01+00:00', 1, 1);
INSERT INTO graph_snapshot_items (project_id, snapshot_id, task_id, state, ready)
  VALUES ('proj', 1, 't1', 'ready', 1);
INSERT INTO orchestrator_session_pauses (project_id, run_id, reset_at, pause_until, recorded_at)
  VALUES ('proj', 'run-old', '2026-05-28T12:00:05+00:00', '2026-05-28T12:00:05+00:00',
  '2026-05-28T12:00:00+00:00');
INSERT INTO orchestrator_session_pauses (project_id, run_id, reset_at, pause_until, recorded_at)
  VALUES ('proj', 'run-limit', '2026-05-28T12:00:50+00:00', '2026-05-28T12:00:30+00:00',
  '2026-05-28T12:00:00+00:00');

INSERT OR IGNORE INTO orchestrator_schema_version (version, id) VALUES (10, 1);

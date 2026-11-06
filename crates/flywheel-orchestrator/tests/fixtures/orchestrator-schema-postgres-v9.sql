-- FROZEN FIXTURE: the Postgres orchestrator_schema_version 9 layout (before
-- project_id partitioning), kept verbatim so the in-place upgrade tests
-- migrate a real legacy schema. Never edit; the live DDL is SCHEMA_SQL in
-- crates/flywheel-orchestrator/src/claims_postgres.rs.

CREATE TABLE IF NOT EXISTS task_claims (
  task_id            TEXT PRIMARY KEY,
  worker_id          TEXT NOT NULL,
  claimed_at         TIMESTAMPTZ NOT NULL,
  lease_expires_at   TIMESTAMPTZ NOT NULL,
  version            INTEGER NOT NULL,
  conflict_keys_json JSONB NOT NULL DEFAULT '[]'::jsonb
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
  required_capabilities_json JSONB NOT NULL DEFAULT '[]'::jsonb,
  conflict_keys_json         JSONB NOT NULL DEFAULT '[]'::jsonb,
  first_seen_at              TIMESTAMPTZ NOT NULL,
  last_seen_at               TIMESTAMPTZ NOT NULL,
  disappeared_at             TIMESTAMPTZ,
  metadata_json              JSONB NOT NULL DEFAULT '{}'::jsonb
);

CREATE TABLE IF NOT EXISTS work_item_dependencies (
  task_id              TEXT NOT NULL,
  prerequisite_task_id TEXT NOT NULL,
  created_at           TIMESTAMPTZ NOT NULL,
  PRIMARY KEY (task_id, prerequisite_task_id)
);

CREATE INDEX IF NOT EXISTS idx_work_item_dependencies_prerequisite
  ON work_item_dependencies (prerequisite_task_id);

CREATE TABLE IF NOT EXISTS source_syncs (
  id             BIGSERIAL PRIMARY KEY,
  source_kind    TEXT NOT NULL,
  source_name    TEXT NOT NULL,
  started_at     TIMESTAMPTZ NOT NULL,
  finished_at    TIMESTAMPTZ,
  status         TEXT NOT NULL,
  observed_count INTEGER NOT NULL DEFAULT 0,
  error          TEXT,
  metadata_json  JSONB NOT NULL DEFAULT '{}'::jsonb
);

CREATE TABLE IF NOT EXISTS orchestrator_events (
  id               BIGSERIAL PRIMARY KEY,
  task_id          TEXT NOT NULL,
  worker_id        TEXT NOT NULL,
  event_type       TEXT NOT NULL,
  version          INTEGER NOT NULL,
  lease_expires_at TIMESTAMPTZ NOT NULL,
  occurred_at      TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_orchestrator_events_task
  ON orchestrator_events (task_id, id);

CREATE TABLE IF NOT EXISTS graph_snapshots (
  id            BIGSERIAL PRIMARY KEY,
  captured_at   TIMESTAMPTZ NOT NULL,
  item_count    INTEGER NOT NULL,
  last_event_id BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS graph_snapshot_items (
  snapshot_id                 BIGINT NOT NULL,
  task_id                     TEXT NOT NULL,
  source_kind                 TEXT,
  source_ref                  TEXT,
  source_url                  TEXT,
  source_version              TEXT,
  priority                    INTEGER NOT NULL DEFAULT 0,
  required_capabilities_json  JSONB NOT NULL DEFAULT '[]'::jsonb,
  conflict_keys_json          JSONB NOT NULL DEFAULT '[]'::jsonb,
  state                       TEXT NOT NULL,
  ready                       BOOLEAN NOT NULL,
  claim_holder                TEXT,
  resolved_prerequisites_json JSONB NOT NULL DEFAULT '[]'::jsonb,
  PRIMARY KEY (snapshot_id, task_id)
);

CREATE INDEX IF NOT EXISTS idx_graph_snapshot_items_snapshot
  ON graph_snapshot_items (snapshot_id);

CREATE TABLE IF NOT EXISTS orchestrator_stop_events (
  id          BIGSERIAL PRIMARY KEY,
  kind        TEXT NOT NULL,
  subject     TEXT NOT NULL,
  detail      TEXT NOT NULL,
  occurred_at TIMESTAMPTZ NOT NULL,
  run_id      TEXT NOT NULL DEFAULT '',
  attribution TEXT NOT NULL DEFAULT ''
);

CREATE INDEX IF NOT EXISTS idx_orchestrator_stop_events_subject
  ON orchestrator_stop_events (subject, id);

CREATE TABLE IF NOT EXISTS orchestrator_session_pauses (
  id          BIGSERIAL PRIMARY KEY,
  run_id      TEXT NOT NULL DEFAULT '',
  reset_at    TIMESTAMPTZ NOT NULL,
  pause_until TIMESTAMPTZ NOT NULL,
  recorded_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_orchestrator_session_pauses_pause_until
  ON orchestrator_session_pauses (pause_until);

CREATE INDEX IF NOT EXISTS idx_orchestrator_stop_events_kind
  ON orchestrator_stop_events (kind, id);

INSERT INTO orchestrator_schema_version (id, version) VALUES (1, 9) ON CONFLICT (id) DO NOTHING;

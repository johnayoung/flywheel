-- flywheel-eval store (SQLite). The eval layer's own tables beside the core
-- and orchestrator tables in the project store; each layer touches only its
-- own. Every table but the sentinel is partitioned by the opaque project_id
-- the opener supplies. Timestamps are fixed-width UTC text
-- (YYYY-MM-DDTHH:MM:SS.ffffffZ), so text order is time order in SQL on both
-- backends. The Postgres mirror is eval-schema-postgres.sql; the two must
-- declare the same tables and columns (pinned by a test).

-- One frozen eval set version: the files live under the set dir; this row is
-- the index the runs and the report join on.
CREATE TABLE IF NOT EXISTS eval_sets (
  project_id  TEXT NOT NULL,
  name        TEXT NOT NULL,
  version     INTEGER NOT NULL,
  hash        TEXT NOT NULL,
  created_at  TEXT NOT NULL,
  task_count  INTEGER NOT NULL,
  path        TEXT NOT NULL,
  PRIMARY KEY (project_id, name, version)
);

-- A resolved variant manifest, content-addressed by its hash.
CREATE TABLE IF NOT EXISTS eval_variants (
  project_id     TEXT NOT NULL,
  hash           TEXT NOT NULL,
  name           TEXT NOT NULL,
  resolved_json  TEXT NOT NULL,
  created_at     TEXT NOT NULL,
  PRIMARY KEY (project_id, hash)
);

-- One replay: a frozen set, its variants, k, and how it was run.
CREATE TABLE IF NOT EXISTS eval_runs (
  project_id        TEXT NOT NULL,
  run_id            TEXT NOT NULL,
  set_name          TEXT NOT NULL,
  set_version       INTEGER NOT NULL,
  set_hash          TEXT NOT NULL,
  k                 INTEGER NOT NULL,
  seed              INTEGER NOT NULL,
  primary_metrics   TEXT NOT NULL,
  budget_usd        REAL,
  pipeline          TEXT NOT NULL,
  parallel          INTEGER NOT NULL,
  status            TEXT NOT NULL,
  started_at        TEXT NOT NULL,
  ended_at          TEXT,
  analysis_md_hash  TEXT,
  options_json      TEXT NOT NULL DEFAULT '{}',
  PRIMARY KEY (project_id, run_id)
);

CREATE TABLE IF NOT EXISTS eval_run_variants (
  project_id    TEXT NOT NULL,
  run_id        TEXT NOT NULL,
  ordinal       INTEGER NOT NULL,
  variant_hash  TEXT NOT NULL,
  label         TEXT NOT NULL,
  PRIMARY KEY (project_id, run_id, ordinal)
);

-- One cell of variants x tasks x k. ordinal is the launch order (interleaved
-- so a stopped run stays paired); version is the optimistic-concurrency
-- counter every write bumps.
CREATE TABLE IF NOT EXISTS eval_trials (
  project_id        TEXT NOT NULL,
  run_id            TEXT NOT NULL,
  trial_id          TEXT NOT NULL,
  ordinal           INTEGER NOT NULL,
  variant_hash      TEXT NOT NULL,
  task_id           TEXT NOT NULL,
  k                 INTEGER NOT NULL,
  status            TEXT NOT NULL,
  claimed_by        TEXT,
  lease_expires_at  TEXT,
  overlay_hash      TEXT,
  final_commit      TEXT,
  final_where       TEXT,
  oracle            TEXT,
  validity          TEXT,
  validity_detail   TEXT,
  cost_usd          REAL NOT NULL DEFAULT 0,
  turns             INTEGER NOT NULL DEFAULT 0,
  attempts          INTEGER NOT NULL DEFAULT 0,
  agent_secs        REAL NOT NULL DEFAULT 0,
  wall_secs         REAL NOT NULL DEFAULT 0,
  input_tokens      INTEGER NOT NULL DEFAULT 0,
  output_tokens     INTEGER NOT NULL DEFAULT 0,
  cache_read        INTEGER NOT NULL DEFAULT 0,
  cache_write       INTEGER NOT NULL DEFAULT 0,
  flywheel_status   TEXT,
  trace_json        TEXT NOT NULL DEFAULT '{}',
  started_at        TEXT,
  ended_at          TEXT,
  version           INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (project_id, run_id, trial_id)
);

CREATE INDEX IF NOT EXISTS idx_eval_trials_order
  ON eval_trials (project_id, run_id, status, ordinal);

-- Per-constraint scores: an oracle's checks or a rubric's assertions.
CREATE TABLE IF NOT EXISTS eval_trial_checks (
  project_id  TEXT NOT NULL,
  run_id      TEXT NOT NULL,
  trial_id    TEXT NOT NULL,
  source      TEXT NOT NULL,
  check_id    TEXT NOT NULL,
  passed      INTEGER NOT NULL,
  note        TEXT NOT NULL DEFAULT '',
  PRIMARY KEY (project_id, run_id, trial_id, source, check_id)
);

-- A transcript reader's verdict, and the operator's decision on a suspect.
CREATE TABLE IF NOT EXISTS eval_trial_reviews (
  project_id         TEXT NOT NULL,
  run_id             TEXT NOT NULL,
  trial_id           TEXT NOT NULL,
  reader_model       TEXT NOT NULL,
  verdict            TEXT NOT NULL,
  confidence         TEXT NOT NULL,
  evidence_json      TEXT NOT NULL DEFAULT '[]',
  notes              TEXT NOT NULL DEFAULT '',
  operator_decision  TEXT,
  decided_at         TEXT,
  reviewed_at        TEXT NOT NULL,
  PRIMARY KEY (project_id, run_id, trial_id)
);

-- Each kept trial file and its digest (the transcripts-ledger pattern).
CREATE TABLE IF NOT EXISTS eval_trial_artifacts (
  project_id  TEXT NOT NULL,
  run_id      TEXT NOT NULL,
  trial_id    TEXT NOT NULL,
  name        TEXT NOT NULL,
  sha256      TEXT NOT NULL,
  bytes       INTEGER NOT NULL,
  PRIMARY KEY (project_id, run_id, trial_id, name)
);

CREATE TABLE IF NOT EXISTS eval_schema_version (
  version  INTEGER NOT NULL,
  id       INTEGER PRIMARY KEY CHECK (id = 1)
);

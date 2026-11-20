//! The eval store: frozen-set index, resolved variants, runs, trials and
//! their scores, reviews, and artifacts -- the eval layer's own tables in
//! the project store (SQLite or Postgres), partitioned by `project_id` and
//! versioned by their own `eval_schema_version` sentinel, like the
//! orchestrator's. Set content and trial files live on disk (see
//! [`crate::set`] and [`crate::trial`]); these rows index them.
//!
//! Trials are claimed with a lease so a runner that dies leaves its cells
//! to the next one ([`EvalStore::claim_next`]): the claim is an optimistic
//! `UPDATE .. WHERE version = ?`, so two runners on one store never run the
//! same cell.

use chrono::{DateTime, Utc};

use flywheel_core::store::SchemaUpgrade;

use crate::db::{Db, PostgresDb, Row, SqliteDb, Value};

fn upgraded(from: flywheel_core::store_sqlite::Recorded) -> SchemaUpgrade {
    SchemaUpgrade {
        from: match from {
            flywheel_core::store_sqlite::Recorded::Missing => None,
            flywheel_core::store_sqlite::Recorded::Version(v) => v,
        },
        to: CURRENT_EVAL_SCHEMA_VERSION,
        partitioned: Vec::new(),
    }
}

/// This build's eval schema version.
pub const CURRENT_EVAL_SCHEMA_VERSION: u32 = 1;
/// The eval schema-version sentinel table.
pub const EVAL_VERSION_TABLE: &str = "eval_schema_version";
/// Every eval table but the sentinel.
pub const EVAL_TABLES: [&str; 8] = [
    "eval_sets",
    "eval_variants",
    "eval_runs",
    "eval_run_variants",
    "eval_trials",
    "eval_trial_checks",
    "eval_trial_reviews",
    "eval_trial_artifacts",
];
/// The SQLite DDL.
pub const SCHEMA_SQL: &str = include_str!("../schema/eval-schema.sql");
/// The Postgres DDL (the same tables and columns).
pub const SCHEMA_SQL_POSTGRES: &str = include_str!("../schema/eval-schema-postgres.sql");

#[derive(Debug, thiserror::Error)]
pub enum EvalStoreError {
    #[error(transparent)]
    Backend(Box<dyn std::error::Error + Send + Sync + 'static>),
    #[error(
        "eval store schema is {}, this flywheel expects {CURRENT_EVAL_SCHEMA_VERSION}: run `fw db upgrade`",
        observed.map_or_else(|| "unreadable".to_owned(), |v| v.to_string())
    )]
    Schema { observed: Option<u32> },
    #[error("eval store row is corrupt: {0}")]
    Corrupt(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    NotFound(String),
}

pub(crate) fn schema_error(observed: Option<u32>) -> EvalStoreError {
    EvalStoreError::Schema { observed }
}

/// Fixed-width UTC text: text order is time order on both backends.
pub fn ts(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
}

/// Parse [`ts`] text (or any RFC 3339).
pub fn parse_ts(text: &str) -> Result<DateTime<Utc>, EvalStoreError> {
    DateTime::parse_from_rfc3339(text.trim())
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| EvalStoreError::Corrupt(format!("timestamp {text:?}: {e}")))
}

fn opt_ts(text: Option<String>) -> Result<Option<DateTime<Utc>>, EvalStoreError> {
    text.as_deref().map(parse_ts).transpose()
}

// --- Rows -----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct SetRow {
    pub name: String,
    pub version: u32,
    pub hash: String,
    pub created_at: DateTime<Utc>,
    pub task_count: u32,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VariantRow {
    pub hash: String,
    pub name: String,
    pub resolved_json: String,
    pub created_at: DateTime<Utc>,
}

/// Where a run stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    /// Cells still to run.
    Running,
    /// Stopped short by its budget, quota, or the operator; resumable.
    Paused,
    /// Every cell ran.
    Done,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Done => "done",
        }
    }

    pub fn parse(s: &str) -> Result<Self, EvalStoreError> {
        match s {
            "running" => Ok(Self::Running),
            "paused" => Ok(Self::Paused),
            "done" => Ok(Self::Done),
            other => Err(EvalStoreError::Corrupt(format!("run status {other:?}"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunRow {
    pub run_id: String,
    pub set_name: String,
    pub set_version: u32,
    pub set_hash: String,
    pub k: u32,
    pub seed: u64,
    /// Comma-separated metric names declared primary at launch.
    pub primary_metrics: String,
    pub budget_usd: Option<f64>,
    pub pipeline: String,
    pub parallel: u32,
    pub status: RunStatus,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub analysis_md_hash: Option<String>,
    /// Everything else the run was launched with (JSON object).
    pub options_json: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunVariantRow {
    pub ordinal: u32,
    pub variant_hash: String,
    pub label: String,
}

/// Where a trial stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrialStatus {
    Pending,
    Running,
    /// Ran and was graded (its oracle verdict and validity say how it went).
    Done,
    /// The machinery failed (not the agent): excluded from the scores.
    InfraError,
}

impl TrialStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Done => "done",
            Self::InfraError => "infra_error",
        }
    }

    pub fn parse(s: &str) -> Result<Self, EvalStoreError> {
        match s {
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "done" => Ok(Self::Done),
            "infra_error" => Ok(Self::InfraError),
            other => Err(EvalStoreError::Corrupt(format!("trial status {other:?}"))),
        }
    }
}

/// One trial: a cell of variants x tasks x k, and what it produced.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct TrialRow {
    pub trial_id: String,
    pub ordinal: u32,
    pub variant_hash: String,
    pub task_id: String,
    pub k: u32,
    pub status: Option<TrialStatus>,
    pub claimed_by: Option<String>,
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub overlay_hash: Option<String>,
    pub final_commit: Option<String>,
    pub final_where: Option<String>,
    /// `pass`, `fail`, or `error` (the held-out oracle on the final commit).
    pub oracle: Option<String>,
    /// `valid`, `contaminated`, `suspect`, or `voided`.
    pub validity: Option<String>,
    pub validity_detail: Option<String>,
    pub cost_usd: f64,
    pub turns: u32,
    pub attempts: u32,
    pub agent_secs: f64,
    pub wall_secs: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub flywheel_status: Option<String>,
    pub trace_json: String,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub version: i64,
}

impl TrialRow {
    /// A pending cell.
    pub fn planned(
        trial_id: String,
        ordinal: u32,
        variant_hash: String,
        task_id: String,
        k: u32,
    ) -> Self {
        Self {
            trial_id,
            ordinal,
            variant_hash,
            task_id,
            k,
            status: Some(TrialStatus::Pending),
            trace_json: "{}".to_owned(),
            ..Self::default()
        }
    }

    pub fn status(&self) -> TrialStatus {
        self.status.unwrap_or(TrialStatus::Pending)
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CheckRow {
    pub trial_id: String,
    /// `oracle` or `rubric`.
    pub source: String,
    pub check_id: String,
    pub passed: bool,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReviewRow {
    pub trial_id: String,
    pub reader_model: String,
    pub verdict: String,
    pub confidence: String,
    pub evidence_json: String,
    pub notes: String,
    pub operator_decision: Option<String>,
    pub decided_at: Option<DateTime<Utc>>,
    pub reviewed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ArtifactRow {
    pub name: String,
    pub sha256: String,
    pub bytes: u64,
}

// --- Store ----------------------------------------------------------------------------

/// The eval store over one backend, scoped to one project partition.
pub struct EvalStore {
    db: std::sync::Mutex<Box<dyn Db>>,
    partition: String,
}

impl std::fmt::Debug for EvalStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EvalStore")
            .field("partition", &self.partition)
            .finish_non_exhaustive()
    }
}

const TRIAL_COLUMNS: &str = "trial_id, ordinal, variant_hash, task_id, k, status, claimed_by, \
     lease_expires_at, overlay_hash, final_commit, final_where, oracle, validity, validity_detail, \
     cost_usd, turns, attempts, agent_secs, wall_secs, input_tokens, output_tokens, cache_read, \
     cache_write, flywheel_status, trace_json, started_at, ended_at, version";

const RUN_COLUMNS: &str = "run_id, set_name, set_version, set_hash, k, seed, primary_metrics, \
     budget_usd, pipeline, parallel, status, started_at, ended_at, analysis_md_hash, options_json";

fn u32_of(v: i64, what: &str) -> Result<u32, EvalStoreError> {
    u32::try_from(v).map_err(|_| EvalStoreError::Corrupt(format!("{what} {v} out of range")))
}

fn trial_of(r: &Row) -> Result<TrialRow, EvalStoreError> {
    Ok(TrialRow {
        trial_id: r.text(0)?,
        ordinal: u32_of(r.int(1)?, "ordinal")?,
        variant_hash: r.text(2)?,
        task_id: r.text(3)?,
        k: u32_of(r.int(4)?, "k")?,
        status: Some(TrialStatus::parse(&r.text(5)?)?),
        claimed_by: r.opt_text(6)?,
        lease_expires_at: opt_ts(r.opt_text(7)?)?,
        overlay_hash: r.opt_text(8)?,
        final_commit: r.opt_text(9)?,
        final_where: r.opt_text(10)?,
        oracle: r.opt_text(11)?,
        validity: r.opt_text(12)?,
        validity_detail: r.opt_text(13)?,
        cost_usd: r.real(14)?,
        turns: u32_of(r.int(15)?, "turns")?,
        attempts: u32_of(r.int(16)?, "attempts")?,
        agent_secs: r.real(17)?,
        wall_secs: r.real(18)?,
        input_tokens: r.int(19)?.max(0) as u64,
        output_tokens: r.int(20)?.max(0) as u64,
        cache_read: r.int(21)?.max(0) as u64,
        cache_write: r.int(22)?.max(0) as u64,
        flywheel_status: r.opt_text(23)?,
        trace_json: r.text(24)?,
        started_at: opt_ts(r.opt_text(25)?)?,
        ended_at: opt_ts(r.opt_text(26)?)?,
        version: r.int(27)?,
    })
}

fn run_of(r: &Row) -> Result<RunRow, EvalStoreError> {
    Ok(RunRow {
        run_id: r.text(0)?,
        set_name: r.text(1)?,
        set_version: u32_of(r.int(2)?, "set_version")?,
        set_hash: r.text(3)?,
        k: u32_of(r.int(4)?, "k")?,
        seed: r.int(5)? as u64,
        primary_metrics: r.text(6)?,
        budget_usd: r.opt_real(7)?,
        pipeline: r.text(8)?,
        parallel: u32_of(r.int(9)?, "parallel")?,
        status: RunStatus::parse(&r.text(10)?)?,
        started_at: parse_ts(&r.text(11)?)?,
        ended_at: opt_ts(r.opt_text(12)?)?,
        analysis_md_hash: r.opt_text(13)?,
        options_json: r.text(14)?,
    })
}

fn tokens(n: u64) -> Value {
    Value::int(i64::try_from(n).unwrap_or(i64::MAX))
}

impl EvalStore {
    /// The eval tables in the SQLite store at `path`.
    pub fn open_sqlite(path: &std::path::Path, partition: &str) -> Result<Self, EvalStoreError> {
        Ok(Self::over(Box::new(SqliteDb::open(path)?), partition))
    }

    /// The eval tables in the Postgres store at `dsn`, under `schema`.
    pub fn connect_postgres(
        dsn: &str,
        schema: &str,
        partition: &str,
    ) -> Result<Self, EvalStoreError> {
        Ok(Self::over(
            Box::new(PostgresDb::connect(dsn, schema)?),
            partition,
        ))
    }

    /// `fw db upgrade`'s eval half for the SQLite store at `path`: create
    /// the eval tables when absent (schema 1 is the first, so nothing older
    /// exists to migrate); a store at another version is refused.
    pub fn upgrade_sqlite(path: &std::path::Path) -> Result<SchemaUpgrade, EvalStoreError> {
        let from = {
            let conn = rusqlite::Connection::open(path)
                .map_err(|e| EvalStoreError::Backend(Box::new(e)))?;
            flywheel_core::store_sqlite::recorded_version(&conn, EVAL_VERSION_TABLE)
                .map_err(|e| EvalStoreError::Backend(Box::new(e)))?
        };
        drop(SqliteDb::open(path)?);
        Ok(upgraded(from))
    }

    /// `fw db upgrade`'s eval half for Postgres (see [`Self::upgrade_sqlite`]).
    pub fn upgrade_postgres(dsn: &str, schema: &str) -> Result<SchemaUpgrade, EvalStoreError> {
        let from = flywheel_core::store_postgres::PostgresStore::recorded_versions(
            dsn,
            schema,
            &[EVAL_VERSION_TABLE],
        )
        .map_err(|e| EvalStoreError::Backend(Box::new(e)))?
        .first()
        .copied()
        .unwrap_or(flywheel_core::store_sqlite::Recorded::Missing);
        drop(PostgresDb::connect(dsn, schema)?);
        Ok(upgraded(from))
    }

    /// A private in-memory store (tests).
    pub fn in_memory(partition: &str) -> Result<Self, EvalStoreError> {
        Ok(Self::over(Box::new(SqliteDb::in_memory()?), partition))
    }

    fn over(db: Box<dyn Db>, partition: &str) -> Self {
        Self {
            db: std::sync::Mutex::new(db),
            partition: partition.to_owned(),
        }
    }

    pub fn partition(&self) -> &str {
        &self.partition
    }

    fn db(&self) -> std::sync::MutexGuard<'_, Box<dyn Db>> {
        self.db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn p(&self) -> Value {
        Value::text(self.partition.clone())
    }

    // --- sets ---

    /// Index a frozen set version; a version already indexed is refused.
    pub fn put_set(&self, set: &SetRow) -> Result<(), EvalStoreError> {
        let n = self.db().execute(
            "INSERT INTO eval_sets (project_id, name, version, hash, created_at, task_count, path)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) ON CONFLICT (project_id, name, version) DO NOTHING",
            &[
                self.p(),
                Value::text(set.name.clone()),
                Value::int(set.version),
                Value::text(set.hash.clone()),
                Value::text(ts(set.created_at)),
                Value::int(set.task_count),
                Value::text(set.path.clone()),
            ],
        )?;
        if n == 0 {
            return Err(EvalStoreError::Conflict(format!(
                "eval set {}@v{} is already frozen",
                set.name, set.version
            )));
        }
        Ok(())
    }

    /// Every indexed set version, by name then version.
    pub fn sets(&self) -> Result<Vec<SetRow>, EvalStoreError> {
        let rows = self.db().query(
            "SELECT name, version, hash, created_at, task_count, path FROM eval_sets
             WHERE project_id = ?1 ORDER BY name, version",
            &[self.p()],
        )?;
        rows.iter()
            .map(|r| {
                Ok(SetRow {
                    name: r.text(0)?,
                    version: u32_of(r.int(1)?, "version")?,
                    hash: r.text(2)?,
                    created_at: parse_ts(&r.text(3)?)?,
                    task_count: u32_of(r.int(4)?, "task_count")?,
                    path: r.text(5)?,
                })
            })
            .collect()
    }

    // --- variants ---

    /// Record a resolved variant (idempotent by hash).
    pub fn put_variant(&self, v: &VariantRow) -> Result<(), EvalStoreError> {
        self.db().execute(
            "INSERT INTO eval_variants (project_id, hash, name, resolved_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (project_id, hash) DO NOTHING",
            &[
                self.p(),
                Value::text(v.hash.clone()),
                Value::text(v.name.clone()),
                Value::text(v.resolved_json.clone()),
                Value::text(ts(v.created_at)),
            ],
        )?;
        Ok(())
    }

    pub fn variant(&self, hash: &str) -> Result<Option<VariantRow>, EvalStoreError> {
        let rows = self.db().query(
            "SELECT hash, name, resolved_json, created_at FROM eval_variants
             WHERE project_id = ?1 AND hash = ?2",
            &[self.p(), Value::text(hash)],
        )?;
        rows.first()
            .map(|r| {
                Ok(VariantRow {
                    hash: r.text(0)?,
                    name: r.text(1)?,
                    resolved_json: r.text(2)?,
                    created_at: parse_ts(&r.text(3)?)?,
                })
            })
            .transpose()
    }

    // --- runs ---

    /// Create a run, its variants, and every planned cell, atomically.
    pub fn create_run(
        &self,
        run: &RunRow,
        variants: &[RunVariantRow],
        trials: &[TrialRow],
    ) -> Result<(), EvalStoreError> {
        let mut statements = vec![(
            format!(
                "INSERT INTO eval_runs (project_id, {RUN_COLUMNS})
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)"
            ),
            vec![
                self.p(),
                Value::text(run.run_id.clone()),
                Value::text(run.set_name.clone()),
                Value::int(run.set_version),
                Value::text(run.set_hash.clone()),
                Value::int(run.k),
                Value::int(i64::try_from(run.seed & (i64::MAX as u64)).unwrap_or(0)),
                Value::text(run.primary_metrics.clone()),
                Value::Real(run.budget_usd),
                Value::text(run.pipeline.clone()),
                Value::int(run.parallel),
                Value::text(run.status.as_str()),
                Value::text(ts(run.started_at)),
                Value::opt_text(run.ended_at.map(ts)),
                Value::opt_text(run.analysis_md_hash.clone()),
                Value::text(run.options_json.clone()),
            ],
        )];
        for v in variants {
            statements.push((
                "INSERT INTO eval_run_variants (project_id, run_id, ordinal, variant_hash, label)
                 VALUES (?1, ?2, ?3, ?4, ?5)"
                    .to_owned(),
                vec![
                    self.p(),
                    Value::text(run.run_id.clone()),
                    Value::int(v.ordinal),
                    Value::text(v.variant_hash.clone()),
                    Value::text(v.label.clone()),
                ],
            ));
        }
        for t in trials {
            statements.push((
                "INSERT INTO eval_trials (project_id, run_id, trial_id, ordinal, variant_hash,
                     task_id, k, status, trace_json, version)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', '{}', 0)"
                    .to_owned(),
                vec![
                    self.p(),
                    Value::text(run.run_id.clone()),
                    Value::text(t.trial_id.clone()),
                    Value::int(t.ordinal),
                    Value::text(t.variant_hash.clone()),
                    Value::text(t.task_id.clone()),
                    Value::int(t.k),
                ],
            ));
        }
        self.db().batch(&statements)
    }

    pub fn runs(&self) -> Result<Vec<RunRow>, EvalStoreError> {
        let rows = self.db().query(
            &format!(
                "SELECT {RUN_COLUMNS} FROM eval_runs WHERE project_id = ?1 ORDER BY started_at DESC"
            ),
            &[self.p()],
        )?;
        rows.iter().map(run_of).collect()
    }

    pub fn run(&self, run_id: &str) -> Result<RunRow, EvalStoreError> {
        let rows = self.db().query(
            &format!("SELECT {RUN_COLUMNS} FROM eval_runs WHERE project_id = ?1 AND run_id = ?2"),
            &[self.p(), Value::text(run_id)],
        )?;
        rows.first()
            .map(run_of)
            .transpose()?
            .ok_or_else(|| EvalStoreError::NotFound(format!("no eval run {run_id}")))
    }

    pub fn run_variants(&self, run_id: &str) -> Result<Vec<RunVariantRow>, EvalStoreError> {
        let rows = self.db().query(
            "SELECT ordinal, variant_hash, label FROM eval_run_variants
             WHERE project_id = ?1 AND run_id = ?2 ORDER BY ordinal",
            &[self.p(), Value::text(run_id)],
        )?;
        rows.iter()
            .map(|r| {
                Ok(RunVariantRow {
                    ordinal: u32_of(r.int(0)?, "ordinal")?,
                    variant_hash: r.text(1)?,
                    label: r.text(2)?,
                })
            })
            .collect()
    }

    /// Set a run's status (and its end, when it ends).
    pub fn set_run_status(
        &self,
        run_id: &str,
        status: RunStatus,
        ended_at: Option<DateTime<Utc>>,
    ) -> Result<(), EvalStoreError> {
        self.db().execute(
            "UPDATE eval_runs SET status = ?3, ended_at = ?4 WHERE project_id = ?1 AND run_id = ?2",
            &[
                self.p(),
                Value::text(run_id),
                Value::text(status.as_str()),
                Value::opt_text(ended_at.map(ts)),
            ],
        )?;
        Ok(())
    }

    /// Raise (or set) a run's budget cap.
    pub fn set_run_budget(
        &self,
        run_id: &str,
        budget_usd: Option<f64>,
    ) -> Result<(), EvalStoreError> {
        self.db().execute(
            "UPDATE eval_runs SET budget_usd = ?3 WHERE project_id = ?1 AND run_id = ?2",
            &[self.p(), Value::text(run_id), Value::Real(budget_usd)],
        )?;
        Ok(())
    }

    // --- trials ---

    /// Every cell of a run, in launch order.
    pub fn trials(&self, run_id: &str) -> Result<Vec<TrialRow>, EvalStoreError> {
        let rows = self.db().query(
            &format!(
                "SELECT {TRIAL_COLUMNS} FROM eval_trials WHERE project_id = ?1 AND run_id = ?2
                 ORDER BY ordinal"
            ),
            &[self.p(), Value::text(run_id)],
        )?;
        rows.iter().map(trial_of).collect()
    }

    pub fn trial(&self, run_id: &str, trial_id: &str) -> Result<TrialRow, EvalStoreError> {
        let rows = self.db().query(
            &format!(
                "SELECT {TRIAL_COLUMNS} FROM eval_trials
                 WHERE project_id = ?1 AND run_id = ?2 AND trial_id = ?3"
            ),
            &[self.p(), Value::text(run_id), Value::text(trial_id)],
        )?;
        rows.first()
            .map(trial_of)
            .transpose()?
            .ok_or_else(|| EvalStoreError::NotFound(format!("no trial {trial_id} in run {run_id}")))
    }

    /// Claim the first cell (in launch order) that is pending, or running
    /// under a lease that has run out, for `worker` until `lease_until`.
    /// `skip` passes over cells the caller cannot take now. `None` when no
    /// cell is claimable.
    pub fn claim_next(
        &self,
        run_id: &str,
        worker: &str,
        now: DateTime<Utc>,
        lease_until: DateTime<Utc>,
        skip: &dyn Fn(&TrialRow) -> bool,
    ) -> Result<Option<TrialRow>, EvalStoreError> {
        for _ in 0..16 {
            let candidates = self.trials(run_id)?;
            let pick = candidates.into_iter().find(|t| {
                let open = match t.status() {
                    TrialStatus::Pending => true,
                    TrialStatus::Running => t.lease_expires_at.is_none_or(|l| l <= now),
                    TrialStatus::Done | TrialStatus::InfraError => false,
                };
                open && !skip(t)
            });
            let Some(mut trial) = pick else {
                return Ok(None);
            };
            let n = self.db().execute(
                "UPDATE eval_trials SET status = 'running', claimed_by = ?4,
                     lease_expires_at = ?5, started_at = ?6, version = version + 1
                 WHERE project_id = ?1 AND run_id = ?2 AND trial_id = ?3 AND version = ?7",
                &[
                    self.p(),
                    Value::text(run_id),
                    Value::text(trial.trial_id.clone()),
                    Value::text(worker),
                    Value::text(ts(lease_until)),
                    Value::text(ts(now)),
                    Value::int(trial.version),
                ],
            )?;
            if n == 1 {
                trial.status = Some(TrialStatus::Running);
                trial.claimed_by = Some(worker.to_owned());
                trial.lease_expires_at = Some(lease_until);
                trial.started_at = Some(now);
                trial.version += 1;
                return Ok(Some(trial));
            }
        }
        Err(EvalStoreError::Conflict(format!(
            "run {run_id}: could not claim a cell after repeated contention"
        )))
    }

    /// Extend `worker`'s lease on a running cell; false when it is no longer
    /// `worker`'s.
    pub fn renew(
        &self,
        run_id: &str,
        trial_id: &str,
        worker: &str,
        lease_until: DateTime<Utc>,
    ) -> Result<bool, EvalStoreError> {
        let n = self.db().execute(
            "UPDATE eval_trials SET lease_expires_at = ?5
             WHERE project_id = ?1 AND run_id = ?2 AND trial_id = ?3 AND claimed_by = ?4
               AND status = 'running'",
            &[
                self.p(),
                Value::text(run_id),
                Value::text(trial_id),
                Value::text(worker),
                Value::text(ts(lease_until)),
            ],
        )?;
        Ok(n == 1)
    }

    /// Put a claimed cell back to pending (a stop before it ran).
    pub fn release(
        &self,
        run_id: &str,
        trial_id: &str,
        worker: &str,
    ) -> Result<(), EvalStoreError> {
        self.db().execute(
            "UPDATE eval_trials SET status = 'pending', claimed_by = NULL, lease_expires_at = NULL,
                 started_at = NULL, version = version + 1
             WHERE project_id = ?1 AND run_id = ?2 AND trial_id = ?3 AND claimed_by = ?4
               AND status = 'running'",
            &[
                self.p(),
                Value::text(run_id),
                Value::text(trial_id),
                Value::text(worker),
            ],
        )?;
        Ok(())
    }

    /// Record a finished cell's outcome. Refused unless `worker` still
    /// holds it (a runner whose lease ran out and was taken over loses).
    pub fn finish(&self, run_id: &str, worker: &str, t: &TrialRow) -> Result<(), EvalStoreError> {
        let n = self.db().execute(
            "UPDATE eval_trials SET status = ?5, overlay_hash = ?6, final_commit = ?7,
                 final_where = ?8, oracle = ?9, validity = ?10, validity_detail = ?11,
                 cost_usd = ?12, turns = ?13, attempts = ?14, agent_secs = ?15, wall_secs = ?16,
                 input_tokens = ?17, output_tokens = ?18, cache_read = ?19, cache_write = ?20,
                 flywheel_status = ?21, trace_json = ?22, ended_at = ?23,
                 lease_expires_at = NULL, version = version + 1
             WHERE project_id = ?1 AND run_id = ?2 AND trial_id = ?3 AND claimed_by = ?4
               AND status = 'running'",
            &[
                self.p(),
                Value::text(run_id),
                Value::text(t.trial_id.clone()),
                Value::text(worker),
                Value::text(t.status().as_str()),
                Value::opt_text(t.overlay_hash.clone()),
                Value::opt_text(t.final_commit.clone()),
                Value::opt_text(t.final_where.clone()),
                Value::opt_text(t.oracle.clone()),
                Value::opt_text(t.validity.clone()),
                Value::opt_text(t.validity_detail.clone()),
                Value::real(t.cost_usd),
                Value::int(t.turns),
                Value::int(t.attempts),
                Value::real(t.agent_secs),
                Value::real(t.wall_secs),
                tokens(t.input_tokens),
                tokens(t.output_tokens),
                tokens(t.cache_read),
                tokens(t.cache_write),
                Value::opt_text(t.flywheel_status.clone()),
                Value::text(t.trace_json.clone()),
                Value::opt_text(t.ended_at.map(ts)),
            ],
        )?;
        if n != 1 {
            return Err(EvalStoreError::Conflict(format!(
                "trial {} of run {run_id} is no longer held by {worker}",
                t.trial_id
            )));
        }
        Ok(())
    }

    /// Change a finished cell's validity (a suspect pass confirmed or
    /// cleared by the operator).
    pub fn set_validity(
        &self,
        run_id: &str,
        trial_id: &str,
        validity: &str,
        detail: &str,
    ) -> Result<(), EvalStoreError> {
        let n = self.db().execute(
            "UPDATE eval_trials SET validity = ?4, validity_detail = ?5, version = version + 1
             WHERE project_id = ?1 AND run_id = ?2 AND trial_id = ?3",
            &[
                self.p(),
                Value::text(run_id),
                Value::text(trial_id),
                Value::text(validity),
                Value::text(detail),
            ],
        )?;
        if n != 1 {
            return Err(EvalStoreError::NotFound(format!(
                "no trial {trial_id} in run {run_id}"
            )));
        }
        Ok(())
    }

    /// Put a stopped cell back to pending for a retry (an infra error's
    /// one re-queue).
    pub fn requeue(&self, run_id: &str, trial_id: &str) -> Result<(), EvalStoreError> {
        self.db().execute(
            "UPDATE eval_trials SET status = 'pending', claimed_by = NULL, lease_expires_at = NULL,
                 started_at = NULL, ended_at = NULL, version = version + 1
             WHERE project_id = ?1 AND run_id = ?2 AND trial_id = ?3",
            &[self.p(), Value::text(run_id), Value::text(trial_id)],
        )?;
        Ok(())
    }

    // --- checks, reviews, artifacts ---

    /// Replace a trial's per-constraint scores from one source.
    pub fn put_checks(
        &self,
        run_id: &str,
        trial_id: &str,
        source: &str,
        checks: &[CheckRow],
    ) -> Result<(), EvalStoreError> {
        let mut statements = vec![(
            "DELETE FROM eval_trial_checks
             WHERE project_id = ?1 AND run_id = ?2 AND trial_id = ?3 AND source = ?4"
                .to_owned(),
            vec![
                self.p(),
                Value::text(run_id),
                Value::text(trial_id),
                Value::text(source),
            ],
        )];
        for c in checks {
            statements.push((
                "INSERT INTO eval_trial_checks (project_id, run_id, trial_id, source, check_id,
                     passed, note) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT (project_id, run_id, trial_id, source, check_id)
                 DO UPDATE SET passed = excluded.passed, note = excluded.note"
                    .to_owned(),
                vec![
                    self.p(),
                    Value::text(run_id),
                    Value::text(trial_id),
                    Value::text(source),
                    Value::text(c.check_id.clone()),
                    Value::int(i64::from(c.passed)),
                    Value::text(c.note.clone()),
                ],
            ));
        }
        self.db().batch(&statements)
    }

    pub fn checks(&self, run_id: &str) -> Result<Vec<CheckRow>, EvalStoreError> {
        let rows = self.db().query(
            "SELECT trial_id, source, check_id, passed, note FROM eval_trial_checks
             WHERE project_id = ?1 AND run_id = ?2 ORDER BY trial_id, source, check_id",
            &[self.p(), Value::text(run_id)],
        )?;
        rows.iter()
            .map(|r| {
                Ok(CheckRow {
                    trial_id: r.text(0)?,
                    source: r.text(1)?,
                    check_id: r.text(2)?,
                    passed: r.int(3)? != 0,
                    note: r.text(4)?,
                })
            })
            .collect()
    }

    /// Record (or replace) a trial's transcript review.
    pub fn put_review(&self, run_id: &str, r: &ReviewRow) -> Result<(), EvalStoreError> {
        self.db().execute(
            "INSERT INTO eval_trial_reviews (project_id, run_id, trial_id, reader_model, verdict,
                 confidence, evidence_json, notes, operator_decision, decided_at, reviewed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT (project_id, run_id, trial_id) DO UPDATE SET
                 reader_model = excluded.reader_model, verdict = excluded.verdict,
                 confidence = excluded.confidence, evidence_json = excluded.evidence_json,
                 notes = excluded.notes, operator_decision = excluded.operator_decision,
                 decided_at = excluded.decided_at, reviewed_at = excluded.reviewed_at",
            &[
                self.p(),
                Value::text(run_id),
                Value::text(r.trial_id.clone()),
                Value::text(r.reader_model.clone()),
                Value::text(r.verdict.clone()),
                Value::text(r.confidence.clone()),
                Value::text(r.evidence_json.clone()),
                Value::text(r.notes.clone()),
                Value::opt_text(r.operator_decision.clone()),
                Value::opt_text(r.decided_at.map(ts)),
                Value::text(ts(r.reviewed_at)),
            ],
        )?;
        Ok(())
    }

    pub fn reviews(&self, run_id: &str) -> Result<Vec<ReviewRow>, EvalStoreError> {
        let rows = self.db().query(
            "SELECT trial_id, reader_model, verdict, confidence, evidence_json, notes,
                 operator_decision, decided_at, reviewed_at FROM eval_trial_reviews
             WHERE project_id = ?1 AND run_id = ?2 ORDER BY trial_id",
            &[self.p(), Value::text(run_id)],
        )?;
        rows.iter()
            .map(|r| {
                Ok(ReviewRow {
                    trial_id: r.text(0)?,
                    reader_model: r.text(1)?,
                    verdict: r.text(2)?,
                    confidence: r.text(3)?,
                    evidence_json: r.text(4)?,
                    notes: r.text(5)?,
                    operator_decision: r.opt_text(6)?,
                    decided_at: opt_ts(r.opt_text(7)?)?,
                    reviewed_at: parse_ts(&r.text(8)?)?,
                })
            })
            .collect()
    }

    /// Record the operator's decision on a reviewed trial.
    pub fn decide_review(
        &self,
        run_id: &str,
        trial_id: &str,
        decision: &str,
        at: DateTime<Utc>,
    ) -> Result<(), EvalStoreError> {
        let n = self.db().execute(
            "UPDATE eval_trial_reviews SET operator_decision = ?4, decided_at = ?5
             WHERE project_id = ?1 AND run_id = ?2 AND trial_id = ?3",
            &[
                self.p(),
                Value::text(run_id),
                Value::text(trial_id),
                Value::text(decision),
                Value::text(ts(at)),
            ],
        )?;
        if n != 1 {
            return Err(EvalStoreError::NotFound(format!(
                "no review of trial {trial_id} in run {run_id}"
            )));
        }
        Ok(())
    }

    /// Record a trial's kept files and their digests (replacing earlier ones).
    pub fn put_artifacts(
        &self,
        run_id: &str,
        trial_id: &str,
        artifacts: &[ArtifactRow],
    ) -> Result<(), EvalStoreError> {
        let mut statements = vec![(
            "DELETE FROM eval_trial_artifacts WHERE project_id = ?1 AND run_id = ?2 AND trial_id = ?3"
                .to_owned(),
            vec![self.p(), Value::text(run_id), Value::text(trial_id)],
        )];
        for a in artifacts {
            statements.push((
                "INSERT INTO eval_trial_artifacts (project_id, run_id, trial_id, name, sha256, bytes)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
                    .to_owned(),
                vec![
                    self.p(),
                    Value::text(run_id),
                    Value::text(trial_id),
                    Value::text(a.name.clone()),
                    Value::text(a.sha256.clone()),
                    tokens(a.bytes),
                ],
            ));
        }
        self.db().batch(&statements)
    }

    pub fn artifacts(
        &self,
        run_id: &str,
        trial_id: &str,
    ) -> Result<Vec<ArtifactRow>, EvalStoreError> {
        let rows = self.db().query(
            "SELECT name, sha256, bytes FROM eval_trial_artifacts
             WHERE project_id = ?1 AND run_id = ?2 AND trial_id = ?3 ORDER BY name",
            &[self.p(), Value::text(run_id), Value::text(trial_id)],
        )?;
        rows.iter()
            .map(|r| {
                Ok(ArtifactRow {
                    name: r.text(0)?,
                    sha256: r.text(1)?,
                    bytes: r.int(2)?.max(0) as u64,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;

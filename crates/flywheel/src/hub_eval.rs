//! `fw eval` for the hub and the CLI alike: opening a project's eval store,
//! the runs list (`fw eval runs --json`, the Flywheel panel's Evals
//! section), a run's report, and one trial's detail -- one code path, so
//! the view and the verbs cannot disagree.

use std::path::{Path, PathBuf};

use flywheel_eval::set::SetDir;
use flywheel_eval::store::{EvalStore, TrialStatus};
use flywheel_orchestrator::policy::WorkPolicy;
use flywheel_orchestrator::workflow::store_partition;
use serde_json::{Value, json};

/// The most of a diff or log a trial view carries (bytes, the tail kept).
const TEXT_CAP: usize = 200_000;

/// The eval tables of the store `policy` selects (`db_path` for SQLite).
pub fn open_store(policy: Option<&WorkPolicy>, db_path: &Path) -> Result<EvalStore, String> {
    let partition = store_partition(policy);
    if policy.map(|p| p.store_backend.as_str()) == Some("postgres") {
        let dsn = policy
            .and_then(|p| p.store_dsn.as_ref())
            .map(|d| d.expose().to_owned())
            .or_else(|| {
                flywheel_orchestrator::store_factory::resolve_postgres_dsn(
                    &crate::tui::process_environ(),
                )
            })
            .ok_or("store backend is postgres but no DSN is configured")?;
        return EvalStore::connect_postgres(
            &dsn,
            flywheel_core::store_postgres::DEFAULT_SCHEMA,
            &partition,
        )
        .map_err(|e| e.to_string());
    }
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    EvalStore::open_sqlite(db_path, &partition).map_err(|e| e.to_string())
}

/// The project's evals dir, when the policy names a project.
pub fn evals_dir(policy: Option<&WorkPolicy>) -> Option<PathBuf> {
    policy
        .and_then(|p| p.project.as_ref())
        .map(|p| p.dir.join("evals"))
}

/// Every run, newest first: set, variants, progress, spend.
pub fn runs(store: &EvalStore) -> Result<Vec<Value>, String> {
    let mut rows = Vec::new();
    for r in store.runs().map_err(|e| e.to_string())? {
        let trials = store.trials(&r.run_id).map_err(|e| e.to_string())?;
        let variants = store.run_variants(&r.run_id).map_err(|e| e.to_string())?;
        let count =
            |pred: &dyn Fn(TrialStatus) -> bool| trials.iter().filter(|t| pred(t.status())).count();
        rows.push(json!({
            "run_id": r.run_id,
            "set": format!("{}@v{}", r.set_name, r.set_version),
            "k": r.k,
            "status": r.status.as_str(),
            "variants": variants.iter().map(|v| v.label.clone()).collect::<Vec<_>>(),
            "done": count(&|s| !matches!(s, TrialStatus::Pending | TrialStatus::Running)),
            "running": count(&|s| s == TrialStatus::Running),
            "infra": count(&|s| s == TrialStatus::InfraError),
            "total": trials.len(),
            "passed": trials.iter().filter(|t| t.status() == TrialStatus::Done && flywheel_eval::report::passed(t)).count(),
            "spent_usd": trials.iter().map(|t| t.cost_usd).sum::<f64>(),
            "budget_usd": r.budget_usd,
            "started_at": r.started_at,
            "ended_at": r.ended_at,
        }));
    }
    Ok(rows)
}

fn set_dir(evals: &Path, set_name: &str, version: u32) -> SetDir {
    if version == 0 {
        SetDir::draft(evals, set_name)
    } else {
        SetDir::frozen(evals, set_name, version)
    }
}

/// A run's report (`fw eval report --json`).
pub fn report(
    store: &EvalStore,
    evals: &Path,
    run_id: &str,
    filter: &flywheel_eval::report::Filter,
) -> Result<flywheel_eval::report::EvalReport, String> {
    let run = store.run(run_id).map_err(|e| e.to_string())?;
    let variants = store.run_variants(run_id).map_err(|e| e.to_string())?;
    let trials = store.trials(run_id).map_err(|e| e.to_string())?;
    let manifest = set_dir(evals, &run.set_name, run.set_version)
        .load()
        .map_err(|e| e.to_string())?;
    flywheel_eval::report::build(&run, &variants, &trials, &manifest, filter)
}

fn tail(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let start = bytes.len().saturating_sub(TEXT_CAP);
    let mut text = String::from_utf8_lossy(&bytes[start..]).into_owned();
    if start > 0 {
        text.insert_str(0, &format!("... ({start} bytes before this)\n"));
    }
    Some(text)
}

/// One trial: its row (trace parsed), per-check scores, review, the
/// trial record, and the tails of its diff and oracle log.
pub fn trial(
    store: &EvalStore,
    evals: &Path,
    run_id: &str,
    trial_id: &str,
) -> Result<Value, String> {
    let row = store.trial(run_id, trial_id).map_err(|e| e.to_string())?;
    let dir = evals
        .join("runs")
        .join(run_id)
        .join("trials")
        .join(trial_id);
    let checks: Vec<Value> = store
        .checks(run_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|c| c.trial_id == trial_id)
        .map(|c| json!({"source": c.source, "id": c.check_id, "passed": c.passed, "note": c.note}))
        .collect();
    let review = store
        .reviews(run_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|r| r.trial_id == trial_id)
        .map(|r| {
            json!({"verdict": r.verdict, "confidence": r.confidence, "notes": r.notes,
                   "evidence": serde_json::from_str::<Value>(&r.evidence_json).unwrap_or(Value::Null),
                   "decision": r.operator_decision, "model": r.reader_model})
        });
    let mut row_json = serde_json::to_value(&row).map_err(|e| e.to_string())?;
    row_json["trace"] = serde_json::from_str(&row.trace_json).unwrap_or(Value::Null);
    Ok(json!({
        "row": row_json,
        "checks": checks,
        "review": review,
        "record": std::fs::read_to_string(dir.join("trial.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()),
        "overlay": std::fs::read_to_string(dir.join("overlay.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()),
        "diff": tail(&dir.join("final.diff")),
        "oracle_log": tail(&dir.join("oracle.log")),
        "dir": dir.display().to_string(),
    }))
}

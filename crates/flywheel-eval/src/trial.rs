//! One trial: one set task, under one variant, from its historical base.
//!
//! 1. A base mirror (only the base and its past) is cloned into the
//!    trial's own working repository, with no remote.
//! 2. The variant's overlay is committed onto the base (with the hub's
//!    capture hooks, when the trial reports to a hub), and that commit
//!    becomes the local branch the worker cuts from and lands on.
//! 3. A fresh flywheel home holds only the task (its project-home token
//!    resolved to that home), its specs, and its held-out registration
//!    pointing at the run's materialized oracle (outside the home).
//! 4. `fw worker --task ID --base eval/trial` runs it through the real
//!    pipeline -- the variant's `fw`, model, setting sources, permission
//!    mode, and env -- under a wall-clock bound.
//! 5. The final commit (landed on the trial branch, else the parked work
//!    branch, else the base) is graded by the oracle in a clean checkout,
//!    out of band; the trial's store rows and transcripts are harvested.
//! 6. Claude Code's per-path state for every directory the trial used
//!    (`~/.claude/projects/<path key>/`: transcripts, auto-memory) is copied
//!    into the trial's artifacts and removed, so no trial writes into any
//!    real project's memory; the working directories are deleted, the
//!    final commit kept in the run's results repository.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use flywheel_core::store::{AttemptStore, LifecycleFilter, LifecycleStore};
use flywheel_core::store_sqlite::SqliteStore;
use serde::Serialize;

use crate::git;
use crate::mine::PROJECT_HOME_TOKEN;
use crate::mirror;
use crate::oracle::{self, GradeTree, Oracle, RunOptions, SCRUBBED_ENV};
use crate::proc::{self, Locking, Spec};
use crate::set::{SetDir, SetError, SetManifest, SetTask, file_sha256, files_under, io, pin_ref};
use crate::store::{ArtifactRow, CheckRow, TrialRow, TrialStatus};
use crate::variant::{ResolvedVariant, apply_overlay};

/// The branch a trial's worker cuts from and lands on.
pub const TRIAL_BRANCH: &str = "eval/trial";
/// The phase dir a trial's task is filed under.
pub const TRIAL_PHASE: &str = "eval";

/// Claude Code's per-directory state key: every character that is not an
/// ASCII letter or digit becomes `-`.
pub fn claude_project_key(path: &Path) -> String {
    path.display()
        .to_string()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Everything one trial needs (the file `fw eval trial --plan` reads).
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct TrialPlan {
    pub run_id: String,
    pub trial_id: String,
    pub set: SetDir,
    pub manifest: SetManifest,
    pub task: SetTask,
    pub variant: ResolvedVariant,
    /// The run's variant blob store.
    pub blobs: PathBuf,
    /// `evals/runs/<run>`.
    pub run_dir: PathBuf,
    /// `evals/cache`: mirrors and grading build caches.
    pub cache: PathBuf,
    /// The run's materialized oracle for this task.
    pub oracle: Oracle,
    pub worker_timeout_secs: u64,
    pub oracle_timeout_secs: u64,
    /// Turn-taking for the worker (the run's slot and world leases).
    pub worker_locking: Locking,
    /// Turn-taking for grading (the test-suite lock; exclusive world for an
    /// oracle that must run alone).
    pub grade_locking: Locking,
    /// The hub's capture hooks (settings JSON) when the trial reports to one.
    pub capture: Option<serde_json::Value>,
    /// The batch budget the hub shows beside the run (USD).
    pub batch_budget: Option<f64>,
    /// Claude Code's config dir (`~/.claude`), whose per-path state the
    /// trial removes.
    pub claude_dir: PathBuf,
}

/// What the trial left on disk and recorded.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TrialRecord {
    pub trial_id: String,
    pub task_id: String,
    pub variant_hash: String,
    pub base: String,
    pub overlay_commit: String,
    pub overlay_tree: String,
    pub overlay_effects: Vec<crate::variant::OpEffect>,
    pub final_commit: String,
    pub final_where: String,
    pub worker_exit: Option<i32>,
    pub worker_timed_out: bool,
    pub oracle: String,
    pub oracle_detail: String,
    pub flywheel_status: Option<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
}

/// A trial's outcome: the row to record, its per-check scores, its
/// artifacts, and where they are.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct TrialOutcome {
    pub row: TrialRow,
    pub checks: Vec<CheckRow>,
    pub artifacts: Vec<ArtifactRow>,
    pub dir: PathBuf,
}

fn infra(trial: &TrialPlan, row: &mut TrialRow, why: String) {
    row.status = Some(TrialStatus::InfraError);
    row.validity = Some("infra_error".into());
    row.validity_detail = Some(why);
    row.variant_hash.clone_from(&trial.variant.hash);
}

/// Run one trial. Machinery failures (not the agent's) come back as an
/// `InfraError` row, never an `Err`; `Err` is only for a trial that could
/// not record anything at all.
pub fn run_trial(
    plan: &TrialPlan,
    now: &dyn Fn() -> DateTime<Utc>,
) -> Result<TrialOutcome, SetError> {
    let dir = plan.run_dir.join("trials").join(&plan.trial_id);
    let work = dir.join("work");
    if work.exists() {
        std::fs::remove_dir_all(&work).map_err(io(&work))?;
    }
    std::fs::create_dir_all(&work).map_err(io(&work))?;
    let started = now();
    let mut row = TrialRow {
        trial_id: plan.trial_id.clone(),
        task_id: plan.task.id.clone(),
        variant_hash: plan.variant.hash.clone(),
        status: Some(TrialStatus::Done),
        trace_json: "{}".into(),
        started_at: Some(started),
        ..TrialRow::default()
    };
    let result = execute(plan, &dir, &work, &mut row, now);
    if let Err(e) = result {
        infra(plan, &mut row, e);
    }
    // Claude Code's per-path state for this trial's directories: kept as
    // artifacts, then removed.
    let keep = dir.join("claude-projects");
    let prefix = claude_project_key(&work);
    if let Ok(entries) = std::fs::read_dir(plan.claude_dir.join("projects")) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(&prefix) {
                for rel in files_under(&entry.path()).unwrap_or_default() {
                    let to = keep.join(&name).join(&rel);
                    if let Some(parent) = to.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    let _ = std::fs::copy(entry.path().join(&rel), &to);
                }
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
    if work.exists() {
        let _ = std::fs::remove_dir_all(&work);
    }
    row.ended_at = Some(now());
    row.wall_secs = (row.ended_at.expect("set") - started).num_milliseconds() as f64 / 1000.0;
    let mut artifacts = Vec::new();
    for rel in files_under(&dir)? {
        let path = dir.join(&rel);
        artifacts.push(ArtifactRow {
            name: rel.display().to_string(),
            sha256: file_sha256(&path)?,
            bytes: std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
        });
    }
    let checks = std::fs::read_to_string(dir.join("checks.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Vec<oracle::CheckScore>>(&t).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|c| CheckRow {
            trial_id: plan.trial_id.clone(),
            source: "oracle".into(),
            check_id: c.id,
            passed: c.passed,
            note: c.note,
        })
        .collect();
    Ok(TrialOutcome {
        row,
        checks,
        artifacts,
        dir,
    })
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    crate::set::write_json(path, value).map_err(|e| e.to_string())
}

fn fw(
    plan: &TrialPlan,
    cwd: &Path,
    home: &Path,
    args: &[&str],
    log: &Path,
) -> Result<proc::Ran, String> {
    let mut argv = vec![plan.variant.fw_path.display().to_string()];
    argv.extend(args.iter().map(|a| (*a).to_owned()));
    proc::run(&Spec {
        argv,
        cwd: Some(cwd),
        env: vec![("FLYWHEEL_HOME".into(), home.display().to_string())],
        unset: SCRUBBED_ENV.iter().map(|s| (*s).to_owned()).collect(),
        timeout: Some(Duration::from_secs(120)),
        log: Some(log),
        ..Spec::default()
    })
    .map_err(|e| format!("fw {}: {e}", args.join(" ")))
}

fn execute(
    plan: &TrialPlan,
    dir: &Path,
    work: &Path,
    row: &mut TrialRow,
    now: &dyn Fn() -> DateTime<Utc>,
) -> Result<(), String> {
    let task = &plan.task;
    let repo = &plan.manifest.repo;
    if claude_project_key(work) == claude_project_key(repo) {
        return Err("the trial's directory would share Claude Code state with the repo".into());
    }
    // 1. Mirror and clone.
    let mirror_dir = plan.cache.join("mirrors").join(format!(
        "{}-{}",
        task.id,
        &task.base[..12.min(task.base.len())]
    ));
    let pin = plan
        .manifest
        .version
        .map(|v| pin_ref(&plan.manifest.name, v, &task.id, "base"));
    mirror::base_mirror(repo, &task.base, pin.as_deref(), &mirror_dir)
        .map_err(|e| e.to_string())?;
    if !mirror::future_free(&mirror_dir, &task.base).map_err(|e| e.to_string())? {
        return Err("the base mirror reaches past the base".into());
    }
    let clone = work.join("repo");
    mirror::trial_clone(&mirror_dir, &clone).map_err(|e| e.to_string())?;
    for (k, v) in [
        ("user.name", "flywheel eval"),
        ("user.email", "eval@flywheel.invalid"),
        ("commit.gpgsign", "false"),
    ] {
        git::run(&clone, &["config", k, v]).map_err(|e| e.to_string())?;
    }

    // 2. The variant's overlay, on its own branch.
    let overlay = apply_overlay(&plan.variant, &plan.blobs, &clone, plan.capture.as_ref())
        .map_err(|e| e.to_string())?;
    row.overlay_hash = Some(overlay.tree.clone());
    git::run(&clone, &["branch", "-f", TRIAL_BRANCH, &overlay.commit])
        .map_err(|e| e.to_string())?;
    git::run(
        &clone,
        &["checkout", "--quiet", "-B", "eval/idle", &overlay.commit],
    )
    .map_err(|e| e.to_string())?;
    // The operator's default branch names the base, as it did when the
    // task first ran: an oracle the in-trial landing gate runs may measure
    // against it (out-of-band grading pins the same in its tree).
    let default_branch = git::default_branch(&plan.manifest.repo);
    if default_branch != TRIAL_BRANCH && default_branch != "eval/idle" {
        git::pin_branch(&clone, &default_branch, &task.base).map_err(|e| e.to_string())?;
    }
    write_json(&dir.join("overlay.json"), &overlay)?;

    // 3. The trial's flywheel home.
    let home = work.join("home");
    let setup_log = dir.join("setup.log");
    let path_ran = fw(plan, &clone, &home, &["project", "path"], &setup_log)?;
    if !path_ran.passed() {
        return Err(format!("fw project path: {}", path_ran.describe()));
    }
    let pid = git::run(&clone, &["config", "--get", "flywheel.projectid"])
        .map_err(|e| format!("the trial repo has no project id: {e}"))?;
    let project_dir = home.join("projects").join(&pid);
    // A repo file (`.flywheel.toml`: its standing verify, protected
    // paths) is trusted as the operator's own repo would be.
    if clone.join(".flywheel.toml").is_file() {
        let trusted = fw(plan, &clone, &home, &["trust"], &setup_log)?;
        if !trusted.passed() {
            return Err(format!("fw trust: {}", trusted.describe()));
        }
    }
    let token_home = project_dir.display().to_string();
    let task_text = std::fs::read_to_string(plan.set.task_file(&task.id))
        .map_err(|e| format!("task file: {e}"))?
        .replace(PROJECT_HOME_TOKEN, &token_home);
    let task_file = project_dir
        .join("tasks/active")
        .join(TRIAL_PHASE)
        .join(format!("{}.json", task.id));
    std::fs::create_dir_all(task_file.parent().expect("has parent")).map_err(|e| e.to_string())?;
    std::fs::write(&task_file, &task_text).map_err(|e| e.to_string())?;
    let specs = plan.set.spec_dir(&task.id);
    for rel in files_under(&specs).map_err(|e| e.to_string())? {
        let to = project_dir.join(&rel);
        std::fs::create_dir_all(to.parent().expect("has parent")).map_err(|e| e.to_string())?;
        std::fs::copy(specs.join(&rel), &to).map_err(|e| e.to_string())?;
    }
    let registration = serde_json::json!({"graders": plan.oracle.commands.iter().map(|(name, run)| {
        serde_json::json!({"type": "command", "run": run, "name": name.clone().unwrap_or_else(|| "held-out-oracle".into())})
    }).collect::<Vec<_>>()});
    let held_out = project_dir.join("held-out");
    std::fs::create_dir_all(&held_out).map_err(|e| e.to_string())?;
    std::fs::write(
        held_out.join(format!("{}.json", task.id)),
        registration.to_string(),
    )
    .map_err(|e| e.to_string())?;

    // 4. The worker.
    let mut argv = vec![
        plan.variant.fw_path.display().to_string(),
        "worker".into(),
        "--task".into(),
        task.id.clone(),
        "--base".into(),
        TRIAL_BRANCH.into(),
        "--worker-id".into(),
        format!("eval-{}", plan.trial_id),
    ];
    if let Some(model) = &plan.variant.model {
        argv.extend(["--model".into(), model.clone()]);
    }
    if plan.variant.is_claude() {
        argv.extend([
            "--setting-sources".into(),
            plan.variant.setting_sources.join(","),
            "--permission-mode".into(),
            plan.variant.permission_mode.clone(),
        ]);
    } else {
        argv.extend(["--agent".into(), "codex".into()]);
    }
    let mut env = vec![
        ("FLYWHEEL_HOME".to_owned(), home.display().to_string()),
        (
            "FLYWHEEL_BATCH".to_owned(),
            format!("eval:{}:{}", plan.run_id, plan.trial_id),
        ),
    ];
    if let Some(budget) = plan.batch_budget {
        env.push(("FLYWHEEL_BATCH_BUDGET".into(), format!("{budget:.2}")));
    }
    env.extend(plan.variant.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    let ran = proc::run(&Spec {
        argv,
        cwd: Some(&clone),
        env,
        unset: SCRUBBED_ENV.iter().map(|s| (*s).to_owned()).collect(),
        timeout: Some(Duration::from_secs(plan.worker_timeout_secs)),
        log: Some(&dir.join("worker.log")),
        locking: plan.worker_locking.clone(),
    })
    .map_err(|e| format!("cannot start the worker: {e}"))?;

    // 5. Harvest the trial store.
    let db = project_dir.join("flywheel.sqlite");
    let mut flywheel_status = None;
    if db.is_file() {
        let store = SqliteStore::open_partition(&db, &pid).map_err(|e| e.to_string())?;
        let lifecycles = store
            .list_lifecycles(&LifecycleFilter::default())
            .map_err(|e| e.to_string())?;
        let mut attempts_json = Vec::new();
        for lc in lifecycles.iter().filter(|l| l.task_id == task.id) {
            flywheel_status = Some(lc.status.as_str().to_owned());
            for a in store.list_attempts(&lc.run_id).map_err(|e| e.to_string())? {
                row.attempts += 1;
                row.cost_usd += a.total_cost_usd;
                row.turns += u32::try_from(a.turns).unwrap_or(u32::MAX);
                row.input_tokens += a.input_tokens;
                row.output_tokens += a.output_tokens;
                row.cache_read += a.cache_read_input_tokens;
                row.cache_write += a.cache_creation_input_tokens;
                if let Some(end) = a.ended_at {
                    row.agent_secs += (end - a.started_at).num_milliseconds() as f64 / 1000.0;
                }
                attempts_json.push(serde_json::json!({
                    "run_id": lc.run_id, "number": a.number, "outcome": a.outcome.map(|o| format!("{o:?}")),
                    "error": a.error, "turns": a.turns, "cost_usd": a.total_cost_usd,
                    "input_tokens": a.input_tokens, "output_tokens": a.output_tokens,
                    "cache_read": a.cache_read_input_tokens, "cache_write": a.cache_creation_input_tokens,
                    "started_at": a.started_at, "ended_at": a.ended_at, "agent_context": a.agent_context,
                }));
            }
        }
        write_json(&dir.join("attempts.json"), &attempts_json)?;
    }
    if ran.code == Some(2) && flywheel_status.is_none() {
        return Err(format!(
            "the worker refused to start ({}); see worker.log",
            ran.describe()
        ));
    }
    row.flywheel_status.clone_from(&flywheel_status);
    let transcripts = project_dir.join("transcripts");
    for rel in files_under(&transcripts).map_err(|e| e.to_string())? {
        let to = dir.join("transcripts").join(&rel);
        std::fs::create_dir_all(to.parent().expect("has parent")).map_err(|e| e.to_string())?;
        std::fs::copy(transcripts.join(&rel), &to).map_err(|e| e.to_string())?;
    }

    // What the agent did, from the harness's transcript.
    let trace =
        crate::trace::read_dir(&dir.join("transcripts"), &trace_bounds(plan)).unwrap_or_default();
    row.trace_json = serde_json::to_string(&trace).map_err(|e| e.to_string())?;
    if trace.contaminated() {
        row.validity = Some("contaminated".into());
        row.validity_detail = Some(format!(
            "read outside the trial: {}",
            trace.reads_outside_trial.join(" | ")
        ));
    } else {
        row.validity = Some("valid".into());
    }

    // The final commit: landed, else the parked work branch, else the base.
    let landed = git::rev_parse(&clone, TRIAL_BRANCH).map_err(|e| e.to_string())?;
    let work_branch = format!("flywheel/{TRIAL_PHASE}/{}", task.id);
    let parked = git::rev_parse(&clone, &work_branch).ok();
    let (final_commit, final_where) = if landed != overlay.commit {
        (landed, "landed")
    } else if let Some(p) = parked.filter(|p| *p != overlay.commit) {
        (p, "parked_branch")
    } else {
        (overlay.commit.clone(), "no_commit")
    };
    row.final_commit = Some(final_commit.clone());
    row.final_where = Some(final_where.into());
    let diff = git::run(&clone, &["diff", &overlay.commit, &final_commit]).unwrap_or_default();
    std::fs::write(dir.join("final.diff"), diff).map_err(|e| e.to_string())?;

    // Keep the final commit in the run's results repository (trials of a
    // run finish in parallel processes: one at a time here).
    {
        let results = plan.run_dir.join("results.git");
        let _results = crate::proc::FileLock::exclusive(&plan.run_dir.join("results.lock"))
            .map_err(|e| format!("the results lock: {e}"))?;
        if !results.join("HEAD").exists() {
            std::fs::create_dir_all(&results).map_err(|e| e.to_string())?;
            git::run(&results, &["init", "--quiet", "--bare"]).map_err(|e| e.to_string())?;
        }
        git::run(
            &clone,
            &[
                "push",
                "--quiet",
                "--force",
                &results.display().to_string(),
                &format!("{final_commit}:refs/trials/{}", plan.trial_id),
            ],
        )
        .map_err(|e| e.to_string())?;
    }

    // 6. Grade the final commit out of band, in a clean checkout: the
    // task's one grading tree, moved to this commit under the task's grading
    // lock (trials of a task grade one at a time there). One tree per
    // build cache is what keeps the cache honest: cargo keys a workspace's
    // artifacts by paths relative to its root and judges freshness by
    // mtime, so two trees at different commits sharing one target dir can
    // each take the other's stale build for their own.
    let grade_root = plan.cache.join("grade").join(&task.id);
    std::fs::create_dir_all(&grade_root).map_err(|e| e.to_string())?;
    let _grading = crate::proc::FileLock::exclusive(&grade_root.join("lock"))
        .map_err(|e| format!("the grading lock: {e}"))?;
    let tree = GradeTree::open(&clone, &grade_root.join("tree"))
        .map_err(|e| e.to_string())?
        .with_base(&git::default_branch(&plan.manifest.repo), &task.base);
    tree.checkout_from(&clone, &final_commit)
        .map_err(|e| e.to_string())?;
    let verdict = oracle::run(
        &plan.oracle,
        &tree.path,
        &RunOptions {
            target_dir: Some(grade_root.join("target")),
            timeout: Some(Duration::from_secs(plan.oracle_timeout_secs)),
            locking: plan.grade_locking.clone(),
            log: Some(dir.join("oracle.log")),
            env: Vec::new(),
        },
    )
    .map_err(|e| format!("cannot run the oracle: {e}"))?;
    if verdict.ran.timed_out || (verdict.ran.code.is_none() && !verdict.passed) {
        return Err(format!("the oracle did not finish: {}", verdict.detail));
    }
    let mut passed = verdict.passed;
    let mut detail = verdict.detail.clone();
    let mut checks: Vec<serde_json::Value> = verdict
        .checks
        .iter()
        .map(|c| serde_json::json!({"id": c.id, "passed": c.passed, "note": c.note}))
        .collect();
    // A don't-act task also needs evidence the agent looked, read from the
    // harness's transcript (which the agent cannot write).
    if task.kind == crate::set::TaskKind::DontAct {
        let (looked, why) = crate::dont_act::inspected(
            &dir.join("transcripts"),
            &crate::dont_act::relevant_paths(&task_text),
        )
        .unwrap_or((false, "no transcript".into()));
        checks.push(serde_json::json!({"id": "inspected", "passed": looked, "note": why}));
        if passed && !looked {
            passed = false;
            detail = format!("no change, but {why}");
        }
    }
    row.oracle = Some(if passed { "pass" } else { "fail" }.into());
    write_json(&dir.join("checks.json"), &checks)?;

    let record = TrialRecord {
        trial_id: plan.trial_id.clone(),
        task_id: task.id.clone(),
        variant_hash: plan.variant.hash.clone(),
        base: task.base.clone(),
        overlay_commit: overlay.commit.clone(),
        overlay_tree: overlay.tree.clone(),
        overlay_effects: overlay.effects.clone(),
        final_commit,
        final_where: final_where.into(),
        worker_exit: ran.code,
        worker_timed_out: ran.timed_out,
        oracle: row.oracle.clone().unwrap_or_default(),
        oracle_detail: detail,
        flywheel_status,
        started_at: row.started_at.unwrap_or_else(now),
        ended_at: now(),
    };
    write_json(&dir.join("trial.json"), &record)?;
    Ok(())
}

/// What a trial's agent must not name: the operator's project home
/// (held-out registrations, oracles, specs, tasks, eval sets and blobs)
/// and the run's materialized oracles. Its own work dir lies under the
/// project home too, so the forbidden paths are those subdirectories,
/// never the whole home.
pub fn trace_bounds(plan: &TrialPlan) -> crate::trace::Bounds {
    let mut forbidden = vec![plan.run_dir.join("oracles").display().to_string()];
    // run_dir = <project home>/evals/runs/<run>
    if let Some(project) = plan
        .run_dir
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
    {
        for sub in [
            "held-out",
            "verification",
            "specs",
            "tasks",
            "evals/sets",
            "evals/blobs",
            "flywheel.sqlite",
        ] {
            forbidden.push(project.join(sub).display().to_string());
        }
    }
    crate::trace::Bounds { forbidden }
}

/// The per-run oracle dir for a task (materialized once per run).
pub fn run_oracle_dir(run_dir: &Path, task: &str) -> PathBuf {
    run_dir.join("oracles").join(task)
}

/// Materialize every task's oracle for a run.
pub fn materialize_oracles(
    set: &SetDir,
    manifest: &SetManifest,
    run_dir: &Path,
) -> Result<BTreeMap<String, Oracle>, SetError> {
    let mut out = BTreeMap::new();
    for task in &manifest.tasks {
        let dest = run_oracle_dir(run_dir, &task.id);
        out.insert(task.id.clone(), oracle::materialize(set, &task.id, &dest)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests;

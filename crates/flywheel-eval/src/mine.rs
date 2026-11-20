//! Mine eval-set candidates from a project's past work, and write them as a
//! draft set.
//!
//! Only tasks with a held-out oracle registration qualify (the oracle is
//! what judges a trial). Each candidate's source is decided from the
//! store alone:
//!
//! | source | when |
//! | --- | --- |
//! | `landed` | a run of the task landed (a `landed` event with a merge sha) |
//! | `hard-parked` | the held-out gate parked one of its runs, and the work that finally landed came from a later attempt or run (a park the same attempt later re-landed is the oracle or the machine flaking, not hardness) |
//! | `hard-retried` | the landed run needed more than one attempt, an earlier one failing validation |
//! | `hard-failed` | no run landed and one failed or was interrupted: its reference is found by bisect at validation |
//!
//! Standing-verify parks never make a task hard (the efficiency baseline:
//! most re-landed with no code change -- flaky suites, not hard work).
//!
//! The base of a landed task is the parent of the oldest commit in the
//! landed commit's first-parent chain carrying the run's `Flywheel-Run`
//! trailer (the commits the worker made for it), not the attempt's recorded
//! `base_commit_sha`, which can predate a prerequisite's landing (the noise
//! floor's first surprise).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use flywheel_core::events::{DomainEventPayload, PARK_KIND_HELD_OUT_GATE};
use flywheel_core::lifecycle::{Lifecycle, Outcome, Status};
use flywheel_core::store::{HarnessStore, LifecycleFilter};
use flywheel_orchestrator::home::ProjectPaths;
use serde_json::Value;

use crate::git;
use crate::set::{
    Provenance, SetDir, SetError, SetManifest, SetTask, Source, Split, TaskKind, Tier, io,
    write_json,
};

/// The token a set's files carry in place of the project home's absolute
/// path; a trial or a grading substitutes its own location.
pub const PROJECT_HOME_TOKEN: &str = "@EVAL_PROJECT_HOME@";

/// Which candidate kinds to mine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MineOptions {
    pub landed: bool,
    pub hard: bool,
    /// Only runs started at or after this.
    pub since: Option<DateTime<Utc>>,
    /// At most this many candidates (hard ones first).
    pub max: Option<usize>,
}

impl Default for MineOptions {
    fn default() -> Self {
        Self {
            landed: true,
            hard: true,
            since: None,
            max: None,
        }
    }
}

/// A task the miner proposes.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub task: SetTask,
    /// The task definition file it came from.
    pub task_file: PathBuf,
    /// Why this source.
    pub reason: String,
}

/// A task the miner passed over, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub task_id: String,
    pub reason: String,
}

#[derive(Debug, thiserror::Error)]
pub enum MineError {
    #[error(transparent)]
    Store(#[from] flywheel_core::store::StoreError),
    #[error(transparent)]
    Set(#[from] SetError),
    #[error("{0}")]
    Invalid(String),
}

/// The definition file of `task_id` under the project's tasks dir
/// (`active/`, `archive/`, or `staged/`, any phase).
pub fn find_task_file(tasks_dir: &Path, task_id: &str) -> Option<PathBuf> {
    for bucket in ["active", "archive", "staged"] {
        let Ok(phases) = std::fs::read_dir(tasks_dir.join(bucket)) else {
            continue;
        };
        let mut phases: Vec<PathBuf> = phases.flatten().map(|e| e.path()).collect();
        phases.sort();
        for phase in phases {
            let file = phase.join(format!("{task_id}.json"));
            if file.is_file() {
                return Some(file);
            }
        }
    }
    None
}

/// A task file's `prerequisites`, each with the commit it landed as
/// (empty when it never landed).
fn prerequisites_of(
    task_file: &Path,
    landed: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    std::fs::read_to_string(task_file)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("prerequisites").cloned())
        .and_then(|p| serde_json::from_value::<Vec<String>>(p).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|id| {
            let sha = landed.get(&id).cloned().unwrap_or_default();
            (id, sha)
        })
        .collect()
}

fn phase_of(task_file: &Path) -> Option<String> {
    task_file
        .parent()
        .and_then(Path::file_name)
        .and_then(|n| n.to_str())
        .map(str::to_owned)
}

/// The commit a landed run's work starts from: the parent of the oldest
/// commit, on `landed`'s first-parent chain, whose message names `run_id`
/// in a `Flywheel-Run:` trailer (or `task_id` in `Flywheel-Task:`). `None`
/// when `landed` itself carries neither.
pub fn work_base(repo: &Path, landed: &str, run_id: &str, task_id: &str) -> Option<String> {
    let log = git::run(
        repo,
        &[
            "log",
            "--first-parent",
            "--format=%H%x00%P%x00%B%x1e",
            "-n",
            "200",
            landed,
        ],
    )
    .ok()?;
    let mut base = None;
    for record in log.split('\u{1e}') {
        let record = record.trim_start_matches('\n');
        if record.is_empty() {
            continue;
        }
        let mut fields = record.splitn(3, '\0');
        let (Some(_sha), Some(parents), Some(body)) = (fields.next(), fields.next(), fields.next())
        else {
            break;
        };
        let ours = body.lines().any(|l| {
            let l = l.trim();
            l == format!("Flywheel-Run: {run_id}") || l == format!("Flywheel-Task: {task_id}")
        });
        if !ours {
            break;
        }
        base = parents.split_whitespace().next().map(str::to_owned);
    }
    base
}

#[derive(Debug, Default)]
struct RunFacts {
    lifecycle: Option<Lifecycle>,
    started: Option<DateTime<Utc>>,
    landed_ref: Option<String>,
    landed_attempt: Option<u32>,
    held_out_parks: Vec<u32>,
    attempts: u32,
    failed_attempts: u32,
    cost: f64,
    base_commit: Option<String>,
}

fn facts(store: &dyn HarnessStore, lifecycle: &Lifecycle) -> Result<RunFacts, MineError> {
    let mut f = RunFacts {
        lifecycle: Some(lifecycle.clone()),
        ..RunFacts::default()
    };
    let attempts = store.list_attempts(&lifecycle.run_id)?;
    f.attempts = u32::try_from(attempts.len()).unwrap_or(u32::MAX);
    for a in &attempts {
        f.cost += a.total_cost_usd;
        if f.started.is_none_or(|s| a.started_at < s) {
            f.started = Some(a.started_at);
        }
        if a.number == 1 {
            f.base_commit = a.agent_context.get("base_commit_sha").cloned();
        }
        if matches!(a.outcome, Some(Outcome::ValidationFailed)) {
            f.failed_attempts += 1;
        }
    }
    // Landing events carry no attempt number: each belongs to the latest
    // attempt started before it.
    let attempt_at = |ts: DateTime<Utc>| {
        attempts
            .iter()
            .filter(|a| a.started_at <= ts)
            .map(|a| a.number)
            .max()
            .unwrap_or(0)
    };
    for event in store.list_domain_events(&lifecycle.run_id)? {
        let attempt = event.attempt_number.unwrap_or_else(|| attempt_at(event.ts));
        match &event.payload {
            DomainEventPayload::Landed(landed) if landed.strategy == "merge" => {
                f.landed_ref = Some(landed.landed_ref.clone());
                f.landed_attempt = Some(attempt);
            }
            DomainEventPayload::LandingParked(p) if p.park_kind == PARK_KIND_HELD_OUT_GATE => {
                f.held_out_parks.push(attempt);
            }
            _ => {}
        }
    }
    Ok(f)
}

/// Mine `store`'s runs into candidates. `paths` is the project home (for
/// task files and held-out registrations); `repo` the repository the
/// commits live in.
pub fn mine(
    store: &dyn HarnessStore,
    paths: &ProjectPaths,
    repo: &Path,
    options: &MineOptions,
) -> Result<(Vec<Candidate>, Vec<Skipped>), MineError> {
    let lifecycles = store.list_lifecycles(&LifecycleFilter::default())?;
    let mut by_task: BTreeMap<String, Vec<RunFacts>> = BTreeMap::new();
    for lifecycle in &lifecycles {
        let f = facts(store, lifecycle)?;
        if let (Some(since), Some(started)) = (options.since, f.started)
            && started < since
        {
            continue;
        }
        by_task
            .entry(lifecycle.task_id.clone())
            .or_default()
            .push(f);
    }
    let landed_by_task: BTreeMap<String, String> = by_task
        .iter()
        .filter_map(|(id, runs)| {
            runs.iter()
                .filter_map(|r| r.landed_ref.clone())
                .rfind(|r| git::is_full_sha(r))
                .map(|r| (id.clone(), r))
        })
        .collect();
    let mut candidates = Vec::new();
    let mut skipped = Vec::new();
    let skip = |skipped: &mut Vec<Skipped>, id: &str, reason: String| {
        skipped.push(Skipped {
            task_id: id.to_owned(),
            reason,
        });
    };
    for (task_id, mut runs) in by_task {
        runs.sort_by_key(|r| r.started);
        if !paths.held_out().join(format!("{task_id}.json")).is_file() {
            skip(
                &mut skipped,
                &task_id,
                "no held-out oracle registration".into(),
            );
            continue;
        }
        let Some(task_file) = find_task_file(&paths.tasks(), &task_id) else {
            skip(&mut skipped, &task_id, "no task definition file".into());
            continue;
        };
        let cost: Vec<f64> = runs.iter().map(|r| r.cost).filter(|c| *c > 0.0).collect();
        let prior = cost.last().copied();
        let landed_index = runs
            .iter()
            .position(|r| r.landed_ref.as_deref().is_some_and(git::is_full_sha));
        let provenance = |run: &RunFacts| Provenance {
            run_id: run.lifecycle.as_ref().map(|l| l.run_id.clone()),
            phase: phase_of(&task_file),
            ..Provenance::default()
        };
        if let Some(i) = landed_index {
            let run = &runs[i];
            let landed = run.landed_ref.clone().expect("checked");
            if git::rev_parse(repo, &landed).is_err() {
                skip(
                    &mut skipped,
                    &task_id,
                    format!("landed commit {landed} not in the repo"),
                );
                continue;
            }
            let run_id = run
                .lifecycle
                .as_ref()
                .map(|l| l.run_id.as_str())
                .unwrap_or("");
            let (base, base_note) = match work_base(repo, &landed, run_id, &task_id) {
                Some(base) => (base, "the parent of the run's own commits"),
                None => match git::rev_parse(repo, &format!("{landed}^")) {
                    Ok(parent) => (
                        parent,
                        "the landed commit's parent (no Flywheel-Run trailer)",
                    ),
                    Err(_) => {
                        skip(
                            &mut skipped,
                            &task_id,
                            "the landed commit has no parent".into(),
                        );
                        continue;
                    }
                },
            };
            // Hard: a held-out park whose work did not land as it was (a
            // later attempt or a later run landed instead).
            let landed_attempt = run.landed_attempt.unwrap_or(run.attempts);
            let parked_then_redone = runs[..=i].iter().enumerate().any(|(j, r)| {
                r.held_out_parks
                    .iter()
                    .any(|a| j < i || *a < landed_attempt)
            });
            let parked_then_relanded = run.held_out_parks.contains(&landed_attempt);
            let (source, reason) = if parked_then_redone {
                (
                    Source::HardParked,
                    "the held-out gate parked it; later work landed".to_owned(),
                )
            } else if run.failed_attempts > 0 && run.attempts > 1 {
                (
                    Source::HardRetried,
                    format!(
                        "{} attempts, {} failed validation",
                        run.attempts, run.failed_attempts
                    ),
                )
            } else if parked_then_relanded {
                (
                    Source::Landed,
                    "landed; an earlier held-out park of the same work re-landed unchanged \
                     (oracle or machine flake, not hardness)"
                        .to_owned(),
                )
            } else {
                (Source::Landed, "landed".to_owned())
            };
            if (source.is_hard() && !options.hard) || (!source.is_hard() && !options.landed) {
                continue;
            }
            candidates.push(Candidate {
                task: SetTask {
                    id: task_id.clone(),
                    source,
                    tier: Tier::Uncalibrated,
                    split: Split::Dev,
                    kind: TaskKind::Build,
                    base,
                    reference: landed,
                    files_digest: String::new(),
                    parallel_safe: None,
                    prior_cost_usd: prior,
                    oracle_secs: None,
                    prerequisites: prerequisites_of(&task_file, &landed_by_task),
                    provenance: provenance(run),
                },
                task_file: task_file.clone(),
                reason: format!("{reason}; base is {base_note}"),
            });
        } else if options.hard {
            let failed = runs.iter().find(|r| {
                r.lifecycle
                    .as_ref()
                    .is_some_and(|l| matches!(l.status, Status::Failed | Status::Interrupted))
            });
            let Some(run) = failed else {
                skip(
                    &mut skipped,
                    &task_id,
                    "never landed and never failed".into(),
                );
                continue;
            };
            let Some(base) = run.base_commit.clone().filter(|b| git::is_full_sha(b)) else {
                skip(
                    &mut skipped,
                    &task_id,
                    "failed run recorded no base commit".into(),
                );
                continue;
            };
            candidates.push(Candidate {
                task: SetTask {
                    id: task_id.clone(),
                    source: Source::HardFailed,
                    tier: Tier::Uncalibrated,
                    split: Split::Dev,
                    kind: TaskKind::Build,
                    base,
                    reference: String::new(),
                    files_digest: String::new(),
                    parallel_safe: None,
                    prior_cost_usd: prior,
                    oracle_secs: None,
                    prerequisites: prerequisites_of(&task_file, &landed_by_task),
                    provenance: provenance(run),
                },
                task_file: task_file.clone(),
                reason: "its run failed; the reference is found by bisect at validation".into(),
            });
        } else {
            skip(&mut skipped, &task_id, "never landed".into());
        }
    }
    // Hard ones first, then by id.
    candidates.sort_by(|a, b| {
        b.task
            .source
            .is_hard()
            .cmp(&a.task.source.is_hard())
            .then_with(|| a.task.id.cmp(&b.task.id))
    });
    if let Some(max) = options.max {
        candidates.truncate(max);
    }
    Ok((candidates, skipped))
}

// --- Writing the draft ----------------------------------------------------------------

/// Replace every occurrence of the project home's path with the token.
fn tokenize(text: &str, home: &str) -> String {
    text.replace(home.trim_end_matches('/'), PROJECT_HOME_TOKEN)
}

/// Every absolute path under `home` a text mentions (up to a quote,
/// whitespace, or a closing bracket).
fn home_paths(text: &str, home: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut rest = text;
    while let Some(i) = rest.find(home) {
        let tail = &rest[i..];
        let end = tail
            .find(|c: char| c.is_whitespace() || "\"'`)]}>,;".contains(c))
            .unwrap_or(tail.len());
        out.insert(tail[..end].trim_end_matches(['.', ':']).to_owned());
        rest = &tail[end..];
    }
    out
}

/// The project home prefix every path in it starts with (`.../<id>/`).
fn home_prefix(paths: &ProjectPaths) -> String {
    format!("{}/", paths.root().display())
}

/// Copy `rel` (a file under the project home) to `dest_root/rel`; a
/// directory copies its files (not recursing into `held-out/`).
fn copy_home_file(home: &Path, rel: &str, dest_root: &Path) -> Result<(), SetError> {
    let from = home.join(rel);
    let to = dest_root.join(rel);
    if from.is_file() {
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(io(parent))?;
        }
        let text = std::fs::read(&from).map_err(io(&from))?;
        std::fs::write(&to, text).map_err(io(&to))?;
    }
    Ok(())
}

/// The task as the set keeps it: `prerequisites` dropped (the base already
/// contains them), every project-home path tokenized.
pub fn set_task_json(raw: &str, home: &str) -> Result<String, MineError> {
    let mut value: Value =
        serde_json::from_str(raw).map_err(|e| MineError::Invalid(format!("task json: {e}")))?;
    if let Some(obj) = value.as_object_mut() {
        obj.remove("prerequisites");
    }
    let text = serde_json::to_string_pretty(&value)
        .map_err(|e| MineError::Invalid(format!("task json: {e}")))?;
    Ok(tokenize(&text, home))
}

/// Every file under `dir`, symlinks followed (a symlinked node_modules is
/// part of an oracle's package), as sorted relative paths; at most 64
/// levels deep, so a symlink cycle ends.
fn files_following(dir: &Path) -> Result<Vec<PathBuf>, SetError> {
    fn walk(root: &Path, at: &Path, depth: u32, out: &mut Vec<PathBuf>) -> Result<(), SetError> {
        if depth > 64 {
            return Ok(());
        }
        for entry in std::fs::read_dir(at).map_err(io(at))?.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::metadata(&path) else {
                continue; // a dangling symlink
            };
            if meta.is_dir() {
                walk(root, &path, depth + 1, out)?;
            } else if meta.is_file() {
                out.push(
                    path.strip_prefix(root)
                        .expect("walked under root")
                        .to_owned(),
                );
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, dir, 0, &mut out)?;
    out.sort();
    Ok(out)
}

/// Write one candidate's files into the draft: the task (tokenized), the
/// spec files it names under the project home (not `held-out/` or
/// `verification/`, which an agent must not see), and the oracle: its
/// registration and every project-home file the registration's commands
/// or the oracle files name, copied under `oracles/<id>/home/`.
pub fn write_candidate(dir: &SetDir, paths: &ProjectPaths, c: &Candidate) -> Result<(), MineError> {
    let home = home_prefix(paths);
    let id = &c.task.id;
    let raw = std::fs::read_to_string(&c.task_file).map_err(io(&c.task_file))?;
    std::fs::create_dir_all(dir.root().join("tasks")).map_err(io(dir.root()))?;
    std::fs::write(
        dir.task_file(id),
        format!("{}\n", set_task_json(&raw, &home)?),
    )
    .map_err(io(&dir.task_file(id)))?;
    for path in home_paths(&raw, &home) {
        let rel = path.trim_start_matches(&home);
        if rel.starts_with("held-out") || rel.starts_with("verification") || rel.is_empty() {
            continue;
        }
        copy_home_file(paths.root(), rel, &dir.spec_dir(id))?;
    }
    let registration = paths.held_out().join(format!("{id}.json"));
    let reg_text = std::fs::read_to_string(&registration).map_err(io(&registration))?;
    let oracle_home = dir.oracle_dir(id).join("home");
    // The registration, then every file it reaches: each file a path names
    // (and every project-home path inside those files), and the whole
    // directory of each oracle under verification/ -- an oracle's directory
    // is its package (helpers it imports, its node_modules), symlinks
    // followed.
    let mut queue: Vec<(String, bool)> = home_paths(&reg_text, &home)
        .into_iter()
        .map(|p| (p, true))
        .collect();
    let mut seen = BTreeSet::new();
    let mut packaged = BTreeSet::new();
    while let Some((path, named)) = queue.pop() {
        if !seen.insert(path.clone()) {
            continue;
        }
        let rel = path.trim_start_matches(&home).to_owned();
        let from = paths.root().join(&rel);
        if from.is_dir() {
            for file in files_following(&from)? {
                queue.push((
                    format!("{path}/{}", file.display()).replace("//", "/"),
                    false,
                ));
            }
            continue;
        }
        if !from.is_file() {
            continue;
        }
        if named
            && let Some(parent) = from.parent()
            && parent != paths.root()
            && parent.starts_with(paths.root().join("verification"))
            && packaged.insert(parent.to_owned())
        {
            for file in files_following(parent)? {
                queue.push((format!("{}/{}", parent.display(), file.display()), false));
            }
        }
        let bytes = std::fs::read(&from).map_err(io(&from))?;
        let to = oracle_home.join(&rel);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(io(parent))?;
        }
        match String::from_utf8(bytes) {
            Ok(text) => {
                for more in home_paths(&text, &home) {
                    queue.push((more, true));
                }
                std::fs::write(&to, tokenize(&text, &home)).map_err(io(&to))?;
            }
            Err(bytes) => std::fs::write(&to, bytes.into_bytes()).map_err(io(&to))?,
        }
    }
    let reg_out = dir.registration(id);
    std::fs::create_dir_all(dir.oracle_dir(id)).map_err(io(&dir.oracle_dir(id)))?;
    std::fs::write(&reg_out, tokenize(&reg_text, &home)).map_err(io(&reg_out))?;
    Ok(())
}

/// Write candidates into `name`'s draft (created, or added to), replacing
/// any task of the same id. Returns the draft manifest.
pub fn build_draft(
    evals: &Path,
    name: &str,
    project_id: &str,
    repo: &Path,
    paths: &ProjectPaths,
    candidates: &[Candidate],
    now: DateTime<Utc>,
) -> Result<SetManifest, MineError> {
    crate::set::validate_name(name).map_err(MineError::Invalid)?;
    let dir = SetDir::draft(evals, name);
    let mut manifest = if dir.manifest().is_file() {
        dir.load()?
    } else {
        SetManifest {
            name: name.to_owned(),
            version: None,
            project_id: project_id.to_owned(),
            repo: repo.to_owned(),
            created_at: now,
            tasks: Vec::new(),
            hash: None,
        }
    };
    for c in candidates {
        let id = &c.task.id;
        for stale in [dir.oracle_dir(id), dir.spec_dir(id)] {
            if stale.exists() {
                std::fs::remove_dir_all(&stale).map_err(io(&stale))?;
            }
        }
        let proof = dir.proof_file(id);
        if proof.exists() {
            std::fs::remove_file(&proof).map_err(io(&proof))?;
        }
        write_candidate(&dir, paths, c)?;
        manifest.tasks.retain(|t| &t.id != id);
        manifest.tasks.push(c.task.clone());
    }
    manifest.tasks.sort_by(|a, b| a.id.cmp(&b.id));
    dir.save(&manifest)?;
    write_json(
        &dir.root().join("mined.json"),
        &candidates
            .iter()
            .map(|c| serde_json::json!({"task": c.task.id, "source": c.task.source.as_str(), "reason": c.reason}))
            .collect::<Vec<_>>(),
    )?;
    Ok(manifest)
}

#[cfg(test)]
mod tests;

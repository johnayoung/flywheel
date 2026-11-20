//! Validation: every set task proves, before it is frozen, that it can
//! judge a trial. Each check is a receipt in the task's [`Proof`].
//!
//! | check | what | failure it catches |
//! | --- | --- | --- |
//! | V1 | the oracle fails at the base, twice | a vacuous oracle |
//! | V2 | the oracle passes at the reference, twice, with `CARGO_TARGET_DIR` outside the tree and an empty `FLYWHEEL_HOME` | a broken oracle; one that looks for `target/debug` (00083) |
//! | V3 | every prerequisite's landed commit is in the base (else the base moves to the reference's parent when that holds them, and V1/V2 run there) | a base that predates a prerequisite |
//! | V4 | a mirror of the base holds no commit past it | an agent reading the answer from history |
//! | V5 | two copies at the reference pass at once; one passes while `fw` processes start and exit beside it (else `parallel_safe = false`, graded alone) | oracles that abort on other `fw` processes (00088-00090) |
//! | V6 | no oracle file names a live flywheel home | oracles that depend on live state |
//! | V7 | the task parses, has no prerequisites, names no live home path | agents reading the real project home |
//!
//! A hard-failed task has no reference yet: the first commit on the source
//! repo's first-parent history after the base that the oracle passes on is
//! found by bisection (the oracle must fail on the commit before it).
//!
//! Validation runs oracles with no turn-taking of its own: the
//! `fw eval set validate` verb holds the machine's test-suite lease for the
//! whole run, like `scripts/check.sh`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::git;
use crate::mine::PROJECT_HOME_TOKEN;
use crate::mirror;
use crate::oracle::{self, GradeTree, Oracle, RunOptions, Verdict};
use crate::set::{Proof, ProofCheck, SetDir, SetError, SetManifest, files_digest, files_under};

/// How validation runs.
#[derive(Debug, Clone)]
pub struct ValidateOptions {
    /// Scratch space: grading clones, mirrors, materialized oracles.
    pub work: PathBuf,
    /// The build cache oracles share (`CARGO_TARGET_DIR`).
    pub target_dir: PathBuf,
    /// One oracle run's bound.
    pub timeout: Duration,
    /// A command to start and stop beside the oracle in V5 (`fw --version`);
    /// `None` skips that probe (and `parallel_safe` rests on the
    /// concurrent-copies probe alone).
    pub noise: Option<Vec<String>>,
    /// The source repo's ref a hard-failed task's reference is searched on.
    pub search_head: String,
}

fn check(name: &str, passed: bool, detail: impl Into<String>) -> ProofCheck {
    ProofCheck {
        name: name.to_owned(),
        passed,
        detail: detail.into(),
    }
}

/// Whether prerequisite `id` is in the history of `at`: its landed commit
/// is an ancestor, or -- when the store recorded no landing (it landed out
/// of band) -- a commit carrying its `Flywheel-Task: <id>` trailer is.
fn prerequisite_in(repo: &Path, id: &str, landed: &str, at: &str) -> bool {
    if !landed.is_empty() {
        return git::is_ancestor(repo, landed, at);
    }
    let grep = format!("--grep=^Flywheel-Task: {}$", regex::escape(id));
    git::run(repo, &["log", "-1", "--format=%H", &grep, at]).is_ok_and(|found| !found.is_empty())
}

fn short(sha: &str) -> &str {
    &sha[..12.min(sha.len())]
}

/// Strings that name a live flywheel project's files. A bare mention of
/// `~/.flywheel` (a comment saying a test keeps out of it) is not one; the
/// oracle runs with an empty `FLYWHEEL_HOME` either way.
fn names_live_home(text: &str) -> bool {
    text.contains("/.flywheel/projects/") || text.contains("~/.flywheel/projects")
}

/// The project-home files a task's own graders run that the set does not
/// carry (a trial's fresh home would not have them, so every in-loop grade
/// would fail on a missing file).
fn grader_files_missing(set: &SetDir, id: &str, raw: &str) -> Vec<String> {
    let runs: Vec<String> = serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| v.get("graders").and_then(|g| g.as_array()).cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|g| g.get("run").and_then(|r| r.as_str()).map(str::to_owned))
        .collect();
    let mut missing = Vec::new();
    for run in runs {
        for path in crate::mine::home_paths(&run, PROJECT_HOME_TOKEN) {
            let rel = path
                .trim_start_matches(PROJECT_HOME_TOKEN)
                .trim_start_matches('/');
            if !rel.is_empty()
                && !rel.starts_with("held-out")
                && !set.spec_dir(id).join(rel).exists()
                && !missing.contains(&rel.to_owned())
            {
                missing.push(rel.to_owned());
            }
        }
    }
    missing
}

struct Grader<'a> {
    oracle: Oracle,
    repo: &'a Path,
    tree: GradeTree,
    options: RunOptions,
    log: PathBuf,
}

impl Grader<'_> {
    fn at(&self, sha: &str) -> Result<Verdict, SetError> {
        self.tree.checkout(sha)?;
        oracle::run(&self.oracle, &self.tree.path, &self.options)
            .map_err(|e| SetError::Invalid(format!("cannot run the oracle: {e}")))
    }

    fn twice(&self, sha: &str) -> Result<(Verdict, Verdict), SetError> {
        Ok((self.at(sha)?, self.at(sha)?))
    }
}

/// The first commit on `head`'s first-parent history after `base` where
/// the oracle passes, with the oracle failing on its predecessor.
fn bisect_reference(g: &Grader<'_>, base: &str, head: &str) -> Result<Option<String>, SetError> {
    let list = git::run(
        g.repo,
        &[
            "rev-list",
            "--first-parent",
            "--reverse",
            &format!("{base}..{head}"),
        ],
    )?;
    let commits: Vec<&str> = list.lines().filter(|l| !l.is_empty()).collect();
    let Some(last) = commits.last() else {
        return Ok(None);
    };
    if !g.at(last)?.passed {
        return Ok(None);
    }
    // Invariant: fails at lo (the base, index -1), passes at hi.
    let (mut lo, mut hi) = (-1_i64, (commits.len() - 1) as i64);
    while hi - lo > 1 {
        let mid = (lo + hi) / 2;
        if g.at(commits[mid as usize])?.passed {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    Ok(Some(commits[hi as usize].to_owned()))
}

/// Validate one task of `set`, updating its base or reference in
/// `manifest` when V3 or the bisection settles them. The proof is saved
/// beside the set's other proofs and returned.
pub fn validate_task(
    set: &SetDir,
    manifest: &mut SetManifest,
    id: &str,
    options: &ValidateOptions,
    now: DateTime<Utc>,
) -> Result<Proof, SetError> {
    let index = manifest
        .tasks
        .iter()
        .position(|t| t.id == id)
        .ok_or_else(|| SetError::Invalid(format!("no task {id} in {}", manifest.label())))?;
    let repo = manifest.repo.clone();
    let mut checks = Vec::new();

    // V7: the task file.
    let task_path = set.task_file(id);
    let raw = std::fs::read_to_string(&task_path).map_err(crate::set::io(&task_path))?;
    let substituted = raw.replace(PROJECT_HOME_TOKEN, "/eval/project");
    let v7 =
        match flywheel_core::loaders::load_task_str(&substituted, &task_path.display().to_string())
        {
            Err(e) => check("V7", false, format!("the task does not load: {e}")),
            Ok(_) if raw.contains("\"prerequisites\"") => {
                check("V7", false, "the task still names prerequisites")
            }
            Ok(_) if names_live_home(&raw) => {
                check("V7", false, "the task names a live flywheel home")
            }
            Ok(_) => match grader_files_missing(set, id, &raw) {
                missing if missing.is_empty() => {
                    check("V7", true, "loads; no prerequisites; home paths tokenized")
                }
                missing => check(
                    "V7",
                    false,
                    format!(
                        "its graders run files the set does not carry: {}",
                        missing.join(", ")
                    ),
                ),
            },
        };
    checks.push(v7);

    // V6: the oracle files.
    let mut live = Vec::new();
    for rel in files_under(&set.oracle_dir(id))? {
        let path = set.oracle_dir(id).join(&rel);
        if std::fs::read_to_string(&path).is_ok_and(|t| names_live_home(&t)) {
            live.push(rel.display().to_string());
        }
    }
    checks.push(if live.is_empty() {
        check("V6", true, "no oracle file names a live home")
    } else {
        check(
            "V6",
            false,
            format!("names a live home: {}", live.join(", ")),
        )
    });

    let work = options.work.join(id);
    if !set.registration(id).is_file() {
        checks.push(check(
            "oracle",
            false,
            format!(
                "no oracle yet: blind-author it (/fw-verify, from the goal and the reference \
                 diff) into {}, its files under {} with project-home paths as {PROJECT_HOME_TOKEN}",
                set.registration(id).display(),
                set.oracle_dir(id).join("home").display()
            ),
        ));
        return finish(set, manifest, index, checks, false, false, 0.0, now);
    }
    let oracle = oracle::materialize(set, id, &work.join("oracle"))?;
    let log = work.join("oracle.log");
    let _ = std::fs::remove_file(&log);
    let run_options = RunOptions {
        // A cache per task: see `trial`'s grading for why trees at
        // different commits must not share one.
        target_dir: Some(options.target_dir.join(id)),
        timeout: Some(options.timeout),
        log: Some(log.clone()),
        ..RunOptions::default()
    };
    let branch = git::default_branch(&repo);
    let task_base = manifest.tasks[index].base.clone();
    let mut grader = Grader {
        oracle: oracle.clone(),
        repo: &repo,
        tree: GradeTree::open(&repo, &work.join("tree-a"))?.with_base(&branch, &task_base),
        options: run_options.clone(),
        log,
    };

    // The reference of a hard-failed task.
    if manifest.tasks[index].reference.is_empty() {
        let base = manifest.tasks[index].base.clone();
        match bisect_reference(&grader, &base, &options.search_head)? {
            Some(found) => {
                checks.push(check("ref", true, format!("bisected to {found}")));
                manifest.tasks[index].reference = found;
            }
            None => {
                checks.push(check(
                    "ref",
                    false,
                    format!(
                        "no commit after the base on {} passes the oracle",
                        options.search_head
                    ),
                ));
                return finish(set, manifest, index, checks, false, false, 0.0, now);
            }
        }
    }

    // V3: prerequisites in the base.
    let task = manifest.tasks[index].clone();
    let missing: Vec<String> = task
        .prerequisites
        .iter()
        .filter(|(id, sha)| !prerequisite_in(&repo, id, sha, &task.base))
        .map(|(id, _)| id.clone())
        .collect();
    if missing.is_empty() {
        checks.push(check(
            "V3",
            true,
            format!("{} prerequisites in the base", task.prerequisites.len()),
        ));
    } else {
        let parent = git::rev_parse(&repo, &format!("{}^", task.reference)).ok();
        let holds = parent.as_ref().is_some_and(|p| {
            task.prerequisites
                .iter()
                .all(|(id, sha)| prerequisite_in(&repo, id, sha, p))
        });
        match (holds, parent) {
            (true, Some(parent)) => {
                checks.push(check(
                    "V3",
                    true,
                    format!(
                        "base moved to the reference's parent {parent}: {} landed after the recorded base",
                        missing.join(", ")
                    ),
                ));
                manifest.tasks[index].base = parent;
            }
            _ => {
                checks.push(check(
                    "V3",
                    false,
                    format!("prerequisites not in the base: {}", missing.join(", ")),
                ));
                return finish(set, manifest, index, checks, false, false, 0.0, now);
            }
        }
    }
    let task = manifest.tasks[index].clone();
    // The base may have moved to cover a prerequisite.
    grader.tree = grader.tree.clone().with_base(&branch, &task.base);

    let (v2, oracle_secs) = if task.kind == crate::set::TaskKind::DontAct {
        // A don't-act task is inverted: the work is done at the base, so the
        // oracle passes there (V1) and fails once anything is committed on
        // top of it (V2).
        let (a, b) = grader.twice(&task.base)?;
        let v1 = a.passed && b.passed;
        checks.push(check(
            "V1",
            v1,
            format!(
                "at base {} (the work is done): {}, {}",
                short(&task.base),
                a.detail,
                b.detail
            ),
        ));
        grader.tree.checkout(&task.base)?;
        git::run(
            &grader.tree.path,
            &[
                "-c",
                "user.name=flywheel eval",
                "-c",
                "user.email=eval@flywheel.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "--allow-empty",
                "--no-verify",
                "-m",
                "probe: an agent change",
            ],
        )?;
        let probe = oracle::run(&grader.oracle, &grader.tree.path, &grader.options)
            .map_err(|e| SetError::Invalid(format!("cannot run the oracle: {e}")))?;
        let v2 = !probe.passed && !probe.ran.timed_out;
        checks.push(check(
            "V2",
            v2,
            format!("a commit on top of the base: {}", probe.detail),
        ));
        (v1 && v2, (a.secs + b.secs) / 2.0)
    } else {
        // V1: fails at the base, twice.
        let (a, b) = grader.twice(&task.base)?;
        let v1 = !a.passed && !b.passed && !a.ran.timed_out && !b.ran.timed_out;
        checks.push(check(
            "V1",
            v1,
            format!("at base {}: {}, {}", short(&task.base), a.detail, b.detail),
        ));
        // V2: passes at the reference, twice.
        let (a, b) = grader.twice(&task.reference)?;
        let v2 = a.passed && b.passed;
        let oracle_secs = (a.secs + b.secs) / 2.0;
        checks.push(check(
            "V2",
            v2,
            format!(
                "at reference {}: {}, {} ({oracle_secs:.0}s){}",
                short(&task.reference),
                a.detail,
                b.detail,
                if v2 {
                    String::new()
                } else {
                    format!("; log {}", grader.log.display())
                }
            ),
        ));
        (v2, oracle_secs)
    };

    // V4: a mirror of the base holds nothing past it.
    let mirror_dir = work.join("mirror");
    let v4 = mirror::base_mirror(&repo, &task.base, None, &mirror_dir)
        .and_then(|()| mirror::future_free(&mirror_dir, &task.base));
    checks.push(match v4 {
        Ok(true) => check("V4", true, "the base mirror holds no later commit"),
        Ok(false) => check("V4", false, "the base mirror reaches commits past the base"),
        Err(e) => check("V4", false, e.to_string()),
    });

    // V5: concurrent copies, then fw processes coming and going beside it.
    let mut parallel_safe = false;
    if v2 {
        let tree_b = GradeTree::open(&repo, &work.join("tree-b"))?.with_base(&branch, &task.base);
        grader.tree.checkout(&task.reference)?;
        tree_b.checkout(&task.reference)?;
        let (o1, o2) = (oracle.clone(), oracle.clone());
        let (p1, p2) = (grader.tree.path.clone(), tree_b.path.clone());
        let (r1, r2) = (run_options.clone(), run_options.clone());
        let h1 = std::thread::spawn(move || oracle::run(&o1, &p1, &r1));
        let h2 = std::thread::spawn(move || oracle::run(&o2, &p2, &r2));
        let together = (h1.join(), h2.join());
        let concurrent = matches!(&together, (Ok(Ok(a)), Ok(Ok(b))) if a.passed && b.passed);
        if !concurrent {
            checks.push(check(
                "V5",
                false,
                "two copies at the reference did not both pass",
            ));
        } else if let Some(noise) = &options.noise {
            let stop = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&stop);
            let argv = noise.clone();
            let noisy = std::thread::spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    let _ = std::process::Command::new(&argv[0])
                        .args(&argv[1..])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status();
                    std::thread::sleep(Duration::from_millis(200));
                }
            });
            let beside = grader.at(&task.reference);
            stop.store(true, Ordering::Relaxed);
            let _ = noisy.join();
            let beside = beside?;
            parallel_safe = beside.passed;
            checks.push(check(
                "V5",
                true,
                if parallel_safe {
                    "two copies pass at once; passes beside fw processes".to_owned()
                } else {
                    format!(
                        "fails beside fw processes ({}): parallel_safe = false, graded alone",
                        beside.detail
                    )
                },
            ));
        } else {
            parallel_safe = true;
            checks.push(check(
                "V5",
                true,
                "two copies pass at once; fw-process probe skipped",
            ));
        }
    } else {
        checks.push(check("V5", false, "skipped: V2 failed"));
    }

    let passed = checks.iter().all(|c| c.passed);
    finish(
        set,
        manifest,
        index,
        checks,
        passed,
        parallel_safe,
        oracle_secs,
        now,
    )
}

#[allow(clippy::too_many_arguments)]
fn finish(
    set: &SetDir,
    manifest: &mut SetManifest,
    index: usize,
    checks: Vec<ProofCheck>,
    passed: bool,
    parallel_safe: bool,
    oracle_secs: f64,
    now: DateTime<Utc>,
) -> Result<Proof, SetError> {
    set.save(manifest)?;
    let task = &manifest.tasks[index];
    let proof = Proof {
        task_id: task.id.clone(),
        passed,
        files_digest: files_digest(set, &task.id)?,
        base: task.base.clone(),
        reference: task.reference.clone(),
        parallel_safe,
        oracle_secs,
        checks,
        at: now,
    };
    set.save_proof(&proof)?;
    Ok(proof)
}

/// Validate every task of the draft at `set` (or only `only`), calling
/// `progress` with each finished proof.
pub fn validate_set(
    set: &SetDir,
    only: Option<&str>,
    options: &ValidateOptions,
    now: &dyn Fn() -> DateTime<Utc>,
    progress: &mut dyn FnMut(&Proof),
) -> Result<Vec<Proof>, SetError> {
    let mut manifest = set.load()?;
    if manifest.version.is_some() {
        return Err(SetError::Invalid(format!(
            "{} is frozen; validate a draft",
            manifest.label()
        )));
    }
    let ids: Vec<String> = manifest
        .tasks
        .iter()
        .filter(|t| only.is_none_or(|o| t.id == o))
        .map(|t| t.id.clone())
        .collect();
    if ids.is_empty() {
        return Err(SetError::Invalid(format!(
            "no task to validate in {}",
            manifest.label()
        )));
    }
    let mut proofs = Vec::new();
    for id in ids {
        let proof = validate_task(set, &mut manifest, &id, options, now())?;
        progress(&proof);
        proofs.push(proof);
    }
    Ok(proofs)
}

#[cfg(test)]
mod tests;

//! Worker housekeeping: parked-worktree retention and run-log retention.
//!
//! Port of the retention functions of the Python `flywheel_worktree.worker`
//! module (`sweep_run_logs`, `retention_sweep`, `retention_targets`,
//! `retention_cadence_tick`). Both sweeps are bounded, best-effort, and
//! never fail: housekeeping cannot escape into the run loop, so every
//! unremovable file or directory is skipped, not raised.
//!
//! * [`sweep_run_logs`] holds `.flywheel/logs/runs/` at or under a cap by
//!   reclaiming the oldest per-run telemetry files (`docs/cli.md`,
//!   `--run-log-retention`).
//! * [`retention_sweep`] removes parked worktrees older than the window and
//!   their `flywheel/<phase>/<task-id>` branches (`docs/sandbox.md`,
//!   `[sandbox.retention] sweep_days` / `--worktree-retention-days`).
//!   [`retention_cadence_tick`] runs it once per daemon cycle against a fresh
//!   clock reading; [`retention_targets`] lists the roots one pass covers.
//!
//! The daemon (`crate::daemon`) drives these; nothing here knows about
//! policies or the store.

use crate::submitter::git;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Default parked-worktree retention window in days
/// (`--worktree-retention-days`).
pub const DEFAULT_RETENTION_DAYS: u64 = 7;

/// Default cap on per-run telemetry JSONL files kept under
/// `.flywheel/logs/runs/` (`--run-log-retention`). The harness's sink writes
/// one `<run_id>.jsonl` per run and never rotates them, so without an active
/// bound the directory grows without limit. The worker reclaims the oldest
/// files past this cap each cycle; the most-recent runs (including the one
/// in flight) always survive. Magnitude is a default, not a contract.
pub const DEFAULT_RUN_LOG_RETENTION: usize = 500;

// The run-log cap is default-on: a zero default would silently disable
// retention for every operator who never set `--run-log-retention`.
const _: () = assert!(DEFAULT_RUN_LOG_RETENTION > 0);

const SECONDS_PER_DAY: f64 = 86_400.0;

/// Hold `runs_dir` at or under `max_run_files` per-run JSONL files by
/// deleting the oldest (by mtime) and preserving the most-recent
/// `max_run_files`.
///
/// Default-on companion to [`retention_sweep`] for the telemetry stream: the
/// newest file -- the run in flight -- is always in the surviving set, so an
/// active run is never reclaimed. A no-op when the directory is absent,
/// `max_run_files` is zero (retention disabled), or there are already at or
/// fewer than `max_run_files` files. Only `*.jsonl` regular files are
/// candidates; an operator's note beside them is never touched. Reclaim
/// failures on individual files are skipped, never raised.
pub fn sweep_run_logs(runs_dir: &Path, max_run_files: usize, log: &dyn Fn(&str)) {
    if max_run_files == 0 || !runs_dir.is_dir() {
        return;
    }
    let Ok(dir) = fs::read_dir(runs_dir) else {
        return;
    };
    let mut entries: Vec<(f64, String, PathBuf)> = Vec::new();
    for entry in dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") || !path.is_file() {
            continue;
        }
        let Some(mtime) = mtime_seconds(&path) else {
            continue;
        };
        entries.push((
            mtime,
            entry.file_name().to_string_lossy().into_owned(),
            path,
        ));
    }
    if entries.len() <= max_run_files {
        return;
    }
    // Newest first: (mtime, name) descending keeps exactly the most-recent
    // files and breaks mtime ties deterministically by name. Everything past
    // the bound is the oldest tail, which is what we reclaim.
    entries.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    let reclaimed = entries
        .iter()
        .skip(max_run_files)
        .filter(|(_, _, path)| fs::remove_file(path).is_ok())
        .count();
    if reclaimed > 0 {
        log(&format!(
            "Run-log retention: reclaimed {reclaimed} old run file(s), keeping the most-recent \
             {max_run_files}"
        ));
    }
}

/// Remove parked worktrees under `worktrees_dir` older than the window and
/// their `flywheel/<phase>/<task-id>` branches. Prunes dangling worktree
/// entries first so hand-removed dirs stop blocking new adds.
///
/// `now_ts` is the clock reading (seconds since the Unix epoch) the window
/// is measured from; a worktree whose directory mtime is at or after
/// `now_ts - retention_days * 86400` survives. A worktree git refuses to
/// remove is deleted from disk directly (its registration is pruned on the
/// next pass).
pub fn retention_sweep(
    repo_root: &Path,
    worktrees_dir: &Path,
    retention_days: u64,
    now_ts: f64,
    log: &dyn Fn(&str),
) {
    // Spawn failures are not actionable here: the sweep is housekeeping and
    // must never escape into the daemon loop.
    let _ = git(repo_root, &["worktree", "prune"]);
    if !worktrees_dir.is_dir() {
        return;
    }
    let Ok(dir) = fs::read_dir(worktrees_dir) else {
        return;
    };
    let cutoff = now_ts - retention_days as f64 * SECONDS_PER_DAY;
    let mut worktrees: Vec<PathBuf> = dir.flatten().map(|e| e.path()).collect();
    worktrees.sort();
    for wt in worktrees {
        if !wt.is_dir() {
            continue;
        }
        let Some(mtime) = mtime_seconds(&wt) else {
            continue;
        };
        if mtime >= cutoff {
            continue;
        }
        let task_id = wt
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        log(&format!(
            "Sweep: removing worktree {} (age > {retention_days}d)",
            wt.display()
        ));
        let wt_str = wt.to_string_lossy().into_owned();
        let removed = git(
            repo_root,
            &["worktree", "remove", "--force", wt_str.as_str()],
        )
        .map(|out| out.ok())
        .unwrap_or(false);
        if !removed {
            let _ = fs::remove_dir_all(&wt);
        }
        let pattern = format!("refs/heads/flywheel/*/{task_id}");
        let refs = git(
            repo_root,
            &[
                "for-each-ref",
                "--format=%(refname:short)",
                pattern.as_str(),
            ],
        )
        .map(|out| out.stdout)
        .unwrap_or_default();
        for branch in refs.lines().map(str::trim).filter(|b| !b.is_empty()) {
            let _ = git(repo_root, &["branch", "-D", branch]);
        }
    }
}

/// The worktree roots one retention pass covers.
///
/// Always the active root; additionally the legacy hardcoded
/// `.flywheel/worktrees` while it still exists on disk. `[paths]
/// sandbox_root` used to be silently ignored on this path, so a repo that
/// pinned it has in-flight/parked worktrees at the legacy location the
/// moment the knob became live -- those must keep aging out of the same
/// retention window instead of stranding forever.
pub fn retention_targets(repo_root: &Path, worktrees_dir: &Path) -> Vec<PathBuf> {
    let mut targets = vec![worktrees_dir.to_path_buf()];
    let legacy = repo_root.join(".flywheel").join("worktrees");
    let distinct = match (fs::canonicalize(&legacy), fs::canonicalize(worktrees_dir)) {
        (Ok(a), Ok(b)) => a != b,
        _ => true,
    };
    if distinct && legacy.is_dir() {
        targets.push(legacy);
    }
    targets
}

/// Run one [`retention_sweep`] pass, reading the clock fresh.
///
/// The daemon invokes this once per cycle -- not once at boot -- so a parked
/// worktree that ages past the retention window is reclaimed mid-run without
/// a restart, while a within-window worktree survives. Reading `now` on each
/// tick is what advances the cutoff between cycles; it is injectable so a
/// test can drive the cadence without wall-clock time. A single pass has
/// exactly [`retention_sweep`]'s removal semantics -- this controls only
/// *when* the sweep runs, never *what* one sweep removes.
pub fn retention_cadence_tick(
    repo_root: &Path,
    worktrees_dir: &Path,
    retention_days: u64,
    log: &dyn Fn(&str),
    now: &dyn Fn() -> f64,
) {
    retention_sweep(repo_root, worktrees_dir, retention_days, now(), log);
}

/// The wall-clock reading [`retention_cadence_tick`] uses in production:
/// seconds since the Unix epoch, as `time.time()` reports.
pub fn wall_clock_seconds() -> f64 {
    system_time_seconds(SystemTime::now())
}

/// A path's modification time in seconds since the Unix epoch, `None` when
/// it cannot be stat'ed (the entry vanished under the sweep).
fn mtime_seconds(path: &Path) -> Option<f64> {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .map(system_time_seconds)
}

fn system_time_seconds(time: SystemTime) -> f64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(since) => since.as_secs_f64(),
        Err(before) => -before.duration().as_secs_f64(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::submitter::test_support::{git_ok, init_repo};
    use std::fs::File;
    use std::sync::Mutex;
    use std::time::Duration;

    const DAY: f64 = 86_400.0;
    const RETENTION_DAYS: u64 = 7;
    const WINDOW: f64 = RETENTION_DAYS as f64 * DAY;

    fn quiet(_m: &str) {}

    fn set_mtime(path: &Path, when: f64) {
        let file = File::open(path).unwrap();
        file.set_modified(UNIX_EPOCH + Duration::from_secs_f64(when))
            .unwrap();
    }

    fn make_run_file(runs_dir: &Path, run_id: &str, mtime: f64) -> PathBuf {
        fs::create_dir_all(runs_dir).unwrap();
        let path = runs_dir.join(format!("{run_id}.jsonl"));
        fs::write(&path, "{\"kind\": \"x\"}\n").unwrap();
        set_mtime(&path, mtime);
        path
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn worktrees_dir(repo: &Path) -> PathBuf {
        let d = repo.join(".flywheel").join("worktrees");
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// Register a real git worktree at `worktrees_dir/<task_id>` on the
    /// `flywheel/_root/<task_id>` branch `retention_sweep` matches and
    /// deletes.
    fn add_worktree(repo: &Path, worktrees_dir: &Path, task_id: &str) -> PathBuf {
        let wt = worktrees_dir.join(task_id);
        git_ok(
            repo,
            &[
                "worktree",
                "add",
                "-b",
                &format!("flywheel/_root/{task_id}"),
                wt.to_str().unwrap(),
                "main",
            ],
        );
        wt
    }

    fn branch_exists(repo: &Path, branch: &str) -> bool {
        !git_ok(
            repo,
            &[
                "for-each-ref",
                "--format=%(refname:short)",
                &format!("refs/heads/{branch}"),
            ],
        )
        .is_empty()
    }

    fn repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repo");
        init_repo(&path);
        (dir, path)
    }

    // --- run-log retention --------------------------------------------------------------

    #[test]
    fn reclaims_oldest_and_pins_survivors() {
        let tmp = tempfile::tempdir().unwrap();
        let runs = tmp.path().join("logs").join("runs");
        // Six runs, strictly increasing mtimes: run-0 oldest ... run-5 newest.
        let files: Vec<PathBuf> = (0..6)
            .map(|i| make_run_file(&runs, &format!("run-{i}"), 1000.0 + i as f64))
            .collect();
        sweep_run_logs(&runs, 2, &quiet);
        assert_eq!(names(&runs), ["run-4.jsonl", "run-5.jsonl"]);
        for file in &files[..4] {
            assert!(!file.exists());
        }
        assert!(files[4].exists() && files[5].exists());
    }

    #[test]
    fn fewer_files_than_bound_reclaims_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let runs = tmp.path().join("logs").join("runs");
        let kept: Vec<PathBuf> = (0..3)
            .map(|i| make_run_file(&runs, &format!("run-{i}"), 1000.0 + i as f64))
            .collect();
        sweep_run_logs(&runs, 10, &quiet);
        assert_eq!(names(&runs), ["run-0.jsonl", "run-1.jsonl", "run-2.jsonl"]);
        assert!(kept.iter().all(|p| p.exists()));
    }

    #[test]
    fn at_bound_reclaims_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let runs = tmp.path().join("logs").join("runs");
        let kept: Vec<PathBuf> = (0..4)
            .map(|i| make_run_file(&runs, &format!("run-{i}"), 1000.0 + i as f64))
            .collect();
        let logged = Mutex::new(Vec::new());
        sweep_run_logs(&runs, 4, &|m| logged.lock().unwrap().push(m.to_owned()));
        assert!(kept.iter().all(|p| p.exists()));
        assert!(logged.lock().unwrap().is_empty());
    }

    #[test]
    fn newest_run_is_never_reclaimed() {
        let tmp = tempfile::tempdir().unwrap();
        let runs = tmp.path().join("logs").join("runs");
        // The in-flight run is the newest; even at bound=1 it must survive
        // and the older run must be the one deleted.
        make_run_file(&runs, "run-old", 1000.0);
        let newest = make_run_file(&runs, "run-inflight", 2000.0);
        sweep_run_logs(&runs, 1, &quiet);
        assert!(newest.exists());
        assert_eq!(names(&runs), ["run-inflight.jsonl"]);
    }

    #[test]
    fn mtime_ties_break_by_name_descending() {
        let tmp = tempfile::tempdir().unwrap();
        let runs = tmp.path().join("logs").join("runs");
        for name in ["run-a", "run-b", "run-c"] {
            make_run_file(&runs, name, 1000.0);
        }
        sweep_run_logs(&runs, 1, &quiet);
        assert_eq!(names(&runs), ["run-c.jsonl"]);
    }

    #[test]
    fn missing_runs_dir_is_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        sweep_run_logs(&tmp.path().join("logs").join("runs"), 5, &quiet);
    }

    #[test]
    fn zero_bound_disables_retention() {
        let tmp = tempfile::tempdir().unwrap();
        let runs = tmp.path().join("logs").join("runs");
        let kept: Vec<PathBuf> = (0..5)
            .map(|i| make_run_file(&runs, &format!("run-{i}"), 1000.0 + i as f64))
            .collect();
        sweep_run_logs(&runs, 0, &quiet);
        assert!(kept.iter().all(|p| p.exists()));
    }

    #[test]
    fn only_jsonl_files_are_reclaimed() {
        let tmp = tempfile::tempdir().unwrap();
        let runs = tmp.path().join("logs").join("runs");
        make_run_file(&runs, "run-0", 1000.0);
        make_run_file(&runs, "run-1", 1001.0);
        make_run_file(&runs, "run-2", 1002.0);
        // An older non-JSONL sibling (an operator note) is left untouched
        // even though it predates the reclaimed run files; so is a
        // subdirectory that happens to carry the suffix.
        let other = runs.join("NOTES.txt");
        fs::write(&other, "keep me\n").unwrap();
        set_mtime(&other, 500.0);
        let sub = runs.join("archive.jsonl");
        fs::create_dir(&sub).unwrap();
        set_mtime(&sub, 400.0);
        sweep_run_logs(&runs, 1, &quiet);
        assert!(other.exists());
        assert!(sub.is_dir());
        assert_eq!(names(&runs), ["NOTES.txt", "archive.jsonl", "run-2.jsonl"]);
    }

    #[test]
    fn reclaim_is_logged() {
        let tmp = tempfile::tempdir().unwrap();
        let runs = tmp.path().join("logs").join("runs");
        for i in 0..4 {
            make_run_file(&runs, &format!("run-{i}"), 1000.0 + i as f64);
        }
        let logged = Mutex::new(Vec::new());
        sweep_run_logs(&runs, 1, &|m| logged.lock().unwrap().push(m.to_owned()));
        assert_eq!(
            logged.lock().unwrap().as_slice(),
            ["Run-log retention: reclaimed 3 old run file(s), keeping the most-recent 1"]
        );
    }

    #[test]
    fn retention_is_default_on() {
        assert_eq!(DEFAULT_RETENTION_DAYS, 7);
        assert!(wall_clock_seconds() > 1_600_000_000.0);
    }

    // --- worktree retention ---------------------------------------------------------------

    #[test]
    fn sweep_tick_reclaims_aged() {
        let (_dir, repo) = repo();
        let worktrees = worktrees_dir(&repo);
        let wt = add_worktree(&repo, &worktrees, "aged");
        let now = 1_000_000_000.0;
        set_mtime(&wt, now - (WINDOW + DAY));
        let logged = Mutex::new(Vec::new());

        retention_cadence_tick(
            &repo,
            &worktrees,
            RETENTION_DAYS,
            &|m| logged.lock().unwrap().push(m.to_owned()),
            &|| now,
        );

        assert!(!wt.exists());
        assert!(!branch_exists(&repo, "flywheel/_root/aged"));
        assert_eq!(
            logged.lock().unwrap().as_slice(),
            [format!(
                "Sweep: removing worktree {} (age > 7d)",
                wt.display()
            )]
        );
        // The registration is gone too: a fresh add at the same path works.
        git_ok(
            &repo,
            &["worktree", "add", "--detach", wt.to_str().unwrap(), "main"],
        );
    }

    #[test]
    fn within_window_survives() {
        let (_dir, repo) = repo();
        let worktrees = worktrees_dir(&repo);
        let wt = add_worktree(&repo, &worktrees, "fresh");
        let now = 1_000_000_000.0;
        set_mtime(&wt, now - DAY);

        retention_cadence_tick(&repo, &worktrees, RETENTION_DAYS, &quiet, &|| now);

        assert!(wt.exists());
        assert!(branch_exists(&repo, "flywheel/_root/fresh"));
    }

    #[test]
    fn exactly_at_the_cutoff_survives() {
        let (_dir, repo) = repo();
        let worktrees = worktrees_dir(&repo);
        let wt = add_worktree(&repo, &worktrees, "edge");
        let now = 1_000_000_000.0;
        set_mtime(&wt, now - WINDOW);
        retention_sweep(&repo, &worktrees, RETENTION_DAYS, now, &quiet);
        assert!(wt.exists());
    }

    #[test]
    fn cadence_recurs_and_reclaims_mid_run() {
        // `aging` is within the window at boot but crosses it as the clock
        // advances; `survivor` is touched every cycle so it never ages out.
        let (_dir, repo) = repo();
        let worktrees = worktrees_dir(&repo);
        let aging = add_worktree(&repo, &worktrees, "aging");
        let survivor = add_worktree(&repo, &worktrees, "survivor");
        let start = 1_000_000_000.0;
        set_mtime(&aging, start);
        set_mtime(&survivor, start);

        let clock = Mutex::new(start);
        let now = || *clock.lock().unwrap();
        let cycle = || {
            set_mtime(&survivor, now());
            retention_cadence_tick(&repo, &worktrees, RETENTION_DAYS, &quiet, &now);
            *clock.lock().unwrap() += 3.0 * DAY;
        };

        // Control: a single boot-only tick reclaims a within-window tree never.
        cycle();
        assert!(aging.exists());

        for _ in 0..3 {
            cycle();
        }
        assert!(!aging.exists());
        assert!(!branch_exists(&repo, "flywheel/_root/aging"));
        assert!(survivor.exists());
        assert!(branch_exists(&repo, "flywheel/_root/survivor"));
    }

    #[test]
    fn hand_removed_worktree_directory_is_pruned_and_its_branch_kept_until_aged() {
        let (_dir, repo) = repo();
        let worktrees = worktrees_dir(&repo);
        let wt = add_worktree(&repo, &worktrees, "gone");
        fs::remove_dir_all(&wt).unwrap();
        // The dangling registration is pruned; a missing dir has no mtime to
        // age, so its branch stays (it is reclaimed only with a directory).
        retention_sweep(&repo, &worktrees, RETENTION_DAYS, 1_000_000_000.0, &quiet);
        assert!(!git_ok(&repo, &["worktree", "list", "--porcelain"]).contains("gone"));
        assert!(branch_exists(&repo, "flywheel/_root/gone"));
    }

    #[test]
    fn unregistered_aged_directory_is_removed_from_disk() {
        // A directory git does not know about (an operator's stray copy)
        // cannot be `worktree remove`d; the sweep deletes it directly.
        let (_dir, repo) = repo();
        let worktrees = worktrees_dir(&repo);
        let stray = worktrees.join("stray");
        fs::create_dir_all(&stray).unwrap();
        fs::write(stray.join("x"), "x").unwrap();
        let now = 1_000_000_000.0;
        set_mtime(&stray, now - (WINDOW + DAY));
        retention_sweep(&repo, &worktrees, RETENTION_DAYS, now, &quiet);
        assert!(!stray.exists());
    }

    #[test]
    fn absent_worktrees_dir_is_a_noop() {
        let (_dir, repo) = repo();
        retention_sweep(
            &repo,
            &repo.join("nowhere"),
            RETENTION_DAYS,
            1_000_000_000.0,
            &quiet,
        );
    }

    // --- retention targets ---------------------------------------------------------------

    #[test]
    fn retention_targets_single_when_root_is_the_legacy_default() {
        let (_dir, repo) = repo();
        let worktrees = worktrees_dir(&repo);
        assert_eq!(retention_targets(&repo, &worktrees), vec![worktrees]);
    }

    #[test]
    fn retention_targets_include_populated_legacy_location() {
        let (dir, repo) = repo();
        let legacy = worktrees_dir(&repo);
        let relocated = dir.path().join("elsewhere").join("worktrees");
        fs::create_dir_all(&relocated).unwrap();
        assert_eq!(
            retention_targets(&repo, &relocated),
            vec![relocated, legacy]
        );
    }

    #[test]
    fn retention_targets_skip_absent_legacy_location() {
        let (dir, repo) = repo();
        let relocated = dir.path().join("elsewhere").join("worktrees");
        fs::create_dir_all(&relocated).unwrap();
        assert_eq!(retention_targets(&repo, &relocated), vec![relocated]);
    }

    #[test]
    fn retention_targets_dedupe_a_symlinked_legacy_root() {
        // The active root reached through a symlink to the legacy dir is
        // the same directory: it is swept once, not twice.
        let (dir, repo) = repo();
        let legacy = worktrees_dir(&repo);
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&legacy, &link).unwrap();
        assert_eq!(retention_targets(&repo, &link), vec![link]);
    }

    #[test]
    fn sweep_reclaims_aged_legacy_worktree_after_relocation() {
        let (dir, repo) = repo();
        let legacy = worktrees_dir(&repo);
        let relocated = dir.path().join("elsewhere").join("worktrees");
        fs::create_dir_all(&relocated).unwrap();
        let stranded = add_worktree(&repo, &legacy, "stranded");
        let now = 1_000_000_000.0;
        set_mtime(&stranded, now - (WINDOW + DAY));

        for target in retention_targets(&repo, &relocated) {
            retention_cadence_tick(&repo, &target, RETENTION_DAYS, &quiet, &|| now);
        }

        assert!(!stranded.exists());
        assert!(!branch_exists(&repo, "flywheel/_root/stranded"));
    }
}

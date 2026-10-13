//! Port of the Python `flywheel._autopilot_supervisor` module: the
//! console's subprocess supervisor for the operator-started autopilot
//! daemon.
//!
//! The console keeps a second supervised child running underneath it,
//! beside the git-worktree worker: the autopilot intake daemon, launched as
//! a `flywheel autopilot` child with NO `--once` so it is the neverending
//! loop. This module is deliberately the same shape as
//! [`WorkerSupervisor`](crate::worker_supervisor::WorkerSupervisor) (it
//! shares the spawn / detach / stop core) so the console gains an autopilot
//! status the way it shows worker status.
//!
//! The one difference from the worker supervisor: autopilot writes no
//! `task_claims` lease. Cross-process liveness instead rides on the daemon's
//! activity snapshot, which doubles as a **liveness record** -- it carries
//! the daemon `pid` plus an `expires_at` freshness deadline the daemon pushes
//! forward every cycle. A second supervisor reads that record
//! (`read_live_activity`) and, finding a live daemon it does not own, adopts
//! it -- reporting `Detached` and spawning nothing, exactly as the worker
//! supervisor treats a live foreign lease. A *stale* record (`expires_at` at
//! or before now) reads as not-live by the identical rule the worker uses
//! for a lapsed lease, so a dead daemon's leftover record does not block a
//! respawn. A detached daemon keeps running after the console exits; the
//! next console adopts it via its record.

use crate::tui::{DaemonState, DaemonStatus};
use crate::worker_supervisor::{DEFAULT_MAX_LOG_BYTES, SupervisedProcess, console_program};
use flywheel_orchestrator::autopilot_activity::{
    AutopilotActivity, read_activity, read_live_activity,
};
use flywheel_orchestrator::supervision_policy::SupervisionPolicy;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where the autopilot supervisor's logs (and the activity record) land
/// by default.
pub const DEFAULT_AUTOPILOT_LOG_DIR: &str = ".flywheel/logs/autopilot";

/// Every supervisor log is `autopilot-supervisor-<ts>-<seq>.log` under the
/// log dir; the reclaim only ever touches files carrying this prefix,
/// never the sibling `activity.json`.
pub const LOG_FILENAME_PREFIX: &str = "autopilot-supervisor-";

/// The activity record's filename inside the log dir.
pub const ACTIVITY_FILENAME: &str = "activity.json";

/// Compose the `<program> autopilot [--tasks-dir D] [--model M]
/// [--activity-file F]` argv.
///
/// The spawned process is the neverending daemon: the argv carries NO
/// `--once` flag -- the console must launch the continuous daemon, not a
/// single pass. `tasks_dir` / `model` are forwarded when set, mirroring the
/// worker supervisor's default argv; `activity_file` is the liveness record
/// the supervisor reads back.
pub fn build_autopilot_spawn_argv(
    program: &Path,
    tasks_dir: Option<&Path>,
    model: Option<&str>,
    activity_file: Option<&Path>,
) -> Vec<String> {
    let mut argv = vec![
        program.to_string_lossy().into_owned(),
        "autopilot".to_owned(),
    ];
    if let Some(dir) = tasks_dir {
        argv.push("--tasks-dir".to_owned());
        argv.push(dir.to_string_lossy().into_owned());
    }
    if let Some(model) = model {
        argv.push("--model".to_owned());
        argv.push(model.to_owned());
    }
    if let Some(file) = activity_file {
        argv.push("--activity-file".to_owned());
        argv.push(file.to_string_lossy().into_owned());
    }
    argv
}

/// Spawn, detach from, and stop one autopilot daemon child.
///
/// Construction spawns nothing; call [`start`](Self::start). The supervisor
/// manages only the child it spawned and is independent of the worker
/// supervisor -- starting or stopping autopilot never touches the worker,
/// and vice versa.
pub struct AutopilotSupervisor {
    tasks_dir: Option<PathBuf>,
    model: Option<String>,
    activity_override: Option<PathBuf>,
    spawn_argv: Option<Vec<String>>,
    process: SupervisedProcess,
}

impl std::fmt::Debug for AutopilotSupervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutopilotSupervisor")
            .field("tasks_dir", &self.tasks_dir)
            .field("model", &self.model)
            .field("activity_path", &self.activity_path())
            .field("spawn_argv", &self.spawn_argv)
            .field("process", &self.process)
            .finish()
    }
}

impl Default for AutopilotSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl AutopilotSupervisor {
    /// A supervisor logging under [`DEFAULT_AUTOPILOT_LOG_DIR`], with the
    /// activity record beside the logs, no crash-loop policy, and the
    /// default-on log ceiling.
    pub fn new() -> Self {
        Self {
            tasks_dir: None,
            model: None,
            activity_override: None,
            spawn_argv: None,
            process: SupervisedProcess::new(
                "autopilot",
                PathBuf::from(DEFAULT_AUTOPILOT_LOG_DIR),
                LOG_FILENAME_PREFIX,
                DEFAULT_MAX_LOG_BYTES,
                None,
            ),
        }
    }

    pub fn with_log_dir(mut self, log_dir: impl Into<PathBuf>) -> Self {
        self.process.set_log_dir(log_dir.into());
        self
    }

    /// Override the spawn command (tests pass a trivial child).
    pub fn with_spawn_argv(mut self, argv: Vec<String>) -> Self {
        self.spawn_argv = Some(argv);
        self
    }

    pub fn with_tasks_dir(mut self, tasks_dir: Option<PathBuf>) -> Self {
        self.tasks_dir = tasks_dir;
        self
    }

    pub fn with_model(mut self, model: Option<String>) -> Self {
        self.model = model;
        self
    }

    /// Where the daemon writes (and this supervisor reads) the activity
    /// record; defaults to `activity.json` inside the log dir.
    pub fn with_activity_path(mut self, activity_path: impl Into<PathBuf>) -> Self {
        self.activity_override = Some(activity_path.into());
        self
    }

    pub fn with_policy(mut self, policy: SupervisionPolicy) -> Self {
        self.process.set_policy(Some(policy));
        self
    }

    pub fn with_max_log_bytes(mut self, max_log_bytes: u64) -> Self {
        self.process.set_max_log_bytes(max_log_bytes);
        self
    }

    pub fn log_dir(&self) -> &Path {
        self.process.log_dir()
    }

    pub fn max_log_bytes(&self) -> u64 {
        self.process.max_log_bytes()
    }

    /// The activity record both ends agree on: the supervisor owns the
    /// path and hands it to the child via `--activity-file`.
    pub fn activity_path(&self) -> PathBuf {
        self.activity_override
            .clone()
            .unwrap_or_else(|| self.process.log_dir().join(ACTIVITY_FILENAME))
    }

    /// The argv the next spawn launches.
    pub fn spawn_argv(&self) -> Vec<String> {
        self.spawn_argv.clone().unwrap_or_else(|| {
            build_autopilot_spawn_argv(
                &console_program(),
                self.tasks_dir.as_deref(),
                self.model.as_deref(),
                Some(&self.activity_path()),
            )
        })
    }

    /// Compute the current status.
    ///
    /// A live owned child reports `Supervised` (carrying the activity record
    /// only when its pid matches the child -- a stale file from a previous
    /// daemon is ignored); a child that exited on its own reads `Dead` (or
    /// is respawned under the policy). With no owned child, a live record
    /// from a daemon this console does not own reports `Detached` with that
    /// daemon's pid and record (adopt, spawn nothing); otherwise `None` (or
    /// `Error` after a failed spawn). Local state resolves first, so a
    /// just-died owned child's own lapsing record never masks the death.
    pub fn status(&mut self) -> DaemonStatus {
        let argv = self.spawn_argv();
        let activity_path = self.activity_path();
        if let Some(owned) = self.process.owned_status(&argv) {
            if owned.state == DaemonState::Supervised
                && let Some(activity) = owned
                    .pid
                    .and_then(|pid| read_owned_activity(&activity_path, pid))
            {
                return owned.with_activity(activity);
            }
            return owned;
        }
        if let Some(live) = read_live_activity(Some(&activity_path), None) {
            let pid = u32::try_from(live.pid).ok();
            let mut status = DaemonStatus::new(DaemonState::Detached);
            status.pid = pid;
            return status.with_activity(live);
        }
        DaemonStatus::new(DaemonState::None)
    }

    /// Whether this supervisor has a live child it spawned.
    pub fn owns_supervised_child(&mut self) -> bool {
        self.status().state == DaemonState::Supervised
    }

    /// Spawn the autopilot daemon if none is owned; idempotent otherwise.
    ///
    /// Already `Supervised` returns unchanged without spawning a second
    /// daemon. `Detached` -- a live daemon it does not own holds a fresh
    /// liveness record -- likewise returns unchanged, adopting that daemon
    /// instead of duplicating it. Leftover dead / error markers are cleared
    /// and the record re-read first, so a live record is never missed
    /// behind a stale `Dead`.
    pub fn start(&mut self) -> DaemonStatus {
        let current = self.status();
        if matches!(
            current.state,
            DaemonState::Supervised | DaemonState::Detached
        ) {
            return current;
        }
        self.process.clear_markers();
        let rechecked = self.status();
        if rechecked.state == DaemonState::Detached {
            return rechecked;
        }
        let argv = self.spawn_argv();
        self.process.arm_and_spawn(&argv)
    }

    /// Stop the supervised daemon and everything it spawned (group SIGTERM,
    /// escalating to SIGKILL past `timeout`).
    pub fn stop(&mut self, timeout: Duration) -> bool {
        self.process.stop(timeout)
    }

    /// Forget the supervised child; the detached daemon survives console
    /// exit and the next console adopts it via its record.
    pub fn detach(&mut self) {
        self.process.detach();
    }

    pub fn close(&mut self) {
        self.detach();
    }

    pub fn death_reason(&self) -> Option<String> {
        self.process.death_reason()
    }

    pub fn child_pid(&self) -> Option<u32> {
        self.process.child_pid()
    }

    pub fn wait_for_child_exit(&mut self, timeout: Duration) -> bool {
        self.process.wait_for_exit(timeout)
    }
}

/// Read the activity snapshot iff it belongs to the owned child.
///
/// Guards against a stale `activity.json` left by a previous daemon: a
/// snapshot whose `pid` does not match the child this supervisor spawned is
/// treated as absent, so the status surface shows plain liveness until the
/// live daemon records its first cycle.
fn read_owned_activity(activity_path: &Path, child_pid: u32) -> Option<AutopilotActivity> {
    let activity = read_activity(Some(activity_path))?;
    (activity.pid == i64::from(child_pid)).then_some(activity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker_supervisor::test_support::*;
    use flywheel_orchestrator::autopilot_activity::{PHASE_IDLE, epoch_now, write_activity};
    use flywheel_orchestrator::supervision_policy::SupervisionBudget;
    use std::fs;
    use tempfile::TempDir;

    fn supervisor(tmp: &TempDir, spawn: Vec<String>) -> AutopilotSupervisor {
        AutopilotSupervisor::new()
            .with_log_dir(tmp.path())
            .with_activity_path(tmp.path().join("activity.json"))
            .with_spawn_argv(spawn)
    }

    fn write_record(path: &Path, pid: i64, expires_at: f64) {
        let mut activity = AutopilotActivity::new(pid, PHASE_IDLE, 3, epoch_now(), 300.0);
        activity.next_cycle_at = Some(expires_at);
        activity.expires_at = Some(expires_at);
        write_activity(path, &activity).unwrap();
    }

    fn supervisor_logs(dir: &Path) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with(LOG_FILENAME_PREFIX))
            })
            .collect();
        paths.sort();
        paths
    }

    #[test]
    fn spawn_argv_targets_the_daemon_not_a_single_pass() {
        let argv = build_autopilot_spawn_argv(Path::new("/opt/fw"), None, None, None);
        assert_eq!(argv, vec!["/opt/fw".to_owned(), "autopilot".to_owned()]);
        assert!(!argv.iter().any(|a| a == "--once"));
        assert!(!argv.iter().any(|a| a == "worker"));
    }

    #[test]
    fn spawn_argv_forwards_tasks_dir_model_and_activity_file() {
        let argv = build_autopilot_spawn_argv(
            Path::new("fw"),
            Some(Path::new("/t/tasks")),
            Some("claude-opus-4"),
            Some(Path::new("/t/activity.json")),
        );
        assert_eq!(
            argv,
            vec![
                "fw",
                "autopilot",
                "--tasks-dir",
                "/t/tasks",
                "--model",
                "claude-opus-4",
                "--activity-file",
                "/t/activity.json"
            ]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn default_argv_wires_the_activity_file_beside_the_logs() {
        let tmp = TempDir::new().unwrap();
        let sup = AutopilotSupervisor::new().with_log_dir(tmp.path());
        let argv = sup.spawn_argv();
        let position = argv.iter().position(|a| a == "--activity-file").unwrap();
        assert_eq!(
            argv[position + 1],
            tmp.path().join(ACTIVITY_FILENAME).display().to_string()
        );
        assert_eq!(argv[1], "autopilot");
    }

    #[test]
    fn start_spawns_a_live_child_idempotently_and_stop_signals_it() {
        let tmp = TempDir::new().unwrap();
        let mut sup = supervisor(&tmp, sleep_argv(60));
        let started = sup.start();
        assert_eq!(started.state, DaemonState::Supervised);
        let pid = started.pid.unwrap();
        assert!(process_alive(pid));
        assert_eq!(sup.start().pid, Some(pid));
        assert_eq!(supervisor_logs(tmp.path()).len(), 1);
        assert!(sup.stop(Duration::from_secs(5)));
        assert!(wait_dead(pid, Duration::from_secs(5)));
        assert_eq!(sup.status().state, DaemonState::Dead);
        assert!(!sup.stop(Duration::from_secs(1)));
    }

    #[test]
    fn detached_child_survives_and_status_falls_back_to_the_record() {
        let tmp = TempDir::new().unwrap();
        let mut sup = supervisor(&tmp, sleep_argv(60));
        let pid = sup.start().pid.unwrap();
        sup.detach();
        assert_eq!(sup.status().state, DaemonState::None);
        assert!(process_alive(pid));
        let _ = std::process::Command::new("kill")
            .arg("-9")
            .arg(pid.to_string())
            .status();
    }

    #[test]
    fn status_adopts_a_live_foreign_record_as_detached() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("activity.json");
        write_record(&path, 9999, epoch_now() + 3600.0);
        let mut sup = supervisor(&tmp, sleep_argv(60));
        let status = sup.status();
        assert_eq!(status.state, DaemonState::Detached);
        assert_eq!(status.pid, Some(9999));
        assert_eq!(status.activity.as_ref().map(|a| a.pid), Some(9999));
        assert!(!sup.owns_supervised_child());
    }

    #[test]
    fn start_adopts_a_live_record_and_spawns_nothing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("activity.json");
        write_record(&path, 9999, epoch_now() + 3600.0);
        let sentinel = tmp.path().join("spawned.marker");
        let mut sup = supervisor(
            &tmp,
            argv(&["sh", "-c", &format!("touch '{}'", sentinel.display())]),
        );
        assert_eq!(sup.start().state, DaemonState::Detached);
        std::thread::sleep(Duration::from_millis(100));
        assert!(!sentinel.exists());
    }

    #[test]
    fn stale_record_does_not_block_a_respawn_and_missing_reads_none() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("activity.json");
        let mut sup = supervisor(&tmp, sleep_argv(60));
        assert_eq!(sup.status().state, DaemonState::None);
        write_record(&path, 9999, epoch_now() - 3600.0);
        let status = sup.start();
        assert_eq!(status.state, DaemonState::Supervised);
        assert!(process_alive(status.pid.unwrap()));
        assert!(sup.stop(Duration::from_secs(5)));
    }

    #[test]
    fn owned_death_takes_precedence_over_a_live_foreign_record() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("activity.json");
        let mut sup = supervisor(&tmp, sleep_argv(60));
        let pid = sup.start().pid.unwrap();
        assert!(sup.stop(Duration::from_secs(5)));
        write_record(&path, 9999, epoch_now() + 3600.0);
        let status = sup.status();
        assert_eq!(status.state, DaemonState::Dead);
        assert_eq!(status.pid, Some(pid));
        // After a detach the foreign record is adopted.
        sup.detach();
        assert_eq!(sup.status().state, DaemonState::Detached);
    }

    #[test]
    fn status_carries_the_owned_activity_only_when_the_pid_matches() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("activity.json");
        let mut sup = supervisor(&tmp, sleep_argv(60));
        let pid = sup.start().pid.unwrap();
        assert_eq!(
            sup.status().activity,
            None,
            "no record yet: plain supervised"
        );
        write_record(&path, 4242, epoch_now() + 3600.0);
        assert_eq!(
            sup.status().activity,
            None,
            "a stale foreign record is ignored"
        );
        write_record(&path, i64::from(pid), epoch_now() + 3600.0);
        let status = sup.status();
        assert_eq!(status.state, DaemonState::Supervised);
        assert_eq!(status.activity.as_ref().map(|a| a.cycle_index), Some(3));
        assert!(sup.stop(Duration::from_secs(5)));
        assert_eq!(
            sup.status().activity,
            None,
            "a dead child drops the activity"
        );
    }

    #[test]
    fn dead_status_surfaces_the_captured_reason() {
        let tmp = TempDir::new().unwrap();
        let reason = "flywheel autopilot: policy error: [autopilot] target_depth must be positive";
        let mut sup = supervisor(&tmp, dying_argv(reason, 2));
        sup.start();
        assert!(sup.wait_for_child_exit(Duration::from_secs(5)));
        let dead = sup.status();
        assert_eq!(dead.state, DaemonState::Dead);
        assert_eq!(dead.message.as_deref(), Some(&*format!("exit=2: {reason}")));
        assert_eq!(sup.death_reason(), dead.message);
    }

    #[test]
    fn respawn_within_window_and_exhaustion_match_the_worker() {
        let tmp = TempDir::new().unwrap();
        let policy = SupervisionPolicy::with_clock(
            SupervisionBudget::new(1, 1000.0).unwrap(),
            Box::new(|| 0.0),
        );
        let mut sup = supervisor(&tmp, exit_argv(3)).with_policy(policy);
        let first = sup.start();
        assert!(sup.wait_for_child_exit(Duration::from_secs(5)));
        let respawned = sup.status();
        assert_eq!(respawned.state, DaemonState::Supervised);
        assert_ne!(respawned.pid, first.pid);
        assert!(sup.wait_for_child_exit(Duration::from_secs(5)));
        let exhausted = sup.status();
        assert_eq!(exhausted.state, DaemonState::DeadAfterBudget);
        assert_eq!(exhausted.message.as_deref(), Some("exit=3"));
    }

    #[test]
    fn repeated_starts_hold_the_log_directory_under_the_ceiling() {
        let tmp = TempDir::new().unwrap();
        let sup = supervisor(&tmp, argv(&["true"])).with_max_log_bytes(400);
        assert_eq!(sup.max_log_bytes(), 400);
        let defaults = AutopilotSupervisor::new();
        assert_eq!(defaults.max_log_bytes(), DEFAULT_MAX_LOG_BYTES);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stop_force_kills_a_sigterm_ignoring_group() {
        let tmp = TempDir::new().unwrap();
        let pidfile = tmp.path().join("grandchild.pid");
        let mut sup = supervisor(&tmp, ignore_term_with_child_argv(&pidfile));
        let child_pid = sup.start().pid.unwrap();
        let grandchild_pid = read_pid(&pidfile, Duration::from_secs(5));
        assert!(process_alive(child_pid) && process_alive(grandchild_pid));
        assert!(sup.stop(Duration::from_millis(500)));
        assert!(wait_dead(child_pid, Duration::from_secs(5)));
        assert!(wait_dead(grandchild_pid, Duration::from_secs(5)));
        assert_eq!(sup.status().state, DaemonState::Dead);
    }
}

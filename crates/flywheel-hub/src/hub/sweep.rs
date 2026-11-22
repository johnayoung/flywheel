//! The hub's side of the build sweep (see [`crate::sweep`]): the repos it
//! knows (workspace folders, sessions' and adopted worktrees), the
//! sessions that keep a worktree, the automatic look at the disks every
//! [`CHECK_EVERY`], and each deletion into the activity journal and the
//! event log.

use super::Hub;
use crate::state::SessionState;
use crate::sweep::{self, LastSweep, Look, Report, SHORT_EVENT, SWEPT_EVENT, disk::DiskUse};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// The first automatic look, after the hub starts.
const FIRST_LOOK: Duration = Duration::from_secs(60);

/// What the hub keeps about sweeps.
#[derive(Debug, Default)]
pub(crate) struct SweepRuntime {
    /// One sweep at a time (automatic or asked for).
    running: Mutex<()>,
    last: Mutex<Option<LastSweep>>,
    /// The disks ran out of candidates and that was said: said again only
    /// after a sweep that was not short.
    told_short: AtomicBool,
}

impl SweepRuntime {
    pub(crate) fn load(state_dir: &Path) -> Self {
        Self {
            last: Mutex::new(sweep::load_last(state_dir)),
            ..Self::default()
        }
    }
}

impl Hub {
    /// The workspace folders, and every repo the hub has a worktree of.
    fn sweep_roots(&self) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let store = self.store();
        let folders: Vec<PathBuf> = store
            .workspaces()
            .iter()
            .flat_map(|w| w.folders.iter().map(|f| f.path.clone()))
            .collect();
        let mut repos = folders.clone();
        let tracked = store
            .sessions()
            .iter()
            .filter_map(|s| s.worktree.as_ref())
            .chain(store.changes().iter().map(|c| &c.worktree));
        for worktree in tracked {
            if !repos.contains(&worktree.repo) {
                repos.push(worktree.repo.clone());
            }
        }
        (folders, repos)
    }

    /// The first session that has not exited working in `dir` (canonical)
    /// or below it, as `name (id)`.
    pub(crate) fn session_in(&self, dir: &Path) -> Option<String> {
        let store = self.store();
        store
            .sessions()
            .iter()
            .filter(|s| !matches!(s.state, SessionState::Exited { .. }))
            .find(|s| {
                let cwd = s.cwd.canonicalize().unwrap_or_else(|_| s.cwd.clone());
                cwd.starts_with(dir) || s.worktree.as_ref().is_some_and(|w| w.path.starts_with(dir))
            })
            .map(|s| format!("session {} ({})", s.name, s.id))
    }

    /// The filesystems the workspace folders are on, now.
    pub fn workspace_disks(&self) -> Vec<DiskUse> {
        let (folders, _) = self.sweep_roots();
        let mut disks: Vec<DiskUse> = Vec::new();
        for folder in folders {
            let Ok(dev) = sweep::disk::device(&folder) else {
                continue;
            };
            if disks.iter().any(|d| d.dev == dev) {
                continue;
            }
            if let Ok(d) = sweep::disk::disk_use(&folder) {
                disks.push(d);
            }
        }
        disks
    }

    pub fn last_sweep(&self) -> Option<LastSweep> {
        self.sweep
            .last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Sweep now: `all` takes every eligible candidate, else the watermark
    /// rule; `dry_run` deletes nothing. `by`: `auto` or `manual`. Blocking
    /// (git, `/proc`, and directory walks): call off the async executor.
    pub fn sweep(&self, all: bool, dry_run: bool, by: &str) -> Report {
        let _one = self
            .sweep
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let config = self.options.build_sweep;
        let (folders, repos) = self.sweep_roots();
        let sessions = |dir: &Path| self.session_in(dir);
        let mut report = sweep::survey(&Look {
            repos: &repos,
            folders: &folders,
            config,
            sessions: &sessions,
            all,
        });
        if dry_run {
            return report;
        }
        sweep::execute(&mut report, config, &sessions, &mut |gone| {
            let data = json!({
                "path": gone.path,
                "target": gone.target,
                "bytes": gone.bytes,
                "reason": gone.why.as_str(),
                "by": by,
            });
            self.log_hub_event(SWEPT_EVENT, None, data);
            self.write_activity(
                crate::activity::Draft::new(
                    SWEPT_EVENT,
                    format!(
                        "deleted {}/target ({}, {})",
                        gone.path.display(),
                        crate::util::human_bytes(gone.bytes),
                        gone.why.as_str()
                    ),
                )
                .with("path", gone.path.to_string_lossy().into_owned())
                .with("bytes", gone.bytes)
                .with("reason", gone.why.as_str()),
            );
        });
        if report.short {
            if !self.sweep.told_short.swap(true, Ordering::SeqCst) {
                let full: Vec<String> = report
                    .disks
                    .iter()
                    .filter(|d| d.used_pct >= f64::from(config.high_watermark))
                    .map(|d| d.mount.as_ref().unwrap_or(&d.path).display().to_string())
                    .collect();
                let text = format!(
                    "disk past {}% with no build output left to sweep: {}",
                    config.high_watermark,
                    full.join(", ")
                );
                self.log_hub_event(SHORT_EVENT, None, json!({ "disks": full }));
                self.write_activity(
                    crate::activity::Draft::new(SHORT_EVENT, text)
                        .with("count", u64::try_from(full.len()).unwrap_or(0)),
                );
            }
        } else {
            self.sweep.told_short.store(false, Ordering::SeqCst);
        }
        let last = LastSweep {
            at: report.at,
            by: by.to_owned(),
            deleted: report.deleted.len(),
            reclaimed_bytes: report.reclaimed_bytes,
            short: report.short,
        };
        if let Err(e) = sweep::save_last(&self.options.state_dir, &last) {
            eprintln!("fw hub: build sweep: cannot keep its record: {e}");
        }
        *self
            .sweep
            .last
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(last);
        report
    }

    /// One automatic look: sweep when a workspace folder's filesystem is
    /// past the high watermark (or was short last time and still is).
    fn sweep_tick(&self) {
        let high = f64::from(self.options.build_sweep.high_watermark);
        if self.workspace_disks().iter().any(|d| d.used_pct >= high) {
            let report = self.sweep(false, false, "auto");
            if !report.deleted.is_empty() {
                eprintln!("fw hub: build sweep: {}", sweep::summary(&report));
            }
        } else {
            self.sweep.told_short.store(false, Ordering::SeqCst);
        }
    }

    /// Look at the disks every `build_sweep_every` (its own thread), with
    /// automatic sweeps on.
    pub(crate) fn start_sweep(self: &Arc<Self>) {
        if !self.options.build_sweep.enabled {
            return;
        }
        let every = self.options.build_sweep_every;
        let weak = Arc::downgrade(self);
        let spawned = std::thread::Builder::new()
            .name("hub-sweep".into())
            .spawn(move || {
                let mut next = Instant::now() + FIRST_LOOK.min(every);
                loop {
                    std::thread::sleep(Duration::from_secs(1).min(every));
                    let Some(hub) = weak.upgrade() else {
                        break;
                    };
                    if hub.queue_runtime().is_stopped() {
                        break;
                    }
                    if Instant::now() >= next {
                        hub.sweep_tick();
                        next = Instant::now() + every;
                    }
                }
            });
        if let Err(e) = spawned {
            eprintln!("fw hub: build sweep: no automatic sweeps: cannot start its thread: {e}");
        }
    }
}

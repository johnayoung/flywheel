//! The grader runner seam: what to grade, cancellation, and the shared
//! grading control (pause, resume, stop) the engine and runner obey.

use super::{GraderRun, GraderSpec};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

/// What to grade: the candidate's worktree and the fan-out's graders.
#[derive(Debug, Clone)]
pub struct GradeRequest<'a> {
    pub dir: &'a Path,
    pub graders: &'a [GraderSpec],
    /// Per grader.
    pub timeout: Duration,
    /// Kept of each stream.
    pub tail_bytes: usize,
}

/// Set to stop a grading (its graders are killed; the result is dropped).
#[derive(Debug, Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// Why a runner returned no verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerError {
    /// [`Cancel`] was set: the graders were killed.
    Cancelled,
    /// The runner itself failed (it could not start a runtime, ...).
    Failed(String),
}

/// Runs grader commands with flywheel's command-grader semantics: in list
/// order, each `/bin/sh -c <run>` in `dir` in its own process group, pass =
/// exit 0, the group killed at `timeout`, and the first grader that does
/// not pass ends the run (the rest are not returned). Implemented by the
/// embedding binary over flywheel-core (see the module doc).
pub trait GraderRunner: Send + Sync + 'static {
    /// Shown on the board: what ran the graders.
    fn name(&self) -> String;

    /// Run `request`'s graders; stop (killing them) once `cancel` is set.
    fn run(
        &self,
        request: &GradeRequest<'_>,
        cancel: &Cancel,
    ) -> Result<Vec<GraderRun>, RunnerError>;
}

/// Pauses, resumes, and stops the hub's gradings: a live handoff pauses
/// them (cancelling what runs, to run again in whichever hub carries on)
/// and a shutdown stops them. Held by the hub.
#[derive(Debug, Default)]
pub struct GradingControl {
    state: Mutex<ControlState>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct ControlState {
    paused: bool,
    stopped: bool,
    active: Vec<Cancel>,
}

impl GradingControl {
    fn lock(&self) -> std::sync::MutexGuard<'_, ControlState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Cancel what runs and hold new gradings until [`Self::resume`].
    pub fn pause(&self) {
        let mut state = self.lock();
        state.paused = true;
        for cancel in &state.active {
            cancel.cancel();
        }
    }

    pub fn resume(&self) {
        self.lock().paused = false;
        self.changed.notify_all();
    }

    /// Cancel what runs and start nothing more.
    pub fn stop(&self) {
        let mut state = self.lock();
        state.stopped = true;
        for cancel in &state.active {
            cancel.cancel();
        }
        drop(state);
        self.changed.notify_all();
    }

    pub fn is_stopped(&self) -> bool {
        self.lock().stopped
    }

    /// A token for a grading about to start, once not paused (`None`:
    /// stopped, or still paused after `wait`).
    pub(crate) fn begin(&self, wait: Duration) -> Option<Cancel> {
        let mut state = self.lock();
        let deadline = std::time::Instant::now() + wait;
        while state.paused && !state.stopped {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return None;
            }
            state = self
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        if state.stopped || state.paused {
            return None;
        }
        let cancel = Cancel::new();
        state.active.push(cancel.clone());
        Some(cancel)
    }

    pub(crate) fn end(&self, cancel: &Cancel) {
        self.lock().active.retain(|c| !c.same(cancel));
    }
}

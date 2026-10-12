//! In-process reactive notification for the per-run audit log.
//!
//! A [`RunNotifier`] is a *doorbell with a watermark*, not a delivery
//! truck. A producer (a store) calls [`RunNotifier::notify`] with the
//! per-run `sequence` it just committed; a consumer calls
//! [`RunNotifier::wait`] and, once woken, re-reads the authoritative records
//! from its source. The notification carries only a watermark, never a
//! payload, so a dropped or coalesced signal costs latency, never
//! correctness -- every `wait` is bounded by a timeout, so the consumer makes
//! progress even if a notification is missed entirely.
//!
//! Scope: in-process only. A wakeup bridges a producer and a consumer only
//! when they share the same `RunNotifier` instance (in practice, the same
//! store object). Separate store instances or separate processes fall back
//! to the consumer's poll timeout. Cross-process push is a later,
//! backend-specific layer (e.g. Postgres `LISTEN`/`NOTIFY`).
//!
//! Thread-safety: every method is safe to call from any thread, and the
//! notifier is cheap to share behind an `Arc`. Producers typically run on
//! the harness thread; consumers on a follower's thread.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// Per-run condition variable and the highest sequence seen so far.
#[derive(Debug, Default)]
struct RunState {
    watermark: Mutex<u64>,
    cond: Condvar,
}

impl RunState {
    /// Lock the watermark, tolerating a poisoned lock: the guarded value is
    /// a plain integer that no panic can leave half-written.
    fn lock(&self) -> MutexGuard<'_, u64> {
        self.watermark
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Per-run sequence watermarks with condition-variable wakeups.
#[derive(Debug, Default)]
pub struct RunNotifier {
    runs: Mutex<HashMap<String, Arc<RunState>>>,
}

impl RunNotifier {
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self, run_id: &str) -> Arc<RunState> {
        let mut runs = self
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(state) = runs.get(run_id) {
            return Arc::clone(state);
        }
        let state = Arc::new(RunState::default());
        runs.insert(run_id.to_owned(), Arc::clone(&state));
        state
    }

    /// Advance `run_id`'s watermark to `sequence` and wake waiters.
    ///
    /// Monotonic: an out-of-order or duplicate `sequence` never lowers the
    /// watermark. Waiters are always notified so a consumer that passed a
    /// stale `after` re-checks promptly.
    pub fn notify(&self, run_id: &str, sequence: u64) {
        let state = self.state(run_id);
        let mut watermark = state.lock();
        if sequence > *watermark {
            *watermark = sequence;
        }
        state.cond.notify_all();
    }

    /// Block until `run_id`'s watermark exceeds `after` or `timeout`
    /// elapses; return the current watermark.
    ///
    /// Returning the observed watermark lets the caller advance its own
    /// `after` past sequences it has been woken for -- including
    /// state-bearing domain events that the audit stream does not surface
    /// -- so a subsequent `wait` does not return immediately in a spin.
    pub fn wait(&self, run_id: &str, after: u64, timeout: Duration) -> u64 {
        let state = self.state(run_id);
        let watermark = state.lock();
        if *watermark > after {
            return *watermark;
        }
        let (watermark, _timed_out) = state
            .cond
            .wait_timeout(watermark, timeout)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *watermark
    }

    /// Wake any waiters on `run_id` without advancing the watermark.
    ///
    /// Used to make a blocked consumer re-check a condition other than the
    /// watermark -- e.g. its own stop flag -- promptly, so cancellation does
    /// not have to wait out the consumer's timeout. A woken `wait` returns
    /// the unchanged watermark.
    pub fn wake(&self, run_id: &str) {
        let state = self.state(run_id);
        let _guard = state.lock();
        state.cond.notify_all();
    }

    /// Drop the cached state for `run_id` to bound memory.
    ///
    /// Optional cleanup for long-lived processes. A waiter blocked on the
    /// forgotten state simply times out (a later `notify` allocates a fresh
    /// state); callers that race `forget` against live consumers accept that
    /// bounded-latency miss.
    pub fn forget(&self, run_id: &str) {
        let mut runs = self
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        runs.remove(run_id);
    }

    /// The current watermark for `run_id` without blocking (0 if unseen).
    pub fn watermark(&self, run_id: &str) -> u64 {
        let runs = self
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        runs.get(run_id).map_or(0, |state| *state.lock())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Instant;

    #[test]
    fn wait_returns_immediately_when_watermark_already_ahead() {
        let notifier = RunNotifier::new();
        notifier.notify("r1", 3);
        let start = Instant::now();
        assert_eq!(notifier.wait("r1", 1, Duration::from_secs(5)), 3);
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn wait_times_out_and_returns_current_watermark() {
        let notifier = RunNotifier::new();
        let start = Instant::now();
        assert_eq!(notifier.wait("r1", 0, Duration::from_millis(50)), 0);
        assert!(start.elapsed() >= Duration::from_millis(40));
    }

    #[test]
    fn notify_wakes_a_blocked_waiter() {
        let notifier = Arc::new(RunNotifier::new());
        let (entered_tx, entered_rx) = mpsc::channel();
        let waiter = {
            let notifier = Arc::clone(&notifier);
            thread::spawn(move || {
                entered_tx.send(()).unwrap();
                notifier.wait("r1", 0, Duration::from_secs(5))
            })
        };
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        // Give the waiter a beat to block; even if notify races ahead,
        // wait() sees watermark > after and returns without blocking.
        thread::sleep(Duration::from_millis(50));
        let start = Instant::now();
        notifier.notify("r1", 7);
        assert_eq!(waiter.join().unwrap(), 7);
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn wake_unblocks_without_advancing_watermark() {
        let notifier = Arc::new(RunNotifier::new());
        notifier.notify("r1", 2);
        let waiter = {
            let notifier = Arc::clone(&notifier);
            thread::spawn(move || notifier.wait("r1", 2, Duration::from_secs(5)))
        };
        thread::sleep(Duration::from_millis(50));
        let start = Instant::now();
        notifier.wake("r1");
        assert_eq!(waiter.join().unwrap(), 2);
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn watermark_is_monotonic() {
        let notifier = RunNotifier::new();
        notifier.notify("r1", 5);
        notifier.notify("r1", 2);
        assert_eq!(notifier.wait("r1", 0, Duration::from_millis(10)), 5);
        assert_eq!(notifier.watermark("r1"), 5);
    }

    #[test]
    fn watermarks_are_isolated_per_run() {
        let notifier = RunNotifier::new();
        notifier.notify("r1", 5);
        assert_eq!(notifier.wait("r2", 0, Duration::from_millis(10)), 0);
        assert_eq!(notifier.watermark("r2"), 0);
    }

    #[test]
    fn forget_resets_run_state() {
        let notifier = RunNotifier::new();
        notifier.notify("r1", 5);
        notifier.forget("r1");
        assert_eq!(notifier.wait("r1", 0, Duration::from_millis(10)), 0);
    }
}

//! The claim heartbeat: renew a task lease on a timer while its run is in
//! flight.
//!
//! The heartbeat is the worker's liveness proof: as long as the process is
//! alive the lease is renewed, so another worker never reclaims a task that
//! is actually running. It runs on a plain OS thread (the Rust spelling of
//! the Python daemon thread): if the process dies the thread dies with it,
//! the lease lapses, and the task becomes reclaimable. A lost lease (the
//! worker stalled past the window, or clock skew let a peer steal it) stops
//! renewal and raises the `lost` flag so the driving future can stop before
//! landing or reporting a run whose claim a peer now owns.

use crate::claims::{ClaimStore, ClaimStoreError, TaskClaim};
use flywheel_core::invoker_client::Clock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Renews one claim every `interval` seconds until stopped.
pub struct ClaimHeartbeat {
    claims: Arc<dyn ClaimStore>,
    claim: Arc<Mutex<TaskClaim>>,
    lease_seconds: f64,
    now: Clock,
    lost: Arc<AtomicBool>,
    finished: Arc<(Mutex<bool>, Condvar)>,
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
    interval: Duration,
}

impl std::fmt::Debug for ClaimHeartbeat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaimHeartbeat")
            .field(
                "task_id",
                &self.claim.lock().map(|c| c.task_id.clone()).ok(),
            )
            .field("lost", &self.lost.load(Ordering::SeqCst))
            .field("interval", &self.interval)
            .finish()
    }
}

impl ClaimHeartbeat {
    /// Start renewing `claim` against `claims` every `interval` seconds,
    /// extending the lease by `lease_seconds` each time, stamping renewals
    /// with the injected `now`.
    ///
    /// # Errors
    ///
    /// Returns the OS error when the renewal thread cannot be spawned (thread
    /// or memory limits); the caller still owns the lease and must release it.
    pub fn start(
        claims: Arc<dyn ClaimStore>,
        claim: TaskClaim,
        lease_seconds: f64,
        interval: f64,
        now: Clock,
    ) -> std::io::Result<Self> {
        let interval = Duration::from_secs_f64(interval.max(0.001));
        let claim = Arc::new(Mutex::new(claim));
        let lost = Arc::new(AtomicBool::new(false));
        let finished = Arc::new((Mutex::new(false), Condvar::new()));
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let thread = {
            let claims = Arc::clone(&claims);
            let now = Arc::clone(&now);
            let claim = Arc::clone(&claim);
            let lost = Arc::clone(&lost);
            let finished = Arc::clone(&finished);
            let name = format!(
                "flywheel-lease-{}",
                claim.lock().map(|c| c.task_id.clone()).unwrap_or_default()
            );
            thread::Builder::new().name(name).spawn(move || {
                loop {
                    match stop_rx.recv_timeout(interval) {
                        // Stopped, or the heartbeat handle was dropped.
                        Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                        Err(RecvTimeoutError::Timeout) => {}
                    }
                    // Hold the token across the renew so a concurrent
                    // `confirm` never renews the same version (which would
                    // read as a spurious loss).
                    let Ok(mut current) = claim.lock() else {
                        break;
                    };
                    match claims.renew_claim(&current, now(), lease_seconds) {
                        Ok(renewed) => *current = renewed,
                        Err(ClaimStoreError::ClaimLost(_)) => {
                            // Signal the driving future that the lease was
                            // stolen so it stops before writing a terminal
                            // result against a stale version.
                            lost.store(true, Ordering::SeqCst);
                            break;
                        }
                        // Transient store error: retry on the next tick.
                        Err(_) => continue,
                    }
                }
                let (done, cv) = &*finished;
                if let Ok(mut guard) = done.lock() {
                    *guard = true;
                }
                cv.notify_all();
            })?
        };
        Ok(Self {
            claims,
            claim,
            lease_seconds,
            now,
            lost,
            finished,
            stop: Some(stop_tx),
            thread: Some(thread),
            interval,
        })
    }

    /// True once renewal has observed the lease was stolen by a peer.
    pub fn lost(&self) -> bool {
        self.lost.load(Ordering::SeqCst)
    }

    /// Prove the lease is still ours at this instant by renewing it now.
    ///
    /// [`lost`](Self::lost) only reflects the most recent timer tick, so a
    /// peer that stole the lease after that tick (the worker was suspended
    /// past the window, or a peer's clock ran ahead) is invisible to it for
    /// up to a full interval. Call this immediately before an irreversible
    /// step (landing, reporting) so a stolen lease is observed
    /// synchronously. Returns `Ok(false)` (and raises the `lost` flag) when a
    /// peer now owns the claim; a successful renewal also extends the lease
    /// across the step that follows.
    ///
    /// # Errors
    ///
    /// Returns the store error when ownership cannot be established either
    /// way (the token mutex is poisoned, or the renew failed transiently).
    pub fn confirm(&self) -> Result<bool, ClaimStoreError> {
        if self.lost() {
            return Ok(false);
        }
        let mut current = self.claim.lock().map_err(|_| ClaimStoreError::Poisoned)?;
        match self
            .claims
            .renew_claim(&current, (self.now)(), self.lease_seconds)
        {
            Ok(renewed) => {
                *current = renewed;
                Ok(true)
            }
            Err(ClaimStoreError::ClaimLost(_)) => {
                self.lost.store(true, Ordering::SeqCst);
                Ok(false)
            }
            Err(err) => Err(err),
        }
    }

    /// The latest renewed token (for release).
    pub fn latest(&self) -> TaskClaim {
        self.claim
            .lock()
            .map(|c| c.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }

    /// Stop renewing and return the latest token for release.
    ///
    /// Waits at most `interval + 1s` for the thread to wind down (a renew
    /// blocked on a wedged store must not hang the worker); a thread still
    /// alive past that is detached, exactly as the Python `join(timeout)`.
    pub fn stop(mut self) -> TaskClaim {
        self.signal_stop();
        let (done, cv) = &*self.finished;
        if let Ok(guard) = done.lock() {
            let _ = cv.wait_timeout_while(guard, self.interval + Duration::from_secs(1), |d| !*d);
        }
        if let Some(thread) = self.thread.take()
            && thread.is_finished()
        {
            let _ = thread.join();
        }
        self.latest()
    }

    fn signal_stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

impl Drop for ClaimHeartbeat {
    fn drop(&mut self) {
        // A dropped heartbeat (the driving future was cancelled) stops
        // renewing; the lease then lapses on its own if nobody releases it.
        self.signal_stop();
    }
}

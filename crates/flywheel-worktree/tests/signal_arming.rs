//! `arm_signals` installs the worker's shutdown handlers before it returns.
//!
//! The worker claims a task right after arming; a SIGTERM/SIGINT landing in
//! that instant must be an orderly stop, never the default action (instant
//! death holding the claim). The runtime here is a current-thread runtime
//! that is not driven until after the signals are raised, so nothing
//! spawned onto it has run yet: only handlers installed synchronously by
//! `arm_signals` itself can catch them.
//!
//! A dedicated test binary holding a single test: it signals its own
//! process.
#![cfg(target_os = "linux")]

use flywheel_core::audit::StopFlag;
use flywheel_core::harness::ShutdownSignal;
use flywheel_worktree::daemon::arm_signals;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[test]
fn signals_raised_right_after_arming_are_an_orderly_stop() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let stop = StopFlag::new();
    let shutdown = ShutdownSignal::new();
    let lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&lines);
    arm_signals(
        &runtime,
        stop.clone(),
        shutdown.clone(),
        Arc::new(move |line: &str| sink.lock().unwrap().push(line.to_owned())),
    );
    for sig in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: raise(3) on this process; the signal is either handled by
        // the handler under test or (the defect) kills the test binary.
        assert_eq!(unsafe { libc::raise(sig) }, 0);
    }
    // Still alive: the handlers were in place. Now let the watcher run.
    let observed = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(60), shutdown.triggered()).await
    });
    assert!(observed.is_ok(), "the watcher never saw the raised signals");
    assert!(stop.is_set());
    let lines = lines.lock().unwrap();
    assert!(
        lines
            .iter()
            .any(|l| l.contains(&format!("Shutdown requested (signal {})", libc::SIGTERM))),
        "{lines:?}"
    );
}

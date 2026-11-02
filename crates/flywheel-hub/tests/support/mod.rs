//! Helpers shared by the integration tests (each test binary uses a
//! subset).
#![allow(dead_code)]

mod script;

#[allow(unused_imports)] // tests/remote.rs runs no stand-in scripts
pub use script::write_script;

use flywheel_hub::{Hub, HubError, HubOptions};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// `Hub::open`, waiting out a serve lock still held for a hub just shut
/// down on the same state dir. Its lock outlives it while any child
/// another test thread forked meanwhile (PTY spawns) has not yet exec'd:
/// the child inherited the lock's descriptor (close-on-exec only closes
/// it at exec), and a flock belongs to the open file, not the descriptor.
pub fn open_hub(options: HubOptions) -> Arc<Hub> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match Hub::open(options.clone()) {
            Err(HubError::InUse(_)) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            result => return result.unwrap(),
        }
    }
}

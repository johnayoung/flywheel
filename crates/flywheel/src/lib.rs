//! flywheel: the unified product shell, top of the stack.
//!
//! Console scripts `flywheel` and `fw` (one implementation, two entries)
//! route every operator verb and own the operator console (dashboard,
//! session view, persistent input bar). Bundles every lower crate.
//!
//! Authoritative specs: `docs/cli.md`, `docs/autopilot.md`, `docs/workflow.md`.

pub mod autopilot_supervisor;
pub mod cli;
pub mod dashboard;
pub mod docs;
pub mod history_screen;
pub mod quit_prompt;
pub mod session;
pub mod session_screen;
pub mod slash;
pub mod snapshot;
pub mod tui;
pub mod worker_supervisor;

/// Shared entry point for both console scripts: parses argv, routes the
/// verb, and returns the process exit code.
///
/// On the way out it terminates any agent or grader process group still
/// registered (`flywheel_agents::process_groups`): each runs in a group of
/// its own, so nothing else would stop it once this process exits.
pub fn main_entry() -> i32 {
    let code = cli::run(std::env::args().collect());
    flywheel_agents::process_groups::terminate_all_blocking(
        flywheel_agents::hosts::DEFAULT_KILL_GRACE,
    );
    code
}

//! flywheel hub: a web workspace for AI coding agents.
//!
//! The hub server owns every terminal: each session is a real PTY whose
//! process, shell state, and scrollback live in the server, so browsers (a
//! laptop, a phone) attach and detach without disturbing it, and many
//! clients can watch one session at once. Sessions belong to workspaces --
//! a name plus one or more folders (git repos or plain directories).
//!
//! A sibling consumer like `flywheel-worktree`: the `flywheel` crate routes
//! `fw hub <verb>` to [`cli::main`]; nothing below depends on this crate.
//! Operator doc: `docs/hub.md`.

pub mod agent;
pub mod attach;
pub mod auth;
pub mod browser;
pub mod cli;
pub mod commands;
pub mod commands_cli;
pub mod daemon;
pub mod detect;
pub mod endpoint;
pub mod flywheel;
pub mod git;
pub mod handoff;
pub mod hub;
pub mod layout;
pub mod lifecycle;
pub mod notify;
pub mod paths;
pub mod pty;
pub mod remote;
pub mod report;
pub mod restore;
pub mod review;
pub mod review_cli;
pub mod script;
pub mod search;
pub mod search_cli;
pub mod server;
pub mod state;
pub mod status;
pub mod template_cli;
pub mod transcript;
pub mod upgrade;
pub mod usage;
pub mod vscode;
pub mod vt;
pub mod wait;
pub mod web;

/// The integration tests' helpers (stand-in scripts), for unit tests too.
#[cfg(test)]
#[path = "../tests/support/script.rs"]
mod test_support;

pub use hub::{CreateSession, Hub, HubError, HubEvent, HubOptions, RestoreOutcome, SessionView};
pub use server::{ServerOptions, router, serve};

/// Make rustls on ring this process's default crypto provider, so any
/// `reqwest` client built without its own TLS config (reqwest is compiled
/// with `rustls-no-provider`) has one. Idempotent; a provider installed
/// first stays.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

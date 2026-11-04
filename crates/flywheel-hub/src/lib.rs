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
//! Operator docs: `docs/hub.md` (front door) and `docs/hub/*.md`.

pub mod activity;
pub mod activity_cli;
pub mod agent;
pub mod agent_handoff;
pub mod agent_handoff_cli;
pub mod agentlog;
pub mod attach;
pub mod auth;
pub mod browser;
pub mod cli;
pub mod commands;
pub mod commands_cli;
pub mod config;
pub mod config_cli;
pub mod daemon;
pub mod detect;
pub mod doctor;
pub mod endpoint;
pub mod fanout;
pub mod fanout_cli;
pub mod flywheel;
pub mod git;
pub mod handoff;
pub mod hub;
pub mod layout;
pub mod lifecycle;
pub mod machine_cli;
pub mod machines;
pub mod net;
pub mod notify;
pub mod paths;
pub mod peers;
pub mod perms;
pub mod perms_cli;
pub mod plugin_cli;
pub mod plugins;
pub mod pty;
pub mod queue;
pub mod queue_cli;
pub mod record;
pub mod remote;
pub mod report;
pub mod restore;
pub mod review;
pub mod review_cli;
pub mod script;
pub mod search;
pub mod search_cli;
pub mod server;
pub mod settings;
pub mod share;
pub mod share_cli;
pub mod state;
pub mod status;
pub mod template_cli;
pub mod tls;
pub mod tls_cli;
pub mod transcript;
pub mod upgrade;
pub mod usage;
pub mod util;
pub mod vscode;
pub mod vt;
pub mod wait;
pub mod web;

/// `docs/hub/*.md` checked against the CLI, hub.toml, keymap, and routes.
#[cfg(test)]
mod docs_drift;
/// The integration tests' helpers (stand-in scripts), for unit tests too.
#[cfg(test)]
mod test_golden;
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

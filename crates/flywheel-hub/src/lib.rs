//! flywheel hub: a web workspace for AI coding agents.
//!
//! The hub server owns every terminal: each session is a real PTY whose
//! process, shell state, and scrollback live in the server, so browsers (a
//! laptop, a phone) attach and detach without disturbing it, and many
//! clients can watch one session at once. Sessions belong to workspaces --
//! a name plus one or more git repos.
//!
//! A sibling consumer like `flywheel-worktree`: the `flywheel` crate routes
//! `fw hub <verb>` to [`cli::main`]; nothing below depends on this crate.
//! Operator doc: `docs/hub.md`.

pub mod agent;
pub mod auth;
pub mod cli;
pub mod hub;
pub mod paths;
pub mod pty;
pub mod scrollback;
pub mod server;
pub mod state;
pub mod web;

pub use hub::{CreateSession, Hub, HubError, HubOptions, SessionView};
pub use server::{ServerOptions, router, serve};

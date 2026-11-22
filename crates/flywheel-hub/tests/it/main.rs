//! The flywheel-hub integration tests, one binary: each module is a former
//! `tests/<name>.rs` file. Shared helpers are wired once here.

#[path = "../support/mod.rs"]
mod support;

mod activity;
mod broadcast;
mod commands;
mod digest;
mod endings;
mod event_log;
mod glyphs;
mod handoff;
mod labels;
mod launcher;
mod layout;
mod machines;
mod notify;
mod perms;
mod plugins;
mod pr;
mod queue;
mod record;
mod remote;
mod review;
mod scale;
mod search;
mod server;
mod settings;
mod share;
mod stalled_readers;
mod status;
mod telemetry;
mod tls;
mod watches;
mod weak_link;
mod worktrees;

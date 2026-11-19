//! `fw hub change adopt|ls|close`: worktrees the hub tracks apart from a
//! session's own (see [`crate::changes`]).

use crate::endpoint::{Endpoint, EndpointArgs};
use crate::script::query_escape;
use crate::util::{fail, print_json, str_field};
use serde_json::{Value, json};
use std::io::Write;

#[derive(Debug, clap::Args)]
pub(crate) struct ChangeArgs {
    #[command(subcommand)]
    command: ChangeCommand,
}

#[derive(Debug, clap::Subcommand)]
enum ChangeCommand {
    /// Track a worktree made outside the hub (plain git worktree add) as
    /// if a session had made it: its counts show and its work is never
    /// removed.
    Adopt(AdoptArgs),
    /// Tracked changes with uncommitted, untracked, and unmerged counts;
    /// --untracked also lists the workspaces' worktrees nothing tracks.
    Ls(LsArgs),
    /// Close a change: its checklist (--dry-run), or the cleanups that
    /// lose nothing (see fw hub close).
    Close(CloseArgs),
}

#[derive(Debug, clap::Args)]
struct AdoptArgs {
    /// The worktree's directory.
    path: String,
    /// The branch it merges into (default: worked out from git).
    #[arg(long, value_name = "BRANCH")]
    base: Option<String>,
    /// A session that works in it (id, prefix, or name).
    #[arg(long, value_name = "SESSION")]
    session: Option<String>,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
struct LsArgs {
    /// Also list the workspaces' worktrees nothing tracks.
    #[arg(long)]
    untracked: bool,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Debug, clap::Args)]
struct CloseArgs {
    /// The change's id (or 4+ of it).
    change: String,
    /// Remove its worktree: only if clean; its branch is kept unless
    /// fully merged.
    #[arg(long)]
    remove_worktree: bool,
    /// Delete its own target/.
    #[arg(long)]
    clean_build: bool,
    /// A final report to keep.
    #[arg(long, value_name = "PATH")]
    report: Option<String>,
    /// Print the checklist only.
    #[arg(long)]
    dry_run: bool,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

pub(crate) fn main(args: &ChangeArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match run(args, out) {
        Ok(code) => code,
        Err(message) => fail(err, &message),
    }
}

fn absolute(path: &str) -> String {
    std::path::Path::new(path)
        .canonicalize()
        .map_or_else(|_| path.to_owned(), |p| p.to_string_lossy().into_owned())
}

fn run(args: &ChangeArgs, out: &mut dyn Write) -> Result<i32, String> {
    match &args.command {
        ChangeCommand::Adopt(a) => {
            let conn = Endpoint::resolve(&a.endpoint)?;
            let change = conn.blocking_call(
                "POST",
                "/api/changes/adopt",
                Some(&json!({ "path": absolute(&a.path), "base": a.base, "session": a.session })),
            )?;
            if a.json {
                print_json(out, &change);
            } else {
                let _ = writeln!(
                    out,
                    "{}  {}  {} -> {} (base {})",
                    str_field(&change, "id"),
                    str_field(&change["worktree"], "path"),
                    str_field(&change["worktree"], "branch"),
                    str_field(&change["worktree"], "base"),
                    str_field(&change, "base_from")
                );
            }
            Ok(0)
        }
        ChangeCommand::Ls(a) => {
            let conn =
                Endpoint::resolve(&a.endpoint)?.with_timeout(std::time::Duration::from_secs(120));
            let view = conn.blocking_call(
                "GET",
                &format!(
                    "/api/changes{}",
                    if a.untracked { "?untracked=1" } else { "" }
                ),
                None,
            )?;
            if a.json {
                print_json(out, &view);
                return Ok(0);
            }
            let changes = view["changes"].as_array().cloned().unwrap_or_default();
            if changes.is_empty() {
                let _ = writeln!(out, "no adopted worktrees");
            }
            for c in &changes {
                let check = &c["check"];
                let _ = writeln!(
                    out,
                    "{}  {}  {} (base {})  {} dirty, {} untracked, {} unmerged{}",
                    str_field(c, "id"),
                    str_field(&c["worktree"], "path"),
                    str_field(&c["worktree"], "branch"),
                    str_field(&c["worktree"], "base"),
                    check["dirty"],
                    check["untracked"],
                    check["unmerged"],
                    c.get("session_name")
                        .and_then(Value::as_str)
                        .map(|n| format!("  session {n}"))
                        .unwrap_or_default()
                );
            }
            let untracked = view["untracked"].as_array().cloned().unwrap_or_default();
            if a.untracked {
                let _ = writeln!(out, "not tracked: {}", untracked.len());
                for w in &untracked {
                    let _ = writeln!(
                        out,
                        "  {}  {}",
                        str_field(w, "path"),
                        w["branch"].as_str().unwrap_or("(detached)")
                    );
                }
            }
            Ok(0)
        }
        ChangeCommand::Close(a) => {
            let conn =
                Endpoint::resolve(&a.endpoint)?.with_timeout(std::time::Duration::from_secs(180));
            let done = conn.blocking_call(
                "POST",
                &format!("/api/changes/{}/close", query_escape(&a.change)),
                Some(&json!({
                    "worktree": if a.remove_worktree { "remove" } else { "keep" },
                    "build": if a.clean_build { "clean" } else { "keep" },
                    "report": a.report.as_deref().map(absolute),
                    "dry_run": a.dry_run,
                })),
            )?;
            if a.json {
                print_json(out, &done);
            } else {
                let _ = writeln!(out, "{}", str_field(&done, "text"));
            }
            Ok(0)
        }
    }
}

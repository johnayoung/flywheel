//! `fw hub close SESSION [--remove-worktree] [--clean-build] [--report
//! PATH] [--dry-run]`: the operator's close (see [`crate::close`]): the
//! session's checklist, or its end at rest with the cleanups that lose
//! nothing.

use crate::endpoint::{Endpoint, EndpointArgs};
use crate::script::{find_session, query_escape};
use crate::util::{fail, print_json, str_field};
use serde_json::json;
use std::io::Write;

#[derive(Debug, clap::Args)]
pub(crate) struct CloseArgs {
    /// The session (id, unique id prefix, or name).
    session: String,
    /// Remove its worktree: only if clean and unshared; its branch is
    /// kept unless fully merged.
    #[arg(long)]
    remove_worktree: bool,
    /// Delete its own target/ (never `cargo clean`).
    #[arg(long)]
    clean_build: bool,
    /// Its final status file, kept by the hub.
    #[arg(long, value_name = "PATH")]
    report: Option<String>,
    /// Print the checklist only; end nothing.
    #[arg(long)]
    dry_run: bool,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

pub(crate) fn close(args: &CloseArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match run(args) {
        Ok(view) => {
            if args.json {
                print_json(out, &view);
            } else {
                let _ = writeln!(out, "{}", str_field(&view, "text"));
            }
            0
        }
        Err(message) => fail(err, &message),
    }
}

fn run(args: &CloseArgs) -> Result<serde_json::Value, String> {
    let conn = Endpoint::resolve(&args.endpoint)?.with_timeout(std::time::Duration::from_secs(180));
    let (id, _) = find_session(&conn, &args.session)?;
    let report = args.report.as_deref().map(|p| {
        std::path::Path::new(p)
            .canonicalize()
            .map_or_else(|_| p.to_owned(), |c| c.to_string_lossy().into_owned())
    });
    conn.blocking_call(
        "POST",
        &format!("/api/sessions/{}/close", query_escape(&id)),
        Some(&json!({
            "worktree": if args.remove_worktree { "remove" } else { "keep" },
            "build": if args.clean_build { "clean" } else { "keep" },
            "report": report,
            "dry_run": args.dry_run,
        })),
    )
}
